//! Watches (reflectors) of everything rendering reads, and snapshots of them.
//!
//! Third-party kinds (Gateway API, rproxy's CRDs, Traefik) are watched as dynamic
//! objects at the version the API server serves (found by discovery). Kinds
//! whose CRDs are not installed are skipped and looked for again now and then
//! (`rediscover`): a CRD installed later is watched without a restart.

use std::sync::{Arc, RwLock};

use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::{Api, DynamicObject};
use kube::discovery::ApiResource;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use serde::de::DeserializeOwned;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::render::world::World;

/// A watched dynamic kind.
struct DynKind {
	kind: &'static str,
	store: Multi<DynamicObject>,
}

/// One watch per namespace watched (or one for all namespaces), read as one.
pub struct Multi<K: kube::Resource + 'static>(Vec<Store<K>>)
where
	K::DynamicType: Eq + std::hash::Hash + Clone;

impl<K: kube::Resource + Clone + 'static> Multi<K>
where
	K::DynamicType: Eq + std::hash::Hash + Clone,
{
	pub fn state(&self) -> Vec<Arc<K>> {
		self.0.iter().flat_map(|s| s.state()).collect()
	}

	async fn wait_until_ready(&self) {
		for s in &self.0 {
			let _ = s.wait_until_ready().await;
		}
	}
}

/// The namespaces watched: `None` for all (`--watch-namespaces` empty).
pub type Scope = Option<Vec<String>>;

/// An API for each namespace of `scope` (or one for all).
fn apis<K>(client: &kube::Client, scope: &Scope) -> Vec<Api<K>>
where
	K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope>,
	K::DynamicType: Default,
{
	match scope {
		None => vec![Api::all(client.clone())],
		Some(nss) => nss.iter().map(|ns| Api::namespaced(client.clone(), ns)).collect(),
	}
}

fn watch_all<K>(client: &kube::Client, scope: &Scope, changed: &Arc<Notify>, what: &'static str) -> Multi<K>
where
	K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope> + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
	K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
	Multi(apis::<K>(client, scope).into_iter().map(|api| spawn_watch(api, changed.clone(), what)).collect())
}

pub struct Cache {
	pub changed: Arc<Notify>,
	services: Multi<Service>,
	slices: Multi<EndpointSlice>,
	secrets: Multi<Secret>,
	config_maps: Multi<ConfigMap>,
	namespaces: Store<Namespace>,
	pub pods: Multi<Pod>,
	ingresses: Option<Multi<Ingress>>,
	scope: Scope,
	/// The watched dynamic kinds and their resources (found by discovery; for status patches).
	dynamic: RwLock<Vec<(DynKind, ApiResource)>>,
	/// Whether Traefik's kinds are wanted (migration).
	migration: bool,
}

/// Traefik's CRDs read for migration.
pub const TRAEFIK_KINDS: &[&str] = &["IngressRoute", "IngressRouteTCP", "IngressRouteUDP", "Middleware", "TLSOption"];

/// The kinds watched dynamically: (group, kind).
pub const DYNAMIC_KINDS: &[(&str, &str)] = &[
	(crate::k8s::gateway::GROUP, "GatewayClass"),
	(crate::k8s::gateway::GROUP, "Gateway"),
	(crate::k8s::gateway::GROUP, "HTTPRoute"),
	(crate::k8s::gateway::GROUP, "GRPCRoute"),
	(crate::k8s::gateway::GROUP, "TLSRoute"),
	(crate::k8s::gateway::GROUP, "TCPRoute"),
	(crate::k8s::gateway::GROUP, "UDPRoute"),
	(crate::k8s::gateway::GROUP, "ReferenceGrant"),
	(crate::k8s::gateway::GROUP, "ListenerSet"),
	(crate::k8s::gateway::GROUP, "BackendTLSPolicy"),
	(crate::k8s::crd::GROUP, "RproxyMiddleware"),
	(crate::k8s::crd::GROUP, "RproxyPolicy"),
	(crate::k8s::crd::GROUP, "RproxyRule"),
	(crate::k8s::crd::GROUP, "RproxyGatewayParameters"),
];

fn spawn_watch<K>(api: Api<K>, changed: Arc<Notify>, what: &'static str) -> Store<K>
where
	K: kube::Resource + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
	K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
	let (reader, writer) = reflector::store();
	let stream = reflector::reflector(writer, watcher(api, watcher::Config::default()).default_backoff());
	tokio::spawn(async move {
		let mut stream = std::pin::pin!(stream);
		while let Some(ev) = stream.next().await {
			match ev {
				Ok(_) => changed.notify_one(),
				Err(e) => warn!(kind = what, error = %e, "watch error"),
			}
		}
	});
	reader
}

fn spawn_dyn_watch(client: &kube::Client, ar: &ApiResource, scope: &Scope, changed: Arc<Notify>) -> Multi<DynamicObject> {
	// GatewayClass is the one cluster-scoped kind read
	let apis: Vec<Api<DynamicObject>> = match scope {
		Some(nss) if ar.kind != "GatewayClass" => nss.iter().map(|ns| Api::namespaced_with(client.clone(), ns, ar)).collect(),
		_ => vec![Api::all_with(client.clone(), ar)],
	};
	Multi(apis.into_iter().map(|api| spawn_dyn_watch_one(api, ar, changed.clone())).collect())
}

fn spawn_dyn_watch_one(api: Api<DynamicObject>, ar: &ApiResource, changed: Arc<Notify>) -> Store<DynamicObject> {
	let writer = reflector::store::Writer::new(ar.clone());
	let reader = writer.as_reader();
	let stream = reflector::reflector(writer, watcher(api, watcher::Config::default()).default_backoff());
	let kind = ar.kind.clone();
	tokio::spawn(async move {
		let mut stream = std::pin::pin!(stream);
		while let Some(ev) = stream.next().await {
			match ev {
				Ok(_) => changed.notify_one(),
				Err(e) => warn!(kind, error = %e, "watch error"),
			}
		}
	});
	reader
}

impl Cache {
	/// Starts the watches. `migration`: also watch Ingress and Traefik's CRDs. `scope`:
	/// the namespaces watched (`None`: all).
	pub async fn start(client: &kube::Client, migration: bool, scope: Scope) -> anyhow::Result<Cache> {
		let changed = Arc::new(Notify::new());
		let cache = Cache {
			services: watch_all(client, &scope, &changed, "Service"),
			slices: watch_all(client, &scope, &changed, "EndpointSlice"),
			secrets: watch_all(client, &scope, &changed, "Secret"),
			config_maps: watch_all(client, &scope, &changed, "ConfigMap"),
			namespaces: spawn_watch(Api::all(client.clone()), changed.clone(), "Namespace"),
			pods: Multi(apis::<Pod>(client, &scope).into_iter().map(|api| spawn_pod_watch(api, changed.clone())).collect()),
			ingresses: migration.then(|| watch_all(client, &scope, &changed, "Ingress")),
			dynamic: RwLock::new(vec![]),
			migration,
			scope,
			changed,
		};
		cache.rediscover(client, true).await?;
		Ok(cache)
	}

	/// The (group, kind) pairs to watch, given what the API server serves.
	fn wanted(&self, groups: &[(String, String, ApiResource)]) -> Vec<(&'static str, &'static str)> {
		let mut kinds: Vec<(&'static str, &'static str)> = DYNAMIC_KINDS.to_vec();
		if self.migration {
			for kind in TRAEFIK_KINDS {
				// traefik.io, else the older traefik.containo.us
				let group = crate::k8s::traefik::GROUPS
					.iter()
					.find(|g| groups.iter().any(|(gg, k, _)| gg == *g && k == kind))
					.copied()
					.unwrap_or("traefik.io");
				kinds.push((group, kind));
			}
		}
		kinds
	}

	/// Starts watching the wanted kinds the API server serves now and that are not
	/// watched yet (CRDs installed after the controller started). `first`: log
	/// the kinds that are missing.
	pub async fn rediscover(&self, client: &kube::Client, first: bool) -> anyhow::Result<()> {
		let groups = discover(client).await?;
		let mut added = false;
		for (group, kind) in self.wanted(&groups) {
			if self.dynamic.read().unwrap().iter().any(|(d, _)| d.kind == kind) {
				continue;
			}
			match groups.iter().find(|(g, k, _)| g == group && k == kind) {
				Some((_, _, ar)) => {
					info!(kind, version = %ar.version, "watching");
					let store = spawn_dyn_watch(client, ar, &self.scope, self.changed.clone());
					if !first {
						// a new kind: render once it has listed
						let _ = store.wait_until_ready().await;
					}
					self.dynamic.write().unwrap().push((DynKind { kind, store }, ar.clone()));
					added = true;
				}
				None if first => warn!(group, kind, "not served by the API server (CRD not installed); looked for again later"),
				None => {}
			}
		}
		if added && !first {
			self.changed.notify_one();
		}
		Ok(())
	}

	/// Waits until every watch has listed once.
	pub async fn ready(&self) {
		let _ = self.services.wait_until_ready().await;
		let _ = self.slices.wait_until_ready().await;
		let _ = self.secrets.wait_until_ready().await;
		let _ = self.config_maps.wait_until_ready().await;
		let _ = self.namespaces.wait_until_ready().await;
		let _ = self.pods.wait_until_ready().await;
		if let Some(i) = &self.ingresses {
			let _ = i.wait_until_ready().await;
		}
		let stores: Vec<Store<DynamicObject>> = self.dynamic.read().unwrap().iter().flat_map(|(d, _)| d.store.0.clone()).collect();
		for s in stores {
			let _ = s.wait_until_ready().await;
		}
	}

	pub fn resource(&self, kind: &str) -> Option<ApiResource> {
		self.dynamic.read().unwrap().iter().find(|(d, _)| d.kind == kind).map(|(_, ar)| ar.clone())
	}

	/// Everything as it is now.
	pub fn snapshot(&self) -> World {
		let mut w = World::default();
		for s in self.services.state() {
			w.services.insert(crate::render::world::key(&s.metadata), (*s).clone());
		}
		for s in self.slices.state() {
			w.add_slice((*s).clone());
		}
		for s in self.secrets.state() {
			// only Secrets that can be certificates, to keep the snapshot small
			if s.data.as_ref().is_some_and(|d| ["tls.crt", "ca.crt", "tls.ca", "users"].iter().any(|k| d.contains_key(*k))) {
				w.secrets.insert(crate::render::world::key(&s.metadata), (*s).clone());
			}
		}
		for c in self.config_maps.state() {
			// CA certificates (frontend client certificate validation, BackendTLSPolicy)
			if c.data.as_ref().is_some_and(|d| d.contains_key("ca.crt")) {
				w.config_maps.insert(crate::render::world::key(&c.metadata), (*c).clone());
			}
		}
		for i in self.ingresses.iter().flat_map(|s| s.state()) {
			w.migration.ingresses.push((*i).clone());
		}
		for n in self.namespaces.state() {
			w.namespaces.insert(n.metadata.name.clone().unwrap_or_default(), n.metadata.labels.clone().unwrap_or_default());
		}
		for (d, ar) in self.dynamic.read().unwrap().iter() {
			for o in d.store.state() {
				let mut v = match serde_json::to_value(&*o) {
					Ok(v) => v,
					Err(_) => continue,
				};
				// the dynamic object's type information comes from the store
				v["apiVersion"] = ar.api_version.clone().into();
				v["kind"] = ar.kind.clone().into();
				if let Err(e) = w.insert(v) {
					warn!(kind = d.kind, error = %e, "skipped an object that does not have the expected shape");
				}
			}
		}
		w
	}
}

/// rproxy pods: managed ones next to their Gateways (any namespace), the fleet in the controller's.
fn spawn_pod_watch(api: Api<Pod>, changed: Arc<Notify>) -> Store<Pod> {
	let (reader, writer) = reflector::store();
	let cfg = watcher::Config::default().labels("app.kubernetes.io/name=rproxy");
	let stream = reflector::reflector(writer, watcher(api, cfg).default_backoff());
	tokio::spawn(async move {
		let mut stream = std::pin::pin!(stream);
		while let Some(ev) = stream.next().await {
			match ev {
				Ok(_) => changed.notify_one(),
				Err(e) => warn!(kind = "Pod", error = %e, "watch error"),
			}
		}
	});
	reader
}

/// (group, kind, resource at the preferred version) of the groups the controller reads.
async fn discover(client: &kube::Client) -> anyhow::Result<Vec<(String, String, ApiResource)>> {
	let mut out = vec![];
	let mut groups: Vec<&str> = DYNAMIC_KINDS.iter().map(|(g, _)| *g).collect();
	groups.extend(["traefik.io", "traefik.containo.us"]);
	groups.sort();
	groups.dedup();
	for g in groups {
		match kube::discovery::group(client, g).await {
			Ok(api_group) => {
				for (ar, _caps) in api_group.recommended_resources() {
					out.push((g.to_string(), ar.kind.clone(), ar));
				}
			}
			Err(e) => tracing::debug!(group = g, error = %e, "API group not served"),
		}
	}
	Ok(out)
}
