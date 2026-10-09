// Stress test for rproxy-gateway (scripts/stress.sh): random traffic through a
// Gateway, every byte checked.
//
//	stress echo                      # in the cluster: TCP :9000, UDP :9001, HTTP :8080 echo servers
//	stress load -addr 10.96.0.10 ... # on the runner: random HTTP / TCP / UDP requests
//
// Each request picks its protocol at random, with a random payload of random size
// (HTTP 0-64 KiB body on a random method and path, TCP 1-32 KiB, UDP 1-1400 bytes);
// the echo servers send back exactly what they got, so any changed, cut or lost
// byte shows. The seed is printed so a run can be repeated. The summary is JSON on
// stdout; the exit status is 1 when a check fails.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"math/rand/v2"
	"net"
	"net/http"
	"os"
	"sort"
	"strconv"
	"sync"
	"sync/atomic"
	"time"
)

func main() {
	if len(os.Args) < 2 {
		fmt.Fprintln(os.Stderr, "usage: stress echo | stress load [flags]")
		os.Exit(2)
	}
	switch os.Args[1] {
	case "echo":
		fs := flag.NewFlagSet("echo", flag.ExitOnError)
		tcp := fs.String("tcp", ":9000", "TCP echo address")
		udp := fs.String("udp", ":9001", "UDP echo address")
		web := fs.String("http", ":8080", "HTTP echo address")
		fs.Parse(os.Args[2:])
		echo(*tcp, *udp, *web)
	case "load":
		os.Exit(load(os.Args[2:]))
	default:
		fmt.Fprintln(os.Stderr, "unknown command", os.Args[1])
		os.Exit(2)
	}
}

// ---- echo servers ----

func echo(tcpAddr, udpAddr, httpAddr string) {
	go func() {
		ln, err := net.Listen("tcp", tcpAddr)
		if err != nil {
			panic(err)
		}
		for {
			c, err := ln.Accept()
			if err != nil {
				continue
			}
			go func(c net.Conn) {
				defer c.Close()
				_, _ = io.Copy(c, c)
			}(c)
		}
	}()
	go func() {
		pc, err := net.ListenPacket("udp", udpAddr)
		if err != nil {
			panic(err)
		}
		buf := make([]byte, 65536)
		for {
			n, from, err := pc.ReadFrom(buf)
			if err != nil {
				continue
			}
			_, _ = pc.WriteTo(buf[:n], from)
		}
	}()
	mux := http.NewServeMux()
	mux.HandleFunc("/", func(w http.ResponseWriter, r *http.Request) {
		body, err := io.ReadAll(r.Body)
		if err != nil {
			http.Error(w, err.Error(), 400)
			return
		}
		w.Header().Set("Content-Type", "application/octet-stream")
		w.Header().Set("X-Method", r.Method)
		w.Header().Set("X-Path", r.URL.Path)
		_, _ = w.Write(body)
	})
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) { _, _ = w.Write([]byte("ok")) })
	panic(http.ListenAndServe(httpAddr, mux))
}

// ---- load generator ----

type kind int

const (
	kHTTP kind = iota
	kTCP
	kUDP
)

var kindNames = []string{"http", "tcp", "udp"}

type stats struct {
	mu        sync.Mutex
	latencies [3][]time.Duration
	ok        [3]int64
	errs      [3]int64
	mismatch  [3]int64
	bytes     [3]int64
	firstErr  [3]string
}

func (s *stats) record(k kind, d time.Duration, n int, err error, mism bool) {
	s.mu.Lock()
	defer s.mu.Unlock()
	switch {
	case mism:
		s.mismatch[k]++
		if s.firstErr[k] == "" && err != nil {
			s.firstErr[k] = err.Error()
		}
	case err != nil:
		s.errs[k]++
		if s.firstErr[k] == "" {
			s.firstErr[k] = err.Error()
		}
	default:
		s.ok[k]++
		s.bytes[k] += int64(n)
		s.latencies[k] = append(s.latencies[k], d)
	}
}

var errMismatch = errors.New("payload changed")

func randBytes(r *rand.Rand, n int) []byte {
	b := make([]byte, n)
	for i := range b {
		b[i] = byte(r.Uint32())
	}
	return b
}

func load(args []string) int {
	fs := flag.NewFlagSet("load", flag.ExitOnError)
	addr := fs.String("addr", "", "the Gateway's address")
	host := fs.String("host", "stress.example.com", "HTTP Host")
	httpPort := fs.Int("http-port", 80, "")
	tcpPort := fs.Int("tcp-port", 9000, "")
	udpPort := fs.Int("udp-port", 9001, "")
	total := fs.Int("total", 120000, "requests in all (at least 100000 asked by the owner)")
	conc := fs.Int("concurrency", 64, "requests in flight")
	seed := fs.Uint64("seed", uint64(time.Now().UnixNano()), "random seed (printed)")
	maxUDPLoss := fs.Float64("max-udp-loss", 0.005, "allowed share of UDP datagrams without an answer")
	fs.Parse(args)
	if *addr == "" {
		fmt.Fprintln(os.Stderr, "-addr is required")
		return 2
	}
	fmt.Fprintf(os.Stderr, "seed %d, %d requests, %d in flight\n", *seed, *total, *conc)

	transport := &http.Transport{MaxIdleConnsPerHost: *conc, MaxConnsPerHost: *conc, IdleConnTimeout: 30 * time.Second}
	client := &http.Client{Transport: transport, Timeout: 30 * time.Second}
	base := fmt.Sprintf("http://%s:%d", *addr, *httpPort)
	tcpAddr := net.JoinHostPort(*addr, strconv.Itoa(*tcpPort))
	udpAddr := net.JoinHostPort(*addr, strconv.Itoa(*udpPort))

	var st stats
	var next atomic.Int64
	start := time.Now()
	var wg sync.WaitGroup
	for w := 0; w < *conc; w++ {
		wg.Add(1)
		go func(w int) {
			defer wg.Done()
			r := rand.New(rand.NewPCG(*seed, uint64(w)))
			for {
				i := next.Add(1)
				if i > int64(*total) {
					return
				}
				k := kind(r.IntN(3))
				t0 := time.Now()
				var n int
				var err error
				switch k {
				case kHTTP:
					n, err = doHTTP(client, base, *host, r)
				case kTCP:
					n, err = doTCP(tcpAddr, r)
				case kUDP:
					n, err = doUDP(udpAddr, r)
				}
				st.record(k, time.Since(t0), n, err, errors.Is(err, errMismatch))
				if i%10000 == 0 {
					fmt.Fprintf(os.Stderr, "%d/%d after %s\n", i, *total, time.Since(start).Round(time.Second))
				}
			}
		}(w)
	}
	wg.Wait()
	elapsed := time.Since(start)
	return report(&st, *seed, *total, *conc, elapsed, *maxUDPLoss)
}

func doHTTP(c *http.Client, base, host string, r *rand.Rand) (int, error) {
	methods := []string{"GET", "POST", "PUT", "PATCH", "DELETE"}
	m := methods[r.IntN(len(methods))]
	var body []byte
	if m != "GET" && m != "DELETE" {
		body = randBytes(r, r.IntN(64*1024+1))
	}
	path := fmt.Sprintf("/stress/%x/%d", r.Uint64(), r.IntN(1000))
	req, err := http.NewRequestWithContext(context.Background(), m, base+path, bytes.NewReader(body))
	if err != nil {
		return 0, err
	}
	req.Host = host
	resp, err := c.Do(req)
	if err != nil {
		return 0, err
	}
	defer resp.Body.Close()
	got, err := io.ReadAll(resp.Body)
	if err != nil {
		return 0, fmt.Errorf("reading the body: %w", err)
	}
	if resp.StatusCode != 200 {
		return 0, fmt.Errorf("status %d", resp.StatusCode)
	}
	if resp.Header.Get("X-Path") != path || resp.Header.Get("X-Method") != m {
		return 0, fmt.Errorf("%w: method/path %q %q", errMismatch, resp.Header.Get("X-Method"), resp.Header.Get("X-Path"))
	}
	if !bytes.Equal(got, body) {
		return 0, fmt.Errorf("%w: http body %d bytes, sent %d", errMismatch, len(got), len(body))
	}
	return len(body), nil
}

func doTCP(addr string, r *rand.Rand) (int, error) {
	payload := randBytes(r, 1+r.IntN(32*1024))
	c, err := net.DialTimeout("tcp", addr, 5*time.Second)
	if err != nil {
		return 0, err
	}
	defer c.Close()
	_ = c.SetDeadline(time.Now().Add(15 * time.Second))
	errc := make(chan error, 1)
	go func() {
		_, err := c.Write(payload)
		if err == nil {
			err = c.(*net.TCPConn).CloseWrite()
		}
		errc <- err
	}()
	got, err := io.ReadAll(c)
	if werr := <-errc; werr != nil {
		return 0, fmt.Errorf("write: %w", werr)
	}
	if err != nil {
		return 0, fmt.Errorf("read: %w", err)
	}
	if !bytes.Equal(got, payload) {
		return 0, fmt.Errorf("%w: tcp %d bytes back, sent %d", errMismatch, len(got), len(payload))
	}
	return len(payload), nil
}

func doUDP(addr string, r *rand.Rand) (int, error) {
	payload := randBytes(r, 1+r.IntN(1400))
	c, err := net.Dial("udp", addr)
	if err != nil {
		return 0, err
	}
	defer c.Close()
	if _, err := c.Write(payload); err != nil {
		return 0, err
	}
	_ = c.SetReadDeadline(time.Now().Add(2 * time.Second))
	buf := make([]byte, 2048)
	n, err := c.Read(buf)
	if err != nil {
		return 0, fmt.Errorf("no answer: %w", err)
	}
	if !bytes.Equal(buf[:n], payload) {
		return 0, fmt.Errorf("%w: udp %d bytes back, sent %d", errMismatch, n, len(payload))
	}
	return len(payload), nil
}

type kindReport struct {
	Requests   int64   `json:"requests"`
	OK         int64   `json:"ok"`
	Errors     int64   `json:"errors"`
	Mismatches int64   `json:"mismatches"`
	MiB        float64 `json:"mib"`
	P50ms      float64 `json:"p50_ms"`
	P99ms      float64 `json:"p99_ms"`
	MaxMs      float64 `json:"max_ms"`
	FirstError string  `json:"first_error,omitempty"`
}

func pct(d []time.Duration, p float64) float64 {
	if len(d) == 0 {
		return 0
	}
	i := int(float64(len(d)-1) * p)
	return float64(d[i].Microseconds()) / 1000
}

func report(st *stats, seed uint64, total, conc int, elapsed time.Duration, maxUDPLoss float64) int {
	out := map[string]any{"seed": seed, "total": total, "concurrency": conc, "seconds": elapsed.Seconds(), "rps": float64(total) / elapsed.Seconds()}
	failed := false
	var reasons []string
	for k := kHTTP; k <= kUDP; k++ {
		l := st.latencies[k]
		sort.Slice(l, func(i, j int) bool { return l[i] < l[j] })
		kr := kindReport{
			Requests: st.ok[k] + st.errs[k] + st.mismatch[k], OK: st.ok[k], Errors: st.errs[k], Mismatches: st.mismatch[k],
			MiB: float64(st.bytes[k]) / (1 << 20), P50ms: pct(l, 0.5), P99ms: pct(l, 0.99), FirstError: st.firstErr[k],
		}
		if len(l) > 0 {
			kr.MaxMs = float64(l[len(l)-1].Microseconds()) / 1000
		}
		out[kindNames[k]] = kr
		if kr.Mismatches > 0 {
			failed = true
			reasons = append(reasons, fmt.Sprintf("%s: %d changed payloads", kindNames[k], kr.Mismatches))
		}
		if k == kUDP {
			if kr.Requests > 0 && float64(kr.Errors)/float64(kr.Requests) > maxUDPLoss {
				failed = true
				reasons = append(reasons, fmt.Sprintf("udp: %d of %d unanswered", kr.Errors, kr.Requests))
			}
		} else if kr.Errors > 0 {
			failed = true
			reasons = append(reasons, fmt.Sprintf("%s: %d errors (%s)", kindNames[k], kr.Errors, kr.FirstError))
		}
	}
	out["passed"] = !failed
	out["failures"] = reasons
	enc := json.NewEncoder(os.Stdout)
	enc.SetIndent("", "  ")
	_ = enc.Encode(out)
	if failed {
		return 1
	}
	return 0
}
