//! The controller: watches, renders every Gateway of its GatewayClasses, applies
//! the rule sets to rproxy (`PUT /rulesets/{name}` with `generation` and
//! `If-Match`) and writes status back.
//!
//! One loop renders everything on each change (debounced) and every `resync`.
//! A set is PUT again when what the controller renders changed, or when the pod's
//! set is not the one last applied (an rproxy restart: rule sets live in
//! rproxy's memory; the controller waits for `GET /readyz` first).

pub mod bootstrap;
pub mod cache;
pub mod leader;
pub mod provision;
pub mod status;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use kube::api::{Api, DynamicObject, Patch, PatchParams};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::render::{self, GatewayPlan, RouteKind};
use crate::rproxy::client::{ApiError, Client};
use crate::rproxy::model::{Capabilities, RuleView};
use provision::{Endpoint, Mode};
use status::PodSync;

/// The Gateway API features this controller supports (GatewayClass `status.supportedFeatures`).
pub const SUPPORTED_FEATURES: &[&str] = &[
	"Gateway",
	"GatewayPort8080",
	"HTTPRoute",
	"ReferenceGrant",
	"HTTPRouteMethodMatching",
	"HTTPRouteQueryParamMatching",
	"HTTPRouteResponseHeaderModification",
	"HTTPRoutePortRedirect",
	"HTTPRouteSchemeRedirect",
	"HTTPRoutePathRedirect",
	"HTTPRoutePathRewrite",
	"GatewayHTTPListenerIsolation",
	"TLSRoute",
	"TCPRoute",
	"UDPRoute",
	"TLSRouteModeTerminate",
	"TLSRouteModeMixed",
	"HTTPRouteParentRefPort",
	"HTTPRouteDestinationPortMatching",
	"HTTPRouteNamedRouteRule",
	"HTTPRouteBackendProtocolWebSocket",
	"HTTPRouteBackendTimeout",
];

#[derive(Clone, Debug)]
pub struct Config {
	pub controller_name: String,
	/// The controller's namespace (its Secrets, managed rproxy pods).
	pub namespace: String,
	pub mode: Mode,
	pub listen_addrs: Vec<String>,
	pub resync: Duration,
	pub health: SocketAddr,
	/// Ingress / Traefik migration (off when `None`).
	pub migration: Option<render::migrate::Settings>,
	/// Leader election (`None`: this replica always leads, for a single replica).
	pub leader: Option<leader::Settings>,
}

/// Migration notes already logged (by digest), so each set is logged once.
static NOTES_SEEN: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

/// What was last applied to a pod: (pod uid, set) → (hash of what was sent, etag rproxy answered).
pub type Applied = HashMap<(String, String), (String, String, Vec<RuleView>)>;

/// What the leader remembers between passes (forgotten when it stops leading).
#[derive(Default)]
pub struct State {
	pub applied: Applied,
	/// rproxy's capabilities by pod uid.
	pub caps: HashMap<String, Capabilities>,
	/// Certificate Secret → files no rule needs any more, since when (`provision::with_linger`).
	pub absent: HashMap<String, BTreeMap<String, std::time::Instant>>,
}

pub async fn run(cfg: Config) -> anyhow::Result<()> {
	let client = kube::Client::try_default().await.context("connecting to Kubernetes")?;
	let boot = bootstrap::ensure(&client, &cfg.namespace).await.context("the controller's Secrets")?;
	let rp = Client::new(Some(&boot.ca_pem), &boot.token)?;
	let cache = cache::Cache::start(&client, &cfg.namespace, cfg.migration.is_some()).await?;
	tokio::spawn(health(cfg.health));
	let (leading_tx, mut leading) = tokio::sync::watch::channel(cfg.leader.is_none());
	let lease_api: Api<k8s_openapi::api::coordination::v1::Lease> = Api::namespaced(client.clone(), &cfg.namespace);
	if let Some(s) = cfg.leader.clone() {
		info!(lease = s.lease, identity = s.identity, "leader election");
		tokio::spawn(leader::run(lease_api.clone(), s, leading_tx));
	}
	cache.ready().await;
	info!(controller = %cfg.controller_name, "started");
	let mut state = State::default();
	let shutdown = shutdown_signal();
	tokio::pin!(shutdown);
	loop {
		if !*leading.borrow_and_update() {
			// a follower keeps its watches warm and waits; what it knew about pods is stale
			state = State::default();
			tokio::select! {
				_ = leading.changed() => continue,
				_ = &mut shutdown => return Ok(()),
			}
		}
		// a pass stops as soon as leadership is lost (another replica may be leading already)
		let pass = tokio::select! {
			r = reconcile_all(&client, &cache, &rp, &cfg, &mut state) => Some(r),
			_ = lost(&mut leading) => None,
			_ = &mut shutdown => {
				if let Some(s) = &cfg.leader { leader::release(&lease_api, s).await; }
				return Ok(());
			}
		};
		let soon = match pass {
			None => continue,
			Some(Ok(soon)) => soon,
			Some(Err(e)) => {
				warn!(error = format!("{e:#}"), "reconcile failed");
				true
			}
		};
		let wait = if soon { Duration::from_secs(2) } else { cfg.resync };
		tokio::select! {
			_ = cache.changed.notified() => {}
			_ = tokio::time::sleep(wait) => {}
			_ = lost(&mut leading) => continue,
			_ = &mut shutdown => {
				if let Some(s) = &cfg.leader { leader::release(&lease_api, s).await; }
				return Ok(());
			}
		}
		// let a burst of changes settle
		tokio::time::sleep(Duration::from_millis(250)).await;
	}
}

/// Resolves when this replica stops leading.
async fn lost(leading: &mut tokio::sync::watch::Receiver<bool>) {
	while *leading.borrow_and_update() {
		if leading.changed().await.is_err() {
			// the elector is gone (a single replica without election never changes)
			std::future::pending::<()>().await;
		}
	}
}

/// SIGTERM or Ctrl-C.
async fn shutdown_signal() {
	#[cfg(unix)]
	{
		let mut term = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
			Ok(t) => t,
			Err(_) => return std::future::pending().await,
		};
		tokio::select! {
			_ = term.recv() => {}
			_ = tokio::signal::ctrl_c() => {}
		}
	}
	#[cfg(not(unix))]
	{
		let _ = tokio::signal::ctrl_c().await;
	}
	info!("shutting down");
}

async fn health(addr: SocketAddr) {
	let Ok(listener) = tokio::net::TcpListener::bind(addr).await else {
		warn!(%addr, "cannot listen for health checks");
		return;
	};
	loop {
		let Ok((tcp, _)) = listener.accept().await else { continue };
		tokio::spawn(async move {
			let svc = hyper::service::service_fn(|_req| async {
				Ok::<_, std::convert::Infallible>(hyper::Response::new(http_body_util::Full::new(hyper::body::Bytes::from_static(b"ok"))))
			});
			let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tcp), svc).await;
		});
	}
}

fn dyn_api(client: &kube::Client, cache: &cache::Cache, kind: &str, ns: Option<&str>) -> Option<Api<DynamicObject>> {
	let ar = cache.resource(kind)?;
	Some(match ns {
		Some(ns) => Api::namespaced_with(client.clone(), ns, ar),
		None => Api::all_with(client.clone(), ar),
	})
}

async fn patch_status(api: &Api<DynamicObject>, name: &str, status: Value) -> anyhow::Result<()> {
	api.patch_status(name, &PatchParams::default(), &Patch::Merge(json!({ "status": status }))).await?;
	Ok(())
}

/// One pass over everything; `Ok(true)` asks for another pass soon.
async fn reconcile_all(client: &kube::Client, cache: &cache::Cache, rp: &Client, cfg: &Config, state: &mut State) -> anyhow::Result<bool> {
	let world = cache.snapshot();
	let now = render::status::now();
	let mut soon = false;

	// GatewayClasses
	let classes: BTreeSet<String> =
		world.classes.iter().filter(|c| c.spec.controller_name == cfg.controller_name).filter_map(|c| c.metadata.name.clone()).collect();
	if let Some(api) = dyn_api(client, cache, "GatewayClass", None) {
		for c in world.classes.iter().filter(|c| c.spec.controller_name == cfg.controller_name) {
			let name = c.metadata.name.clone().unwrap_or_default();
			let previous = api.get_status(&name).await.ok().and_then(|o| o.data.get("status").cloned());
			let st = status::class_status(c.metadata.generation.unwrap_or(0), SUPPORTED_FEATURES, previous.as_ref(), &now);
			if previous.as_ref() != Some(&st) {
				if let Err(e) = patch_status(&api, &name, st).await {
					warn!(class = name, error = %e, "cannot write GatewayClass status");
				}
			}
		}
	}

	let pods_now = cache.pods.state();
	let mut notes_seen = NOTES_SEEN.lock().unwrap().clone();
	let mut plans: Vec<(GatewayPlan, Vec<PodSync>)> = vec![];
	let mut keep_ids = vec![];
	let opts = render::Options {
		listen_addrs: cfg.listen_addrs.clone(),
		cert_dir: provision::CERT_DIR.into(),
		labels: true,
		migration: cfg.migration.clone(),
	};
	let rendered: Vec<(&crate::k8s::gateway::Gateway, GatewayPlan)> = world
		.gateways
		.iter()
		.filter(|g| classes.contains(&g.spec.gateway_class_name))
		.map(|gw| (gw, render::render_gateway(&world, gw, &opts)))
		.collect();
	// fleet: one certificate Secret with every Gateway's files, mounted into every pod
	let fleet_pods = match &cfg.mode {
		Mode::Fleet(f) => {
			let eps = provision::pods(&pods_now, &provision::parse_selector(&f.selector));
			let mut files = BTreeMap::new();
			for (_, plan) in &rendered {
				files.extend(plan.files.iter().map(|(k, v)| (k.clone(), v.clone())));
			}
			let absent = state.absent.entry(provision::FLEET_CERTS_SECRET.into()).or_default();
			match provision::apply_certs(client, &cfg.namespace, provision::FLEET_CERTS_SECRET, "fleet", None, &files, absent).await {
				Ok(hash) => provision::touch_pods(client, &cfg.namespace, &eps, &hash).await,
				Err(e) => {
					warn!(error = format!("{e:#}"), "cannot write the fleet's certificate Secret");
					soon = true;
				}
			}
			eps
		}
		Mode::Managed(_) => vec![],
	};
	for (gw, plan) in rendered {
		let id = provision::gateway_id(&plan.namespace, &plan.name);
		keep_ids.push(id.clone());
		let infra = gw.spec.infrastructure.clone().unwrap_or_default();
		let (addresses, endpoints) = match &cfg.mode {
			Mode::Managed(m) => {
				let absent = state.absent.entry(provision::certs_secret_name(&id)).or_default();
				let selector: BTreeMap<String, String> = [(provision::LABEL_GATEWAY.to_string(), id.clone())].into();
				let eps = provision::pods(&pods_now, &selector);
				let svc =
					match provision::apply_managed(client, &cfg.namespace, &plan, m, (&infra.labels, &infra.annotations), absent).await {
						Ok((s, hash)) => {
							provision::touch_pods(client, &cfg.namespace, &eps, &hash).await;
							s
						}
						Err(e) => {
							warn!(gateway = %plan.ruleset, error = format!("{e:#}"), "cannot deploy rproxy");
							soon = true;
							None
						}
					};
				(svc.as_ref().map(provision::service_addresses).unwrap_or_default(), eps)
			}
			Mode::Fleet(f) => {
				let eps = fleet_pods.clone();
				let addrs = if f.addresses.is_empty() {
					let mut a: Vec<String> = eps.iter().filter_map(|e| e.host_ip.clone()).collect();
					a.sort();
					a.dedup();
					a
				} else {
					f.addresses.clone()
				};
				(
					addrs
						.into_iter()
						.map(|a| (if a.parse::<std::net::IpAddr>().is_ok() { "IPAddress" } else { "Hostname" }.to_string(), a))
						.collect(),
					eps,
				)
			}
		};
		if !plan.notes.is_empty() {
			let digest = crate::pem::short_hash(plan.notes.join("\n").as_bytes());
			if notes_seen.insert(digest) {
				for n in &plan.notes {
					warn!(gateway = %plan.ruleset, note = %n, "migration: not converted");
				}
			}
		}
		let mut results = vec![];
		for ep in &endpoints {
			let (r, again) = sync_pod(rp, ep, &plan, &mut state.applied, &mut state.caps).await;
			soon |= again;
			results.push(r);
		}
		if let Some(api) = dyn_api(client, cache, "Gateway", Some(&plan.namespace)) {
			let st = status::gateway_status(&plan, &addresses, &results, gw.status.as_ref(), &now);
			if gw.status.as_ref() != Some(&st) {
				if let Err(e) = patch_status(&api, &plan.name, st).await {
					warn!(gateway = %plan.ruleset, error = %e, "cannot write Gateway status");
				}
			}
		}
		plans.push((plan, results));
	}

	*NOTES_SEEN.lock().unwrap() = notes_seen;

	// Gateways that are gone
	state
		.absent
		.retain(|name, _| name == provision::FLEET_CERTS_SECRET || keep_ids.iter().any(|id| provision::certs_secret_name(id) == *name));
	if let Err(e) = provision::collect_garbage(client, &cfg.namespace, &keep_ids).await {
		warn!(error = %e, "cannot remove rproxy of deleted Gateways");
	}
	if let Mode::Fleet(f) = &cfg.mode {
		let keep: BTreeSet<&str> = plans.iter().map(|(p, _)| p.ruleset.as_str()).collect();
		for ep in provision::pods(&pods_now, &provision::parse_selector(&f.selector)) {
			let Ok(addr) = api_addr(&ep) else { continue };
			if let Ok(sets) = rp.list_rulesets(addr).await {
				for s in sets.iter().filter(|s| s.name.starts_with("k8s/") && !keep.contains(s.name.as_str())) {
					info!(pod = ep.pod, set = s.name, "deleting the rule set of a Gateway that is gone");
					if let Err(e) = rp.delete_ruleset(addr, &s.name).await {
						warn!(pod = ep.pod, set = s.name, error = %e, "cannot delete the rule set");
					}
				}
			}
		}
	}

	write_route_status(client, cache, &world, &plans, cfg, &now).await;
	write_crd_status(client, cache, &world, &plans, cfg, &now).await;
	Ok(soon)
}

/// RproxyPolicy `status.ancestors` and RproxyRule `status`.
async fn write_crd_status(
	client: &kube::Client,
	cache: &cache::Cache,
	world: &render::world::World,
	plans: &[(GatewayPlan, Vec<PodSync>)],
	cfg: &Config,
	now: &str,
) {
	let mut ancestors: BTreeMap<(String, String), Vec<Value>> = BTreeMap::new();
	for p in &world.policies {
		let key = (p.metadata.namespace.clone().unwrap_or_default(), p.metadata.name.clone().unwrap_or_default());
		let prev = p.status.as_ref().and_then(|s| serde_json::to_value(s).ok());
		for (plan, _) in plans {
			for ps in plan.policies.iter().filter(|ps| ps.namespace == key.0 && ps.name == key.1) {
				ancestors.entry(key.clone()).or_default().push(status::policy_entry(ps, &cfg.controller_name, prev.as_ref(), now));
			}
		}
		let mine = ancestors.remove(&key).unwrap_or_default();
		let had = prev
			.as_ref()
			.and_then(|s| s["ancestors"].as_array())
			.is_some_and(|a| a.iter().any(|e| e["controllerName"] == cfg.controller_name.as_str()));
		if mine.is_empty() && !had {
			continue;
		}
		let new = json!({ "ancestors": status::policy_ancestors(prev.as_ref(), mine, &cfg.controller_name) });
		if prev.as_ref().map(|p| &p["ancestors"]) == Some(&new["ancestors"]) {
			continue;
		}
		if let Some(api) = dyn_api(client, cache, "RproxyPolicy", Some(&key.0)) {
			if let Err(e) = patch_status(&api, &key.1, new).await {
				warn!(policy = format!("{}/{}", key.0, key.1), error = %e, "cannot write RproxyPolicy status");
			}
		}
	}
	for r in &world.raw_rules {
		let (ns, name) = (r.metadata.namespace.clone().unwrap_or_default(), r.metadata.name.clone().unwrap_or_default());
		let prev = r.status.as_ref().and_then(|s| serde_json::to_value(s).ok());
		let found =
			plans.iter().find_map(|(plan, pods)| plan.raw_status.iter().find(|s| s.namespace == ns && s.name == name).map(|s| (s, pods)));
		let new = match found {
			Some((st, pods)) => status::raw_status(st, pods, prev.as_ref(), now),
			None => {
				let st = render::policy::RawStatus {
					namespace: ns.clone(),
					name: name.clone(),
					generation: r.metadata.generation.unwrap_or(0),
					rule_key: None,
					conds: vec![render::status::Cond::new(
						"Accepted",
						false,
						"NoMatchingParent",
						"the Gateway does not exist or is not handled by this controller",
					)],
				};
				status::raw_status(&st, &[], prev.as_ref(), now)
			}
		};
		if prev.as_ref() == Some(&new) {
			continue;
		}
		if let Some(api) = dyn_api(client, cache, "RproxyRule", Some(&ns)) {
			if let Err(e) = patch_status(&api, &name, new).await {
				warn!(rule = format!("{ns}/{name}"), error = %e, "cannot write RproxyRule status");
			}
		}
	}
}

fn api_addr(ep: &Endpoint) -> anyhow::Result<SocketAddr> {
	let ip: std::net::IpAddr = ep.ip.parse()?;
	Ok(SocketAddr::new(ip, ep.api_port))
}

/// Brings one pod's copy of the plan's set up to date; returns its state and
/// whether to come back soon.
pub async fn sync_pod(
	rp: &Client,
	ep: &Endpoint,
	plan: &GatewayPlan,
	applied: &mut Applied,
	caps: &mut HashMap<String, Capabilities>,
) -> (PodSync, bool) {
	let addr = match api_addr(ep) {
		Ok(a) => a,
		Err(e) => return (PodSync::NotReady(format!("pod {}: {e}", ep.pod)), true),
	};
	if !caps.contains_key(&ep.uid) {
		match rp.capabilities(addr).await {
			Ok(c) => {
				caps.insert(ep.uid.clone(), c);
			}
			Err(e) => return (PodSync::NotReady(format!("pod {}: {e:#}", ep.pod)), true),
		}
	}
	let cap = caps[&ep.uid].clone();
	if !cap.feature("rulesets") {
		return (
			PodSync::Rejected(format!(
				"pod {}: rproxy {} has no rule sets (features.rulesets; rproxy v0.4.0 or later)",
				ep.pod, cap.version
			)),
			false,
		);
	}
	match rp.ready(addr).await {
		Ok(true) => {}
		Ok(false) if !cap.feature("readyz") => {}
		Ok(false) => return (PodSync::NotReady(format!("pod {}: rproxy is not ready", ep.pod)), true),
		Err(e) => return (PodSync::NotReady(format!("pod {}: {e:#}", ep.pod)), true),
	}
	if !plan.files.is_empty() {
		match crate::certsync::files(&ep.ip, ep.certsync_port).await {
			Ok(present) => {
				let missing: Vec<&String> = plan.files.keys().filter(|f| !present.contains(*f)).collect();
				if !missing.is_empty() {
					return (PodSync::Pending(format!("pod {}: certificate files not written yet ({})", ep.pod, missing.len())), true);
				}
			}
			Err(e) => return (PodSync::Pending(format!("pod {}: {e:#}", ep.pod)), true),
		}
	}
	let mut rules = plan.rules_json();
	if !cap.feature("labels") {
		for r in &mut rules {
			if let Some(o) = r.as_object_mut() {
				o.remove("labels");
			}
		}
	}
	let hash = crate::pem::sha256_hex(&serde_json::to_vec(&(plan.generation, &rules)).unwrap_or_default());
	let current = match rp.get_ruleset(addr, &plan.ruleset).await {
		Ok(c) => c,
		Err(e) => return (PodSync::NotReady(format!("pod {}: {e:#}", ep.pod)), true),
	};
	let key = (ep.uid.clone(), plan.ruleset.clone());
	if let (Some(cur), Some((h, etag, rejected))) = (&current, applied.get(&key)) {
		if *h == hash && cur.etag == *etag {
			let mut views = cur.rules.clone();
			views.extend(rejected.iter().cloned());
			return (PodSync::Synced(views), false);
		}
	}
	let mut if_match = current.as_ref().map(|c| c.etag.clone());
	let mut generation = plan.generation;
	if let Some(cur) = &current {
		// a Gateway created again starts at generation 1; rproxy refuses a lower one
		if cur.generation > generation {
			generation = cur.generation;
		}
	}
	// rules rproxy refuses (`rules[i]: ...`) are left out and the rest is PUT again,
	// so one bad rule (an RproxyRule, a migrated route) does not stop the whole set
	let mut rejected: Vec<RuleView> = vec![];
	let answer = loop {
		debug!(pod = ep.pod, set = plan.ruleset, generation, rules = rules.len(), "PUT rule set");
		match rp.put_ruleset(addr, &plan.ruleset, generation, &rules, if_match.as_deref()).await {
			Ok(answer) => break answer,
			Err(e) => match e.downcast_ref::<ApiError>() {
				Some(a) if a.code == "precondition_failed" || a.code == "stale_generation" => {
					return (PodSync::Pending(format!("pod {}: {a}", ep.pod)), true);
				}
				Some(a) if a.status.is_client_error() => {
					match refused_index(&a.message).filter(|i| *i < rules.len() && rejected.len() < 32) {
						Some(i) => {
							let rule = rules.remove(i);
							warn!(pod = ep.pod, set = plan.ruleset, error = %a, "rproxy refused a rule; applying the others");
							rejected.push(refused_view(&rule, &a.message));
							if_match = None;
							if let Ok(Some(cur)) = rp.get_ruleset(addr, &plan.ruleset).await {
								if_match = Some(cur.etag);
							}
						}
						None => {
							warn!(pod = ep.pod, set = plan.ruleset, error = %a, "rproxy refused the rule set");
							return (PodSync::Rejected(a.to_string()), false);
						}
					}
				}
				_ => return (PodSync::NotReady(format!("pod {}: {e:#}", ep.pod)), true),
			},
		}
	};
	for r in answer.results.iter().filter(|r| r.error.is_some()) {
		info!(pod = ep.pod, set = plan.ruleset, rule = r.rule, error = r.error.as_deref().unwrap_or(""), "rule not running");
	}
	applied.insert(key.clone(), (hash, answer.etag.clone(), rejected.clone()));
	match rp.get_ruleset(addr, &plan.ruleset).await {
		Ok(Some(set)) => {
			// a rule failing on a file the kubelet has just written is tried again soon
			let again = set.rules.iter().any(|r| r.state == "failed");
			if again {
				applied.remove(&key);
			}
			let mut views = set.rules;
			views.extend(rejected);
			(PodSync::Synced(views), again)
		}
		Ok(None) => (PodSync::Pending(format!("pod {}: the rule set disappeared", ep.pod)), true),
		Err(e) => (PodSync::NotReady(format!("pod {}: {e:#}", ep.pod)), true),
	}
}

/// The index in `rules[3]: ...` (rproxy's errors for one rule of a set).
fn refused_index(message: &str) -> Option<usize> {
	let rest = message.strip_prefix("rules[")?;
	rest[..rest.find(']')?].parse().ok()
}

/// A rule view for a rule rproxy refused: `failed`, `Accepted: False`.
fn refused_view(rule: &Value, message: &str) -> RuleView {
	let protocol = if rule["protocol"].as_str().is_some_and(|p| p.eq_ignore_ascii_case("udp")) {
		crate::rproxy::model::Protocol::Udp
	} else {
		crate::rproxy::model::Protocol::Tcp
	};
	let cond = |kind: &str| crate::rproxy::model::RuleCondition {
		kind: kind.into(),
		status: "False".into(),
		reason: "Invalid".into(),
		message: message.into(),
	};
	RuleView {
		protocol,
		listen_addr: rule["listen_addr"].as_str().unwrap_or_default().into(),
		listen_port: rule["listen_port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(0),
		state: "failed".into(),
		error: Some(message.into()),
		conditions: vec![cond("Accepted"), cond("Programmed")],
	}
}

/// Writes `status.parents` of every route our Gateways touch (and removes our
/// entries from routes that no longer refer to them).
async fn write_route_status(
	client: &kube::Client,
	cache: &cache::Cache,
	world: &render::world::World,
	plans: &[(GatewayPlan, Vec<PodSync>)],
	cfg: &Config,
	now: &str,
) {
	let mut ours: BTreeMap<(RouteKind, String, String), Vec<Value>> = BTreeMap::new();
	let previous_of = |kind: RouteKind, ns: &str, name: &str| -> Option<Value> {
		match kind {
			RouteKind::Http => world
				.http_routes
				.iter()
				.find(|r| r.metadata.namespace.as_deref() == Some(ns) && r.metadata.name.as_deref() == Some(name))
				.and_then(|r| r.status.clone()),
			RouteKind::Tls => world
				.tls_routes
				.iter()
				.find(|r| r.metadata.namespace.as_deref() == Some(ns) && r.metadata.name.as_deref() == Some(name))
				.and_then(|r| r.status.clone()),
			RouteKind::Tcp => world
				.tcp_routes
				.iter()
				.find(|r| r.metadata.namespace.as_deref() == Some(ns) && r.metadata.name.as_deref() == Some(name))
				.and_then(|r| r.status.clone()),
			RouteKind::Udp => world
				.udp_routes
				.iter()
				.find(|r| r.metadata.namespace.as_deref() == Some(ns) && r.metadata.name.as_deref() == Some(name))
				.and_then(|r| r.status.clone()),
		}
	};
	for (plan, pods) in plans {
		for p in &plan.parents {
			let prev = previous_of(p.kind, &p.namespace, &p.name);
			let conds = status::parent_conds(p, pods);
			ours.entry((p.kind, p.namespace.clone(), p.name.clone())).or_default().push(status::parent_entry(
				p,
				&conds,
				&cfg.controller_name,
				prev.as_ref(),
				now,
			));
		}
	}
	// routes with our entries but nothing from us now
	let mut all: Vec<(RouteKind, String, String, Option<Value>)> = vec![];
	for r in &world.http_routes {
		all.push((
			RouteKind::Http,
			r.metadata.namespace.clone().unwrap_or_default(),
			r.metadata.name.clone().unwrap_or_default(),
			r.status.clone(),
		));
	}
	for (kind, list) in [(RouteKind::Tls, &world.tls_routes), (RouteKind::Tcp, &world.tcp_routes), (RouteKind::Udp, &world.udp_routes)] {
		for r in list {
			all.push((
				kind,
				r.metadata.namespace.clone().unwrap_or_default(),
				r.metadata.name.clone().unwrap_or_default(),
				r.status.clone(),
			));
		}
	}
	for (kind, ns, name, prev) in all {
		let mine = ours.remove(&(kind, ns.clone(), name.clone())).unwrap_or_default();
		let had_ours = prev
			.as_ref()
			.and_then(|s| s["parents"].as_array())
			.is_some_and(|a| a.iter().any(|e| e["controllerName"] == cfg.controller_name.as_str()));
		if mine.is_empty() && !had_ours {
			continue;
		}
		let parents = status::route_parents(prev.as_ref(), mine, &cfg.controller_name);
		let new = json!({ "parents": parents });
		if prev.as_ref().map(|p| &p["parents"]) == Some(&new["parents"]) {
			continue;
		}
		let Some(api) = dyn_api(client, cache, kind.kind(), Some(&ns)) else { continue };
		if let Err(e) = patch_status(&api, &name, new).await {
			warn!(kind = kind.kind(), route = format!("{ns}/{name}"), error = %e, "cannot write route status");
		}
	}
}
