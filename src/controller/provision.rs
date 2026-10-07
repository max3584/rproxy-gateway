//! Where rproxy runs for a Gateway.
//!
//! - `managed` (default): the controller creates, in its own namespace, one
//!   rproxy Deployment and Service per Gateway (the Service type is a flag:
//!   `LoadBalancer` by default). Each pod runs rproxy and `certsync` (writes the
//!   Gateway's certificate files to a shared `emptyDir`).
//! - `fleet`: rproxy pods deployed beforehand (e.g. the chart's DaemonSet with
//!   `hostNetwork: true`, rproxy-api docs/DESIGN-v0.4.md 3.3) serve every
//!   Gateway; the controller PUTs each Gateway's set to every pod.
//!
//! Certificate files reach rproxy as a Secret the controller writes and the
//! kubelet mounts into the pod (`rproxy-<id>-certs` per Gateway; in fleet mode
//! one `rproxy-fleet-certs` with every Gateway's files). rproxy pods have no
//! ServiceAccount token and no RBAC: they cannot read any Secret through the
//! API, only what is mounted. When the Secret changes, the controller sets an
//! annotation on the pods, which makes the kubelet update the mounted files at
//! once instead of at its next periodic sync.

use std::collections::BTreeMap;
use std::time::Instant;

use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
	Capabilities, Container, ContainerPort, EnvVar, HTTPGetAction, KeyToPath, Pod, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
	Secret, SecretVolumeSource, SecurityContext, Service, ServicePort, ServiceSpec, Sysctl, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use tracing::info;

use crate::controller::bootstrap::{API_TLS_SECRET, TOKEN_SECRET};
use crate::render::GatewayPlan;

pub const MANAGER: &str = "rproxy-gateway";
pub const LABEL_GATEWAY: &str = "rproxy.max3584.net/gateway";
pub const LABEL_CERTS_FOR: &str = "rproxy.max3584.net/certs-for";
pub const ANNOTATION_GATEWAY: &str = "rproxy.max3584.net/gateway";
/// On rproxy pods: the hash of the certificate Secret's content they should have mounted.
pub const ANNOTATION_CERTS: &str = "rproxy.max3584.net/certs";
/// The certificate Secret of fleet pods (every Gateway's files).
pub const FLEET_CERTS_SECRET: &str = "rproxy-fleet-certs";
/// The control API port of rproxy pods.
pub const API_PORT: u16 = 9443;
/// certsync's port (`GET /files`).
pub const CERTSYNC_PORT: u16 = 9444;
pub const CERT_DIR: &str = "/var/run/rproxy-gateway/certs";

/// How rproxy is deployed.
#[derive(Clone, Debug)]
pub enum Mode {
	Managed(Managed),
	Fleet(Fleet),
}

#[derive(Clone, Debug)]
pub struct Managed {
	pub rproxy_image: String,
	pub controller_image: String,
	pub replicas: i32,
	pub service_type: String,
	pub service_account: String,
	pub pull_policy: String,
}

#[derive(Clone, Debug)]
pub struct Fleet {
	/// Label selector of the rproxy pods (in the controller's namespace).
	pub selector: String,
	/// Addresses written to Gateway status (else the pods' host IPs).
	pub addresses: Vec<String>,
}

/// A short, DNS-safe id of a Gateway: `<namespace>-<name>` (cut) and a hash.
pub fn gateway_id(ns: &str, name: &str) -> String {
	let mut base: String =
		format!("{ns}-{name}").chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
	base.truncate(40);
	let base = base.trim_matches('-').to_string();
	format!("{base}-{}", &crate::pem::short_hash(format!("{ns}/{name}").as_bytes())[..6])
}

pub fn labels(id: &str) -> BTreeMap<String, String> {
	[
		("app.kubernetes.io/name".to_string(), "rproxy".to_string()),
		("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()),
		(LABEL_GATEWAY.to_string(), id.to_string()),
	]
	.into()
}

fn meta(name: &str, ns: &str, labels: BTreeMap<String, String>, gw: &str) -> ObjectMeta {
	ObjectMeta {
		name: Some(name.into()),
		namespace: Some(ns.into()),
		labels: Some(labels),
		annotations: Some([(ANNOTATION_GATEWAY.to_string(), gw.to_string())].into()),
		..Default::default()
	}
}

/// The name of a Gateway's certificate Secret (managed mode).
pub fn certs_secret_name(id: &str) -> String {
	format!("rproxy-{id}-certs")
}

/// A certificate Secret: `certs_for` is the Gateway's id or `fleet`.
pub fn certs_secret(name: &str, ns: &str, certs_for: &str, gw: Option<&str>, data: &BTreeMap<String, Vec<u8>>) -> Secret {
	let mut l: BTreeMap<String, String> = [("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string())].into();
	l.insert(LABEL_CERTS_FOR.into(), certs_for.into());
	let mut metadata = ObjectMeta { name: Some(name.into()), namespace: Some(ns.into()), labels: Some(l), ..Default::default() };
	if let Some(gw) = gw {
		// garbage collection finds a Gateway's objects by this label
		metadata.labels.get_or_insert_default().insert(LABEL_GATEWAY.into(), certs_for.into());
		metadata.annotations = Some([(ANNOTATION_GATEWAY.to_string(), gw.to_string())].into());
	}
	Secret {
		metadata,
		type_: Some("Opaque".into()),
		data: Some(data.iter().map(|(k, v)| (k.clone(), ByteString(v.clone()))).collect()),
		..Default::default()
	}
}

/// What a certificate Secret holds: the files wanted now, and files it held that
/// no rule wants any more for up to `certsync::LINGER` (`absent` remembers since when).
pub fn with_linger(
	wanted: &BTreeMap<String, Vec<u8>>,
	current: &BTreeMap<String, Vec<u8>>,
	absent: &mut BTreeMap<String, Instant>,
	now: Instant,
) -> BTreeMap<String, Vec<u8>> {
	let mut out = wanted.clone();
	absent.retain(|name, _| !wanted.contains_key(name) && current.contains_key(name));
	for (name, content) in current {
		if wanted.contains_key(name) {
			continue;
		}
		let since = *absent.entry(name.clone()).or_insert(now);
		if now.duration_since(since) < crate::certsync::LINGER {
			out.insert(name.clone(), content.clone());
		} else {
			absent.remove(name);
		}
	}
	out
}

/// A short hash of a Secret's content (the pods' annotation).
pub fn content_hash(data: &BTreeMap<String, Vec<u8>>) -> String {
	let mut all = vec![];
	for (k, v) in data {
		all.extend_from_slice(k.as_bytes());
		all.push(0);
		all.extend_from_slice(crate::pem::sha256_hex(v).as_bytes());
		all.push(0);
	}
	crate::pem::short_hash(&all)
}

fn mount(name: &str, path: &str, ro: bool) -> VolumeMount {
	VolumeMount { name: name.into(), mount_path: path.into(), read_only: Some(ro), ..Default::default() }
}

fn env(name: &str, value: &str) -> EnvVar {
	EnvVar { name: name.into(), value: Some(value.into()), ..Default::default() }
}

/// The containers of an rproxy pod: rproxy and certsync (shared by the managed
/// Deployment; the chart's fleet DaemonSet is written the same way).
pub fn deployment(plan: &GatewayPlan, ns: &str, m: &Managed, infra_labels: &BTreeMap<String, String>) -> Deployment {
	let id = gateway_id(&plan.namespace, &plan.name);
	let name = format!("rproxy-{id}");
	let mut pod_labels = infra_labels.clone();
	pod_labels.extend(labels(&id));
	let restricted = SecurityContext {
		allow_privilege_escalation: Some(false),
		read_only_root_filesystem: Some(true),
		capabilities: Some(Capabilities { drop: Some(vec!["ALL".into()]), ..Default::default() }),
		..Default::default()
	};
	let probe = |path: &str| Probe {
		http_get: Some(HTTPGetAction {
			path: Some(path.into()),
			port: IntOrString::Int(API_PORT.into()),
			scheme: Some("HTTPS".into()),
			..Default::default()
		}),
		period_seconds: Some(5),
		..Default::default()
	};
	let rproxy = Container {
		name: "rproxy".into(),
		image: Some(m.rproxy_image.clone()),
		image_pull_policy: Some(m.pull_policy.clone()),
		env: Some(vec![
			env("RPROXY_API_ADDR", "0.0.0.0"),
			env("RPROXY_API_PORT", &API_PORT.to_string()),
			env("RPROXY_TOKEN_FILE", "/etc/rproxy-gateway/token/tokens.yaml"),
			env("RPROXY_TLS_CERT", "/etc/rproxy-gateway/api-tls/tls.crt"),
			env("RPROXY_TLS_KEY", "/etc/rproxy-gateway/api-tls/tls.key"),
		]),
		ports: Some(vec![ContainerPort { name: Some("api".into()), container_port: API_PORT.into(), ..Default::default() }]),
		liveness_probe: Some(probe("/healthz")),
		readiness_probe: Some(probe("/healthz")),
		volume_mounts: Some(vec![
			mount("token", "/etc/rproxy-gateway/token", true),
			mount("api-tls", "/etc/rproxy-gateway/api-tls", true),
			mount("certs", CERT_DIR, true),
		]),
		security_context: Some(restricted.clone()),
		..Default::default()
	};
	let certsync = Container {
		name: "certsync".into(),
		image: Some(m.controller_image.clone()),
		image_pull_policy: Some(m.pull_policy.clone()),
		args: Some(vec!["certsync".into(), "--dir".into(), CERT_DIR.into(), "--listen".into(), format!("0.0.0.0:{CERTSYNC_PORT}")]),
		ports: Some(vec![ContainerPort { name: Some("certsync".into()), container_port: CERTSYNC_PORT.into(), ..Default::default() }]),
		volume_mounts: Some(vec![mount("certs", CERT_DIR, true)]),
		security_context: Some(restricted),
		..Default::default()
	};
	let secret_vol = |vol: &str, secret: &str, items: Option<Vec<KeyToPath>>| Volume {
		name: vol.into(),
		secret: Some(SecretVolumeSource { secret_name: Some(secret.into()), items, ..Default::default() }),
		..Default::default()
	};
	Deployment {
		metadata: meta(&name, ns, labels(&id), &format!("{}/{}", plan.namespace, plan.name)),
		spec: Some(DeploymentSpec {
			replicas: Some(m.replicas),
			selector: LabelSelector { match_labels: Some(labels(&id)), ..Default::default() },
			strategy: Some(DeploymentStrategy { type_: Some("RollingUpdate".into()), ..Default::default() }),
			template: PodTemplateSpec {
				metadata: Some(ObjectMeta { labels: Some(pod_labels), ..Default::default() }),
				spec: Some(PodSpec {
					service_account_name: Some(m.service_account.clone()),
					// rproxy and certsync do not use the Kubernetes API
					automount_service_account_token: Some(false),
					security_context: Some(PodSecurityContext {
						run_as_non_root: Some(true),
						run_as_user: Some(65532),
						run_as_group: Some(65532),
						fs_group: Some(65532),
						// listen on ports below 1024 without root (a namespaced, safe sysctl)
						sysctls: Some(vec![Sysctl { name: "net.ipv4.ip_unprivileged_port_start".into(), value: "0".into() }]),
						..Default::default()
					}),
					containers: vec![rproxy, certsync],
					volumes: Some(vec![
						secret_vol(
							"token",
							TOKEN_SECRET,
							Some(vec![KeyToPath { key: "tokens.yaml".into(), path: "tokens.yaml".into(), ..Default::default() }]),
						),
						secret_vol("api-tls", API_TLS_SECRET, None),
						Volume {
							name: "certs".into(),
							secret: Some(SecretVolumeSource {
								secret_name: Some(certs_secret_name(&id)),
								// readable by the pod's group (fsGroup), nobody else
								default_mode: Some(0o440),
								// the pod starts before the controller has written it
								optional: Some(true),
								..Default::default()
							}),
							..Default::default()
						},
					]),
					..Default::default()
				}),
			},
			..Default::default()
		}),
		..Default::default()
	}
}

pub fn service(plan: &GatewayPlan, ns: &str, m: &Managed, annotations: &BTreeMap<String, String>) -> Service {
	let id = gateway_id(&plan.namespace, &plan.name);
	let mut metadata = meta(&format!("rproxy-{id}"), ns, labels(&id), &format!("{}/{}", plan.namespace, plan.name));
	metadata.annotations.get_or_insert_default().extend(annotations.clone());
	let ports = plan
		.ports()
		.into_iter()
		.map(|(proto, port)| ServicePort {
			name: Some(format!("{}-{port}", proto.as_str())),
			port: port.into(),
			target_port: Some(IntOrString::Int(port.into())),
			protocol: Some(proto.k8s().into()),
			..Default::default()
		})
		.collect();
	Service {
		metadata,
		spec: Some(ServiceSpec {
			type_: Some(m.service_type.clone()),
			selector: Some(labels(&id)),
			ports: Some(ports),
			external_traffic_policy: (m.service_type == "LoadBalancer").then(|| "Local".into()),
			..Default::default()
		}),
		..Default::default()
	}
}

fn pp() -> PatchParams {
	PatchParams::apply(MANAGER).force()
}

/// Writes a certificate Secret (`wanted` and the files still lingering) when its
/// content changed; returns the hash of what it holds.
pub async fn apply_certs(
	client: &kube::Client,
	ns: &str,
	name: &str,
	certs_for: &str,
	gw: Option<&str>,
	wanted: &BTreeMap<String, Vec<u8>>,
	absent: &mut BTreeMap<String, Instant>,
) -> anyhow::Result<String> {
	let api = Api::<Secret>::namespaced(client.clone(), ns);
	let current = api.get_opt(name).await?;
	let current_data: BTreeMap<String, Vec<u8>> =
		current.as_ref().and_then(|s| s.data.clone()).unwrap_or_default().into_iter().map(|(k, v)| (k, v.0)).collect();
	let data = with_linger(wanted, &current_data, absent, Instant::now());
	let s = certs_secret(name, ns, certs_for, gw, &data);
	let same_labels = current.as_ref().is_some_and(|c| c.metadata.labels == s.metadata.labels);
	if current.is_none() || current_data != data || !same_labels {
		api.patch(name, &pp(), &Patch::Apply(&s)).await?;
	}
	Ok(content_hash(&data))
}

/// Sets `ANNOTATION_CERTS` on pods that do not have `hash` yet: the kubelet then
/// updates their mounted Secret at once.
pub async fn touch_pods(client: &kube::Client, ns: &str, pods: &[Endpoint], hash: &str) {
	let api = Api::<Pod>::namespaced(client.clone(), ns);
	for p in pods.iter().filter(|p| p.certs.as_deref() != Some(hash)) {
		let patch = serde_json::json!({"metadata": {"annotations": {ANNOTATION_CERTS: hash}}});
		if let Err(e) = api.patch(&p.pod, &PatchParams::default(), &Patch::Merge(&patch)).await {
			tracing::debug!(pod = p.pod, error = %e, "cannot annotate the pod (its certificate files update at the kubelet's next sync)");
		}
	}
}

/// Applies a Gateway's certificate Secret, Deployment and Service; returns the
/// Service (with its status) and the hash of the certificate Secret.
pub async fn apply_managed(
	client: &kube::Client,
	ns: &str,
	plan: &GatewayPlan,
	m: &Managed,
	infra: (&BTreeMap<String, String>, &BTreeMap<String, String>),
	absent: &mut BTreeMap<String, Instant>,
) -> anyhow::Result<(Option<Service>, String)> {
	let id = gateway_id(&plan.namespace, &plan.name);
	let gw = format!("{}/{}", plan.namespace, plan.name);
	let hash = apply_certs(client, ns, &certs_secret_name(&id), &id, Some(&gw), &plan.files, absent).await?;
	let d = deployment(plan, ns, m, infra.0);
	let name = d.metadata.name.clone().unwrap_or_default();
	Api::<Deployment>::namespaced(client.clone(), ns).patch(&name, &pp(), &Patch::Apply(&d)).await?;
	let svc_api = Api::<Service>::namespaced(client.clone(), ns);
	if plan.ports().is_empty() {
		// a Service needs a port; none until a listener is valid
		let _ = svc_api.delete(&name, &DeleteParams::default()).await;
		return Ok((None, hash));
	}
	let s = service(plan, ns, m, infra.1);
	Ok((Some(svc_api.patch(&name, &pp(), &Patch::Apply(&s)).await?), hash))
}

/// Deletes managed objects of Gateways that are gone (`keep`: ids still in use).
pub async fn collect_garbage(client: &kube::Client, ns: &str, keep: &[String]) -> anyhow::Result<()> {
	let lp = ListParams::default().labels(&format!("app.kubernetes.io/managed-by={MANAGER},{LABEL_GATEWAY}"));
	let gone =
		|labels: &Option<BTreeMap<String, String>>| labels.as_ref().and_then(|l| l.get(LABEL_GATEWAY)).is_some_and(|id| !keep.contains(id));
	let dp = DeleteParams::default();
	let deployments: Api<Deployment> = Api::namespaced(client.clone(), ns);
	for d in deployments.list(&lp).await? {
		if gone(&d.metadata.labels) {
			info!(deployment = d.metadata.name.as_deref().unwrap_or(""), "deleting rproxy of a Gateway that is gone");
			deployments.delete(d.metadata.name.as_deref().unwrap_or(""), &dp).await?;
		}
	}
	let services: Api<Service> = Api::namespaced(client.clone(), ns);
	for s in services.list(&lp).await? {
		if gone(&s.metadata.labels) {
			services.delete(s.metadata.name.as_deref().unwrap_or(""), &dp).await?;
		}
	}
	let secrets: Api<Secret> = Api::namespaced(client.clone(), ns);
	for s in secrets.list(&lp).await? {
		if gone(&s.metadata.labels) {
			secrets.delete(s.metadata.name.as_deref().unwrap_or(""), &dp).await?;
		}
	}
	Ok(())
}

/// Gateway status addresses from the Service: load balancer addresses, else the ClusterIP.
pub fn service_addresses(svc: &Service) -> Vec<(String, String)> {
	let mut out = vec![];
	for i in svc.status.as_ref().and_then(|s| s.load_balancer.as_ref()).and_then(|l| l.ingress.as_ref()).into_iter().flatten() {
		if let Some(ip) = &i.ip {
			out.push(("IPAddress".to_string(), ip.clone()));
		} else if let Some(h) = &i.hostname {
			out.push(("Hostname".to_string(), h.clone()));
		}
	}
	let spec = svc.spec.clone().unwrap_or_default();
	if out.is_empty() && spec.type_.as_deref() != Some("LoadBalancer") {
		for ip in spec.cluster_ips.unwrap_or_default().into_iter().chain(spec.cluster_ip) {
			if ip != "None" && !out.iter().any(|(_, v)| v == &ip) {
				out.push(("IPAddress".to_string(), ip));
			}
		}
	}
	out
}

/// A running rproxy pod: its name, uid and IP.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Endpoint {
	pub pod: String,
	pub uid: String,
	pub ip: String,
	pub host_ip: Option<String>,
	/// rproxy's control API port.
	pub api_port: u16,
	/// certsync's port.
	pub certsync_port: u16,
	/// The pod's `ANNOTATION_CERTS`.
	pub certs: Option<String>,
}

/// Running pods matching `labels` (`key=value,...`).
pub fn pods(store: &[std::sync::Arc<Pod>], selector: &BTreeMap<String, String>) -> Vec<Endpoint> {
	let mut out: Vec<Endpoint> = store
		.iter()
		.filter(|p| {
			let l = p.metadata.labels.clone().unwrap_or_default();
			selector.iter().all(|(k, v)| l.get(k) == Some(v))
		})
		.filter(|p| p.metadata.deletion_timestamp.is_none())
		.filter_map(|p| {
			let st = p.status.as_ref()?;
			if st.phase.as_deref() != Some("Running") {
				return None;
			}
			Some(Endpoint {
				pod: p.metadata.name.clone().unwrap_or_default(),
				uid: p.metadata.uid.clone().unwrap_or_default(),
				ip: st.pod_ip.clone()?,
				host_ip: st.host_ip.clone(),
				api_port: API_PORT,
				certsync_port: CERTSYNC_PORT,
				certs: p.metadata.annotations.as_ref().and_then(|a| a.get(ANNOTATION_CERTS)).cloned(),
			})
		})
		.collect();
	out.sort_by(|a, b| a.pod.cmp(&b.pod));
	out
}

/// `a=b,c=d` → labels.
pub fn parse_selector(s: &str) -> BTreeMap<String, String> {
	s.split(',').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::rproxy::model::Protocol;

	fn plan() -> GatewayPlan {
		GatewayPlan {
			namespace: "default".into(),
			name: "web".into(),
			rules: vec![crate::rproxy::model::Rule { protocol: Protocol::Tcp, listen_port: 443, ..Default::default() }],
			files: [("abc.crt".to_string(), b"x".to_vec())].into(),
			..Default::default()
		}
	}

	#[test]
	fn ids_are_short_and_stable() {
		let id = gateway_id("default", "web");
		assert!(id.starts_with("default-web-"), "{id}");
		assert_eq!(id, gateway_id("default", "web"));
		assert_ne!(id, gateway_id("default-web", ""));
		assert!(gateway_id(&"n".repeat(63), &"g".repeat(253)).len() <= 47);
	}

	#[test]
	fn objects() {
		let m = Managed {
			rproxy_image: "rproxy:1".into(),
			controller_image: "gw:1".into(),
			replicas: 2,
			service_type: "LoadBalancer".into(),
			service_account: "rproxy".into(),
			pull_policy: "IfNotPresent".into(),
		};
		let p = plan();
		let id = gateway_id("default", "web");
		let d = deployment(&p, "rproxy-gateway-system", &m, &BTreeMap::new());
		let spec = d.spec.unwrap();
		assert_eq!(spec.replicas, Some(2));
		let pod = spec.template.spec.unwrap();
		assert_eq!(pod.containers.len(), 2);
		assert_eq!(pod.automount_service_account_token, Some(false));
		let certs = pod.volumes.unwrap().into_iter().find(|v| v.name == "certs").unwrap().secret.unwrap();
		assert_eq!(certs.secret_name, Some(certs_secret_name(&id)));
		assert_eq!(certs.default_mode, Some(0o440));
		assert!(pod.containers.iter().all(|c| c.volume_mounts.iter().flatten().all(|v| v.read_only == Some(true))));
		let s = service(&p, "rproxy-gateway-system", &m, &BTreeMap::new());
		let ports = s.spec.unwrap().ports.unwrap();
		assert_eq!((ports[0].port, ports[0].protocol.as_deref()), (443, Some("TCP")));
		let c = certs_secret(FLEET_CERTS_SECRET, "ns", "fleet", None, &p.files);
		let l = c.metadata.labels.unwrap();
		assert_eq!(l[LABEL_CERTS_FOR], "fleet");
		assert!(!l.contains_key(LABEL_GATEWAY), "the fleet Secret is not one Gateway's");
		let c = certs_secret(&certs_secret_name(&id), "ns", &id, Some("default/web"), &p.files);
		assert_eq!(c.metadata.labels.unwrap()[LABEL_GATEWAY], id);
		assert_eq!(parse_selector("a=b, c=d"), [("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())].into());
	}

	#[test]
	fn old_files_linger() {
		let t0 = Instant::now();
		let mut absent = BTreeMap::new();
		let f = |names: &[&str]| -> BTreeMap<String, Vec<u8>> { names.iter().map(|n| (n.to_string(), n.as_bytes().to_vec())).collect() };
		let out = with_linger(&f(&["b.crt"]), &f(&["a.crt"]), &mut absent, t0);
		assert_eq!(out, f(&["a.crt", "b.crt"]), "a.crt stays for a while");
		let out = with_linger(&f(&["b.crt"]), &out, &mut absent, t0 + crate::certsync::LINGER / 2);
		assert_eq!(out, f(&["a.crt", "b.crt"]));
		let out = with_linger(&f(&["b.crt"]), &out, &mut absent, t0 + crate::certsync::LINGER);
		assert_eq!(out, f(&["b.crt"]));
		assert!(absent.is_empty());
		// wanted again: no longer absent
		let mut absent = BTreeMap::new();
		with_linger(&f(&[]), &f(&["a.crt"]), &mut absent, t0);
		with_linger(&f(&["a.crt"]), &f(&["a.crt"]), &mut absent, t0);
		assert!(absent.is_empty());
		assert_ne!(content_hash(&f(&["a.crt"])), content_hash(&f(&["b.crt"])));
	}
}
