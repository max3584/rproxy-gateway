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
  - {name: e, port: 80, protocol: HTTP, allowedRoutes: {kinds: [{kind: GRPCRoute}]}}
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
		json!({"headers": {"request": {"set": {"X-A": "a", "X-B": "b"}, "remove": ["X-C"]}}})
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
	let p = plan(&yaml);
	let c = cond(&p.parents[0].conds, "Accepted");
	assert_eq!((c.status, c.reason.as_str()), (false, "UnsupportedValue"));
	assert!(p.rules_json()[0]["http"]["routes"].as_array().unwrap().is_empty());
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
	assert_eq!(p.ports().len(), 3);
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
		json!([{"server_names": ["registry.example.com"], "remote_addr": "10.96.0.30", "remote_port": 443, "passthrough": true}])
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
