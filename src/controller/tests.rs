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
	let if_match = req.headers().get("if-match").and_then(|v| v.to_str().ok()).map(str::to_string);
	let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_string);
	let body = req.into_body().collect().await.unwrap().to_bytes();
	let reply = |status: u16, v: Value| Response::builder().status(status).body(Full::new(Bytes::from(v.to_string()))).unwrap();
	if path != "/readyz" && path != "/files" && auth.as_deref() != Some("Bearer secret") {
		return reply(401, json!({"error": "unauthorized", "code": "unauthorized"}));
	}
	let mut f = fake.lock().unwrap();
	match (method.as_str(), path.as_str()) {
		("GET", "/readyz") => reply(200, json!({"ready": true})),
		("GET", "/files") => reply(200, json!(f.files)),
		("GET", "/capabilities") => {
			reply(200, json!({"version": "0.4.0", "features": {"rulesets": true, "labels": true, "conditions": true, "readyz": true}}))
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
	let ep =
		Endpoint { pod: "rproxy-0".into(), uid: "u1".into(), ip: "127.0.0.1".into(), host_ip: None, api_port: port, certsync_port: port };
	let mut applied = Applied::new();
	let mut caps = HashMap::new();

	let p = plan(80, &[]);
	let (r, again) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v) if v.len() == 1), "{r:?}");
	assert!(!again);
	assert_eq!(fake.lock().unwrap().puts, vec![("k8s/default/gw".to_string(), None, 3)]);

	// nothing changed: no PUT
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Synced(_)));
	assert_eq!(fake.lock().unwrap().puts.len(), 1);

	// a change: PUT with If-Match of the current etag
	let p2 = plan(81, &[]);
	let current = fake.lock().unwrap().sets["k8s/default/gw"].1.clone();
	sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert_eq!(fake.lock().unwrap().puts.last().unwrap().1.as_deref(), Some(current.as_str()));

	// rproxy restarted: the set is gone, PUT again (no If-Match)
	fake.lock().unwrap().sets.clear();
	let (r, _) = sync_pod(&rp, &ep, &p2, &mut applied, &mut caps).await;
	assert!(matches!(r, PodSync::Synced(_)));
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
	assert!(matches!(r, PodSync::Synced(_)), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.last().unwrap().2, 3);
}

#[tokio::test]
async fn waits_for_certificate_files_and_reports_failures() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep =
		Endpoint { pod: "rproxy-0".into(), uid: "u1".into(), ip: "127.0.0.1".into(), host_ip: None, api_port: port, certsync_port: port };
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
	let ep =
		Endpoint { pod: "rproxy-0".into(), uid: "u1".into(), ip: "127.0.0.1".into(), host_ip: None, api_port: port, certsync_port: port };
	let (r, _) = sync_pod(&rp, &ep, &plan(80, &[]), &mut Applied::new(), &mut HashMap::new()).await;
	assert!(matches!(&r, PodSync::NotReady(m) if m.contains("401")), "{r:?}");
}

#[tokio::test]
async fn a_refused_rule_does_not_stop_the_others() {
	let fake = Arc::new(Mutex::new(Fake::default()));
	let port = start(fake.clone()).await;
	let rp = Client::new(None, "secret").unwrap();
	let ep =
		Endpoint { pod: "rproxy-0".into(), uid: "u1".into(), ip: "127.0.0.1".into(), host_ip: None, api_port: port, certsync_port: port };
	let mut p = plan(80, &[]);
	p.raw = vec![json!({"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": 25, "remote_addr": "bad name", "remote_port": 25})];
	let mut applied = Applied::new();
	let mut caps = HashMap::new();
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	let PodSync::Synced(views) = r else { panic!("{r:?}") };
	assert_eq!(views.len(), 2);
	let bad = views.iter().find(|v| v.listen_port == 25).unwrap();
	assert_eq!(bad.condition("Accepted").reason, "Invalid");
	assert!(bad.condition("Programmed").message.contains("not a host name"));
	assert_eq!(fake.lock().unwrap().sets["k8s/default/gw"].2.len(), 1, "the good rule is applied");
	// the next pass: nothing to PUT, the refusal is still reported
	let (r, _) = sync_pod(&rp, &ep, &p, &mut applied, &mut caps).await;
	assert!(matches!(&r, PodSync::Synced(v) if v.len() == 2), "{r:?}");
	assert_eq!(fake.lock().unwrap().puts.len(), 2);
}
