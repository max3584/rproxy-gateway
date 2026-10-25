//! The controller against a fake rproxy that follows the documented rule set API
//! (rproxy-api docs/API.md "v0.4 settings": `PUT /rulesets/{name}` with
//! `generation` and `If-Match`, the etag, `stale_generation`, `precondition_failed`).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{Value, json};

use super::*;
use crate::render::GatewayPlan;
use crate::rproxy::model::{Protocol, Rule, Target};

#[derive(Default)]
struct Fake {
	sets: BTreeMap<String, (i64, String, Vec<Value>)>,
	puts: Vec<(String, Option<String>, i64)>,
	files: BTreeSet<String>,
	/// rule keys rproxy fails (e.g. a port in use)
	failing: BTreeSet<String>,
	/// `features.graceful_shutdown` (rproxy v0.4.1)
	graceful: bool,
	/// `features.tokens_reload` (rproxy v0.4.2)
	tokens_reload: bool,
	/// Tokens that may read (`GET /rules`): the UI's
	readers: BTreeSet<String>,
	/// HTTP routes whose match has `BAD` are refused without naming them (else by name)
	unnamed: bool,
	/// dry runs asked for
	dry_runs: usize,
	/// dry runs answer 503
	dry_run_fails: bool,
}

fn etag(generation: i64, rules: &[Value]) -> String {
	format!("g{generation}-{}", crate::pem::short_hash(&serde_json::to_vec(rules).unwrap()))
}

fn view(rule: &Value, failing: &BTreeSet<String>) -> Value {
	let key = format!("{}/{}:{}", rule["protocol"].as_str().unwrap(), rule["listen_addr"].as_str().unwrap(), rule["listen_port"]);
	let failed = failing.contains(&key);
	let mut v = rule.clone();
	v["state"] = json!(if failed { "failed" } else { "running" });
	v["conditions"] = json!([
		{"type": "Accepted", "status": "True", "reason": "Accepted", "message": "", "last_transition": 1},
		{"type": "Programmed", "status": if failed {"False"} else {"True"}, "reason": if failed {"BindFailed"} else {"Listening"}, "message": if failed {"address in use"} else {""}, "last_transition": 1}
	]);
	v
}

async fn handle(fake: Arc<Mutex<Fake>>, req: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
	let method = req.method().clone();
	let path = req.uri().path().to_string();
	let dry_run = req.uri().query() == Some("dry_run=true");
	let if_match = req.headers().get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
	let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
	let body = req.into_body().collect().await.unwrap().to_bytes();
	let reply = |status: u16, v: Value| Response::builder().status(status).body(Full::new(Bytes::from(v.to_string()))).unwrap();
	let reader = auth.as_deref().and_then(|a| a.strip_prefix("Bearer ")).is_some_and(|t| fake.lock().unwrap().readers.contains(t));
	if method == hyper::Method::GET && path == "/rules" && reader {
		return reply(200, json!({"rules": []}));
	}
	if path != "/readyz" && path != "/files" && auth.as_deref() != Some("Bearer secret") {
		return reply(401, json!({"error": "unauthorized", "code": "unauthorized"}));
	}
	let mut f = fake.lock().unwrap();
	match (method.as_str(), path.as_str()) {
		("GET", "/readyz") => reply(200, json!({"ready": true})),
		("POST", "/files") => {
			let asked: Vec<String> = serde_json::from_slice(&body).unwrap_or_default();
			reply(200, json!(asked.into_iter().filter(|n| f.files.contains(n)).collect::<Vec<_>>()))
		}
		("GET", "/capabilities") => {
			let (graceful, reload) = (f.graceful, f.tokens_reload);
			reply(
				200,
				json!({"version": "0.4.0", "features": {"rulesets": true, "labels": true, "conditions": true, "readyz": true, "graceful_shutdown": graceful, "tokens_reload": reload}}),
			)
		}
		("GET", p) if p.starts_with("/rulesets/") => {
			let name = &p["/rulesets/".len()..];
			match f.sets.get(name) {
				Some((g, e, rules)) => {
					let views: Vec<Value> = rules.iter().map(|r| view(r, &f.failing)).collect();
					reply(200, json!({"name": name, "generation": g, "etag": e, "rules": views}))
				}
				None => reply(404, json!({"error": "no such rule set", "code": "not_found"})),
			}
		}
		("PUT", p) if p.starts_with("/rulesets/") => {
			let name = p["/rulesets/".len()..].to_string();
			let req: Value = serde_json::from_slice(&body).unwrap();
			let generation = req["generation"].as_i64().unwrap();
			let rules = req["rules"].as_array().unwrap().clone();
			for (i, r) in rules.iter().enumerate() {
				for route in r["http"]["routes"].as_array().into_iter().flatten() {
					if route["match"].as_str().is_some_and(|m| m.contains("BAD")) {
						let error = if f.unnamed {
							format!("rules[{i}]: http: something is wrong")
						} else {
							format!("rules[{i}]: route {}: match: PathRegexp: regex parse error", route["name"].as_str().unwrap())
						};
						return reply(400, json!({"error": error, "code": "invalid"}));
					}
				}
			}
			if dry_run && f.dry_run_fails {
				f.dry_runs += 1;
				return reply(503, json!({"error": "busy", "code": "unavailable"}));
			}
			if rules.iter().any(|r| r["remote_addr"] == "whole") {
				return reply(400, json!({"error": "invalid body: unknown field `x`", "code": "invalid"}));
			}
			if dry_run {
				f.dry_runs += 1;
				return reply(200, json!({"name": name, "generation": generation, "etag": "dry", "dry_run": true, "results": []}));
			}
			f.puts.push((name.clone(), if_match.clone(), generation));
			if let Some(i) = rules.iter().position(|r| r["remote_addr"] == "bad name") {
				return reply(400, json!({"error": format!("rules[{i}]: remote_addr: not a host name"), "code": "invalid"}));
			}
			if let Some((g, e, _)) = f.sets.get(&name) {
				if let Some(m) = &if_match {
					if m != e {
						return reply(412, json!({"error": "etag differs", "code": "precondition_failed"}));
					}
				}
				if generation < *g {
					return reply(409, json!({"error": "older generation", "code": "stale_generation"}));
				}
			}
			let e = etag(generation, &rules);
			let results: Vec<Value> = rules
				.iter()
				.map(
					|r| json!({"rule": format!("tcp/0.0.0.0:{}", r["listen_port"]), "action": "create", "change": "recreate", "state": "running"}),
				)
				.collect();
			f.sets.insert(name.clone(), (generation, e.clone(), rules));
			reply(200, json!({"name": name, "generation": generation, "etag": e, "dry_run": false, "results": results}))
		}
		_ => reply(404, json!({"error": "not found", "code": "not_found"})),
	}
}

async fn start(fake: Arc<Mutex<Fake>>) -> u16 {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let port = listener.local_addr().unwrap().port();
	tokio::spawn(async move {
		loop {
			let (tcp, _) = listener.accept().await.unwrap();
			let fake = fake.clone();
			tokio::spawn(async move {
				let svc = hyper::service::service_fn(move |req| {
					let fake = fake.clone();
					async move { Ok::<_, std::convert::Infallible>(handle(fake, req).await) }
				});
				let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), svc).await;
			});
		}
	});
	port
}

fn plan(port: u16, files: &[&str]) -> GatewayPlan {
	GatewayPlan {
		namespace: "default".into(),
		name: "gw".into(),
		generation: 3,
		ruleset: "k8s/default/gw".into(),
		rules: vec![Rule {
			protocol: Protocol::Tcp,
			listen_addr: "0.0.0.0".into(),
			listen_port: port,
			targets: vec![Target { addr: "10.0.0.1".into(), port: 80, weight: None }],
			..Default::default()
		}],
		files: files.iter().map(|f| (f.to_string(), b"x".to_vec())).collect(),
		..Default::default()
	}
}

#[tokio::test]
async fn sets_are_put_once_and_again_after_a_restart() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = Endpoint {
		pod: "rproxy-0".into(),
		uid: "u1".into(),
		ip: "127.0.0.1".into(),
		host_ip: None,
		api_port: port,
		certsync_port: port,
		certs: None,
		..Default::default()
	};
	let mut applied = Applied::new();
	let mut caps = HashMap::new();

	let p = plan(80, &[]);
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v, _) if v.len() == 1), "{r:?}");
	assert!(!again);
	assert_eq!(fake.lock().unwrap().puts, vec![("k8s/default/gw".to_string(), None, 3)]);

	// nothing changed: no PUT
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Synced(..)));
	assert_eq!(fake.lock().unwrap().puts.len(), 1);

	// a change: PUT with If-Match of the current etag
	let p2 = plan(81, &[]);
	let current = fake.lock().unwrap().sets["k8s/default/gw"].1.clone();
	sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert_eq!(fake.lock().unwrap().puts.last().unwrap().1.as_deref(), Some(current.as_str()));

	// rproxy restarted: the set is gone, PUT again (no If-Match)
	fake.lock().unwrap().sets.clear();
	let (r, _) = sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Synced(..)));
	assert_eq!(fake.lock().unwrap().puts.last().unwrap().1, None);
	assert_eq!(fake.lock().unwrap().puts.len(), 3);

	// someone else changed the set: the etag differs, so it is PUT again
	fake.lock().unwrap().sets.get_mut("k8s/default/gw").unwrap().1 = "g3-other".into();
	sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert_eq!(fake.lock().unwrap().puts.len(), 4);

	// a Gateway created again (generation 1) still applies over the stored generation
	let mut p3 = plan(82, &[]);
	p3.generation = 1;
	let (r, _) = sync_pod(&rp, &ep, &p3, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Synced(..)), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.last().unwrap().2, 3);
}

#[tokio::test]
async fn waits_for_certificate_files_and_reports_failures() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = Endpoint {
		pod: "rproxy-0".into(),
		uid: "u1".into(),
		ip: "127.0.0.1".into(),
		host_ip: None,
		api_port: port,
		certsync_port: port,
		certs: None,
		..Default::default()
	};
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let p = plan(443, &["abc.crt", "abc.key"]);
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Pending(m) if m.contains("certificate files")), "{r:?}");
	assert!(again);
	assert!(fake.lock().unwrap().puts.is_empty());

	fake.lock().unwrap().files = ["abc.crt".to_string(), "abc.key".to_string()].into();
	fake.lock().unwrap().failing.insert("tcp/0.0.0.0:443".into());
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(again, "a failed rule is tried again soon");
	let st = status::gateway_status(
		&GatewayPlan {
			listeners: vec![crate::render::ListenerPlan {
				name: "https".into(),
				supported_kinds: vec![],
				attached: 0,
				conds: vec![crate::render::status::Cond::ok("Accepted", "Accepted")],
				rule_key: Some("tcp/0.0.0.0:443".into()),
				servable: true,
			}],
			..p.clone()
		},
		&[("IPAddress".into(), "10.0.0.9".into())],
		&[r],
		None,
		"T",
	);
	let c = st["listeners"][0]["conditions"].as_array().unwrap().iter().find(|c| c["type"] == "Programmed").unwrap().clone();
	assert_eq!(c["status"], "False");
	assert!(c["message"].as_str().unwrap().contains("address in use"), "{c}");
	// the next pass PUTs again (rproxy retries the rule)
	sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert_eq!(fake.lock().unwrap().puts.len(), 2);
}

#[tokio::test]
async fn a_wrong_token_is_reported() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "wrong").unwrap();
	let ep = Endpoint {
		pod: "rproxy-0".into(),
		uid: "u1".into(),
		ip: "127.0.0.1".into(),
		host_ip: None,
		api_port: port,
		certsync_port: port,
		certs: None,
		..Default::default()
	};
	let (r, _) = sync_pod(&rp, &ep, &plan(80, &[]), &mut Applied::new(), &mut HashMap::new()).await;
	assert!(matches!(&r, PodSync::NotReady(m) if m.contains("401")), "{r:?}");
}

#[tokio::test]
async fn graceful_shutdown_is_known_from_the_image_or_the_pods() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = |uid: &str, image: &str| Endpoint {
		pod: format!("rproxy-{uid}"),
		uid: uid.into(),
		ip: "127.0.0.1".into(),
		api_port: port,
		certsync_port: port,
		rproxy_image: Some(image.into()),
		..Default::default()
	};
	let mut caps = HashMap::new();
	// the image this controller ships: known without asking
	assert!(graceful(&rp, provision::RPROXY_IMAGE, &[], &mut caps).await);
	assert!(caps.is_empty());
	// another image: not before a pod running it says so
	assert!(!graceful(&rp, "example/rproxy:custom", &[], &mut caps).await, "no pods yet: the preStop stays");
	assert!(!graceful(&rp, "example/rproxy:custom", &[ep("u1", "example/rproxy:custom")], &mut caps).await, "an older rproxy");
	fake.lock().unwrap().graceful = true;
	let pods = [ep("u1", "example/rproxy:custom"), ep("u2", "example/rproxy:custom")];
	assert!(graceful(&rp, "example/rproxy:custom", &pods, &mut caps).await, "u2 answers graceful_shutdown");
	assert!(!graceful(&rp, "example/rproxy:other", &pods, &mut caps).await, "only pods running the image count");
	// remembered: every pod of the Gateway restarting at once does not roll it back to the preStop
	assert!(graceful(&rp, "example/rproxy:custom", &[], &mut HashMap::new()).await);
}

#[tokio::test]
async fn tokens_reload_is_known_from_the_image_or_the_pods() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = |uid: &str| Endpoint {
		pod: format!("rproxy-{uid}"),
		uid: uid.into(),
		ip: "127.0.0.1".into(),
		api_port: port,
		rproxy_image: Some("example/rproxy:reload".into()),
		..Default::default()
	};
	let mut caps = HashMap::new();
	assert!(image_feature(&rp, provision::RPROXY_IMAGE, "tokens_reload", &[], &mut caps).await, "the shipped image (v0.4.17)");
	assert!(!image_feature(&rp, provision::RPROXY_IMAGE, "another", &[], &mut caps).await);
	assert!(!image_feature(&rp, "example/rproxy:reload", "tokens_reload", &[ep("r1")], &mut caps).await, "rproxy before v0.4.2");
	fake.lock().unwrap().tokens_reload = true;
	assert!(image_feature(&rp, "example/rproxy:reload", "tokens_reload", &[ep("r1"), ep("r2")], &mut caps).await);
	assert!(image_feature(&rp, "example/rproxy:reload", "tokens_reload", &[], &mut HashMap::new()).await, "remembered");
	assert!(!graceful(&rp, "example/rproxy:reload", &[], &mut HashMap::new()).await, "per feature");
}

#[tokio::test]
async fn the_ui_gets_pods_that_take_its_token() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = |uid: &str| Endpoint {
		pod: format!("rproxy-{uid}"),
		uid: uid.into(),
		ip: "127.0.0.1".into(),
		api_port: port,
		..Default::default()
	};
	// the token file was not read again yet: 401, not listed, not asked again before ASK_EVERY
	assert!(ui::accepting(&rp, &[ep("ui-1")], "ui-token", "x").await.is_empty());
	fake.lock().unwrap().readers.insert("ui-token".into());
	assert!(ui::accepting(&rp, &[ep("ui-1")], "ui-token", "x").await.is_empty(), "asked a moment ago");
	// another pod that already took it; then remembered without asking
	assert_eq!(ui::accepting(&rp, &[ep("ui-1"), ep("ui-2")], "ui-token", "x").await.len(), 1);
	fake.lock().unwrap().readers.clear();
	assert_eq!(ui::accepting(&rp, &[ep("ui-2")], "ui-token", "x").await.len(), 1);
	// a pod gone is forgotten
	ui::forget(&["ui-1".to_string()].into());
	assert!(ui::accepting(&rp, &[ep("ui-2")], "ui-token", "x").await.is_empty(), "asked again: 401");
}

#[tokio::test]
async fn a_refused_rule_does_not_stop_the_others() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = Endpoint {
		pod: "rproxy-0".into(),
		uid: "u1".into(),
		ip: "127.0.0.1".into(),
		host_ip: None,
		api_port: port,
		certsync_port: port,
		certs: None,
		..Default::default()
	};
	let mut p = plan(80, &[]);
	p.raw = vec![json!({"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": 25, "remote_addr": "bad name", "remote_port": 25})];
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	let PodSync::Synced(views, _) = r else { panic!("{r:?}") };
	assert_eq!(views.len(), 2);
	let bad = views.iter().find(|v| v.listen_port == 25).unwrap();
	assert_eq!(bad.condition("Accepted").reason, "Invalid");
	assert!(bad.condition("Programmed").message.contains("not a host name"));
	assert_eq!(fake.lock().unwrap().sets["k8s/default/gw"].2.len(), 1, "the good rule is applied");
	// the next pass: nothing to PUT, the refusal is still reported
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v, _) if v.len() == 2), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.len(), 2);
	// after `EXCLUDED_FOR` the refused rule is asked again with the plan unchanged (#98)
	for a in applied.values_mut() {
		a.rejected_since = Some(std::time::Instant::now().checked_sub(EXCLUDED_FOR).unwrap());
	}
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v, _) if v.len() == 2), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.len(), 4, "asked again");
	assert!(applied.values().all(|a| a.rejected_since.is_some_and(|at| at.elapsed() < EXCLUDED_FOR)));
}

/// rproxy refuses one tenant's route in the port's HTTP rule: only that route is left out (found by
/// the name in rproxy's message, or by halving with dry runs), and its status says so (#92).
#[tokio::test]
async fn a_refused_route_does_not_stop_the_port() {
	use crate::render::RouteKind;
	use crate::rproxy::model::{Http, HttpRoute, Service};
	for unnamed in [false, true] {
		let fake = Arc::new(Mutex::new(Fake { unnamed, ..Default::default() }));
		let port = start(fake.clone()).await;
		let rp = Client::new(None, "secret").unwrap();
		let ep = Endpoint {
			pod: "rproxy-0".into(),
			uid: "u1".into(),
			ip: "127.0.0.1".into(),
			api_port: port,
			certsync_port: port,
			..Default::default()
		};
		let mut p = plan(443, &[]);
		let route = |owner: &str, m: &str| HttpRoute {
			name: format!("{owner}/r0/m0/h0"),
			rule: m.into(),
			service: Some(format!("{owner}/r0")),
			..Default::default()
		};
		let owners = ["a/one", "b/two", "c/bad", "d/four", "e/five"];
		let mut http = Http::default();
		for o in owners {
			http.routes.push(route(o, if o == "c/bad" { "PathRegexp(`BAD(`)" } else { "PathPrefix(`/`)" }));
			http.services.insert(format!("{o}/r0"), Service { servers: vec![Default::default()], ..Default::default() });
		}
		p.rules[0].targets = vec![];
		p.rules[0].http = Some(http);
		let mut applied = Applied::new();
		let mut caps = HashMap::new();
		let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
		let PodSync::Synced(views, excluded) = &r else { panic!("{unnamed}: {r:?}") };
		assert_eq!(views.len(), 1, "{unnamed}: the rule runs");
		assert_eq!(excluded.iter().map(|e| e.owner.as_str()).collect::<Vec<_>>(), ["c/bad"], "{unnamed}");
		assert_eq!(excluded[0].rule_key, "tcp/0.0.0.0:443");
		{
			let f = fake.lock().unwrap();
			let set = &f.sets["k8s/default/gw"].2[0]["http"];
			let names: Vec<&str> = set["routes"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
			assert_eq!(names, ["a/one/r0/m0/h0", "b/two/r0/m0/h0", "d/four/r0/m0/h0", "e/five/r0/m0/h0"], "{unnamed}");
			assert!(set["services"].get("c/bad/r0").is_none());
			assert_eq!(f.dry_runs > 0, unnamed, "dry runs only when rproxy names no route");
		}
		// the route's status; the others keep theirs
		let parent = |name: &str| crate::render::ParentStatus {
			kind: RouteKind::Http,
			namespace: name.split('/').next().unwrap().into(),
			name: name.split('/').nth(1).unwrap().into(),
			generation: 1,
			parent_ref: Default::default(),
			conds: vec![crate::render::status::Cond::ok("Accepted", "Accepted")],
			rule_keys: ["tcp/0.0.0.0:443".to_string()].into(),
		};
		let pods = [r.clone()];
		let bad = status::parent_conds(&parent("c/bad"), &pods);
		let c = crate::render::status::get(&bad, "Accepted").unwrap();
		assert!(!c.status && c.reason == "UnsupportedValue" && c.message.contains("regex parse error") == !unnamed, "{c:?}");
		assert!(crate::render::status::get(&status::parent_conds(&parent("a/one"), &pods), "Accepted").unwrap().status);
		// the next pass: nothing PUT again, the route still left out
		let puts = fake.lock().unwrap().puts.len();
		let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
		assert!(matches!(&r, PodSync::Synced(_, ex) if ex.len() == 1), "{r:?}");
		assert_eq!(fake.lock().unwrap().puts.len(), puts);
		// another tenant's change: the route refused before (unchanged) stays out without asking again
		let dry = fake.lock().unwrap().dry_runs;
		let mut p2 = p.clone();
		p2.rules[0].http.as_mut().unwrap().routes.push(route("f/six", "PathPrefix(`/six`)"));
		p2.rules[0]
			.http
			.as_mut()
			.unwrap()
			.services
			.insert("f/six/r0".into(), Service { servers: vec![Default::default()], ..Default::default() });
		let (r, _) = sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
		assert!(matches!(&r, PodSync::Synced(_, ex) if ex.len() == 1 && ex[0].owner == "c/bad"), "{r:?}");
		let f = fake.lock().unwrap();
		assert_eq!((f.puts.len(), f.dry_runs), (puts + 1, dry), "one PUT, no dry runs");
	}
}

/// Routes of a ListenerSet's listener (`@<set>/<listener>`): found by halving like any (#95),
/// not taken for "not a route's fault" (which left the whole port out).
#[tokio::test]
async fn listener_set_routes_are_found() {
	let fake = Arc::new(Mutex::new(Fake { unnamed: true, ..Default::default() }));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = local(port);
	let mut p = http_plan(443, &[("a/one", false), ("b/bad", true), ("c/three", false)]);
	for r in &mut p.rules[0].http.as_mut().unwrap().routes {
		r.name = format!("{}@team/https", r.name);
	}
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v, ex) if v.len() == 1 && ex.len() == 1 && ex[0].owner == "b/bad"), "{r:?}");
	assert_eq!(fake.lock().unwrap().sets["k8s/default/gw"].2[0]["http"]["routes"].as_array().unwrap().len(), 2);
}

/// The dry runs run out: the routes found so far stay out and the next pass goes on (#95), until
/// all are found and the rule runs.
#[tokio::test]
async fn running_out_of_dry_runs_goes_on_next_pass() {
	let fake = Arc::new(Mutex::new(Fake { unnamed: true, ..Default::default() }));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = local(port);
	let names: Vec<String> = (0..64).map(|i| format!("t/r{i}")).collect();
	let owners: Vec<(&str, bool)> = names.iter().enumerate().map(|(i, n)| (n.as_str(), i % 16 == 3)).collect();
	let p = http_plan(443, &owners);
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	let PodSync::Synced(views, found) = &r else { panic!("{r:?}") };
	assert!(again, "the next pass comes soon");
	assert_eq!(views.iter().find(|v| v.listen_port == 443).unwrap().state, "failed", "out for this pass");
	assert!(!found.is_empty() && found.len() < 4, "{found:?}");
	let mut passes = 1;
	let r = loop {
		let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
		passes += 1;
		if !again || passes > 6 {
			break r;
		}
	};
	let PodSync::Synced(views, excluded) = &r else { panic!("{r:?}") };
	let mut out: Vec<&str> = excluded.iter().map(|e| e.owner.as_str()).collect();
	out.sort();
	assert_eq!(out, ["t/r19", "t/r3", "t/r35", "t/r51"], "after {passes} passes");
	assert!(views.iter().all(|v| v.state == "running"), "{views:?}");
}

/// Remembered answers are asked again after `EXCLUDED_FOR` even with the plan unchanged (#95).
#[tokio::test]
async fn remembered_routes_are_asked_again() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = local(port);
	let p = http_plan(443, &[("a/one", false), ("b/bad", true)]);
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	let puts = fake.lock().unwrap().puts.len();
	sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert_eq!(fake.lock().unwrap().puts.len(), puts, "remembered");
	for a in applied.values_mut() {
		for e in &mut a.excluded {
			e.since = std::time::Instant::now().checked_sub(EXCLUDED_FOR).unwrap();
		}
	}
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(fake.lock().unwrap().puts.len() > puts, "asked again");
	assert!(matches!(&r, PodSync::Synced(_, ex) if ex.len() == 1 && ex[0].since.elapsed() < EXCLUDED_FOR), "{r:?}");
}

fn http_plan(port: u16, owners: &[(&str, bool)]) -> GatewayPlan {
	use crate::rproxy::model::{Http, HttpRoute, Service};
	let mut p = plan(port, &[]);
	let mut http = Http::default();
	for (o, bad) in owners {
		http.routes.push(HttpRoute {
			name: format!("{o}/r0/m0/h0"),
			rule: if *bad { "PathRegexp(`BAD(`)".into() } else { "PathPrefix(`/`)".into() },
			service: Some(format!("{o}/r0")),
			..Default::default()
		});
		http.services.insert(format!("{o}/r0"), Service { servers: vec![Default::default()], ..Default::default() });
	}
	p.rules[0].targets = vec![];
	p.rules[0].http = Some(http);
	p
}

fn local(port: u16) -> Endpoint {
	Endpoint { pod: "rproxy-0".into(), uid: "u1".into(), ip: "127.0.0.1".into(), api_port: port, certsync_port: port, ..Default::default() }
}

/// Past `MAX_EXCLUDED` refused routes, only that rule is left out (not the set), and the answer is
/// remembered: nothing is PUT again until something changes.
#[tokio::test]
async fn many_refused_routes_leave_out_that_rule_only() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = local(port);
	let names: Vec<String> = (0..MAX_EXCLUDED + 6).map(|i| format!("t/r{i}")).collect();
	let mut owners: Vec<(&str, bool)> = names.iter().map(|n| (n.as_str(), true)).collect();
	owners.push(("ok/one", false));
	let mut p = http_plan(443, &owners);
	p.rules.push(Rule {
		protocol: Protocol::Tcp,
		listen_addr: "0.0.0.0".into(),
		listen_port: 5432,
		targets: vec![Target { addr: "10.0.0.1".into(), port: 5432, weight: None }],
		..Default::default()
	});
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	let PodSync::Synced(views, excluded) = &r else { panic!("{r:?}") };
	assert!(excluded.is_empty(), "{excluded:?}");
	let bad = views.iter().find(|v| v.listen_port == 443).unwrap();
	assert_eq!(bad.state, "failed");
	assert_eq!(fake.lock().unwrap().sets["k8s/default/gw"].2.len(), 1, "the other rule is applied");
	let puts = fake.lock().unwrap().puts.len();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Synced(..)));
	assert_eq!(fake.lock().unwrap().puts.len(), puts, "remembered");
}

/// A set rproxy refuses as a whole is remembered too (not PUT again every pass).
#[tokio::test]
async fn a_refused_set_is_remembered() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = local(port);
	let mut p = plan(80, &[]);
	p.rules[0].targets = vec![Target { addr: "whole".into(), port: 80, weight: None }];
	p.raw = vec![json!({"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": 25, "remote_addr": "whole", "remote_port": 25})];
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Rejected(m) if m.contains("unknown field")), "{r:?}");
	let puts = fake.lock().unwrap().puts.len();
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Rejected(_)) && !again);
	assert_eq!(fake.lock().unwrap().puts.len(), puts);
}

/// A dry run without an answer (5xx, timeout) decides nothing: no rule is left out, nothing is
/// remembered, the pass is tried again soon.
#[tokio::test]
async fn a_failed_dry_run_drops_nothing() {
	let fake = Arc::new(Mutex::new(Fake { unnamed: true, dry_run_fails: true, ..Default::default() }));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep = local(port);
	let p = http_plan(443, &[("a/one", false), ("b/bad", true)]);
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::NotReady(m) if m.contains("dry run")) && again, "{r:?}");
	assert!(applied.values().all(|a| a.hash.is_empty() && a.excluded.is_empty() && a.backoff.as_ref().is_some_and(|(_, n, _)| *n == 1)));
	assert!(fake.lock().unwrap().sets.is_empty(), "no set without the port's rule");
	// the next pass waits before asking again (longer each time)
	let puts = fake.lock().unwrap().puts.len();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::NotReady(m) if m.contains("waiting")), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.len(), puts);
	assert!(backoff(1) < backoff(2) && backoff(10) == std::time::Duration::from_secs(60));
	// the plan changed (the user fixed a route, or endpoints moved): not at once, but after
	// `PLAN_CHANGED_RETRY` rather than the whole wait; the failures keep counting (#98)
	let p2 = http_plan(443, &[("a/one", false), ("b/bad", true), ("c/new", false)]);
	let (r, _) = sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::NotReady(m) if m.contains("waiting")), "{r:?}");
	for a in applied.values_mut() {
		// the last failure was `PLAN_CHANGED_RETRY` ago; its own wait (a minute) is not over
		a.backoff = a.backoff.take().map(|(_, _, h)| (std::time::Instant::now() + backoff(10) - PLAN_CHANGED_RETRY, 10, h));
	}
	let (r, _) = sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::NotReady(m) if !m.contains("waiting")), "{r:?}");
	assert!(applied.values().all(|a| a.backoff.as_ref().is_some_and(|(_, n, h)| *n == 11 && !h.is_empty())));
	let puts = fake.lock().unwrap().puts.len();
	let (r, _) = sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::NotReady(m) if m.contains("waiting")), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.len(), puts);
	// rproxy answers again: only the refused route is left out
	fake.lock().unwrap().dry_run_fails = false;
	for a in applied.values_mut() {
		a.backoff = a.backoff.take().map(|(_, n, h)| (std::time::Instant::now(), n, h));
	}
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v, ex) if v.len() == 1 && ex.len() == 1 && ex[0].owner == "b/bad"), "{r:?}");
}

/// fleet: two Gateways on the same port on two VIPs both keep it; a third on one of those VIPs (or a
/// wildcard next to them) loses to the older one and says so on its listener.
#[test]
fn fleet_gateways_share_a_port_on_different_addresses() {
	use crate::render::ListenerPlan;
	use crate::render::status::{Cond, get};
	let gw = |name: &str, created: &str| -> crate::k8s::gateway::Gateway {
		serde_json::from_value(json!({
			"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
			"metadata": {"name": name, "namespace": "default", "creationTimestamp": created},
			"spec": {"gatewayClassName": "rproxy", "listeners": []}
		}))
		.unwrap()
	};
	let on = |name: &str, addr: &str| {
		let mut p = plan(443, &[]);
		p.name = name.into();
		p.ruleset = format!("k8s/default/{name}");
		p.rules[0].listen_addr = addr.into();
		p.rules[0].listen_freebind = addr != "0.0.0.0";
		let key = p.rules[0].key();
		p.listeners = vec![ListenerPlan {
			name: "https".into(),
			supported_kinds: vec![],
			attached: 1,
			conds: vec![Cond::ok("Accepted", "Accepted")],
			rule_key: Some(key),
			servable: true,
		}];
		p
	};
	let (a, b, c) = (gw("a", "2026-01-01T00:00:00Z"), gw("b", "2026-01-02T00:00:00Z"), gw("c", "2026-01-03T00:00:00Z"));
	// listed newest first: the order does not decide, the age does
	let mut rendered = vec![(&c, on("c", "192.0.2.10")), (&b, on("b", "192.0.2.11")), (&a, on("a", "192.0.2.10"))];
	fleet_conflicts(&mut rendered);
	let (pc, pb, pa) = (&rendered[0].1, &rendered[1].1, &rendered[2].1);
	assert_eq!((pa.rules.len(), pb.rules.len(), pc.rules.len()), (1, 1, 0), "a and b keep 443 on their VIPs; c loses");
	assert_eq!(pb.rules[0].key(), "tcp/192.0.2.11:443");
	let acc = get(&pc.listeners[0].conds, "Accepted").unwrap();
	assert_eq!((acc.status, acc.reason.as_str()), (false, "PortUnavailable"));
	assert!(acc.message.contains("Gateway default/a") && acc.message.contains("tcp/192.0.2.10:443"), "{}", acc.message);
	assert!(pc.listeners[0].rule_key.is_none() && !pc.listeners[0].servable);
	assert!(get(&pa.listeners[0].conds, "Accepted").unwrap().status);
	// a wildcard takes the port on every address
	let mut rendered = vec![(&b, on("b", "192.0.2.11")), (&a, on("a", "0.0.0.0"))];
	fleet_conflicts(&mut rendered);
	assert_eq!((rendered[1].1.rules.len(), rendered[0].1.rules.len()), (1, 0), "the older wildcard keeps the port");
}

/// fleet.listen=addresses: a Gateway's spec.addresses inside --address-cidr (IPv4 first), else the wildcard.
#[test]
fn fleet_listen_addresses() {
	let gw = |addrs: &[&str]| -> crate::k8s::gateway::Gateway {
		let a: Vec<Value> = addrs.iter().map(|v| json!({"type": "IPAddress", "value": v})).collect();
		serde_json::from_value(json!({
			"apiVersion": "gateway.networking.k8s.io/v1", "kind": "Gateway",
			"metadata": {"name": "g", "namespace": "default"},
			"spec": {"gatewayClassName": "rproxy", "addresses": a, "listeners": []}
		}))
		.unwrap()
	};
	let cidrs: Vec<render::Cidr> = vec!["192.0.2.0/24".parse().unwrap(), "2001:db8::/64".parse().unwrap()];
	assert_eq!(listen_addresses(&gw(&["2001:db8::10", "192.0.2.10"]), &cidrs), Some(vec!["192.0.2.10".into(), "2001:db8::10".into()]));
	assert_eq!(listen_addresses(&gw(&[]), &cidrs), None, "no addresses: the wildcard");
	assert_eq!(listen_addresses(&gw(&["192.0.2.10", "198.51.100.1"]), &cidrs), None, "one outside the ranges");
	assert_eq!(listen_addresses(&gw(&["192.0.2.10"]), &[]), None, "static addresses off");
}

#[test]
fn supported_features_match_the_conformance_script() {
	let script = include_str!("../../scripts/conformance.sh");
	let line = script.lines().find(|l| l.starts_with("FEATURES=${FEATURES:-")).expect("FEATURES in scripts/conformance.sh");
	let list = line.trim_start_matches("FEATURES=${FEATURES:-").trim_end_matches('}');
	let mut script_features: Vec<&str> = list.split(',').collect();
	let mut ours: Vec<&str> = super::SUPPORTED_FEATURES.to_vec();
	script_features.sort();
	ours.sort();
	assert_eq!(script_features, ours, "SUPPORTED_FEATURES and scripts/conformance.sh FEATURES differ");
}
