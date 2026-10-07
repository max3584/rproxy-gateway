//! Watches (reflectors) of everything rendering reads, and snapshots of them.
//!
//! Third-party kinds (Gateway API, rproxy's CRDs, Traefik) are watched as dynamic
//! objects at the version the API server serves (found by discovery); kinds
//! whose CRDs are not installed are skipped (restart the controller after
//! installing them).

use std::sync::Arc;

use futures::StreamExt;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
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
	store: Store<DynamicObject>,
}

pub struct Cache {
	pub changed: Arc<Notify>,
	services: Store<Service>,
	slices: Store<EndpointSlice>,
	secrets: Store<Secret>,
	namespaces: Store<Namespace>,
	pub pods: Store<Pod>,
	dynamic: Vec<DynKind>,
	/// Resources found by discovery, by kind (for status patches).
	pub resources: Vec<(&'static str, ApiResource)>,
}

/// The kinds watched dynamically: (group, kind).
pub const DYNAMIC_KINDS: &[(&str, &str)] = &[
	(crate::k8s::gateway::GROUP, "GatewayClass"),
	(crate::k8s::gateway::GROUP, "Gateway"),
	(crate::k8s::gateway::GROUP, "HTTPRoute"),
	(crate::k8s::gateway::GROUP, "TLSRoute"),
	(crate::k8s::gateway::GROUP, "TCPRoute"),
	(crate::k8s::gateway::GROUP, "UDPRoute"),
	(crate::k8s::gateway::GROUP, "ReferenceGrant"),
	(crate::k8s::crd::GROUP, "RproxyMiddleware"),
	(crate::k8s::crd::GROUP, "RproxyPolicy"),
	(crate::k8s::crd::GROUP, "RproxyRule"),
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

fn spawn_dyn_watch(client: &kube::Client, ar: &ApiResource, changed: Arc<Notify>) -> Store<DynamicObject> {
	let api: Api<DynamicObject> = Api::all_with(client.clone(), ar);
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
	/// Starts the watches; `own_ns` is where the controller's rproxy pods run.
	pub async fn start(client: &kube::Client, own_ns: &str, extra_kinds: &[(&'static str, &'static str)]) -> anyhow::Result<Cache> {
		let changed = Arc::new(Notify::new());
		let mut dynamic = vec![];
		let mut resources = vec![];
		let groups = discover(client).await?;
		for (group, kind) in DYNAMIC_KINDS.iter().chain(extra_kinds) {
			match groups.iter().find(|(g, k, _)| g == group && k == kind) {
				Some((_, _, ar)) => {
					info!(kind, version = %ar.version, "watching");
					let store = spawn_dyn_watch(client, ar, changed.clone());
					dynamic.push(DynKind { kind, store });
					resources.push((*kind, ar.clone()));
				}
				None => warn!(group, kind, "not served by the API server (CRD not installed); not watched"),
			}
		}
		let cache = Cache {
			services: spawn_watch(Api::all(client.clone()), changed.clone(), "Service"),
			slices: spawn_watch(Api::all(client.clone()), changed.clone(), "EndpointSlice"),
			secrets: spawn_watch(Api::all(client.clone()), changed.clone(), "Secret"),
			namespaces: spawn_watch(Api::all(client.clone()), changed.clone(), "Namespace"),
			pods: spawn_pod_watch(client, own_ns, changed.clone()),
			dynamic,
			resources,
			changed,
		};
		Ok(cache)
	}

	/// Waits until every watch has listed once.
	pub async fn ready(&self) {
		let _ = self.services.wait_until_ready().await;
		let _ = self.slices.wait_until_ready().await;
		let _ = self.secrets.wait_until_ready().await;
		let _ = self.namespaces.wait_until_ready().await;
		let _ = self.pods.wait_until_ready().await;
		for d in &self.dynamic {
			let _ = d.store.wait_until_ready().await;
		}
	}

	pub fn resource(&self, kind: &str) -> Option<&ApiResource> {
		self.resources.iter().find(|(k, _)| *k == kind).map(|(_, ar)| ar)
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
			if s.data.as_ref().is_some_and(|d| d.contains_key("tls.crt") || d.contains_key("ca.crt")) {
				w.secrets.insert(crate::render::world::key(&s.metadata), (*s).clone());
			}
		}
		for n in self.namespaces.state() {
			w.namespaces.insert(n.metadata.name.clone().unwrap_or_default(), n.metadata.labels.clone().unwrap_or_default());
		}
		for d in &self.dynamic {
			for o in d.store.state() {
				let mut v = match serde_json::to_value(&*o) {
					Ok(v) => v,
					Err(_) => continue,
				};
				// the dynamic object's type information comes from the store
				let ar = self.resource(d.kind).expect("watched kinds have a resource");
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

fn spawn_pod_watch(client: &kube::Client, ns: &str, changed: Arc<Notify>) -> Store<Pod> {
	let api: Api<Pod> = Api::namespaced(client.clone(), ns);
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
			Err(e) => info!(group = g, error = %e, "API group not served"),
		}
	}
	Ok(out)
}
