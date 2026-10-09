#!/bin/bash
# Stress test on kind (the CI runner VM, like e2e.sh): random HTTP / TCP / UDP traffic through a
# Gateway, every byte checked (test/stress). After the same setup as scripts/e2e.sh it adds a
# Gateway "stress" with echo servers behind HTTP :80, TCP :9000 and UDP :9001 and runs
# `stress load` from the runner: TOTAL requests (default 120000), each one of the three
# protocols at random with a random payload of random size, CONCURRENCY in flight. It fails on
# any changed payload, on HTTP / TCP errors, or on more than MAX_UDP_LOSS unanswered UDP
# datagrams, and records the rproxy pods' restarts and memory.
#   scripts/stress.sh                         # after the Alpine jobs built dist/amd64/*
#   TOTAL=200000 SEED=42 scripts/stress.sh    # repeat a run with its seed
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER=${CLUSTER:-rproxy-gateway}
TOTAL=${TOTAL:-120000}
CONCURRENCY=${CONCURRENCY:-64}
SEED=${SEED:-$(date +%s)}
MAX_UDP_LOSS=${MAX_UDP_LOSS:-0.005}
work=${RUNNER_TEMP:-/tmp}/rproxy-gateway-stress
mkdir -p "$work"

SETUP_ONLY=1 scripts/e2e.sh

echo "== echo servers (test/stress)"
docker build -q -t rproxy-stress:e2e test/stress > /dev/null
kind load docker-image rproxy-stress:e2e --name "$CLUSTER" > /dev/null

served() {
  if kubectl get crd "$1" -o jsonpath='{.spec.versions[?(@.name=="v1")].served}' | grep -qx true; then echo v1
  else kubectl get crd "$1" -o jsonpath='{.spec.versions[?(@.storage==true)].name}'; fi
}
tcp_v=$(served tcproutes.gateway.networking.k8s.io)
udp_v=$(served udproutes.gateway.networking.k8s.io)

kubectl apply -f - > /dev/null <<YAML
apiVersion: v1
kind: Namespace
metadata: {name: stress}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo, namespace: stress}
spec:
  replicas: 2
  selector: {matchLabels: {app: stress-echo}}
  template:
    metadata: {labels: {app: stress-echo}}
    spec:
      containers:
        - name: echo
          image: rproxy-stress:e2e
          imagePullPolicy: Never
          ports: [{containerPort: 8080}, {containerPort: 9000}, {containerPort: 9001, protocol: UDP}]
          readinessProbe: {httpGet: {path: /healthz, port: 8080}, periodSeconds: 2}
---
apiVersion: v1
kind: Service
metadata: {name: echo, namespace: stress}
spec:
  selector: {app: stress-echo}
  ports:
    - {name: http, port: 8080}
    - {name: tcp, port: 9000}
    - {name: udp, port: 9001, protocol: UDP}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: stress, namespace: stress}
spec:
  gatewayClassName: rproxy
  listeners:
    - {name: http, port: 80, protocol: HTTP}
    - {name: tcp, port: 9000, protocol: TCP}
    - {name: udp, port: 9001, protocol: UDP}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: echo, namespace: stress}
spec:
  parentRefs: [{name: stress, sectionName: http}]
  hostnames: [stress.example.com]
  rules: [{backendRefs: [{name: echo, port: 8080}]}]
---
apiVersion: gateway.networking.k8s.io/$tcp_v
kind: TCPRoute
metadata: {name: echo, namespace: stress}
spec:
  parentRefs: [{name: stress, sectionName: tcp}]
  rules: [{backendRefs: [{name: echo, port: 9000}]}]
---
apiVersion: gateway.networking.k8s.io/$udp_v
kind: UDPRoute
metadata: {name: echo, namespace: stress}
spec:
  parentRefs: [{name: stress, sectionName: udp}]
  rules: [{backendRefs: [{name: echo, port: 9001}]}]
YAML
kubectl -n stress rollout status deploy/echo --timeout=180s
kubectl -n stress wait --for=condition=Programmed gateway/stress --timeout=240s
addr=$(kubectl -n stress get gateway stress -o jsonpath='{.status.addresses[0].value}')
echo "Gateway address: $addr"
# the routes are programmed once a request goes through
for i in $(seq 60); do
  curl -sf -H 'Host: stress.example.com' "http://$addr/healthz" > /dev/null && break
  sleep 2
  [ "$i" = 60 ] && { echo "the stress Gateway does not answer"; exit 1; }
done

# every TCP request is a new connection (tens of thousands a minute): give the client the whole
# ephemeral port range and let it reuse TIME_WAIT ports, or connect() fails with EADDRNOTAVAIL
if [ -n "${CI:-}" ]; then
  sudo sysctl -q -w net.ipv4.ip_local_port_range="1024 65535" net.ipv4.tcp_tw_reuse=1
fi

echo "== stress load: $TOTAL requests, $CONCURRENCY in flight, seed $SEED"
(cd test/stress && CGO_ENABLED=0 go build -o "$work/stress" .)
status=0
"$work/stress" load -addr "$addr" -total "$TOTAL" -concurrency "$CONCURRENCY" -seed "$SEED" -max-udp-loss "$MAX_UDP_LOSS" \
  > "$work/stress.json" 2> "$work/stress.log" || status=$?
cat "$work/stress.json"

echo "== rproxy pods after the load"
kubectl -n stress get pods -l gateway.networking.k8s.io/gateway-name=stress \
  -o custom-columns=NAME:.metadata.name,READY:.status.containerStatuses[*].ready,RESTARTS:.status.containerStatuses[*].restartCount | tee "$work/pods.txt"
restarts=$(kubectl -n stress get pods -l gateway.networking.k8s.io/gateway-name=stress -o json | jq '[.items[].status.containerStatuses[].restartCount] | add // 0')
mem=""
for node in $(kind get nodes --name "$CLUSTER"); do
  mem+=$(docker exec "$node" crictl stats -o json 2>/dev/null | jq -r --arg ns stress \
    '.stats[] | select(.attributes.labels["io.kubernetes.pod.namespace"] == $ns and .attributes.labels["io.kubernetes.container.name"] == "rproxy")
     | "\(.attributes.labels["io.kubernetes.pod.name"]): \((.memory.workingSetBytes.value | tonumber) / 1048576 | floor) MiB\n"' || true)
done
# what the memory is: the process (VmRSS) and the container's cgroup (anon, file cache, socket buffers)
for pod in $(kubectl -n stress get pods -l gateway.networking.k8s.io/gateway-name=stress -o name); do
  # shellcheck disable=SC2016 # expanded in the pod
  mem+="${pod#pod/}: $(kubectl -n stress exec "$pod" -c rproxy -- sh -c \
    'grep VmRSS /proc/1/status | tr -s " "; grep -E "^(anon|file|sock|kernel) " /sys/fs/cgroup/memory.stat | while read -r k v; do echo "$k $((v / 1048576)) MiB"; done' \
    2>/dev/null | paste -sd, -)
"
done
# sockets left in rproxy's network namespace after the load (UDP sessions keep one socket each until idle)
for pod in $(kubectl -n stress get pods -l gateway.networking.k8s.io/gateway-name=stress -o name); do
  # shellcheck disable=SC2016 # expanded in the pod
  mem+="${pod#pod/}: $(kubectl -n stress exec "$pod" -c rproxy -- sh -c 'echo "udp sockets $(($(wc -l < /proc/net/udp) - 1)), tcp sockets $(($(wc -l < /proc/net/tcp) - 1))"' 2>/dev/null)
"
done
printf '%s' "$mem" | tee "$work/memory.txt"
# why requests failed, from rproxy's own log (the reasons of http.error / conn.error)
errors=""
for pod in $(kubectl -n stress get pods -l gateway.networking.k8s.io/gateway-name=stress -o name); do
  errors+=$(kubectl -n stress logs "$pod" -c rproxy --tail=-1 2>/dev/null \
    | jq -Rr 'fromjson? | select(.fields.event == "http.error" or .fields.event == "conn.error" or .event == "http.error" or .event == "conn.error")
      | "\(.fields.event // .event) \(.fields.status // .status // "") \(.fields.error // .error // "")"' \
    | sort | uniq -c | sort -rn | head -20)
  errors+=$'\n'
done
printf 'rproxy errors:\n%s' "$errors" | tee "$work/errors.txt"
if [ "$restarts" != 0 ]; then echo "::error::rproxy restarted $restarts times during the load"; status=1; fi

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "## Stress: $TOTAL random requests through rproxy-gateway (seed $SEED, $CONCURRENCY in flight)"
    echo
    jq -r '"passed: \(.passed), \(.seconds | floor) s, \(.rps | floor) req/s\n",
      "| protocol | requests | ok | errors | changed payloads | MiB | p50 ms | p99 ms | max ms |",
      "|---|---|---|---|---|---|---|---|---|",
      (["http","tcp","udp"][] as $k | .[$k] | "| \($k) | \(.requests) | \(.ok) | \(.errors) | \(.mismatches) | \(.mib | floor) | \(.p50_ms) | \(.p99_ms) | \(.max_ms) |"),
      "", (if (.failures // []) | length > 0 then "failures: " + ((.failures) | join("; ")) else "" end)' "$work/stress.json"
    echo
    echo "rproxy restarts: $restarts"
    echo
    echo '```'
    cat "$work/memory.txt"
    cat "$work/errors.txt"
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi
exit "$status"
