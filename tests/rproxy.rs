//! The controller against a real rproxy (`RPROXY_BIN`, rproxy-api v0.4 with rule
//! sets): rendered rule sets are accepted as they are, traffic follows the
//! Gateway API semantics, and a restarted rproxy gets its set again.
//!
//! Skipped without `RPROXY_BIN` (fails instead with `RPROXY_TEST_REQUIRE=1`, as
//! in CI).

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use rproxy_gateway::controller::provision::Endpoint;
use rproxy_gateway::controller::status::PodSync;
use rproxy_gateway::controller::{Applied, sync_pod};
use rproxy_gateway::render::{self, world::World};
use rproxy_gateway::rproxy::client::Client;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "test-token-0123456789";

fn rproxy_bin() -> Option<PathBuf> {
	match std::env::var_os("RPROXY_BIN") {
		Some(p) => Some(PathBuf::from(p)),
		None if std::env::var("RPROXY_TEST_REQUIRE").as_deref() == Ok("1") => panic!("RPROXY_BIN is not set"),
		None => {
			eprintln!("RPROXY_BIN not set; skipped");
			None
		}
	}
}

fn free_port() -> u16 {
	std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Rproxy {
	child: Child,
	api: u16,
	dir: PathBuf,
}

impl Rproxy {
	fn start(bin: &Path, dir: &Path, api: u16) -> Rproxy {
		let tokens = dir.join("tokens.yaml");
		std::fs::write(&tokens, rproxy_gateway::controller::bootstrap::token_file(TOKEN)).unwrap();
		let log = std::fs::File::create(dir.join(format!("rproxy-{api}.log"))).unwrap();
		let child = Command::new(bin)
			.env("RPROXY_API_ADDR", "127.0.0.1")
			.env("RPROXY_API_PORT", api.to_string())
			.env("RPROXY_TOKEN_FILE", &tokens)
			.env("RPROXY_LOG_LEVEL", "info")
			.stdout(Stdio::from(log.try_clone().unwrap()))
			.stderr(Stdio::from(log))
			.spawn()
			.expect("start rproxy");
		Rproxy { child, api, dir: dir.to_path_buf() }
	}

	async fn wait(&self) {
		for _ in 0..100 {
			if tokio::net::TcpStream::connect(("127.0.0.1", self.api)).await.is_ok() {
				return;
			}
			tokio::time::sleep(Duration::from_millis(100)).await;
		}
		panic!("rproxy did not start: {}", std::fs::read_to_string(self.dir.join(format!("rproxy-{}.log", self.api))).unwrap_or_default());
	}
}

impl Drop for Rproxy {
	fn drop(&mut self) {
		let _ = self.child.kill();
		let _ = self.child.wait();
	}
}

/// A backend answering with what it got: `<path>|<host>|<x-test header>`.
async fn echo_backend() -> u16 {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let port = listener.local_addr().unwrap().port();
	tokio::spawn(async move {
		loop {
			let Ok((tcp, _)) = listener.accept().await else { continue };
			tokio::spawn(async move {
				let svc = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
					let h = |n: &str| req.headers().get(n).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
					let body = format!("{}|{}|{}", req.uri().path(), h("host"), h("x-test"));
					Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::from(body))))
				});
				let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tcp), svc).await;
			});
		}
	});
	port
}

/// A UDP echo.
async fn udp_echo() -> u16 {
	let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
	let port = sock.local_addr().unwrap().port();
	tokio::spawn(async move {
		let mut buf = [0u8; 2048];
		loop {
			if let Ok((n, from)) = sock.recv_from(&mut buf).await {
				let _ = sock.send_to(&buf[..n], from).await;
			}
		}
	});
	port
}

/// One HTTP/1.1 request over a plain connection: (status, headers + body text).
async fn get(port: u16, host: &str, path: &str) -> (u16, String) {
	let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
	tcp.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
	let mut out = vec![];
	tcp.read_to_end(&mut out).await.unwrap();
	let text = String::from_utf8_lossy(&out).to_string();
	let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
	(status, text)
}

fn indent(s: &str) -> String {
	s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

#[tokio::test]
async fn rule_sets_on_a_real_rproxy() {
	let Some(bin) = rproxy_bin() else { return };
	let _ = rustls::crypto::ring::default_provider().install_default();
	let dir = std::env::temp_dir().join(format!("rproxy-gateway-e2e-{}", std::process::id()));
	let _ = std::fs::remove_dir_all(&dir);
	let certs = dir.join("certs");
	std::fs::create_dir_all(&certs).unwrap();

	let backend = echo_backend().await;
	let udp = udp_echo().await;
	let (http_port, https_port, tcp_port, udp_port, api) = (free_port(), free_port(), free_port(), free_port(), free_port());
	let key = rcgen::KeyPair::generate().unwrap();
	let cert = rcgen::CertificateParams::new(vec!["secure.example.com".to_string()]).unwrap().self_signed(&key).unwrap();
	let yaml = format!(
		r#"
apiVersion: v1
kind: Service
metadata: {{name: echo, namespace: default}}
spec: {{ports: [{{name: http, port: 80}}]}}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {{name: echo-1, namespace: default, labels: {{kubernetes.io/service-name: echo}}}}
addressType: IPv4
ports: [{{name: http, port: {backend}}}]
endpoints: [{{addresses: [127.0.0.1]}}]
---
apiVersion: v1
kind: Service
metadata: {{name: udp, namespace: default}}
spec: {{ports: [{{port: 53, protocol: UDP}}]}}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {{name: udp-1, namespace: default, labels: {{kubernetes.io/service-name: udp}}}}
addressType: IPv4
ports: [{{port: {udp}, protocol: UDP}}]
endpoints: [{{addresses: [127.0.0.1]}}]
---
apiVersion: v1
kind: Secret
metadata: {{name: cert, namespace: default}}
stringData:
  tls.crt: |
{crt}
  tls.key: |
{key}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {{name: gw, namespace: default, generation: 1}}
spec:
  gatewayClassName: rproxy
  listeners:
    - {{name: http, port: {http_port}, protocol: HTTP}}
    - {{name: https, port: {https_port}, protocol: HTTPS, tls: {{certificateRefs: [{{name: cert}}]}}}}
    - {{name: tcp, port: {tcp_port}, protocol: TCP}}
    - {{name: udp, port: {udp_port}, protocol: UDP}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {{name: web, namespace: default}}
spec:
  parentRefs: [{{name: gw, sectionName: http}}, {{name: gw, sectionName: https}}]
  hostnames: [web.example.com, secure.example.com]
  rules:
    - matches: [{{path: {{type: PathPrefix, value: /app}}}}]
      filters: [{{type: RequestHeaderModifier, requestHeaderModifier: {{set: [{{name: X-Test, value: added}}]}}}}]
      backendRefs: [{{name: echo, port: 80}}]
    - matches: [{{path: {{type: Exact, value: /old}}}}]
      filters: [{{type: RequestRedirect, requestRedirect: {{scheme: https, statusCode: 301}}}}]
    - matches: [{{path: {{type: PathPrefix, value: /rewrite}}}}]
      filters: [{{type: URLRewrite, urlRewrite: {{path: {{type: ReplacePrefixMatch, replacePrefixMatch: /new}}}}}}]
      backendRefs: [{{name: echo, port: 80}}]
    - matches: [{{path: {{type: PathPrefix, value: /none}}}}]
      backendRefs: [{{name: missing, port: 80}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {{name: tcp, namespace: default}}
spec:
  parentRefs: [{{name: gw, sectionName: tcp}}]
  rules: [{{backendRefs: [{{name: echo, port: 80}}]}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: UDPRoute
metadata: {{name: udp, namespace: default}}
spec:
  parentRefs: [{{name: gw, sectionName: udp}}]
  rules: [{{backendRefs: [{{name: udp, port: 53}}]}}]
"#,
		crt = indent(&cert.pem()),
		key = indent(&key.serialize_pem()),
	);
	let mut world = World::default();
	world.read_manifests(&yaml).unwrap();
	let opts = render::Options { listen_addrs: vec!["127.0.0.1".into()], cert_dir: certs.display().to_string(), ..Default::default() };
	let plan = render::render_gateway(&world, &world.gateways[0], &opts);
	for (name, content) in &plan.files {
		std::fs::write(certs.join(name), content).unwrap();
	}
	// certsync's GET /files (the directory the kubelet mounts the Secret into)
	let certsync = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let certsync_port = certsync.local_addr().unwrap().port();
	tokio::spawn(rproxy_gateway::certsync::serve(certsync, certs.clone()));

	let rp = Rproxy::start(&bin, &dir, api);
	rp.wait().await;
	let client = Client::new(None, TOKEN).unwrap();
	let caps = client.capabilities(SocketAddr::from(([127, 0, 0, 1], api))).await.unwrap();
	assert!(caps.feature("rulesets"), "this rproxy has no rule sets: {caps:?}");
	let ep = Endpoint {
		pod: "rproxy".into(),
		uid: "1".into(),
		ip: "127.0.0.1".into(),
		host_ip: None,
		api_port: api,
		certsync_port,
		certs: None,
	};
	let mut applied = Applied::new();
	let mut caps_cache = HashMap::new();
	let (r, _) = sync_pod(&client, &ep, &plan, &mut applied, &mut caps_cache).await;
	let PodSync::Synced(views) = r else { panic!("{r:?}\nrules: {}", serde_json::to_string_pretty(&plan.rules_json()).unwrap()) };
	for v in &views {
		assert_eq!(v.state, "running", "{} {:?} {:?}", v.key(), v.error, v.conditions);
		assert_eq!(v.condition("Programmed").status, "True", "{}", v.key());
	}
	assert_eq!(views.len(), 4);

	// HTTP: host and path prefix (at a / boundary), a header, 404, a redirect, a rewrite, 500
	let (s, body) = get(http_port, "web.example.com", "/app/x").await;
	assert_eq!(s, 200, "{body}");
	assert!(body.ends_with("/app/x|web.example.com|added"), "{body}");
	assert_eq!(get(http_port, "web.example.com", "/app").await.0, 200);
	assert_eq!(get(http_port, "web.example.com", "/appx").await.0, 404);
	assert_eq!(get(http_port, "other.example.com", "/app").await.0, 404);
	let (s, body) = get(http_port, "web.example.com", "/old?q=1").await;
	assert_eq!(s, 301, "{body}");
	assert!(body.to_ascii_lowercase().contains("location: https://web.example.com/old?q=1"), "{body}");
	let (_, body) = get(http_port, "web.example.com", "/rewrite/a").await;
	assert!(body.contains("/new/a|"), "{body}");
	let (_, body) = get(http_port, "web.example.com", "/rewrite").await;
	assert!(body.contains("/new|"), "{body}");
	assert_eq!(get(http_port, "web.example.com", "/none").await.0, 500);

	// HTTPS with SNI
	{
		let mut roots = rustls::RootCertStore::empty();
		roots.add(cert.der().clone()).unwrap();
		let config = rustls::ClientConfig::builder().with_root_certificates(roots).with_no_client_auth();
		let tls = tokio_rustls::TlsConnector::from(Arc::new(config));
		let tcp = tokio::net::TcpStream::connect(("127.0.0.1", https_port)).await.unwrap();
		let mut s = tls.connect(rustls_pki_types::ServerName::try_from("secure.example.com").unwrap(), tcp).await.unwrap();
		s.write_all(b"GET /app/s HTTP/1.1\r\nHost: secure.example.com\r\nConnection: close\r\n\r\n").await.unwrap();
		let mut out = vec![];
		let _ = s.read_to_end(&mut out).await;
		let text = String::from_utf8_lossy(&out);
		assert!(text.starts_with("HTTP/1.1 200"), "{text}");
		assert!(text.contains("/app/s|secure.example.com|added"), "{text}");
	}

	// TCP and UDP
	let (_, body) = get(tcp_port, "anything", "/via-tcp").await;
	assert!(body.contains("/via-tcp|anything|"), "{body}");
	let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
	sock.send_to(b"ping", ("127.0.0.1", udp_port)).await.unwrap();
	let mut buf = [0u8; 64];
	let (n, _) = tokio::time::timeout(Duration::from_secs(5), sock.recv_from(&mut buf)).await.expect("UDP echo").unwrap();
	assert_eq!(&buf[..n], b"ping");

	// nothing changed: no PUT; the set still has the etag of the last PUT
	let (r, again) = sync_pod(&client, &ep, &plan, &mut applied, &mut caps_cache).await;
	assert!(matches!(r, PodSync::Synced(_)) && !again);

	// rproxy restarts: the set is gone and is PUT again
	drop(rp);
	let rp = Rproxy::start(&bin, &dir, api);
	rp.wait().await;
	let mut caps_cache = HashMap::new();
	let (r, _) = sync_pod(&client, &ep, &plan, &mut applied, &mut caps_cache).await;
	assert!(matches!(&r, PodSync::Synced(v) if v.iter().all(|v| v.state == "running")), "{r:?}");
	assert_eq!(get(http_port, "web.example.com", "/app/again").await.0, 200);
	drop(rp);
	let _ = std::fs::remove_dir_all(&dir);
}
