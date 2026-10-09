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
pub mod fleet_vip;
pub mod leader;
pub mod provision;
pub mod status;
#[cfg(test)]
mod tests;
pub mod ui;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use futures::StreamExt;
use kube::api::{Api, DynamicObject, Patch, PatchParams};
use serde_json::{Value, json};
use tracing::{debug, info, warn};

use crate::render::{self, GatewayPlan, RouteKind};
use crate::rproxy::client::{ApiError, Client, server_name_for};
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
	"GatewayStaticAddresses",
	"GatewayAddressEmpty",
	"GatewayInfrastructure",
	"HTTPRoute303RedirectStatusCode",
	"HTTPRoute307RedirectStatusCode",
	"HTTPRoute308RedirectStatusCode",
	"HTTPRouteRequestTimeout",
	"HTTPRouteHostRewrite",
	"HTTPRouteBackendRequestHeaderModification",
	"HTTPRouteCORS",
	"HTTPRouteRetry",
	"HTTPRouteRetryBackendTimeout",
	"HTTPRouteRetryConnectionError",
	"HTTPRouteRequestMirror",
	"HTTPRouteRequestMultipleMirrors",
	"HTTPRouteRequestPercentageMirror",
	"HTTPRouteBackendProtocolH2C",
	"GatewayFrontendClientCertificateValidation",
	"GatewayFrontendClientCertificateValidationInsecureFallback",
	"ListenerSet",
	"GRPCRoute",
	"GRPCRouteNamedRouteRule",
	"BackendTLSPolicy",
	"BackendTLSPolicySANValidation",
	"GatewayBackendClientCertificate",
	"GatewayHTTPSListenerDetectMisdirectedRequests",
	"HTTPRouteExternalAuth",
	"HTTPRouteExternalAuthHTTP",
	"HTTPRouteExternalAuthGRPC",
	"HTTPRouteExternalAuthForwardBody",
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
	/// The ranges `spec.addresses` may take (empty: static addresses are off).
	pub address_cidrs: Vec<render::Cidr>,
	/// ExternalName Services may be backends.
	pub allow_external_name: bool,
	/// Annotation prefixes of `spec.infrastructure` allowed onto the Service besides the
	/// ones that are not address or load balancer settings.
	pub service_annotations: Vec<String>,
	/// certificateRefs may name Secrets of other namespaces (with a ReferenceGrant).
	pub cross_namespace_secrets: bool,
	/// Fleet mode: RproxyRules are read (off by default: fleet pods are shared by every Gateway).
	pub fleet_rproxy_rules: bool,
	/// The namespaces watched besides the controller's own (empty: all; then the controller needs a ClusterRole).
	pub watch_namespaces: Vec<String>,
	/// Leader election (`None`: this replica always leads, for a single replica).
	pub leader: Option<leader::Settings>,
	/// The UI's namespace and pods (`--ui-namespace`): it gets the discovery Secret (`ui`).
	pub ui: Option<provision::UiAccess>,
}

impl Config {
	/// The namespaces watched: `None` for all, else `watch_namespaces` and the controller's own.
	pub fn scope(&self) -> cache::Scope {
		if self.watch_namespaces.is_empty() {
			return None;
		}
		let mut nss = self.watch_namespaces.clone();
		nss.push(self.namespace.clone());
		nss.sort();
		nss.dedup();
		Some(nss)
	}
}

/// How often the API server is asked again for kinds that were not served.
const REDISCOVER: Duration = Duration::from_secs(30);

/// Migration notes already logged (by digest), so each set is logged once.
static NOTES_SEEN: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

/// How many Gateways are brought up to date at once.
const PARALLEL: usize = 16;
/// How many pods of one Gateway are brought up to date at once.
const POD_PARALLEL: usize = 4;

/// What was last applied to a pod: (pod uid, set) → (hash of what was sent, etag rproxy answered).
pub type Applied = HashMap<(String, String), (String, String, Vec<RuleView>)>;

/// What the leader remembers between passes (forgotten when it stops leading).
#[derive(Default)]
pub struct State {
	pub applied: Applied,
	/// rproxy's capabilities by pod uid.
	pub caps: HashMap<String, Capabilities>,
	/// Gateway id → since when its Service has had ready endpoints.
	pub ready_since: HashMap<String, std::time::Instant>,
	/// Certificate Secret → files no rule needs any more, since when (`provision::with_linger`).
	pub absent: HashMap<String, BTreeMap<String, std::time::Instant>>,
}

pub async fn run(mut cfg: Config) -> anyhow::Result<()> {
	let client = kube::Client::try_default().await.context("connecting to Kubernetes")?;
	if let Mode::Managed(m) = &mut cfg.mode {
		m.native_sleep = native_sleep(&client).await;
	}
	let boot = bootstrap::ensure(&client, &cfg.namespace).await.context("the controller's Secrets")?;
	let rp = Client::new(Some(&boot.ca_pem), &boot.token)?;
	let cache = std::sync::Arc::new(cache::Cache::start(&client, cfg.migration.is_some(), cfg.scope()).await?);
	tokio::spawn(health(cfg.health));
	{
		// CRDs installed later (Traefik, Gateway API kinds) are watched without a restart
		let (cache, client) = (cache.clone(), client.clone());
		tokio::spawn(async move {
			loop {
				tokio::time::sleep(REDISCOVER).await;
				if let Err(e) = cache.rediscover(&client, false).await {
					debug!(error = %e, "discovery failed");
				}
			}
		});
	}
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
			r = reconcile_all(&client, &cache, &rp, &cfg, &boot, &mut state) => Some(r),
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

/// Whether the cluster has the kubelet's `sleep` preStop action on by default (Kubernetes 1.30 and later).
async fn native_sleep(client: &kube::Client) -> bool {
	match client.apiserver_version().await {
		Ok(v) => {
			let num = |s: &str| s.chars().take_while(char::is_ascii_digit).collect::<String>().parse::<u32>().unwrap_or(0);
			let native = (num(&v.major), num(&v.minor)) >= (1, 30);
			info!(version = v.git_version, native, "rproxy preStop: the sleep action (else exec sleep)");
			native
		}
		Err(e) => {
			warn!(error = %e, "cannot read the Kubernetes version; rproxy's preStop runs `sleep` in the container");
			false
		}
	}
}

/// SIGTERM or Ctrl-C.
pub(crate) async fn shutdown_signal() {
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
		Some(ns) => Api::namespaced_with(client.clone(), ns, &ar),
		None => Api::all_with(client.clone(), &ar),
	})
}

async fn patch_status(api: &Api<DynamicObject>, name: &str, status: Value) -> anyhow::Result<()> {
	api.patch_status(name, &PatchParams::default(), &Patch::Merge(json!({ "status": status }))).await?;
	Ok(())
}

/// One pass over everything; `Ok(true)` asks for another pass soon.
async fn reconcile_all(
	client: &kube::Client,
	cache: &cache::Cache,
	rp: &Client,
	cfg: &Config,
	boot: &bootstrap::Bootstrap,
	state: &mut State,
) -> anyhow::Result<bool> {
	let mut world = cache.snapshot();
	world.allow_external_name = cfg.allow_external_name;
	let world = world;
	let now = render::status::now();
	let mut soon = false;

	let params_opts = render::params::ParamsOptions {
		controller_namespace: cfg.namespace.clone(),
		fleet: matches!(cfg.mode, Mode::Fleet(_)),
		default_replicas: match &cfg.mode {
			Mode::Managed(m) => m.replicas,
			Mode::Fleet(_) => 1,
		},
		service_annotations: cfg.service_annotations.clone(),
	};

	// GatewayClasses
	let classes: BTreeSet<String> =
		world.classes.iter().filter(|c| c.spec.controller_name == cfg.controller_name).filter_map(|c| c.metadata.name.clone()).collect();
	if let Some(api) = dyn_api(client, cache, "GatewayClass", None) {
		for c in world.classes.iter().filter(|c| c.spec.controller_name == cfg.controller_name) {
			let name = c.metadata.name.clone().unwrap_or_default();
			let previous = api.get_status(&name).await.ok().and_then(|o| o.data.get("status").cloned());
			let invalid = render::params::class_parameters(&world, c, &params_opts).err();
			let st =
				status::class_status(c.metadata.generation.unwrap_or(0), SUPPORTED_FEATURES, invalid.as_deref(), previous.as_ref(), &now);
			if previous.as_ref() != Some(&st) {
				if let Err(e) = patch_status(&api, &name, st).await {
					warn!(class = name, error = %e, "cannot write GatewayClass status");
				}
			}
		}
	}

	let pods_now = cache.pods.state();
	if cfg.ui.is_some() {
		ui::forget(&pods_now.iter().filter_map(|p| p.metadata.uid.clone()).collect());
	}
	// rproxy restarted since its readiness gate was set: out of the endpoints until its sets are back
	for pod in &pods_now {
		if provision::gate_change(pod, None) == Some(false) {
			provision::set_gate(client, pod, false).await;
		}
	}
	let mut migration_addresses: Option<Vec<(String, String)>> = None;
	let mut notes_seen = NOTES_SEEN.lock().unwrap().clone();
	let mut plans: Vec<(GatewayPlan, Vec<PodSync>)> = vec![];
	let mut keep_ids = vec![];
	let mut keep_pdb = vec![];
	let mut pod_done: Vec<(String, bool)> = vec![];
	let opts = render::Options {
		listen_addrs: cfg.listen_addrs.clone(),
		cert_dir: provision::CERT_DIR.into(),
		labels: true,
		migration: cfg.migration.clone(),
		features: render::Features::default(),
		address_cidrs: cfg.address_cidrs.clone(),
		raw_rules: !matches!(cfg.mode, Mode::Fleet(_)) || cfg.fleet_rproxy_rules,
		cross_namespace_secrets: cfg.cross_namespace_secrets,
		params: params_opts.clone(),
	};
	let mut rendered: Vec<(&crate::k8s::gateway::Gateway, GatewayPlan)> = vec![];
	for gw in world.gateways.iter().filter(|g| classes.contains(&g.spec.gateway_class_name)) {
		// what the Gateway's rproxy pods take (all of rproxy v0.4.0 before they are asked)
		let (ns, name) = (gw.metadata.namespace.clone().unwrap_or_default(), gw.metadata.name.clone().unwrap_or_default());
		let eps = match &cfg.mode {
			Mode::Managed(_) => {
				let id = provision::gateway_id(&ns, &name, gw.metadata.uid.as_deref().unwrap_or_default());
				provision::pods(&pods_now, &ns, &[(provision::LABEL_GATEWAY.to_string(), id)].into())
			}
			Mode::Fleet(f) => provision::pods(&pods_now, &cfg.namespace, &provision::parse_selector(&f.selector)),
		};
		let features = pod_features(rp, &eps, &mut state.caps).await;
		let opts = render::Options { features, ..opts.clone() };
		rendered.push((gw, render::render_gateway(&world, gw, &opts)));
	}
	// fleet: one certificate Secret with every Gateway's files, mounted into every pod
	let fleet_pods = match &cfg.mode {
		Mode::Fleet(f) => {
			let eps = provision::pods(&pods_now, &cfg.namespace, &provision::parse_selector(&f.selector));
			let mut files = BTreeMap::new();
			for (_, plan) in rendered.iter().filter(|(_, p)| p.accepted()) {
				files.extend(plan.files.iter().map(|(k, v)| (k.clone(), v.clone())));
			}
			let absent = state.absent.entry(provision::FLEET_CERTS_SECRET.into()).or_default();
			let ns = cfg.namespace.clone();
			match provision::apply_certs(client, &cfg.namespace, provision::FLEET_CERTS_SECRET, &files, absent, |d| {
				provision::fleet_certs_secret(&ns, d)
			})
			.await
			{
				Ok(hash) => provision::touch_pods(client, &eps, &hash).await,
				Err(e) => {
					warn!(error = format!("{e:#}"), "cannot write the fleet's certificate Secret");
					soon = true;
				}
			}
			eps
		}
		Mode::Managed(_) => vec![],
	};
	// fleet with VIPs: which can be used, who holds them
	let vips = match &cfg.mode {
		Mode::Fleet(f) if !f.vips.is_empty() => {
			let host_ips: Vec<String> = fleet_pods.iter().filter_map(|e| e.host_ip.clone()).collect();
			let unusable: BTreeMap<String, String> = f
				.vips
				.iter()
				.filter_map(|v| fleet_vip::unusable(&world, v, &cfg.address_cidrs, &host_ips).map(|why| (v.clone(), why)))
				.collect();
			{
				let mut seen = VIPS_SEEN.lock().unwrap();
				for why in unusable.values() {
					if seen.insert(why.clone()) {
						warn!(reason = %why, "a VIP is not used");
					}
				}
			}
			let holders = fleet_vip::holders(client, &cfg.namespace, &f.vips).await;
			soon |= holders.values().any(|h| matches!(h, fleet_vip::Holder::Unheld(_)));
			Some(VipState { unusable, holders })
		}
		_ => None,
	};
	let pass = Pass {
		client,
		vips: vips.as_ref(),
		cache,
		rp,
		cfg,
		boot,
		world: &world,
		pods_now: &pods_now,
		fleet_pods: &fleet_pods,
		gateway_api_version: cache.resource("Gateway").map(|ar| ar.api_version),
		now: &now,
	};
	// each Gateway's part of the state, so the Gateways run side by side
	let mut jobs = vec![];
	for (gw, plan) in rendered {
		if !plan.notes.is_empty() {
			let digest = crate::pem::short_hash(plan.notes.join("\n").as_bytes());
			if notes_seen.insert(digest) {
				for n in &plan.notes {
					warn!(gateway = %plan.ruleset, note = %n, "migration: not converted");
				}
			}
		}
		let id = provision::gateway_id(&plan.namespace, &plan.name, &plan.uid);
		let absent_key = format!("{}/{}", plan.namespace, provision::certs_secret_name(&id));
		let applied: Applied =
			state.applied.iter().filter(|((_, set), _)| *set == plan.ruleset).map(|(k, v)| (k.clone(), v.clone())).collect();
		let sub = Sub {
			applied,
			caps: state.caps.clone(),
			absent: state.absent.remove(&absent_key).unwrap_or_default(),
			ready_since: state.ready_since.get(&id).copied(),
		};
		jobs.push(gateway_pass(&pass, gw, plan, sub));
	}
	let outs: Vec<GatewayOut> = futures::stream::iter(jobs).buffer_unordered(PARALLEL).collect().await;
	let mut ui_groups: Vec<ui::Group> = vec![];
	for out in outs {
		ui_groups.extend(out.ui_group.clone());
		soon |= out.soon;
		state.applied.retain(|(_, set), _| *set != out.plan.ruleset);
		state.applied.extend(out.sub.applied);
		state.caps.extend(out.sub.caps);
		let id = provision::gateway_id(&out.plan.namespace, &out.plan.name, &out.plan.uid);
		state.absent.insert(format!("{}/{}", out.plan.namespace, provision::certs_secret_name(&id)), out.sub.absent);
		match out.sub.ready_since {
			Some(t) => state.ready_since.insert(id.clone(), t),
			None => state.ready_since.remove(&id),
		};
		if cfg.migration.as_ref().is_some_and(|m| m.gateway == (out.plan.namespace.clone(), out.plan.name.clone())) {
			migration_addresses = Some(out.addresses.clone());
		}
		if out.keep_pdb {
			keep_pdb.extend(out.keep_id.clone());
		}
		keep_ids.extend(out.keep_id);
		pod_done.extend(out.pod_done);
		plans.push((out.plan, out.results));
	}

	*NOTES_SEEN.lock().unwrap() = notes_seen;

	// Gateways that are gone
	state.absent.retain(|name, _| {
		name == provision::FLEET_CERTS_SECRET || keep_ids.iter().any(|id| name.ends_with(&format!("/{}", provision::certs_secret_name(id))))
	});
	// fleet: the readiness gate of each pod once every Gateway's set is on it
	if !fleet_pods.is_empty() {
		let mut done: HashMap<&str, bool> = fleet_pods.iter().map(|e| (e.uid.as_str(), true)).collect();
		for (uid, ok) in &pod_done {
			if let Some(d) = done.get_mut(uid.as_str()) {
				*d &= ok;
			}
		}
		set_gates(client, &pods_now, &done).await;
	}
	// fleet: the UI reads the fleet's pods only if it may see every Gateway on them
	if let Mode::Fleet(f) = &cfg.mode {
		let shown = cfg.ui.is_some() && plans.iter().filter(|(p, _)| p.accepted()).all(|(p, _)| ui::visible(p));
		if let Err(e) = ui::apply_fleet_token(client, &cfg.namespace, &boot.token, shown).await {
			warn!(error = %e, "cannot write the fleet's token file");
			soon = true;
		}
		if shown {
			// fleet pods take the UI's token once they read the token file again (rproxy v0.4.2; older rproxy
			// at start only: a rollout restart, docs/SECURITY.md); the ones not Ready, being deleted or not
			// taking it yet are not listed
			let eps = provision::ui_pods(&pods_now, &cfg.namespace, &provision::parse_selector(&f.selector), None);
			let eps =
				ui::accepting(rp, &eps, &bootstrap::derive_ui_token(&boot.token, "fleet"), crate::rproxy::client::API_SERVER_NAME).await;
			ui_groups.extend(ui::fleet_group(&boot.token, &eps));
		}
	}
	if let Some(u) = &cfg.ui {
		if let Err(e) = ui::apply(client, &u.namespace, &ui_groups, &boot.ca_pem).await {
			warn!(namespace = u.namespace, error = format!("{e:#}"), "cannot write the UI's discovery Secret");
		}
	}
	if let Err(e) = provision::collect_garbage(client, &keep_ids, &keep_pdb, &cfg.scope()).await {
		warn!(error = %e, "cannot remove rproxy of deleted Gateways");
	}
	if let Mode::Fleet(f) = &cfg.mode {
		let keep: BTreeSet<&str> = plans.iter().map(|(p, _)| p.ruleset.as_str()).collect();
		for ep in provision::pods(&pods_now, &cfg.namespace, &provision::parse_selector(&f.selector)) {
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

	if let (Some(m), Some(addresses)) = (&cfg.migration, &migration_addresses) {
		write_ingress_status(client, &world, m, addresses).await;
	}
	write_route_status(client, cache, &world, &plans, cfg, &now).await;
	write_crd_status(client, cache, &world, &plans, cfg, &now).await;
	Ok(soon)
}

/// A ListenerSet's `status` (written by the controller of the Gateway it names).
async fn write_listener_set_status(
	client: &kube::Client,
	cache: &cache::Cache,
	world: &render::world::World,
	set: &render::ListenerSetPlan,
	pods: &[PodSync],
	now: &str,
) {
	let prev = world
		.listener_sets
		.iter()
		.find(|ls| {
			ls.metadata.namespace.as_deref() == Some(set.namespace.as_str()) && ls.metadata.name.as_deref() == Some(set.name.as_str())
		})
		.and_then(|ls| ls.status.clone());
	let st = status::listener_set_status(set, pods, prev.as_ref(), now);
	if prev.as_ref() == Some(&st) {
		return;
	}
	if let Some(api) = dyn_api(client, cache, "ListenerSet", Some(&set.namespace)) {
		if let Err(e) = patch_status(&api, &set.name, st).await {
			warn!(listenerset = format!("{}/{}", set.namespace, set.name), error = %e, "cannot write ListenerSet status");
		}
	}
}

/// `status.loadBalancer` of the Ingresses read into the migration Gateway's set: its addresses.
async fn write_ingress_status(
	client: &kube::Client,
	world: &render::world::World,
	m: &render::migrate::Settings,
	addresses: &[(String, String)],
) {
	let want = render::migrate::ingress_status(addresses);
	for i in render::migrate::handled_ingresses(world, m) {
		let have = i.status.as_ref().and_then(|s| serde_json::to_value(s).ok()).unwrap_or(Value::Null);
		let have_lb = have.get("loadBalancer").and_then(|l| l.get("ingress")).cloned().unwrap_or(json!([]));
		if have_lb == want["loadBalancer"]["ingress"] {
			continue;
		}
		let (ns, name) = (i.metadata.namespace.clone().unwrap_or_default(), i.metadata.name.clone().unwrap_or_default());
		let api: Api<k8s_openapi::api::networking::v1::Ingress> = Api::namespaced(client.clone(), &ns);
		// a merge patch replaces the list as a whole
		if let Err(e) = api.patch_status(&name, &PatchParams::default(), &Patch::Merge(json!({ "status": want }))).await {
			warn!(ingress = format!("{ns}/{name}"), error = %e, "cannot write Ingress status");
		}
	}
}

/// `status.ancestors` of policies of `kind` (GEP-713): ours replaced, other controllers' kept.
async fn write_ancestors<'a>(
	client: &kube::Client,
	cache: &cache::Cache,
	kind: &str,
	policies: &[(render::world::Key, Option<Value>)],
	entries: impl Iterator<Item = &'a render::policy::PolicyStatus>,
	cfg: &Config,
	now: &str,
) {
	let entries: Vec<&render::policy::PolicyStatus> = entries.collect();
	for (key, prev) in policies {
		let mine: Vec<Value> = entries
			.iter()
			.filter(|ps| ps.namespace == key.0 && ps.name == key.1)
			.map(|ps| status::policy_entry(ps, &cfg.controller_name, prev.as_ref(), now))
			.collect();
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
		if let Some(api) = dyn_api(client, cache, kind, Some(&key.0)) {
			if let Err(e) = patch_status(&api, &key.1, new).await {
				warn!(kind, policy = format!("{}/{}", key.0, key.1), error = %e, "cannot write policy status");
			}
		}
	}
}

/// What every Gateway's pass reads.
/// fleet with VIPs: the VIPs that cannot be used (why) and who holds each.
struct VipState {
	unusable: BTreeMap<String, String>,
	holders: BTreeMap<String, fleet_vip::Holder>,
}

/// Why VIPs are not used, already logged.
static VIPS_SEEN: std::sync::Mutex<BTreeSet<String>> = std::sync::Mutex::new(BTreeSet::new());

struct Pass<'a> {
	client: &'a kube::Client,
	vips: Option<&'a VipState>,
	cache: &'a cache::Cache,
	rp: &'a Client,
	cfg: &'a Config,
	boot: &'a bootstrap::Bootstrap,
	world: &'a render::world::World,
	pods_now: &'a [std::sync::Arc<k8s_openapi::api::core::v1::Pod>],
	fleet_pods: &'a [Endpoint],
	gateway_api_version: Option<String>,
	now: &'a str,
}

/// The part of `State` one Gateway's pass uses (taken out, run apart, put back).
#[derive(Default)]
struct Sub {
	applied: Applied,
	caps: HashMap<String, Capabilities>,
	absent: BTreeMap<String, std::time::Instant>,
	/// Since when the Gateway's Service has had ready endpoints.
	ready_since: Option<std::time::Instant>,
}

/// How long a Gateway's Service must have had ready endpoints before the Gateway is
/// `Programmed`: kube-proxy (or what stands in for it) programs a new Service's
/// endpoints a little after they appear, and a client connecting before that can
/// get stuck (its first SYN leaves a conntrack entry without the Service's NAT).
const SERVICE_SETTLE: Duration = Duration::from_secs(3);
struct GatewayOut {
	plan: GatewayPlan,
	results: Vec<PodSync>,
	/// Pod uid → whether the set is on it (`Synced`, or refused: nothing more to wait for).
	pod_done: Vec<(String, bool)>,
	addresses: Vec<(String, String)>,
	keep_id: Option<String>,
	/// Whether the Gateway's PodDisruptionBudget stays.
	keep_pdb: bool,
	soon: bool,
	sub: Sub,
	/// The UI's group of the Gateway (shown to the UI, with pods).
	ui_group: Option<ui::Group>,
}

/// One Gateway: deploys its rproxy (managed), applies its rule set to its pods, writes its status.
async fn gateway_pass(p: &Pass<'_>, gw: &crate::k8s::gateway::Gateway, mut plan: GatewayPlan, mut sub: Sub) -> GatewayOut {
	let mut soon = false;
	let mut keep_id = None;
	let mut keep_pdb = false;
	let mut ui_group = None;
	let id = provision::gateway_id(&plan.namespace, &plan.name, &plan.uid);
	let infra = gw.spec.infrastructure.clone().unwrap_or_default();
	let svc_key = (plan.namespace.clone(), provision::object_name(&id));
	if let (Mode::Managed(_), Some(want), Some(have)) = (&p.cfg.mode, &plan.parameters, p.world.services.get(&svc_key)) {
		// a Service's loadBalancerClass cannot change: the old Service stays
		let want = want.service.as_ref().and_then(|s| s.load_balancer_class.clone());
		let have = have.spec.as_ref().and_then(|s| s.load_balancer_class.clone());
		if have.is_some() && want != have {
			let e = format!(
				"spec.service.loadBalancerClass: the Service has {} and it cannot change (create the Gateway again)",
				have.unwrap_or_default()
			);
			render::status::set(&mut plan.conds, render::status::Cond::new("Accepted", false, "InvalidParameters", e.clone()));
			plan.parameters_error = Some(e);
		}
	}
	// invalid parameters: a Gateway whose rproxy runs keeps it as it is (one mistyped reference
	// does not stop its traffic); its rule set is still applied
	if let (Mode::Managed(_), Some(e)) = (&p.cfg.mode, plan.parameters_error.clone()) {
		match provision::deployed(p.client, &plan.namespace, &id).await {
			Ok(true) if plan.address_error.is_none() && status_only_params(&plan) => plan.kept = Some(e),
			Ok(_) => {}
			Err(err) => {
				// unknown: keep what may run (collected later if it does not)
				warn!(gateway = %plan.ruleset, error = %err, "cannot read the rproxy Deployment");
				keep_id = Some(id.clone());
				keep_pdb = true;
				soon = true;
			}
		}
	}
	let (addresses, endpoints) = match &p.cfg.mode {
		Mode::Managed(_) if plan.kept.is_some() => {
			keep_id = Some(id.clone());
			keep_pdb = true;
			let selector: BTreeMap<String, String> = [(provision::LABEL_GATEWAY.to_string(), id.clone())].into();
			let eps = provision::pods(p.pods_now, &plan.namespace, &selector);
			let mut t = provision::Target::new(&plan);
			t.labels = infra.labels.clone();
			t.annotations = infra.annotations.clone();
			t.owner = match (&p.gateway_api_version, &gw.metadata.uid) {
				(Some(v), Some(uid)) => Some(provision::owner(v, &plan.name, uid)),
				_ => None,
			};
			let current_api = p.world.secrets.get(&(plan.namespace.clone(), provision::api_secret_name(&id)));
			match provision::apply_secrets(p.client, &t, p.boot, current_api, &mut sub.absent).await {
				Ok((certs, _)) => provision::touch_pods(p.client, &eps, &certs).await,
				Err(e) => {
					warn!(gateway = %plan.ruleset, error = format!("{e:#}"), "cannot write the rproxy Secrets");
					soon = true;
				}
			}
			let addresses = match p.world.services.get(&svc_key) {
				Some(_) if !plan.addresses.is_empty() => plan.addresses.iter().map(|a| ("IPAddress".to_string(), a.clone())).collect(),
				Some(s) => provision::service_addresses(s),
				None => vec![],
			};
			(addresses, eps)
		}
		// not accepted (an unsupported address type, invalid parameters): nothing is deployed
		_ if !plan.accepted() => (vec![], vec![]),
		Mode::Managed(m) => {
			keep_id = Some(id.clone());
			let absent = &mut sub.absent;
			let selector: BTreeMap<String, String> = [(provision::LABEL_GATEWAY.to_string(), id.clone())].into();
			let eps = provision::pods(p.pods_now, &plan.namespace, &selector);
			let mut t = provision::Target::new(&plan);
			t.labels = infra.labels.clone();
			t.annotations = infra.annotations.clone();
			t.service_annotations = p.cfg.service_annotations.clone();
			t.params = plan.parameters.clone().unwrap_or_default();
			t.graceful = graceful(p.rp, &t.rproxy_image(m), &eps, &mut sub.caps).await;
			// rproxy v0.4.2 reads a changed token file again: the UI's token does not roll the pods
			t.tokens_reload = image_feature(p.rp, &t.rproxy_image(m), "tokens_reload", &eps, &mut sub.caps).await;
			// the UI reads it (its token in the token file, its pods through the NetworkPolicy)
			t.ui = p.cfg.ui.clone().filter(|_| ui::visible(&plan));
			t.owner = match (&p.gateway_api_version, &gw.metadata.uid) {
				(Some(v), Some(uid)) => Some(provision::owner(v, &plan.name, uid)),
				_ => None,
			};
			if plan.address_error.is_none() {
				t.addresses = plan.addresses.clone();
			}
			let current_api = p.world.secrets.get(&(plan.namespace.clone(), provision::api_secret_name(&id)));
			let applied = provision::apply_managed(p.client, &t, m, p.boot, current_api, absent).await;
			let svc = match applied {
				Ok(a) => {
					provision::touch_pods(p.client, &eps, &a.certs).await;
					// the UI reads the pods that already have its token (the current control API Secret, and
					// seen to take it)
					if t.ui.is_some() {
						let pods = provision::ui_pods(p.pods_now, &plan.namespace, &selector, Some(&a.api));
						let pods = ui::accepting(p.rp, &pods, &bootstrap::derive_ui_token(&p.boot.token, &id), &server_name_for(&id)).await;
						ui_group = ui::managed_group(&plan.namespace, &plan.name, &id, &p.boot.token, &pods);
					}
					keep_pdb = a.pdb;
					a.service
				}
				Err(e) => {
					// the Secrets were not written this pass: the UI keeps the pods that are Ready
					if t.ui.is_some() {
						let pods = provision::ui_pods(p.pods_now, &plan.namespace, &selector, None);
						let pods = ui::accepting(p.rp, &pods, &bootstrap::derive_ui_token(&p.boot.token, &id), &server_name_for(&id)).await;
						ui_group = ui::managed_group(&plan.namespace, &plan.name, &id, &p.boot.token, &pods);
					}
					if plan.address_error.is_none() && !plan.addresses.is_empty() && format!("{e:#}").contains("externalIPs") {
						// the cluster refuses the address (validation, an admission policy)
						plan.address_error = Some(format!("the Service cannot take the address: {e:#}"));
					} else {
						warn!(gateway = %plan.ruleset, error = format!("{e:#}"), "cannot deploy rproxy");
					}
					// what runs stays (the PodDisruptionBudget too) until the next pass
					keep_pdb = true;
					soon = true;
					None
				}
			};
			// traffic reaches the pods once the Service's endpoints are programmed on the nodes
			let ready = p
				.world
				.slices
				.get(&svc_key)
				.into_iter()
				.flatten()
				.any(|s| s.endpoints.iter().flatten().any(|e| e.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true)));
			sub.ready_since = if ready { Some(sub.ready_since.unwrap_or_else(std::time::Instant::now)) } else { None };
			if svc.is_some() && sub.ready_since.is_none_or(|t| t.elapsed() < SERVICE_SETTLE) {
				plan.serving_pending = Some("waiting for the rproxy Service's endpoints to be ready on the nodes".into());
				soon = true;
			}
			let addresses = match svc.as_ref() {
				// the addresses asked for, once the Service has them
				Some(_) if !plan.addresses.is_empty() && plan.address_error.is_none() => {
					plan.addresses.iter().map(|a| ("IPAddress".to_string(), a.clone())).collect()
				}
				Some(s) => provision::service_addresses(s),
				None => vec![],
			};
			(addresses, eps)
		}
		Mode::Fleet(f) => {
			let eps = p.fleet_pods.to_vec();
			if let Some(v) = p.vips {
				let addrs = match fleet_vip::gateway_vips(&plan.addresses, &f.vips, &v.unusable, &v.holders) {
					Ok(a) => a,
					Err(e) => {
						plan.address_error.get_or_insert(e);
						vec![]
					}
				};
				let addrs = addrs.into_iter().map(|a| ("IPAddress".to_string(), a)).collect();
				(addrs, eps)
			} else {
				let addrs = if f.addresses.is_empty() {
					let mut a: Vec<String> = eps.iter().filter_map(|e| e.host_ip.clone()).collect();
					a.sort();
					a.dedup();
					a
				} else {
					f.addresses.clone()
				};
				let addrs = if plan.addresses.is_empty() {
					addrs
				} else {
					// the fleet's addresses are fixed: an address asked for must be one of them
					if let Some(a) = plan.addresses.iter().find(|a| !addrs.contains(a)) {
						plan.address_error.get_or_insert(format!("{a} is not an address of the rproxy fleet ({})", addrs.join(", ")));
					}
					plan.addresses.clone()
				};
				(
					addrs
						.into_iter()
						.map(|a| (if a.parse::<std::net::IpAddr>().is_ok() { "IPAddress" } else { "Hostname" }.to_string(), a))
						.collect(),
					eps,
				)
			}
		}
	};
	let addresses = if plan.address_error.is_some() { vec![] } else { addresses };
	// the pods side by side (each with its own part of what was applied)
	let jobs = endpoints.iter().map(|ep| {
		let mut applied: Applied = sub.applied.iter().filter(|((uid, _), _)| *uid == ep.uid).map(|(k, v)| (k.clone(), v.clone())).collect();
		let mut caps: HashMap<String, Capabilities> =
			sub.caps.get(&ep.uid).map(|c| [(ep.uid.clone(), c.clone())].into()).unwrap_or_default();
		let plan = &plan;
		async move {
			let (r, again) = sync_pod(p.rp, ep, plan, &mut applied, &mut caps).await;
			(r, again, applied, caps)
		}
	});
	let synced: Vec<_> = futures::stream::iter(jobs).buffered(POD_PARALLEL).collect().await;
	let mut results = vec![];
	let mut pod_done = vec![];
	for (ep, (r, again, applied, caps)) in endpoints.iter().zip(synced) {
		soon |= again;
		sub.applied.retain(|(uid, _), _| *uid != ep.uid);
		sub.applied.extend(applied);
		sub.caps.extend(caps);
		pod_done.push((ep.uid.clone(), matches!(r, PodSync::Synced(_) | PodSync::Rejected(_))));
		results.push(r);
	}
	// managed: each pod is this Gateway's alone
	if matches!(p.cfg.mode, Mode::Managed(_)) {
		let done: HashMap<&str, bool> = pod_done.iter().map(|(u, d)| (u.as_str(), *d)).collect();
		set_gates(p.client, p.pods_now, &done).await;
	}
	if let Some(api) = dyn_api(p.client, p.cache, "Gateway", Some(&plan.namespace)) {
		let st = status::gateway_status(&plan, &addresses, &results, gw.status.as_ref(), p.now);
		for set in &plan.listener_sets {
			write_listener_set_status(p.client, p.cache, p.world, set, &results, p.now).await;
		}
		if gw.status.as_ref() != Some(&st) {
			if let Err(e) = patch_status(&api, &plan.name, st).await {
				warn!(gateway = %plan.ruleset, error = %e, "cannot write Gateway status");
			}
		}
	}

	GatewayOut { plan, results, pod_done, addresses, keep_id, keep_pdb, soon, sub, ui_group }
}

/// Whether the Gateway is not accepted only because of its parameters (it may keep its last good rproxy).
fn status_only_params(plan: &GatewayPlan) -> bool {
	plan.parameters_error.is_some() && plan.conds.iter().filter(|c| c.kind == "Accepted").all(|c| c.reason == "InvalidParameters")
}

/// Sets the readiness gate of the pods in `done` (by uid) where it changes.
async fn set_gates(client: &kube::Client, pods: &[std::sync::Arc<k8s_openapi::api::core::v1::Pod>], done: &HashMap<&str, bool>) {
	for pod in pods {
		let Some(d) = pod.metadata.uid.as_deref().and_then(|u| done.get(u)) else { continue };
		if let Some(ready) = provision::gate_change(pod, Some(*d)) {
			provision::set_gate(client, pod, ready).await;
		}
	}
}

/// RproxyPolicy and BackendTLSPolicy `status.ancestors`, RproxyRule `status`.
async fn write_crd_status(
	client: &kube::Client,
	cache: &cache::Cache,
	world: &render::world::World,
	plans: &[(GatewayPlan, Vec<PodSync>)],
	cfg: &Config,
	now: &str,
) {
	let rproxy_policies: Vec<(render::world::Key, Option<Value>)> = world
		.policies
		.iter()
		.map(|p| (render::world::key(&p.metadata), p.status.as_ref().and_then(|s| serde_json::to_value(s).ok())))
		.collect();
	write_ancestors(client, cache, "RproxyPolicy", &rproxy_policies, plans.iter().flat_map(|(p, _)| &p.policies), cfg, now).await;
	let tls_policies: Vec<(render::world::Key, Option<Value>)> =
		world.backend_tls_policies.iter().map(|p| (render::world::key(&p.metadata), p.status.clone())).collect();
	write_ancestors(client, cache, "BackendTLSPolicy", &tls_policies, plans.iter().flat_map(|(p, _)| &p.backend_tls), cfg, now).await;
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
	// a managed Gateway's rproxy has its own token and certificate name
	let rp = &rp.scoped(ep.target.as_deref());
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
		let names: Vec<&String> = plan.files.keys().collect();
		match crate::certsync::present(&ep.ip, ep.certsync_port, &names).await {
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

/// (feature, image) whose rproxy said it has the feature (kept while the process runs: a Gateway whose
/// pods all restart at once keeps its shape).
static FEATURE_IMAGES: std::sync::Mutex<BTreeSet<(String, String)>> = std::sync::Mutex::new(BTreeSet::new());

/// Features of the image this controller ships (`--rproxy-image`'s default, rproxy v0.4.2).
const SHIPPED_FEATURES: &[&str] = &["graceful_shutdown", "tokens_reload"];

/// Whether `image` has `feature` (rproxy's `features`): the image this controller ships, or one a pod
/// running it said so of. Unknown (another image before its pods answer): no; the pods roll again
/// once it is known.
async fn image_feature(rp: &Client, image: &str, feature: &str, eps: &[Endpoint], caps: &mut HashMap<String, Capabilities>) -> bool {
	if image == provision::RPROXY_IMAGE && SHIPPED_FEATURES.contains(&feature) {
		return true;
	}
	let key = (feature.to_string(), image.to_string());
	if FEATURE_IMAGES.lock().unwrap().contains(&key) {
		return true;
	}
	let mine: Vec<Endpoint> = eps.iter().filter(|ep| ep.rproxy_image.as_deref() == Some(image)).cloned().collect();
	pod_features(rp, &mine, caps).await;
	let known = mine.iter().any(|ep| caps.get(&ep.uid).is_some_and(|c| c.feature(feature)));
	if known {
		info!(image, feature, "rproxy has the feature");
		FEATURE_IMAGES.lock().unwrap().insert(key);
	}
	known
}

/// Whether `image` has a graceful shutdown (`features.graceful_shutdown`, rproxy v0.4.1): if not
/// known, the pods keep the preStop.
async fn graceful(rp: &Client, image: &str, eps: &[Endpoint], caps: &mut HashMap<String, Capabilities>) -> bool {
	image_feature(rp, image, "graceful_shutdown", eps, caps).await
}

async fn pod_features(rp: &Client, eps: &[Endpoint], caps: &mut HashMap<String, Capabilities>) -> render::Features {
	let mut out = render::Features::default();
	for ep in eps {
		if !caps.contains_key(&ep.uid) {
			let Ok(addr) = api_addr(ep) else { continue };
			match rp.scoped(ep.target.as_deref()).capabilities(addr).await {
				Ok(c) => {
					caps.insert(ep.uid.clone(), c);
				}
				Err(e) => {
					debug!(pod = ep.pod, error = format!("{e:#}"), "capabilities not known yet");
					continue;
				}
			}
		}
		out = out.and(&render::Features::of(&caps[&ep.uid]));
	}
	out
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
			RouteKind::Http | RouteKind::Grpc => if kind == RouteKind::Grpc { &world.grpc_routes } else { &world.http_routes }
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
	for (kind, list) in [(RouteKind::Http, &world.http_routes), (RouteKind::Grpc, &world.grpc_routes)] {
		for r in list {
			all.push((
				kind,
				r.metadata.namespace.clone().unwrap_or_default(),
				r.metadata.name.clone().unwrap_or_default(),
				r.status.clone(),
			));
		}
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
