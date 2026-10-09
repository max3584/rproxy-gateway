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
/// The UI's read-only token (docs/DESIGN-v0.4.x.md 4.): in the same token file, as the controller writes it.
const UI_TOKEN: &str = "ui-token-0123456789";

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

/// `N` distinct free ports. The listeners are all held until every port is known: binding and
/// dropping one at a time can hand out the same port twice, and two listeners of the Gateway on one
/// port conflict (one rule fewer in the set).
fn free_ports<const N: usize>() -> [u16; N] {
	let held: Vec<std::net::TcpListener> = (0..N).map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap()).collect();
	std::array::from_fn(|i| held[i].local_addr().unwrap().port())
}

struct Rproxy {
	child: Child,
	api: u16,
	dir: PathBuf,
}

impl Rproxy {
	fn start(bin: &Path, dir: &Path, api: u16) -> Rproxy {
		let tokens = dir.join("tokens.yaml");
		use rproxy_gateway::controller::bootstrap;
		std::fs::write(&tokens, bootstrap::token_file(TOKEN, bootstrap::FLEET_RULESETS) + &bootstrap::ui_token_entry(UI_TOKEN)).unwrap();
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

/// An HTTP auth server (Envoy's ext_authz over HTTP): 200 with `x-test: <path>` for
/// `authorization: allow`, 403 for anything else.
async fn auth_backend() -> u16 {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let port = listener.local_addr().unwrap().port();
	tokio::spawn(async move {
		loop {
			let Ok((tcp, _)) = listener.accept().await else { continue };
			tokio::spawn(async move {
				let svc = hyper::service::service_fn(|req: hyper::Request<hyper::body::Incoming>| async move {
					let allowed = req.headers().get("authorization").is_some_and(|v| v == "allow");
					let resp = hyper::Response::builder()
						.status(if allowed { 200 } else { 403 })
						.header("x-test", format!("checked {}", req.uri().path()))
						.body(http_body_util::Full::new(hyper::body::Bytes::from("auth")))
						.unwrap();
					Ok::<_, std::convert::Infallible>(resp)
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
	get_with(port, host, path, "").await
}

/// `get` with more header lines (`extra`, each ending in CRLF).
async fn get_with(port: u16, host: &str, path: &str, extra: &str) -> (u16, String) {
	let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
	tcp.write_all(format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{extra}Connection: close\r\n\r\n").as_bytes()).await.unwrap();
	let mut out = vec![];
	tcp.read_to_end(&mut out).await.unwrap();
	let text = String::from_utf8_lossy(&out).to_string();
	let status = text.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
	(status, text)
}

/// A control API request with a token: the status.
async fn api_status(port: u16, method: &str, path: &str, token: &str, body: &str) -> u16 {
	let mut tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
	let req = format!(
		"{method} {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
		body.len()
	);
	tcp.write_all(req.as_bytes()).await.unwrap();
	let mut out = vec![];
	tcp.read_to_end(&mut out).await.unwrap();
	String::from_utf8_lossy(&out).split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
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
	let auth = auth_backend().await;
	let udp = udp_echo().await;
	let [http_port, https_port, tcp_port, udp_port, api] = free_ports();
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
metadata: {{name: auth, namespace: default}}
spec: {{ports: [{{name: http, port: 80}}]}}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {{name: auth-1, namespace: default, labels: {{kubernetes.io/service-name: auth}}}}
addressType: IPv4
ports: [{{name: http, port: {auth}}}]
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
kind: HTTPRoute
metadata: {{name: newer, namespace: default}}
spec:
  parentRefs: [{{name: gw, sectionName: http}}]
  hostnames: [newer.example.com]
  rules:
    - matches: [{{path: {{type: PathPrefix, value: /add}}}}]
      filters:
        - {{type: RequestHeaderModifier, requestHeaderModifier: {{set: [{{name: X-Test, value: one}}]}}}}
        - {{type: RequestHeaderModifier, requestHeaderModifier: {{add: [{{name: X-Test, value: two}}]}}}}
      backendRefs: [{{name: echo, port: 80}}]
    - matches: [{{path: {{type: PathPrefix, value: /see-other}}}}]
      filters: [{{type: RequestRedirect, requestRedirect: {{path: {{type: ReplaceFullPath, replaceFullPath: /elsewhere}}, statusCode: 303}}}}]
    - matches: [{{path: {{type: PathPrefix, value: /host}}}}]
      filters: [{{type: URLRewrite, urlRewrite: {{hostname: rewritten.example.com}}}}]
      backendRefs: [{{name: echo, port: 80}}]
    - matches: [{{path: {{type: PathPrefix, value: /partial}}}}]
      backendRefs: [{{name: echo, port: 80}}, {{name: missing, port: 80}}]
    - matches: [{{path: {{type: PathPrefix, value: /guarded}}}}]
      filters:
        - type: ExternalAuth
          externalAuth: {{protocol: HTTP, backendRef: {{name: auth, port: 80}}, http: {{path: /check, allowedResponseHeaders: [x-test]}}}}
      backendRefs: [{{name: echo, port: 80}}]
    - matches: [{{path: {{type: PathPrefix, value: /per-backend}}}}]
      backendRefs:
        - name: echo
          port: 80
          filters: [{{type: RequestRedirect, requestRedirect: {{hostname: elsewhere.example.com, statusCode: 302}}}}]
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
	let rp = Rproxy::start(&bin, &dir, api);
	rp.wait().await;
	let client = Client::new(None, TOKEN).unwrap();
	let caps = client.capabilities(SocketAddr::from(([127, 0, 0, 1], api))).await.unwrap();
	assert!(caps.feature("rulesets"), "this rproxy has no rule sets: {caps:?}");
	// rendered for what this rproxy takes, as the controller does
	let features = render::Features::of(&caps);
	let opts = render::Options {
		listen_addrs: vec!["127.0.0.1".into()],
		cert_dir: certs.display().to_string(),
		features: features.clone(),
		..Default::default()
	};
	let plan = render::render_gateway(&world, &world.gateways[0], &opts);
	for (name, content) in &plan.files {
		std::fs::write(certs.join(name), content).unwrap();
		// as the kubelet mounts the Secret (mode 0440): keys readable by nobody else
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(certs.join(name), std::fs::Permissions::from_mode(0o440)).unwrap();
		}
	}
	// certsync's GET /files (the directory the kubelet mounts the Secret into)
	let certsync = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let certsync_port = certsync.local_addr().unwrap().port();
	tokio::spawn(rproxy_gateway::certsync::serve(certsync, certs.clone()));
	let ep = Endpoint {
		pod: "rproxy".into(),
		uid: "1".into(),
		ip: "127.0.0.1".into(),
		host_ip: None,
		api_port: api,
		certsync_port,
		certs: None,
		..Default::default()
	};
	let mut applied = Applied::new();
	let mut caps_cache = HashMap::new();
	let (r, _) = sync_pod(&client, &ep, &plan, &mut applied, &mut caps_cache).await;
	let PodSync::Synced(views) = r else { panic!("{r:?}\nrules: {}", serde_json::to_string_pretty(&plan.rules_json()).unwrap()) };
	for v in &views {
		assert_eq!(v.state, "running", "{} {:?} {:?}", v.key(), v.error, v.conditions);
		assert_eq!(v.condition("Programmed").status, "True", "{}", v.key());
	}
	let keys: Vec<String> = views.iter().map(|v| v.key()).collect();
	assert_eq!(views.len(), 4, "rules on rproxy: {keys:?}\nthe Gateway: {:?}\nlisteners: {:?}", plan.conds, plan.listeners);

	// the UI's token reads rules and metrics, and writes nothing
	assert_eq!(api_status(api, "GET", "/rules", UI_TOKEN, "").await, 200);
	assert_eq!(api_status(api, "GET", "/metrics", UI_TOKEN, "").await, 200);
	assert_eq!(api_status(api, "PUT", &format!("/rulesets/{}", plan.ruleset), UI_TOKEN, "{\"generation\": 99, \"rules\": []}").await, 403);
	let v = &views[0];
	assert_eq!(
		api_status(api, "DELETE", &format!("/rules/{}/{}/{}", v.protocol.as_str(), v.listen_addr, v.listen_port), UI_TOKEN, "").await,
		403
	);
	assert_eq!(api_status(api, "GET", &format!("/rulesets/{}", plan.ruleset), TOKEN, "").await, 200, "the set is still there");

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

	// the newer settings (rproxy-api docs/API.md, "Gateway API 向け"), where this rproxy has them
	if features.headers_add && features.redirect_status && features.replace_host && features.server_status {
		let (_, body) = get(http_port, "newer.example.com", "/add").await;
		assert!(body.ends_with("/add|newer.example.com|one,two"), "{body}");
		let (s, body) = get(http_port, "newer.example.com", "/see-other").await;
		assert_eq!(s, 303, "{body}");
		assert!(body.to_ascii_lowercase().contains(&format!("location: http://newer.example.com:{http_port}/elsewhere")), "{body}");
		let (_, body) = get(http_port, "newer.example.com", "/host").await;
		assert!(body.contains("/host|rewritten.example.com|"), "{body}");
		// half the requests go to the backend, the invalid backendRef's half is answered with 500
		let mut codes = std::collections::BTreeSet::new();
		for _ in 0..8 {
			codes.insert(get(http_port, "newer.example.com", "/partial").await.0);
		}
		assert_eq!(codes, [200, 500].into());
	} else {
		eprintln!("this rproxy lacks the newer settings ({features:?}); not tested");
	}
	// rproxy v0.4.3: ExternalAuth (Envoy's HTTP ext_authz) and filters on a backendRef
	if features.ext_auth_http && features.server_redirect {
		let (s, body) = get(http_port, "newer.example.com", "/guarded/x").await;
		assert_eq!(s, 403, "{body}");
		let (s, body) = get_with(http_port, "newer.example.com", "/guarded/x?q=1", "Authorization: allow\r\n").await;
		assert_eq!(s, 200, "{body}");
		assert!(
			body.ends_with("/guarded/x|newer.example.com|checked /check/guarded/x"),
			"the auth server's header, asked at the prefixed path: {body}"
		);
		let (s, body) = get(http_port, "newer.example.com", "/per-backend").await;
		assert_eq!(s, 302, "{body}");
		assert!(body.to_ascii_lowercase().contains("location: http://elsewhere.example.com"), "{body}");
	} else {
		eprintln!("this rproxy lacks the v0.4.3 settings ({features:?}); not tested");
	}

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
