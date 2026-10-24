//! Watches (reflectors) of everything rendering reads, and snapshots of them.
//!
//! Third-party kinds (Gateway API, rproxy's CRDs, Traefik) are watched as dynamic
//! objects at the version the API server serves (found by discovery). Kinds
//! whose CRDs are not installed are skipped and looked for again now and then
//! (`rediscover`): a CRD installed later is watched without a restart.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, RwLock};

use futures::StreamExt;
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Pod, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::api::networking::v1::Ingress;
use kube::api::{Api, DynamicObject, PartialObjectMeta};
use kube::discovery::ApiResource;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use serde::de::DeserializeOwned;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::render::world::{Key, World};

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

/// Secrets and ConfigMaps: watched as metadata only (a cluster may hold many large ones, #92); the
/// ones rendering refers to are read in full (`Cache::load`) and kept while they are referred to.
struct Bodies<K: kube::Resource + 'static>
where
	K::DynamicType: Eq + std::hash::Hash + Clone,
{
	meta: Multi<PartialObjectMeta<K>>,
	/// key → (resourceVersion read at, the object)
	full: Mutex<BTreeMap<Key, (String, Arc<K>)>>,
}

/// How many Secrets / ConfigMaps are read at once.
const LOAD_PARALLEL: usize = 8;

impl<K> Bodies<K>
where
	K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope> + Clone + DeserializeOwned + std::fmt::Debug + Send + Sync + 'static,
	K::DynamicType: Default + Eq + std::hash::Hash + Clone,
{
	fn watch(client: &kube::Client, scope: &Scope, changed: &Arc<Notify>, what: &'static str) -> Self {
		// an `Api` of `PartialObjectMeta` lists and watches with the metadata-only Accept header
		// (`as=PartialObjectMetadataList` / `PartialObjectMetadata`): the API server sends no
		// `data` (kube 4.2; its deprecated `metadata_watcher` is this same watcher; see the test)
		let meta =
			Multi(apis::<PartialObjectMeta<K>>(client, scope).into_iter().map(|api| spawn_watch(api, changed.clone(), what)).collect());
		Bodies { meta, full: Mutex::new(BTreeMap::new()) }
	}

	/// key → resourceVersion of what exists now.
	fn versions(&self) -> HashMap<Key, String> {
		self.meta
			.state()
			.iter()
			.map(|m| (crate::render::world::key(&m.metadata), m.metadata.resource_version.clone().unwrap_or_default()))
			.collect()
	}

	/// The objects read in full that still exist.
	fn current(&self) -> Vec<(Key, Arc<K>)> {
		let now = self.versions();
		self.full.lock().unwrap().iter().filter(|(k, _)| now.contains_key(*k)).map(|(k, (_, o))| (k.clone(), o.clone())).collect()
	}

	/// Reads the `wanted` objects that are not read yet or have changed, and forgets the others.
	/// Whether anything new was read.
	async fn load(&self, client: &kube::Client, wanted: &BTreeSet<Key>, what: &'static str) -> bool {
		let now = self.versions();
		let todo: Vec<(Key, String)> = {
			let mut full = self.full.lock().unwrap();
			full.retain(|k, _| wanted.contains(k) && now.contains_key(k));
			wanted
				.iter()
				.filter_map(|k| now.get(k).map(|rv| (k, rv)))
				.filter(|(k, rv)| full.get(*k).is_none_or(|(have, _)| have != *rv))
				.map(|(k, rv)| (k.clone(), rv.clone()))
				.collect()
		};
		if todo.is_empty() {
			return false;
		}
		let read = futures::stream::iter(todo.into_iter().map(|(k, rv)| async move {
			let api: Api<K> = Api::namespaced(client.clone(), &k.0);
			(api.get_opt(&k.1).await, k, rv)
		}))
		.buffer_unordered(LOAD_PARALLEL)
		.collect::<Vec<_>>()
		.await;
		let mut full = self.full.lock().unwrap();
		let mut any = false;
		for (got, k, rv) in read {
			match got {
				// kept with the version the watch saw: read again once the watch sees a newer one
				Ok(Some(o)) => {
					full.insert(k, (rv, Arc::new(o)));
					any = true;
				}
				Ok(None) => {
					full.remove(&k);
				}
				Err(e) => warn!(kind = what, object = format!("{}/{}", k.0, k.1), error = %e, "cannot read"),
			}
		}
		any
	}
}

pub struct Cache {
	pub changed: Arc<Notify>,
	services: Multi<Service>,
	slices: Multi<EndpointSlice>,
	secrets: Bodies<Secret>,
	config_maps: Bodies<ConfigMap>,
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
			secrets: Bodies::watch(client, &scope, &changed, "Secret"),
			config_maps: Bodies::watch(client, &scope, &changed, "ConfigMap"),
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
		let _ = self.secrets.meta.wait_until_ready().await;
		let _ = self.config_maps.meta.wait_until_ready().await;
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

	/// Reads in full the Secrets and ConfigMaps rendering refers to (and forgets the others);
	/// whether anything new was read (then take the snapshot again).
	pub async fn load(&self, client: &kube::Client, secrets: &BTreeSet<Key>, config_maps: &BTreeSet<Key>) -> bool {
		let a = self.secrets.load(client, secrets, "Secret").await;
		let b = self.config_maps.load(client, config_maps, "ConfigMap").await;
		a || b
	}

	/// Everything as it is now (of Secrets and ConfigMaps, the ones read with `load`).
	pub fn snapshot(&self) -> World {
		let mut w = World::default();
		for s in self.services.state() {
			w.services.insert(crate::render::world::key(&s.metadata), (*s).clone());
		}
		for s in self.slices.state() {
			w.add_slice((*s).clone());
		}
		for (k, s) in self.secrets.current() {
			// only Secrets that can be certificates (or users files)
			if s.data.as_ref().is_some_and(|d| ["tls.crt", "ca.crt", "tls.ca", "users"].iter().any(|k| d.contains_key(*k))) {
				w.secrets.insert(k, (*s).clone());
			}
		}
		for (k, c) in self.config_maps.current() {
			// CA certificates (frontend client certificate validation, BackendTLSPolicy)
			if c.data.as_ref().is_some_and(|d| d.contains_key("ca.crt")) {
				w.config_maps.insert(k, (*c).clone());
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

/// (group, kind, resource) of the groups the controller reads, each kind at its
/// most stable served version: not the group's preferred version only, since
/// older Gateway API CRDs serve some kinds only at older versions (v1.4/v1.5:
/// TCPRoute and UDPRoute v1alpha2, v1.4: TLSRoute v1alpha3, ReferenceGrant
/// v1beta1). The objects are read and their status written at that version.
async fn discover(client: &kube::Client) -> anyhow::Result<Vec<(String, String, ApiResource)>> {
	let mut out = vec![];
	let mut groups: Vec<&str> = DYNAMIC_KINDS.iter().map(|(g, _)| *g).collect();
	groups.extend(["traefik.io", "traefik.containo.us"]);
	groups.sort();
	groups.dedup();
	for g in groups {
		match kube::discovery::group(client, g).await {
			Ok(api_group) => {
				for (ar, _caps) in api_group.resources_by_stability() {
					out.push((g.to_string(), ar.kind.clone(), ar));
				}
			}
			Err(e) => tracing::debug!(group = g, error = %e, "API group not served"),
		}
	}
	Ok(out)
}

#[cfg(test)]
mod tests {
	use k8s_openapi::api::core::v1::{ConfigMap, Secret};
	use kube::Resource;
	use kube::api::PartialObjectMeta;

	/// Secrets and ConfigMaps are listed and watched as metadata only (#92): kube sends the
	/// metadata-only Accept header for an `Api` of `PartialObjectMeta`. A kube upgrade that changes
	/// this would hold every Secret's and ConfigMap's body again.
	#[test]
	fn secrets_and_config_maps_are_watched_as_metadata() {
		assert!(PartialObjectMeta::<Secret>::metadata_api());
		assert!(PartialObjectMeta::<ConfigMap>::metadata_api());
		assert!(!Secret::metadata_api());
	}
}
