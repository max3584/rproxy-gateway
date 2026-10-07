use serde_json::json;

use super::*;

pub fn world(yaml: &str) -> World {
	let mut w = World::default();
	w.read_manifests(yaml).unwrap();
	w
}

pub const BASE: &str = r#"
apiVersion: v1
kind: Namespace
metadata: {name: default}
---
apiVersion: v1
kind: Namespace
metadata: {name: other, labels: {team: a}}
---
apiVersion: v1
kind: Service
metadata: {name: web, namespace: default}
spec:
  ports: [{name: http, port: 80, targetPort: 8080}]
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {name: web-abc, namespace: default, labels: {kubernetes.io/service-name: web}}
addressType: IPv4
ports: [{name: http, port: 8080}]
endpoints:
  - addresses: [10.0.0.1]
    conditions: {ready: true}
  - addresses: [10.0.0.2]
  - addresses: [10.0.0.3]
    conditions: {ready: false}
---
apiVersion: v1
kind: Service
metadata: {name: api, namespace: other}
spec:
  ports: [{port: 8000}]
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {name: api-1, namespace: other, labels: {kubernetes.io/service-name: api}}
addressType: IPv4
ports: [{port: 9000}]
endpoints:
  - addresses: [10.1.0.1]
"#;

fn gw(listeners: &str) -> String {
	format!(
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {{name: gw, namespace: default, generation: 2}}
spec:
  gatewayClassName: rproxy
  listeners:
{listeners}
"#
	)
}

fn plan(yaml: &str) -> GatewayPlan {
	let w = world(yaml);
	render_gateway(&w, &w.gateways[0], &Options::default())
}

fn cond<'a>(conds: &'a [Cond], kind: &str) -> &'a Cond {
	status::get(conds, kind).unwrap_or_else(|| panic!("no {kind} in {conds:?}"))
}

#[test]
fn http_route_to_endpoints() {
	let yaml = format!(
		"{BASE}{}{}",
		gw("  - {name: http, port: 80, protocol: HTTP}"),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: web, namespace: default, generation: 5}
spec:
  parentRefs: [{name: gw}]
  hostnames: [web.example.com]
  rules:
    - matches: [{path: {type: PathPrefix, value: /app}}]
      backendRefs: [{name: web, port: 80}]
"#
	);
	let p = plan(&yaml);
	assert_eq!(p.ruleset, "k8s/default/gw");
	assert_eq!(p.generation, 2);
	let rules = p.rules_json();
	assert_eq!(rules.len(), 1);
	let r = &rules[0];
	assert_eq!(r["protocol"], "tcp");
	assert_eq!(r["listen_port"], 80);
	assert_eq!(r["labels"]["gateway.networking.k8s.io/gateway-name"], "gw");
	assert_eq!(
		r["http"]["routes"],
		json!([{
			"name": "default/web/r0/m0/h0",
			"match": "Host(`web.example.com`) && (Path(`/app`) || PathPrefix(`/app/`))",
			"priority": 1,
			"service": "default/web/r0"
		}])
	);
	assert_eq!(
		r["http"]["services"]["default/web/r0"],
		json!({"servers": [{"url": "http://10.0.0.1:8080"}, {"url": "http://10.0.0.2:8080"}]})
	);
	assert_eq!(p.listeners[0].attached, 1);
	assert_eq!(p.listeners[0].rule_key.as_deref(), Some("tcp/0.0.0.0:80"));
	assert!(cond(&p.listeners[0].conds, "Accepted").status);
	let parent = &p.parents[0];
	assert!(cond(&parent.conds, "Accepted").status);
	assert!(cond(&parent.conds, "ResolvedRefs").status);
	assert_eq!(parent.generation, 5);
	assert!(parent.rule_keys.contains("tcp/0.0.0.0:80"));
}

#[test]
fn cross_namespace_needs_grants() {
	let route = r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: api, namespace: default}
spec:
  parentRefs: [{name: gw}]
  rules:
    - backendRefs: [{name: api, namespace: other, port: 8000}]
"#;
	let yaml = format!("{BASE}{}{route}", gw("  - {name: http, port: 80, protocol: HTTP}"));
	let p = plan(&yaml);
	let rc = cond(&p.parents[0].conds, "ResolvedRefs");
	assert_eq!((rc.status, rc.reason.as_str()), (false, "RefNotPermitted"));
	// the rule answers 500
	let r = &p.rules_json()[0];
	assert_eq!(r["http"]["routes"][0]["middlewares"], json!([http::RESPOND_500]));
	assert!(r["http"]["routes"][0].get("service").is_none());

	let grant = r#"
---
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata: {name: allow, namespace: other}
spec:
  from: [{group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: default}]
  to: [{group: "", kind: Service}]
"#;
	let p = plan(&format!("{yaml}{grant}"));
	assert!(cond(&p.parents[0].conds, "ResolvedRefs").status);
	assert_eq!(p.rules_json()[0]["http"]["services"]["default/api/r0"]["servers"], json!([{"url": "http://10.1.0.1:9000"}]));
}

#[test]
fn route_namespaces_and_hostnames() {
	let yaml = format!(
		"{BASE}{}{}",
		gw(r#"  - {name: same, port: 80, protocol: HTTP, hostname: "*.example.com"}
  - name: team
    port: 8080
    protocol: HTTP
    allowedRoutes: {namespaces: {from: Selector, selector: {matchLabels: {team: a}}}}"#),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: r1, namespace: other}
spec:
  parentRefs: [{name: gw, namespace: default}]
  rules: [{backendRefs: [{name: api, port: 8000}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: r2, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: same}]
  hostnames: [other.net]
  rules: [{backendRefs: [{name: web, port: 80}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: r3, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: nope}]
"#
	);
	let p = plan(&yaml);
	let by = |n: &str| p.parents.iter().find(|x| x.name == n).unwrap();
	assert!(cond(&by("r1").conds, "Accepted").status, "{:?}", by("r1").conds);
	assert_eq!(by("r1").rule_keys.iter().collect::<Vec<_>>(), ["tcp/0.0.0.0:8080"]);
	assert_eq!(cond(&by("r2").conds, "Accepted").reason, "NoMatchingListenerHostname");
	assert_eq!(cond(&by("r3").conds, "Accepted").reason, "NoMatchingParent");
	assert_eq!(p.listeners[0].attached, 0);
	assert_eq!(p.listeners[1].attached, 1);
}

#[test]
fn listener_isolation_and_precedence() {
	let yaml = format!(
		"{BASE}{}{}",
		gw(r#"  - {name: wild, port: 80, protocol: HTTP, hostname: "*.example.com"}
  - {name: foo, port: 80, protocol: HTTP, hostname: foo.example.com}
  - {name: any, port: 80, protocol: HTTP}"#),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: wild, namespace: default, creationTimestamp: "2026-01-01T00:00:00Z"}
spec:
  parentRefs: [{name: gw, sectionName: wild}]
  rules:
    - matches: [{path: {type: Exact, value: /x}}, {path: {type: PathPrefix, value: /}}]
      backendRefs: [{name: web, port: 80}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: foo, namespace: default, creationTimestamp: "2026-01-02T00:00:00Z"}
spec:
  parentRefs: [{name: gw, sectionName: foo}]
  rules: [{backendRefs: [{name: web, port: 80}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: any, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: any}]
  rules: [{backendRefs: [{name: web, port: 80}]}]
"#
	);
	let p = plan(&yaml);
	assert_eq!(p.rules.len(), 1);
	let routes = &p.rules_json()[0]["http"]["routes"];
	let got: Vec<(String, String)> =
		routes.as_array().unwrap().iter().map(|r| (r["name"].as_str().unwrap().into(), r["match"].as_str().unwrap().into())).collect();
	assert_eq!(
		got,
		vec![
			("default/foo/r0/m0/h0".into(), "Host(`foo.example.com`)".into()),
			("default/wild/r0/m0/h0".into(), "Host(`**.example.com`) && !Host(`foo.example.com`) && Path(`/x`)".into()),
			("default/wild/r0/m1/h0".into(), "Host(`**.example.com`) && !Host(`foo.example.com`)".into()),
			("default/any/r0/m0/h0".into(), "!Host(`**.example.com`) && !Host(`foo.example.com`)".into()),
		]
	);
	let prio: Vec<i64> = routes.as_array().unwrap().iter().map(|r| r["priority"].as_i64().unwrap()).collect();
	assert_eq!(prio, vec![4, 3, 2, 1]);
}

#[test]
fn https_listeners_merge_and_check_certificates() {
	let (crt, key) = crate::pem::tests::pair("example.com");
	let secret = format!(
		r#"
---
apiVersion: v1
kind: Secret
metadata: {{name: cert, namespace: default}}
type: kubernetes.io/tls
stringData:
  tls.crt: |
{}
  tls.key: |
{}
---
apiVersion: v1
kind: Secret
metadata: {{name: broken, namespace: default}}
stringData: {{tls.crt: nope, tls.key: nope}}
"#,
		indent(&crt),
		indent(&key)
	);
	let yaml = format!(
		"{BASE}{secret}{}",
		gw(r#"  - {name: a, port: 443, protocol: HTTPS, tls: {certificateRefs: [{name: cert}]}}
  - {name: b, port: 443, protocol: HTTPS, hostname: b.example.com, tls: {certificateRefs: [{name: cert}]}}
  - {name: c, port: 8443, protocol: HTTPS, tls: {certificateRefs: [{name: broken}]}}
  - {name: d, port: 9443, protocol: HTTPS, tls: {certificateRefs: [{name: cert, namespace: other}]}}
  - {name: e, port: 80, protocol: HTTP, allowedRoutes: {kinds: [{kind: TCPRoute}]}}
  - {name: f, port: 81, protocol: SCTP}
  - {name: g, port: 443, protocol: HTTP}"#)
	);
	let p = plan(&yaml);
	let rules = p.rules_json();
	assert_eq!(rules.len(), 2, "{rules:#?}");
	let tls = &rules.iter().find(|r| r["listen_port"] == 443).unwrap()["tls"];
	assert_eq!(tls["mode"], "terminate");
	assert_eq!(tls["certificates"].as_array().unwrap().len(), 1);
	let file = tls["certificates"][0]["cert_file"].as_str().unwrap();
	let name = file.rsplit('/').next().unwrap();
	assert_eq!(p.files[name], crt.as_bytes());
	let l = |n: &str| p.listeners.iter().find(|x| x.name == n).unwrap();
	assert_eq!(cond(&l("c").conds, "ResolvedRefs").reason, "InvalidCertificateRef");
	assert!(cond(&l("c").conds, "Accepted").status && !l("c").servable, "routes attach, nothing is served");
	let gc = cond(&p.conds, "Accepted");
	assert_eq!((gc.status, gc.reason.as_str()), (true, "ListenersNotValid"));
	assert_eq!(cond(&l("d").conds, "ResolvedRefs").reason, "RefNotPermitted");
	assert_eq!(cond(&l("e").conds, "ResolvedRefs").reason, "InvalidRouteKinds");
	assert!(l("e").supported_kinds.is_empty());
	assert_eq!(cond(&l("f").conds, "Accepted").reason, "UnsupportedProtocol");
	assert_eq!(cond(&l("g").conds, "Conflicted").reason, "ProtocolConflict");
	assert!(!cond(&l("a").conds, "Conflicted").status);
}

fn indent(s: &str) -> String {
	s.lines().map(|l| format!("    {l}")).collect::<Vec<_>>().join("\n")
}

#[test]
fn filters_and_weights() {
	let yaml = format!(
		"{BASE}{}{}",
		gw("  - {name: http, port: 8080, protocol: HTTP}"),
		r#"
---
apiVersion: v1
kind: Service
metadata: {name: web2, namespace: default}
spec: {ports: [{name: http, port: 80}]}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {name: web2-1, namespace: default, labels: {kubernetes.io/service-name: web2}}
addressType: IPv4
ports: [{name: http, port: 8081}]
endpoints: [{addresses: [10.0.1.1]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: f, namespace: default}
spec:
  parentRefs: [{name: gw}]
  rules:
    - matches: [{path: {value: /redirect}}]
      filters:
        - type: RequestRedirect
          requestRedirect: {hostname: example.org, statusCode: 301}
    - matches: [{path: {value: /rewrite}}]
      filters:
        - type: URLRewrite
          urlRewrite: {path: {type: ReplacePrefixMatch, replacePrefixMatch: /new}}
        - type: RequestHeaderModifier
          requestHeaderModifier:
            set: [{name: X-A, value: a}]
            add: [{name: X-B, value: b}]
            remove: [X-C]
      backendRefs:
        - {name: web, port: 80, weight: 3}
        - {name: web2, port: 80, weight: 1}
    - matches: [{path: {value: /missing}}]
      backendRefs: [{name: nothing, port: 80}]
"#
	);
	let p = plan(&yaml);
	let http = &p.rules_json()[0]["http"];
	let route = |n: &str| http["routes"].as_array().unwrap().iter().find(|r| r["name"] == n).unwrap().clone();
	let redirect = route("default/f/r0/m0/h0");
	assert!(redirect.get("service").is_none());
	let mw = &http["middlewares"][redirect["middlewares"][0].as_str().unwrap()];
	assert_eq!(mw["redirect_regex"]["replacement"], "http://example.org:8080${2}${3}");
	assert_eq!(mw["redirect_regex"]["permanent"], true);
	let rewrite = route("default/f/r1/m0/h0");
	assert_eq!(rewrite["middlewares"].as_array().unwrap().len(), 2);
	assert_eq!(
		http["middlewares"][rewrite["middlewares"][1].as_str().unwrap()],
		json!({"headers": {"request": {"set": {"X-A": "a"}, "add": {"X-B": "b"}, "remove": ["X-C"]}}})
	);
	assert_eq!(
		http["services"]["default/f/r1"]["servers"],
		json!([{"url": "http://10.0.0.1:8080", "weight": 3}, {"url": "http://10.0.0.2:8080", "weight": 3}, {"url": "http://10.0.1.1:8081", "weight": 2}])
	);
	assert_eq!(route("default/f/r2/m0/h0")["middlewares"], json!([http::RESPOND_500]));
	assert_eq!(cond(&p.parents[0].conds, "ResolvedRefs").reason, "BackendNotFound");
}

#[test]
fn unsupported_filters_are_not_accepted() {
	let yaml = format!(
		"{BASE}{}{}",
		gw("  - {name: http, port: 80, protocol: HTTP}"),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: m, namespace: default}
spec:
  parentRefs: [{name: gw}]
  rules:
    - filters: [{type: RequestMirror, requestMirror: {backendRef: {name: web, port: 80}}}]
      backendRefs: [{name: web, port: 80}]
"#
	);
	// an rproxy without mirrors
	let w = world(&yaml);
	let opts = Options { features: Features { mirror: false, ..Default::default() }, ..Default::default() };
	let p = render_gateway(&w, &w.gateways[0], &opts);
	let c = cond(&p.parents[0].conds, "Accepted");
	assert_eq!((c.status, c.reason.as_str()), (false, "UnsupportedValue"));
	assert!(c.message.contains("middlewares mirror"), "{}", c.message);
	assert!(p.rules_json()[0]["http"]["routes"].as_array().unwrap().is_empty());
	// a newer one
	let p = plan(&yaml);
	assert!(cond(&p.parents[0].conds, "Accepted").status);
}

#[test]
fn newer_rproxy_settings() {
	let yaml = format!(
		"{BASE}{}{}",
		gw("  - {name: http, port: 80, protocol: HTTP}"),
		r#"
---
apiVersion: v1
kind: Service
metadata: {name: grpc, namespace: default}
spec: {ports: [{name: grpc, port: 50051, appProtocol: kubernetes.io/h2c}]}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {name: grpc-1, namespace: default, labels: {kubernetes.io/service-name: grpc}}
addressType: IPv4
ports: [{name: grpc, port: 50051}]
endpoints: [{addresses: [10.0.2.1]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: nr, namespace: default}
spec:
  parentRefs: [{name: gw}]
  rules:
    - matches: [{path: {value: /mirror}}]
      filters:
        - {type: RequestMirror, requestMirror: {backendRef: {name: web, port: 80}, percent: 20}}
        - type: CORS
          cors: {allowOrigins: ["https://www.foo.com"], allowMethods: [GET], allowCredentials: true, maxAge: 3600}
        - {type: URLRewrite, urlRewrite: {hostname: one.example.org}}
      timeouts: {request: 10s, backendRequest: 2s}
      retry: {codes: [500, 502], attempts: 3, backoff: 100ms}
      backendRefs:
        - name: web
          port: 80
          weight: 1
          filters: [{type: RequestHeaderModifier, requestHeaderModifier: {set: [{name: Backend, value: v1}]}}]
        - {name: nothing, port: 80, weight: 1}
    - matches: [{path: {value: /grpc}}]
      backendRefs: [{name: grpc, port: 50051}]
"#
	);
	let p = plan(&yaml);
	assert!(cond(&p.parents[0].conds, "Accepted").status, "{:?}", p.parents[0].conds);
	assert_eq!(cond(&p.parents[0].conds, "ResolvedRefs").reason, "BackendNotFound", "the missing Service");
	let http = &p.rules_json()[0]["http"];
	let route = http["routes"].as_array().unwrap().iter().find(|r| r["name"] == "default/nr/r0/m0/h0").unwrap().clone();
	assert_eq!(route["timeouts"], json!({"request": "10000ms", "backend_request": "2000ms"}));
	let mws: Vec<Value> =
		route["middlewares"].as_array().unwrap().iter().map(|n| http["middlewares"][n.as_str().unwrap()].clone()).collect();
	assert_eq!(mws[0], json!({"mirror": {"service": "default/nr/r0/f0/mirror", "percent": 20}}));
	assert_eq!(mws[1]["cors"]["allow_origins"], json!(["https://www.foo.com"]));
	assert_eq!(mws[1]["cors"]["allow_credentials"], true);
	assert_eq!(mws[1]["cors"]["max_age"], 3600);
	assert_eq!(mws[2], json!({"replace_host": {"host": "one.example.org"}}));
	assert_eq!(mws[3], json!({"retry": {"attempts": 4, "status": ["500", "502"], "initial_interval": "100ms"}}), "retries last");
	assert!(http["services"]["default/nr/r0/f0/mirror"]["servers"].as_array().is_some_and(|s| !s.is_empty()));
	// the web share goes to its pods with the backendRef's middleware, the missing Service's share is answered with 500
	let servers = http["services"]["default/nr/r0"]["servers"].as_array().unwrap().clone();
	assert_eq!(servers.len(), 3);
	assert_eq!(servers[0]["middlewares"], json!(["default/nr/r0/b0/f0.0"]));
	assert_eq!(http["middlewares"]["default/nr/r0/b0/f0.0"], json!({"headers": {"request": {"set": {"Backend": "v1"}}}}));
	assert_eq!(servers[2], json!({"status": 500, "weight": 2}));
	assert_eq!((servers[0]["weight"].clone(), servers[1]["weight"].clone()), (json!(1), json!(1)));
	assert!(http["services"]["default/nr/r0"].get("timeouts").is_none(), "route timeouts, not the service's");
	// appProtocol kubernetes.io/h2c: HTTP/2 without TLS to the backend
	assert_eq!(http["services"]["default/nr/r1"]["protocol"], "h2c");

	// an older rproxy: the backend request timeout as the service's response timeout, no 500 servers
	let w = world(&yaml);
	let old = Features { route_timeouts: false, server_status: false, ..Default::default() };
	let p = render_gateway(&w, &w.gateways[0], &Options { features: old, ..Default::default() });
	let http = &p.rules_json()[0]["http"];
	assert_eq!(http["services"]["default/nr/r0"]["timeouts"], json!({"response": "2000ms"}));
	assert_eq!(http["services"]["default/nr/r0"]["servers"].as_array().unwrap().len(), 2);
}

#[test]
fn durations() {
	assert_eq!(duration_ms("1h30m"), Some(5_400_000));
	assert_eq!(duration_ms("500ms"), Some(500));
	assert_eq!(duration_ms("10s"), Some(10_000));
	assert_eq!(duration_ms(""), None);
	assert_eq!(duration_ms("5x"), None);
}

const L4_BASE: &str = r#"
---
apiVersion: v1
kind: Service
metadata: {name: db, namespace: default}
spec: {clusterIP: 10.96.0.20, ports: [{port: 5432}]}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {name: db-1, namespace: default, labels: {kubernetes.io/service-name: db}}
addressType: IPv4
ports: [{port: 5432}]
endpoints: [{addresses: [10.0.2.1]}, {addresses: [10.0.2.2]}]
---
apiVersion: v1
kind: Service
metadata: {name: tls, namespace: default}
spec: {clusterIP: 10.96.0.30, ports: [{port: 443}]}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {name: tls-1, namespace: default, labels: {kubernetes.io/service-name: tls}}
addressType: IPv4
ports: [{port: 8443}]
endpoints: [{addresses: [10.0.3.1]}]
"#;

#[test]
fn tcp_udp_and_tls_routes() {
	let yaml = format!(
		"{BASE}{L4_BASE}{}{}",
		gw(r#"  - {name: pg, port: 5432, protocol: TCP}
  - {name: dns, port: 53, protocol: UDP}
  - {name: dnstcp, port: 53, protocol: TCP}
  - {name: sni, port: 8443, protocol: TLS, hostname: "*.example.com", tls: {mode: Passthrough}}
  - {name: empty, port: 9000, protocol: TCP}"#),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: pg, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: pg}]
  rules: [{backendRefs: [{name: db, port: 5432}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: UDPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: dns}]
  rules: [{backendRefs: [{name: db, port: 5432}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: t, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: sni}]
  hostnames: [a.example.com, "*.b.example.com", other.net]
  rules: [{backendRefs: [{name: tls, port: 443}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: wrong, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: sni}]
  rules: [{backendRefs: [{name: db, port: 5432}]}]
"#
	);
	let p = plan(&yaml);
	let rules = p.rules_json();
	let rule = |proto: &str, port: u16| rules.iter().find(|r| r["protocol"] == proto && r["listen_port"] == port).cloned();
	assert_eq!(rule("tcp", 5432).unwrap()["targets"], json!([{"addr": "10.0.2.1", "port": 5432}, {"addr": "10.0.2.2", "port": 5432}]));
	assert_eq!(rule("udp", 53).unwrap()["targets"].as_array().unwrap().len(), 2);
	assert!(rule("tcp", 53).is_none(), "a TCP listener without routes has no rule");
	let sni = rule("tcp", 8443).unwrap();
	assert_eq!(sni["tls"]["mode"], "sni");
	assert_eq!(sni["tls"]["unmatched"], "reject");
	assert_eq!(
		sni["tls"]["routes"],
		json!([{"server_names": ["a.example.com", "**.b.example.com"], "targets": [{"addr": "10.0.3.1", "port": 8443}]}]),
		"the pods (tls_route_targets)"
	);
	assert_eq!(sni["targets"], json!([{"addr": "10.0.3.1", "port": 8443}]));
	// an older rproxy: one destination per name, the Service's ClusterIP
	let w = world(&yaml);
	let old = render_gateway(
		&w,
		&w.gateways[0],
		&Options { features: Features { tls_route_targets: false, ..Default::default() }, ..Default::default() },
	);
	let old_rules = old.rules_json();
	let old_sni = old_rules.iter().find(|r| r["protocol"] == "tcp" && r["listen_port"] == 8443).unwrap();
	assert_eq!(
		old_sni["tls"]["routes"],
		json!([{"server_names": ["a.example.com", "**.b.example.com"], "remote_addr": "10.96.0.30", "remote_port": 443}])
	);
	let wrong = p.parents.iter().find(|x| x.name == "wrong").unwrap();
	assert_eq!(cond(&wrong.conds, "Accepted").reason, "NotAllowedByListeners");
	let l = |n: &str| p.listeners.iter().find(|x| x.name == n).unwrap();
	assert_eq!(l("pg").rule_key.as_deref(), Some("tcp/0.0.0.0:5432"));
	assert_eq!(l("dns").rule_key.as_deref(), Some("udp/0.0.0.0:53"));
	assert!(!cond(&l("dnstcp").conds, "Conflicted").status, "TCP and UDP share a port");
	assert_eq!(l("empty").rule_key, None);
	assert_eq!(l("sni").supported_kinds[0].kind, "TLSRoute");
	assert_eq!(p.ports().len(), 5, "listeners without a rule still get a Service port");
}

#[test]
fn tls_passthrough_next_to_https() {
	let (crt, key) = crate::pem::tests::pair("example.com");
	let yaml = format!(
		"{BASE}{L4_BASE}\n---\napiVersion: v1\nkind: Secret\nmetadata: {{name: cert, namespace: default}}\nstringData:\n  tls.crt: |\n{}\n  tls.key: |\n{}\n{}{}",
		indent(&crt),
		indent(&key),
		gw(r#"  - {name: https, port: 443, protocol: HTTPS, hostname: www.example.com, tls: {certificateRefs: [{name: cert}]}}
  - {name: pass, port: 443, protocol: TLS, hostname: registry.example.com, tls: {mode: Passthrough}}"#),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: reg, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: pass}]
  rules: [{backendRefs: [{name: tls, port: 443}]}]
"#
	);
	let p = plan(&yaml);
	let rules = p.rules_json();
	assert_eq!(rules.len(), 1);
	let tls = &rules[0]["tls"];
	assert_eq!(tls["mode"], "terminate");
	assert_eq!(
		tls["routes"],
		json!([{"server_names": ["registry.example.com"], "targets": [{"addr": "10.0.3.1", "port": 8443}], "passthrough": true}])
	);
	assert!(rules[0]["http"].is_object());
	assert!(p.listeners.iter().all(|l| !cond(&l.conds, "Conflicted").status));
}

#[test]
fn policies_and_raw_rules() {
	let yaml = format!(
		"{BASE}{L4_BASE}{}{}",
		gw(r#"  - {name: http, port: 80, protocol: HTTP}
  - {name: pg, port: 5432, protocol: TCP}"#),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: pg, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: pg}]
  rules: [{backendRefs: [{name: db, port: 5432}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: web, namespace: default}
spec:
  parentRefs: [{name: gw, sectionName: http}]
  rules: [{backendRefs: [{name: web, port: 80}]}]
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyPolicy
metadata: {name: old, namespace: default, creationTimestamp: "2026-01-01T00:00:00Z"}
spec:
  targetRefs: [{group: gateway.networking.k8s.io, kind: Gateway, name: gw, sectionName: pg}]
  limits: {max_connections: 100}
  allowFrom: [10.0.0.0/8]
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyPolicy
metadata: {name: new, namespace: default, creationTimestamp: "2026-02-01T00:00:00Z"}
spec:
  targetRefs:
    - {group: gateway.networking.k8s.io, kind: Gateway, name: gw}
    - {group: "", kind: Service, name: web}
  limits: {max_connections: 5}
  crowdsec: true
  outlierDetection: {consecutive_5xx: 5}
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyPolicy
metadata: {name: missing, namespace: default}
spec:
  targetRefs: [{group: gateway.networking.k8s.io, kind: Gateway, name: gw, sectionName: nope}]
  crowdsec: true
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyRule
metadata: {name: smtp, namespace: default, generation: 2}
spec:
  parentRef: {name: gw}
  rule: {protocol: tcp, listen_addr: 0.0.0.0, listen_port: 25, remote_addr: mail.internal, remote_port: 25, starttls: smtp}
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyRule
metadata: {name: clash, namespace: default}
spec:
  parentRef: {name: gw}
  rule: {protocol: tcp, listen_addr: 0.0.0.0, listen_port: 80, remote_addr: x, remote_port: 1}
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyRule
metadata: {name: foreign, namespace: other}
spec:
  parentRef: {name: gw, namespace: default}
  rule: {protocol: udp, listen_addr: 0.0.0.0, listen_port: 9999, remote_addr: x, remote_port: 1}
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyMiddleware
metadata: {name: limit, namespace: default}
spec: {rate_limit: {average: 10}}
"#
	);
	let p = plan(&yaml);
	let rules = p.rules_json();
	let rule = |port: u16| rules.iter().find(|r| r["listen_port"] == port).unwrap().clone();
	let pg = rule(5432);
	assert_eq!(pg["limits"], json!({"max_connections": 100}), "the oldest policy wins");
	assert_eq!(pg["allow_from"], json!(["10.0.0.0/8"]));
	assert_eq!(pg["crowdsec"], true);
	assert_eq!(pg["outlier_detection"], json!({"consecutive_5xx": 5}));
	let web = rule(80);
	assert_eq!(web["limits"], json!({"max_connections": 5}));
	assert!(web.get("outlier_detection").is_none());
	assert_eq!(web["http"]["services"]["default/web/r0"]["outlier_detection"], json!({"consecutive_5xx": 5}));
	let smtp = rule(25);
	assert_eq!(smtp["starttls"], "smtp");
	assert_eq!(smtp["labels"]["gateway.networking.k8s.io/gateway-name"], "gw");
	let raw = |n: &str| p.raw_status.iter().find(|s| s.name == n).unwrap();
	assert_eq!(raw("smtp").rule_key.as_deref(), Some("tcp/0.0.0.0:25"));
	assert_eq!(cond(&raw("clash").conds, "Accepted").reason, "Conflicted");
	assert_eq!(cond(&raw("foreign").conds, "Accepted").reason, "RefNotPermitted");
	let pol = |n: &str| p.policies.iter().find(|s| s.name == n).unwrap();
	assert!(pol("old").conds[0].status);
	assert_eq!(pol("missing").conds[0].reason, "TargetNotFound");
	assert!(p.ports().contains(&(rp::Protocol::Tcp, 25)));
}

#[test]
fn extension_ref_middlewares() {
	let yaml = format!(
		"{BASE}{}{}",
		gw("  - {name: http, port: 80, protocol: HTTP}"),
		r#"
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyMiddleware
metadata: {name: limit, namespace: default}
spec: {rate_limit: {average: 10}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: web, namespace: default}
spec:
  parentRefs: [{name: gw}]
  rules:
    - filters: [{type: ExtensionRef, extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: limit}}]
      backendRefs: [{name: web, port: 80}]
    - matches: [{path: {value: /x}}]
      filters: [{type: ExtensionRef, extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: nope}}]
      backendRefs: [{name: web, port: 80}]
"#
	);
	let p = plan(&yaml);
	let http = &p.rules_json()[0]["http"];
	let r0 = http["routes"].as_array().unwrap().iter().find(|r| r["name"] == "default/web/r0/m0/h0").unwrap().clone();
	assert_eq!(http["middlewares"][r0["middlewares"][0].as_str().unwrap()], json!({"rate_limit": {"average": 10}}));
	let r1 = http["routes"].as_array().unwrap().iter().find(|r| r["name"] == "default/web/r1/m0/h0").unwrap().clone();
	assert_eq!(r1["middlewares"], json!([http::RESPOND_500]));
	assert_eq!(cond(&p.parents[0].conds, "ResolvedRefs").reason, "BackendNotFound");
}

fn migrate_opts() -> Options {
	let mut eps = migrate::Settings::default_entry_points();
	eps.insert("pg".into(), (rp::Protocol::Tcp, 5432));
	eps.insert("dns".into(), (rp::Protocol::Udp, 53));
	Options {
		migration: Some(migrate::Settings { gateway: ("default".into(), "gw".into()), entry_points: eps, ingress_class: "rproxy".into() }),
		..Default::default()
	}
}

#[test]
fn traefik_and_ingress_migration() {
	let (crt, key) = crate::pem::tests::pair("app.example.com");
	let yaml = format!(
		"{BASE}{L4_BASE}\n---\napiVersion: v1\nkind: Secret\nmetadata: {{name: cert, namespace: default}}\nstringData:\n  tls.crt: |\n{}\n  tls.key: |\n{}\n---\napiVersion: v1\nkind: Secret\nmetadata: {{name: users, namespace: default}}\nstringData: {{users: \"admin:$apr1$x$y\"}}\n{}{}",
		indent(&crt),
		indent(&key),
		gw("  - {name: http, port: 80, protocol: HTTP}"),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: native, namespace: default}
spec:
  parentRefs: [{name: gw}]
  hostnames: [native.example.com]
  rules: [{backendRefs: [{name: web, port: 80}]}]
---
apiVersion: traefik.io/v1alpha1
kind: Middleware
metadata: {name: secure, namespace: default}
spec:
  chain:
    middlewares: [{name: hsts}, {name: auth}]
---
apiVersion: traefik.io/v1alpha1
kind: Middleware
metadata: {name: hsts, namespace: default}
spec: {headers: {stsSeconds: 300}}
---
apiVersion: traefik.io/v1alpha1
kind: Middleware
metadata: {name: auth, namespace: default}
spec: {basicAuth: {secret: users}}
---
apiVersion: traefik.io/v1alpha1
kind: Middleware
metadata: {name: to-https, namespace: default}
spec: {redirectScheme: {scheme: https, permanent: true}}
---
apiVersion: traefik.io/v1alpha1
kind: TLSOption
metadata: {name: modern, namespace: default}
spec: {minVersion: VersionTLS13, sniStrict: true}
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata: {name: app, namespace: default}
spec:
  entryPoints: [websecure]
  routes:
    - match: Host(`app.example.com`) && PathPrefix(`/api`)
      kind: Rule
      middlewares: [{name: secure}]
      services: [{name: web, port: 80}]
    - match: HostRegexp(`{sub:[a-z]+}.example.com`) || Headers(`X-A`, `1`)
      services: [{name: web, port: http}]
    - match: Weird(`x`)
      services: [{name: web, port: 80}]
  tls:
    secretName: cert
    options: {name: modern}
---
apiVersion: traefik.io/v1alpha1
kind: IngressRoute
metadata: {name: redirect, namespace: default}
spec:
  entryPoints: [web]
  routes:
    - match: Host(`app.example.com`)
      middlewares: [{name: to-https}]
---
apiVersion: traefik.io/v1alpha1
kind: IngressRouteTCP
metadata: {name: pg, namespace: default}
spec:
  entryPoints: [pg]
  routes: [{match: HostSNI(`*`), services: [{name: db, port: 5432, proxyProtocol: {version: 2}}]}]
---
apiVersion: traefik.io/v1alpha1
kind: IngressRouteTCP
metadata: {name: reg, namespace: default}
spec:
  entryPoints: [websecure]
  routes: [{match: HostSNI(`registry.example.com`), services: [{name: tls, port: 443}]}]
  tls: {passthrough: true}
---
apiVersion: traefik.io/v1alpha1
kind: IngressRouteUDP
metadata: {name: dns, namespace: default}
spec:
  entryPoints: [dns]
  routes: [{services: [{name: db, port: 5432}]}]
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: legacy, namespace: default}
spec:
  ingressClassName: rproxy
  rules:
    - host: legacy.example.com
      http:
        paths:
          - {path: /, pathType: Prefix, backend: {service: {name: web, port: {number: 80}}}}
          - {path: /exact, pathType: Exact, backend: {service: {name: web, port: {name: http}}}}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: other-class, namespace: default}
spec:
  ingressClassName: nginx
  rules: [{host: x.example.com, http: {paths: [{path: /, pathType: Prefix, backend: {service: {name: web, port: {number: 80}}}}]}}]
"#
	);
	let w = world(&yaml);
	let p = render_gateway(&w, &w.gateways[0], &migrate_opts());
	let rules = p.rules_json();
	let rule = |proto: &str, port: u16| {
		rules
			.iter()
			.find(|r| r["protocol"] == proto && r["listen_port"] == port)
			.cloned()
			.unwrap_or_else(|| panic!("no {proto}/{port}: {rules:#?}"))
	};
	// port 80: the Gateway's HTTPRoute, the redirect IngressRoute and the Ingress together
	let web = rule("tcp", 80);
	let names: Vec<&str> = web["http"]["routes"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
	assert!(names.contains(&"default/native/r0/m0/h0"), "{names:?}");
	assert!(names.contains(&"traefik/default/redirect/r0"), "{names:?}");
	assert!(names.contains(&"ingress/default/legacy/r0/p0"), "{names:?}");
	assert!(!names.iter().any(|n| n.contains("other-class")));
	assert_eq!(web["http"]["middlewares"]["traefik/default/to-https"], json!({"redirect_scheme": {"scheme": "https", "permanent": true}}));
	let exact = web["http"]["routes"].as_array().unwrap().iter().find(|r| r["name"] == "ingress/default/legacy/r0/p1").unwrap().clone();
	assert_eq!(exact["match"], "Host(`legacy.example.com`) && Path(`/exact`)");
	// port 443: TLS IngressRoute with a chain, TLSOption, the passthrough IngressRouteTCP
	let secure = rule("tcp", 443);
	assert_eq!(secure["tls"]["mode"], "terminate");
	assert_eq!(secure["tls"]["options"], json!({"min_version": "1.3"}));
	assert_eq!(
		secure["tls"]["routes"],
		json!([{"server_names": ["registry.example.com"], "remote_addr": "10.96.0.30", "remote_port": 443, "passthrough": true}])
	);
	let api = secure["http"]["routes"].as_array().unwrap().iter().find(|r| r["name"] == "traefik/default/app/r0").unwrap().clone();
	assert_eq!(api["middlewares"], json!(["traefik/default/hsts", "traefik/default/auth"]));
	let auth = &secure["http"]["middlewares"]["traefik/default/auth"]["basic_auth"];
	let file = auth["users_file"].as_str().unwrap().rsplit('/').next().unwrap().to_string();
	assert_eq!(p.files[&file], b"admin:$apr1$x$y");
	let second = secure["http"]["routes"].as_array().unwrap().iter().find(|r| r["name"] == "traefik/default/app/r1");
	assert!(second.is_none(), "v2 placeholders are not converted");
	assert!(p.notes.iter().any(|n| n.contains("Weird")), "{:?}", p.notes);
	assert!(p.notes.iter().any(|n| n.contains("sniStrict")), "{:?}", p.notes);
	// L4
	let pg = rule("tcp", 5432);
	assert_eq!(pg["targets"].as_array().unwrap().len(), 2);
	assert_eq!(pg["source_ip"], "proxy_v2");
	assert_eq!(rule("udp", 53)["targets"].as_array().unwrap().len(), 2);
}

#[test]
fn traefik_rules() {
	assert_eq!(migrate::translate_rule("Host(`a`) && Headers(`X`, `1`)").unwrap(), "Host(`a`) && Header(`X`, `1`)");
	assert_eq!(migrate::translate_rule("Query(`a=b`) || HostHeader(\"x\")").unwrap(), "Query(`a`, `b`) || Host(`x`)");
	assert!(migrate::translate_rule("Host(`{x:.+}`)").is_err());
	assert!(migrate::translate_rule("Foo(`x`)").is_err());
	assert_eq!(migrate::translate_rule("!ClientIP(`10.0.0.0/8`)").unwrap(), "!ClientIP(`10.0.0.0/8`)");
	assert_eq!(
		migrate::Settings::parse_entry_points(&["web=8080".into(), "dns=53/udp".into()]).unwrap(),
		[("dns".to_string(), (rp::Protocol::Udp, 53)), ("web".to_string(), (rp::Protocol::Tcp, 8080))].into()
	);
}

#[test]
fn a_route_on_two_listeners_of_one_port_and_the_oldest_tcp_route() {
	let yaml = format!(
		"{BASE}{L4_BASE}{}{}",
		gw(r#"  - {name: a, port: 80, protocol: HTTP, hostname: a.example.com}
  - {name: b, port: 80, protocol: HTTP, hostname: b.example.com}
  - {name: pg, port: 5432, protocol: TCP}"#),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: both, namespace: default}
spec:
  parentRefs: [{name: gw}]
  rules: [{backendRefs: [{name: web, port: 80}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: newer, namespace: default, creationTimestamp: "2026-02-01T00:00:00Z"}
spec:
  parentRefs: [{name: gw, sectionName: pg}]
  rules: [{backendRefs: [{name: tls, port: 443}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: older, namespace: default, creationTimestamp: "2026-01-01T00:00:00Z"}
spec:
  parentRefs: [{name: gw, sectionName: pg}]
  rules: [{backendRefs: [{name: db, port: 5432}]}]
"#
	);
	let p = plan(&yaml);
	let rules = p.rules_json();
	let web = rules.iter().find(|r| r["listen_port"] == 80).unwrap();
	let names: Vec<&str> = web["http"]["routes"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
	assert_eq!(names.len(), 2);
	assert_ne!(names[0], names[1]);
	let pg = rules.iter().find(|r| r["listen_port"] == 5432).unwrap();
	assert_eq!(pg["targets"][0]["addr"], "10.0.2.1", "the oldest route");
	assert!(p.parents.iter().all(|x| cond(&x.conds, "Accepted").status));
	assert_eq!(p.listeners.iter().find(|l| l.name == "pg").unwrap().attached, 2);
}

#[test]
fn ingress_status_and_handled_ingresses() {
	let w = world(
		r#"
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: mine, namespace: default}
spec: {ingressClassName: rproxy}
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata: {name: theirs, namespace: default}
spec: {ingressClassName: nginx}
"#,
	);
	let m = migrate_opts().migration.unwrap();
	let names: Vec<_> = migrate::handled_ingresses(&w, &m).iter().map(|i| i.metadata.name.clone().unwrap()).collect();
	assert_eq!(names, ["mine"]);
	let v = migrate::ingress_status(&[("IPAddress".into(), "10.0.0.1".into()), ("Hostname".into(), "lb.example.com".into())]);
	assert_eq!(v, json!({"loadBalancer": {"ingress": [{"ip": "10.0.0.1"}, {"hostname": "lb.example.com"}]}}));
}

#[test]
fn gateway_addresses_and_parameters() {
	let with = |extra: &str| {
		plan(&format!("{BASE}{}", gw("  - {name: http, port: 80, protocol: HTTP}").replace("spec:\n", &format!("spec:\n{extra}"))))
	};
	let p = with("  addresses: [{type: IPAddress, value: 192.0.2.10}, {value: \"2001:db8::1\"}, {type: IPAddress}]\n");
	assert!(p.accepted());
	assert_eq!(p.addresses, ["192.0.2.10", "2001:db8::1"]);
	assert_eq!(p.address_error, None);
	let p = with("  addresses: [{type: test/fake-invalid-type, value: x}, {value: 192.0.2.10}]\n");
	assert_eq!(cond(&p.conds, "Accepted").reason, "UnsupportedAddress");
	assert!(!p.accepted());
	let p = with("  addresses: [{type: Hostname, value: gw.example.com}]\n");
	assert_eq!(cond(&p.conds, "Accepted").reason, "UnsupportedAddress");
	let p = with("  addresses: [{value: 0.0.0.0}, {value: 192.0.2.10}]\n");
	assert!(p.accepted(), "accepted, but not programmed");
	assert!(p.address_error.as_deref().is_some_and(|e| e.contains("0.0.0.0")));
	let p = with("  infrastructure: {parametersRef: {group: invalid.io, kind: InvalidParameters, name: invalid}}\n");
	assert_eq!((cond(&p.conds, "Accepted").status, cond(&p.conds, "Accepted").reason.as_str()), (false, "InvalidParameters"));
	let p = with("  infrastructure: {labels: {a: b}, annotations: {c: d}}\n");
	assert!(p.accepted());
}

#[test]
fn frontend_client_certificate_validation() {
	let (crt, key) = crate::pem::tests::pair("example.com");
	let (ca, _) = crate::pem::tests::pair("clients");
	let yaml = format!(
		r#"{BASE}
---
apiVersion: v1
kind: Secret
metadata: {{name: cert, namespace: default}}
stringData:
  tls.crt: |
{}
  tls.key: |
{}
---
apiVersion: v1
kind: ConfigMap
metadata: {{name: ca, namespace: default}}
data:
  ca.crt: |
{}
---
apiVersion: v1
kind: ConfigMap
metadata: {{name: ca, namespace: other}}
data:
  ca.crt: |
{}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {{name: gw, namespace: default}}
spec:
  gatewayClassName: rproxy
  tls:
    frontend:
      default:
        validation: {{caCertificateRefs: [{{kind: ConfigMap, group: "", name: ca}}]}}
      perPort:
        - {{port: 8443, tls: {{validation: {{caCertificateRefs: [{{kind: ConfigMap, name: missing}}]}}}}}}
        - {{port: 9443, tls: {{validation: {{caCertificateRefs: [{{kind: Service, group: "", name: web}}]}}}}}}
        - {{port: 10443, tls: {{validation: {{caCertificateRefs: [{{kind: ConfigMap, name: ca, namespace: other}}]}}}}}}
        - {{port: 11443, tls: {{validation: {{caCertificateRefs: [{{kind: ConfigMap, name: ca}}], mode: AllowInsecureFallback}}}}}}
  listeners:
    - {{name: a, port: 443, protocol: HTTPS, tls: {{certificateRefs: [{{name: cert}}]}}}}
    - {{name: b, port: 8443, protocol: HTTPS, tls: {{certificateRefs: [{{name: cert}}]}}}}
    - {{name: c, port: 9443, protocol: HTTPS, tls: {{certificateRefs: [{{name: cert}}]}}}}
    - {{name: d, port: 10443, protocol: HTTPS, tls: {{certificateRefs: [{{name: cert}}]}}}}
    - {{name: e, port: 11443, protocol: HTTPS, tls: {{certificateRefs: [{{name: cert}}]}}}}
    - {{name: f, port: 80, protocol: HTTP}}
"#,
		indent(&crt),
		indent(&key),
		indent(&ca),
		indent(&ca)
	);
	let p = plan(&yaml);
	let l = |n: &str| p.listeners.iter().find(|x| x.name == n).unwrap();
	let rules = p.rules_json();
	let rule = |port: u16| rules.iter().find(|r| r["listen_port"] == port);
	let auth = &rule(443).unwrap()["tls"]["client_auth"];
	assert_eq!(auth["mode"], "required");
	let file = auth["ca_file"].as_str().unwrap().rsplit('/').next().unwrap();
	assert_eq!(String::from_utf8_lossy(&p.files[file]).trim(), ca.trim());
	for (n, reason) in [("b", "InvalidCACertificateRef"), ("c", "InvalidCACertificateKind"), ("d", "RefNotPermitted")] {
		assert_eq!(cond(&l(n).conds, "ResolvedRefs").reason, reason, "{n}");
		let a = cond(&l(n).conds, "Accepted");
		assert_eq!((a.status, a.reason.as_str()), (false, "NoValidCACertificate"), "{n}");
	}
	assert!(rule(8443).is_none() && rule(9443).is_none() && rule(10443).is_none());
	// insecure fallback: no client certificate asked for, a condition on the Gateway
	assert!(rule(11443).unwrap()["tls"].get("client_auth").is_none());
	assert!(cond(&p.conds, "InsecureFrontendValidationMode").status);
	assert!(cond(&l("f").conds, "Accepted").status, "HTTP listeners are not affected");
	assert!(cond(&l("a").conds, "ResolvedRefs").status);
}

#[test]
fn listener_sets() {
	let yaml = format!(
		r#"{BASE}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {{name: gw, namespace: default}}
spec:
  gatewayClassName: rproxy
  allowedListeners: {{namespaces: {{from: Same}}}}
  listeners:
    - {{name: main, port: 80, protocol: HTTP, hostname: gw.example.com}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: ListenerSet
metadata: {{name: ls1, namespace: default, creationTimestamp: "2026-01-01T00:00:00Z"}}
spec:
  parentRef: {{name: gw}}
  listeners:
    - {{name: main, port: 80, protocol: HTTP, hostname: one.example.com}}
    - {{name: clash, port: 80, protocol: HTTP, hostname: gw.example.com}}
    - {{name: tcp, port: 80, protocol: TCP}}
    - {{name: after-tcp, port: 80, protocol: HTTP, hostname: two.example.com}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: ListenerSet
metadata: {{name: ls2, namespace: default, creationTimestamp: "2026-01-02T00:00:00Z"}}
spec:
  parentRef: {{name: gw}}
  listeners:
    - {{name: only, port: 80, protocol: HTTP, hostname: one.example.com}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: ListenerSet
metadata: {{name: far, namespace: other}}
spec:
  parentRef: {{name: gw, namespace: default}}
  listeners: [{{name: x, port: 8080, protocol: HTTP}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {{name: to-set, namespace: default}}
spec:
  parentRefs:
    - {{kind: ListenerSet, group: gateway.networking.k8s.io, name: ls1, sectionName: main}}
    - {{kind: ListenerSet, group: gateway.networking.k8s.io, name: ls1, sectionName: missing}}
  rules: [{{backendRefs: [{{name: web, port: 80}}]}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {{name: to-gateway, namespace: default}}
spec:
  parentRefs: [{{name: gw}}]
  rules: [{{backendRefs: [{{name: web, port: 80}}]}}]
"#
	);
	let p = plan(&yaml);
	assert_eq!(p.listeners.len(), 1, "the Gateway's status has its own listeners only");
	assert_eq!(p.attached_listener_sets, 1, "ls1 (ls2 has no valid listener, far is not allowed)");
	let set = |n: &str| p.listener_sets.iter().find(|s| s.name == n).unwrap();
	assert_eq!(set("far").accepted.reason, "NotAllowed");
	assert!(set("ls1").accepted.status);
	let l = |s: &str, n: &str| set(s).listeners.iter().find(|x| x.name == n).unwrap().clone();
	assert_eq!(cond(&l("ls1", "clash").conds, "Conflicted").reason, "HostnameConflict", "the Gateway's listener wins");
	assert_eq!(cond(&l("ls1", "tcp").conds, "Conflicted").reason, "ProtocolConflict");
	assert!(cond(&l("ls1", "main").conds, "Accepted").status);
	assert!(cond(&l("ls1", "after-tcp").conds, "Accepted").status, "a listener that lost a conflict takes nothing");
	assert_eq!(cond(&l("ls2", "only").conds, "Conflicted").reason, "HostnameConflict", "the older ListenerSet wins");
	assert_eq!((set("ls2").accepted.status, set("ls2").accepted.reason.as_str()), (false, "ListenersNotValid"));
	// routes: through the ListenerSet to its listener only; through the Gateway to the Gateway's
	assert_eq!(l("ls1", "main").attached, 1);
	assert_eq!(p.listeners[0].attached, 1);
	let parents: Vec<_> = p.parents.iter().filter(|x| x.name == "to-set").collect();
	assert_eq!(parents.len(), 2);
	assert!(cond(&parents[0].conds, "Accepted").status);
	assert_eq!(cond(&parents[1].conds, "Accepted").reason, "NoMatchingParent");
	let http = &p.rules_json()[0]["http"];
	let rules: Vec<String> = http["routes"].as_array().unwrap().iter().map(|r| r["match"].as_str().unwrap().to_string()).collect();
	assert!(rules.iter().any(|r| r.contains("Host(`one.example.com`)")), "{rules:?}");
	assert!(rules.iter().any(|r| r.contains("Host(`gw.example.com`)")), "{rules:?}");
}

#[test]
fn grpc_routes() {
	let yaml = format!(
		"{BASE}{}{}",
		gw("  - {name: http, port: 80, protocol: HTTP}"),
		r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: GRPCRoute
metadata: {name: grpc, namespace: default}
spec:
  parentRefs: [{name: gw}]
  hostnames: [grpc.example.com]
  rules:
    - matches: [{method: {service: echo.Echo, method: Hello}}]
      filters: [{type: RequestHeaderModifier, requestHeaderModifier: {set: [{name: X-A, value: a}]}}]
      backendRefs: [{name: web, port: 80}]
    - matches: [{method: {service: echo.Echo}}, {method: {method: Bye}}, {method: {type: RegularExpression, service: "echo\\..*"}, headers: [{name: v, value: "2"}]}]
      backendRefs: [{name: web, port: 80}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: grpc, namespace: default}
spec:
  parentRefs: [{name: gw}]
  hostnames: [web.example.com]
  rules: [{backendRefs: [{name: web, port: 80}]}]
"#
	);
	let p = plan(&yaml);
	let grpc = p.parents.iter().find(|x| x.kind == RouteKind::Grpc).unwrap();
	assert!(cond(&grpc.conds, "Accepted").status, "{:?}", grpc.conds);
	assert_eq!(p.listeners[0].attached, 2);
	let http = &p.rules_json()[0]["http"];
	let routes = http["routes"].as_array().unwrap();
	let m = |n: &str| routes.iter().find(|r| r["name"] == n).unwrap_or_else(|| panic!("{n}: {routes:#?}"))["match"].clone();
	assert_eq!(m("grpc:default/grpc/r0/m0/h0"), "Host(`grpc.example.com`) && Path(`/echo.Echo/Hello`)");
	assert_eq!(m("grpc:default/grpc/r1/m0/h0"), "Host(`grpc.example.com`) && (Path(`/echo.Echo`) || PathPrefix(`/echo.Echo/`))");
	assert_eq!(m("grpc:default/grpc/r1/m1/h0"), "Host(`grpc.example.com`) && PathRegexp(`^(?:/[^/]+/Bye)$`)");
	assert_eq!(
		m("grpc:default/grpc/r1/m2/h0"),
		"Host(`grpc.example.com`) && PathRegexp(`^(?:/(?:echo\\..*)/(?:[^/]+))$`) && Header(`v`, `2`)"
	);
	assert!(routes.iter().any(|r| r["name"] == "default/grpc/r0/m0/h0"), "the HTTPRoute of the same name stays apart");
	// gRPC backends: HTTP/2 without TLS
	assert_eq!(http["services"]["grpc:default/grpc/r0"]["protocol"], "h2c");
	assert!(http["services"]["default/grpc/r0"].get("protocol").is_none());
	let r0 = routes.iter().find(|r| r["name"] == "grpc:default/grpc/r0/m0/h0").unwrap();
	assert!(http["middlewares"][r0["middlewares"][0].as_str().unwrap()]["headers"].is_object());
}

#[test]
fn backend_tls_policies() {
	let (ca, _) = crate::pem::tests::pair("ca");
	let (crt, key) = crate::pem::tests::pair("client");
	let yaml = format!(
		r#"{BASE}
---
apiVersion: v1
kind: Secret
metadata: {{name: client, namespace: default}}
stringData:
  tls.crt: |
{}
  tls.key: |
{}
---
apiVersion: v1
kind: ConfigMap
metadata: {{name: ca, namespace: default}}
data:
  ca.crt: |
{}
---
apiVersion: v1
kind: Service
metadata: {{name: secure, namespace: default}}
spec: {{ports: [{{name: https, port: 443}}]}}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {{name: secure-1, namespace: default, labels: {{kubernetes.io/service-name: secure}}}}
addressType: IPv4
ports: [{{name: https, port: 8443}}]
endpoints: [{{addresses: [10.0.5.1]}}]
---
apiVersion: v1
kind: Service
metadata: {{name: broken, namespace: default}}
spec: {{ports: [{{name: https, port: 443}}]}}
---
apiVersion: discovery.k8s.io/v1
kind: EndpointSlice
metadata: {{name: broken-1, namespace: default, labels: {{kubernetes.io/service-name: broken}}}}
addressType: IPv4
ports: [{{name: https, port: 8443}}]
endpoints: [{{addresses: [10.0.6.1]}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {{name: old, namespace: default, creationTimestamp: "2026-01-01T00:00:00Z"}}
spec:
  targetRefs: [{{group: "", kind: Service, name: secure}}]
  validation:
    caCertificateRefs: [{{group: "", kind: ConfigMap, name: ca}}]
    hostname: abc.example.com
    subjectAltNames: [{{type: URI, uri: "spiffe://abc.example.com/x"}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {{name: newer, namespace: default, creationTimestamp: "2026-01-02T00:00:00Z"}}
spec:
  targetRefs: [{{group: "", kind: Service, name: secure}}]
  validation: {{wellKnownCACertificates: System, hostname: other.example.com}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {{name: no-ca, namespace: default}}
spec:
  targetRefs: [{{group: "", kind: Service, name: broken, sectionName: https}}]
  validation: {{caCertificateRefs: [{{group: "", kind: Secret, name: ca}}], hostname: abc.example.com}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {{name: gw, namespace: default}}
spec:
  gatewayClassName: rproxy
  tls: {{backend: {{clientCertificateRef: {{name: client}}}}}}
  listeners: [{{name: http, port: 80, protocol: HTTP}}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {{name: tls, namespace: default}}
spec:
  parentRefs: [{{name: gw}}]
  rules:
    - matches: [{{path: {{value: /secure}}}}]
      backendRefs: [{{name: secure, port: 443}}]
    - matches: [{{path: {{value: /broken}}}}]
      backendRefs: [{{name: broken, port: 443}}]
"#,
		indent(&crt),
		indent(&key),
		indent(&ca)
	);
	let p = plan(&yaml);
	assert!(cond(&p.conds, "ResolvedRefs").status, "the Gateway's client certificate");
	let http = &p.rules_json()[0]["http"];
	let svc = &http["services"]["default/tls/r0"];
	assert_eq!(svc["servers"][0]["url"], "https://10.0.5.1:8443");
	assert_eq!(svc["tls"]["server_name"], "abc.example.com", "the older policy wins");
	assert_eq!(svc["tls"]["subject_alt_names"], json!(["spiffe://abc.example.com/x"]));
	let ca_name = svc["tls"]["ca_file"].as_str().unwrap().rsplit('/').next().unwrap();
	assert_eq!(String::from_utf8_lossy(&p.files[ca_name]).trim(), ca.trim());
	assert!(svc["tls"]["cert_file"].as_str().is_some() && svc["tls"]["key_file"].as_str().is_some());
	// no usable CA: nothing is sent to that backend
	let broken = http["routes"].as_array().unwrap().iter().find(|r| r["name"] == "default/tls/r1/m0/h0").unwrap();
	assert_eq!(broken["middlewares"], json!([http::RESPOND_500]));
	let st = |n: &str| p.backend_tls.iter().find(|s| s.name == n).cloned();
	assert!(cond(&st("old").unwrap().conds, "Accepted").status);
	assert_eq!(st("old").unwrap().ancestor["name"], "gw");
	assert_eq!(cond(&st("newer").unwrap().conds, "Accepted").reason, "Conflicted");
	let no_ca = st("no-ca").unwrap();
	assert_eq!(cond(&no_ca.conds, "Accepted").reason, "NoValidCACertificate");
	assert_eq!(cond(&no_ca.conds, "ResolvedRefs").reason, "InvalidKind");
	assert!(cond(&p.parents[0].conds, "ResolvedRefs").status, "the route's references are fine");
}
