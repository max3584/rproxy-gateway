//! Where rproxy runs for a Gateway.
//!
//! - `managed` (default): the controller creates, in the Gateway's namespace,
//!   one rproxy Deployment, Service and ServiceAccount per Gateway (the Service
//!   type is a flag: `LoadBalancer` by default), owned by the Gateway. Each pod
//!   runs rproxy and `certsync`. `spec.infrastructure` labels and annotations
//!   go onto all of them.
//! - `fleet`: rproxy pods deployed beforehand (e.g. the chart's DaemonSet with
//!   `hostNetwork: true`, rproxy-api docs/DESIGN-v0.4.md 3.3) serve every
//!   Gateway; the controller PUTs each Gateway's set to every pod.
//!
//! Certificate files reach rproxy as a Secret the controller writes and the
//! kubelet mounts into the pod (`rproxy-<id>-certs` per Gateway, next to it; in
//! fleet mode one `rproxy-fleet-certs` with every Gateway's files). rproxy pods
//! have no ServiceAccount token and no RBAC: they cannot read any Secret through
//! the API, only what is mounted. When the Secret changes, the controller sets
//! an annotation on the pods, which makes the kubelet update the mounted files
//! at once instead of at its next periodic sync.
//!
//! A managed Gateway's rproxy has its own control API credentials
//! (`rproxy-<id>-api`): a certificate for `<id>.rproxy-api.rproxy-gateway.internal`
//! and a token derived from the controller's master token (`bootstrap`).

use std::collections::BTreeMap;
use std::time::Instant;

use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy};
use k8s_openapi::api::core::v1::{
	Capabilities, Container, ContainerPort, EnvVar, HTTPGetAction, KeyToPath, Pod, PodSecurityContext, PodSpec, PodTemplateSpec, Probe,
	Secret, SecretVolumeSource, SecurityContext, Service, ServiceAccount, ServicePort, ServiceSpec, Sysctl, Volume, VolumeMount,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use tracing::info;

use crate::controller::bootstrap;
use crate::render::GatewayPlan;
use crate::rproxy::client::server_name_for;

pub const MANAGER: &str = "rproxy-gateway";
pub const LABEL_GATEWAY: &str = "rproxy.max3584.net/gateway";
pub const LABEL_CERTS_FOR: &str = "rproxy.max3584.net/certs-for";
/// Gateway API's label for objects made for a Gateway (`GatewayNameLabelKey`).
pub const LABEL_GATEWAY_NAME: &str = "gateway.networking.k8s.io/gateway-name";
pub const ANNOTATION_GATEWAY: &str = "rproxy.max3584.net/gateway";
/// On rproxy pods: the hash of the certificate Secret's content they should have mounted.
pub const ANNOTATION_CERTS: &str = "rproxy.max3584.net/certs";
/// On a control API Secret: the name its certificate was issued for.
pub const ANNOTATION_SERVER_NAME: &str = "rproxy.max3584.net/server-name";
/// On rproxy pod templates: the hash of the control API Secret (rproxy reads its certificate at start).
pub const ANNOTATION_API: &str = "rproxy.max3584.net/api";
/// The certificate Secret of fleet pods (every Gateway's files).
pub const FLEET_CERTS_SECRET: &str = "rproxy-fleet-certs";
/// The control API port of rproxy pods.
pub const API_PORT: u16 = 9443;
/// certsync's port (`GET /files`).
pub const CERTSYNC_PORT: u16 = 9444;
pub const CERT_DIR: &str = "/var/run/rproxy-gateway/certs";
const API_DIR: &str = "/etc/rproxy-gateway/api";

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
	pub pull_policy: String,
	/// A NetworkPolicy per Gateway letting only the controller (in this namespace) reach
	/// rproxy's control API and certsync (`None`: none made).
	pub network_policy: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Fleet {
	/// Label selector of the rproxy pods (in the controller's namespace).
	pub selector: String,
	/// Addresses written to Gateway status (else the pods' host IPs).
	pub addresses: Vec<String>,
}

/// A short, DNS-safe id of a Gateway: `<namespace>-<name>` (cut) and a hash of
/// them and the Gateway's uid (a Gateway created again under the same name gets
/// new objects, not the old ones its predecessor's deletion is removing).
pub fn gateway_id(ns: &str, name: &str, uid: &str) -> String {
	let mut base: String =
		format!("{ns}-{name}").chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' }).collect();
	base.truncate(40);
	let base = base.trim_matches('-').to_string();
	format!("{base}-{}", &crate::pem::short_hash(format!("{ns}/{name}/{uid}").as_bytes())[..6])
}

/// The labels selecting a Gateway's rproxy pods.
pub fn labels(id: &str) -> BTreeMap<String, String> {
	[
		("app.kubernetes.io/name".to_string(), "rproxy".to_string()),
		("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()),
		(LABEL_GATEWAY.to_string(), id.to_string()),
	]
	.into()
}

pub fn certs_secret_name(id: &str) -> String {
	format!("rproxy-{id}-certs")
}

pub fn api_secret_name(id: &str) -> String {
	format!("rproxy-{id}-api")
}

/// The name of a Gateway's Deployment, Service and ServiceAccount.
pub fn object_name(id: &str) -> String {
	format!("rproxy-{id}")
}

/// One Gateway's rproxy, as the managed objects are made from it.
#[derive(Clone, Debug)]
pub struct Target<'a> {
	pub plan: &'a GatewayPlan,
	pub id: String,
	/// The Gateway (objects are deleted with it).
	pub owner: Option<OwnerReference>,
	/// `spec.infrastructure` labels and annotations.
	pub labels: BTreeMap<String, String>,
	pub annotations: BTreeMap<String, String>,
	/// `spec.addresses` (IP addresses) the Service takes (`externalIPs`).
	pub addresses: Vec<String>,
	/// Annotation prefixes let onto the Service though they steer addresses (`--service-annotation-prefix`).
	pub service_annotations: Vec<String>,
}

/// Annotation prefixes of `spec.infrastructure` kept off the Service: they ask load
/// balancers for addresses (a tenant could take an address that is not theirs).
pub const SERVICE_ANNOTATION_DENY: &[&str] = &[
	"metallb.universe.tf/",
	"metallb.io/",
	"lbipam.cilium.io/",
	"io.cilium/",
	"service.beta.kubernetes.io/",
	"service.kubernetes.io/",
	"cloud.google.com/",
	"networking.gke.io/",
	"kube-vip.io/",
	"purelb.io/",
	"load-balancer.hetzner.cloud/",
	"loadbalancer.openstack.org/",
];

/// Whether an infrastructure annotation may go onto the Service.
pub fn service_annotation_allowed(key: &str, allow: &[String]) -> bool {
	allow.iter().any(|p| key.starts_with(p.as_str())) || !SERVICE_ANNOTATION_DENY.iter().any(|p| key.starts_with(p))
}

impl<'a> Target<'a> {
	pub fn new(plan: &'a GatewayPlan) -> Target<'a> {
		Target {
			plan,
			id: gateway_id(&plan.namespace, &plan.name, &plan.uid),
			owner: None,
			labels: BTreeMap::new(),
			annotations: BTreeMap::new(),
			addresses: vec![],
			service_annotations: vec![],
		}
	}

	fn ns(&self) -> &str {
		&self.plan.namespace
	}

	/// Metadata of a managed object: the infrastructure labels and annotations,
	/// with ours on top (they select the pods), and the Gateway as owner.
	fn meta(&self, name: &str, own: BTreeMap<String, String>) -> ObjectMeta {
		let mut l = self.labels.clone();
		l.insert(LABEL_GATEWAY_NAME.into(), self.plan.name.clone());
		l.extend(own);
		let mut a = self.annotations.clone();
		a.insert(ANNOTATION_GATEWAY.into(), format!("{}/{}", self.plan.namespace, self.plan.name));
		ObjectMeta {
			name: Some(name.into()),
			namespace: Some(self.ns().into()),
			labels: Some(l),
			annotations: Some(a),
			owner_references: self.owner.clone().map(|o| vec![o]),
			..Default::default()
		}
	}

	fn secret_labels(&self) -> BTreeMap<String, String> {
		[
			("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()),
			(LABEL_GATEWAY.to_string(), self.id.clone()),
			(LABEL_CERTS_FOR.to_string(), self.id.clone()),
		]
		.into()
	}
}

/// The Gateway as the owner of what is made for it.
pub fn owner(api_version: &str, name: &str, uid: &str) -> OwnerReference {
	OwnerReference { api_version: api_version.into(), kind: "Gateway".into(), name: name.into(), uid: uid.into(), ..Default::default() }
}

/// A Secret of `data` with `meta`.
fn secret(meta: ObjectMeta, data: &BTreeMap<String, Vec<u8>>) -> Secret {
	Secret {
		metadata: meta,
		type_: Some("Opaque".into()),
		data: Some(data.iter().map(|(k, v)| (k.clone(), ByteString(v.clone()))).collect()),
		..Default::default()
	}
}

/// The fleet's certificate Secret.
pub fn fleet_certs_secret(ns: &str, data: &BTreeMap<String, Vec<u8>>) -> Secret {
	let l: BTreeMap<String, String> =
		[("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()), (LABEL_CERTS_FOR.to_string(), "fleet".to_string())].into();
	secret(ObjectMeta { name: Some(FLEET_CERTS_SECRET.into()), namespace: Some(ns.into()), labels: Some(l), ..Default::default() }, data)
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

fn data_of(s: Option<&Secret>) -> BTreeMap<String, Vec<u8>> {
	s.and_then(|s| s.data.clone()).unwrap_or_default().into_iter().map(|(k, v)| (k, v.0)).collect()
}

/// A Gateway's control API Secret: a certificate for its name issued by the CA,
/// the CA, and the token file of its derived token. The certificate in
/// `current` is kept while it is still right (same CA, same name).
pub fn api_secret(t: &Target, current: Option<&Secret>, boot: &bootstrap::Bootstrap) -> anyhow::Result<Secret> {
	let name = server_name_for(&t.id);
	let have = data_of(current);
	let issued_for = current.and_then(|s| s.metadata.annotations.as_ref()).and_then(|a| a.get(ANNOTATION_SERVER_NAME));
	let ends = current.and_then(|s| s.metadata.annotations.as_ref()).and_then(|a| a.get(bootstrap::ANNOTATION_NOT_AFTER));
	let keep = have.get("ca.crt") == Some(&boot.ca_pem)
		&& issued_for == Some(&name)
		&& !bootstrap::renew(ends.map(String::as_str))
		&& have.contains_key("tls.crt")
		&& have.contains_key("tls.key");
	let (crt, key) = if keep {
		(have["tls.crt"].clone(), have["tls.key"].clone())
	} else {
		let (c, k) = bootstrap::issue_api_cert(&boot.ca_key_pem, &name)?;
		(c.into_bytes(), k.into_bytes())
	};
	let token = bootstrap::derive_token(&boot.token, &t.id);
	let data: BTreeMap<String, Vec<u8>> = [
		("tls.crt".to_string(), crt),
		("tls.key".to_string(), key),
		("ca.crt".to_string(), boot.ca_pem.clone()),
		("tokens.yaml".to_string(), bootstrap::token_file(&token).into_bytes()),
	]
	.into();
	let mut meta = t.meta(&api_secret_name(&t.id), t.secret_labels());
	meta.labels.get_or_insert_default().remove(LABEL_CERTS_FOR);
	meta.annotations.get_or_insert_default().insert(ANNOTATION_SERVER_NAME.into(), name);
	let ends = if keep { ends.cloned().unwrap_or_default() } else { bootstrap::not_after(bootstrap::LEAF_DAYS).to_string() };
	meta.annotations.get_or_insert_default().insert(bootstrap::ANNOTATION_NOT_AFTER.into(), ends);
	Ok(secret(meta, &data))
}

fn mount(name: &str, path: &str) -> VolumeMount {
	VolumeMount { name: name.into(), mount_path: path.into(), read_only: Some(true), ..Default::default() }
}

fn env(name: &str, value: &str) -> EnvVar {
	EnvVar { name: name.into(), value: Some(value.into()), ..Default::default() }
}

fn secret_volume(vol: &str, secret: &str, items: Option<Vec<KeyToPath>>) -> Volume {
	Volume {
		name: vol.into(),
		secret: Some(SecretVolumeSource {
			secret_name: Some(secret.into()),
			items,
			// readable by the pod's group (fsGroup), nobody else
			default_mode: Some(0o440),
			// the pod may start before the controller has written it
			optional: Some(true),
		}),
		..Default::default()
	}
}

/// The ServiceAccount of a Gateway's rproxy pods: no token, no RBAC.
pub fn service_account(t: &Target) -> ServiceAccount {
	ServiceAccount {
		metadata: t.meta(&object_name(&t.id), labels(&t.id)),
		automount_service_account_token: Some(false),
		..Default::default()
	}
}

/// A Gateway's rproxy Deployment: rproxy and certsync.
/// `api_hash`: the control API Secret's content hash, on the pod template: a new certificate or token rolls the pods.
pub fn deployment(t: &Target, m: &Managed, api_hash: &str) -> Deployment {
	let name = object_name(&t.id);
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
			env("RPROXY_TOKEN_FILE", &format!("{API_DIR}/tokens.yaml")),
			env("RPROXY_TLS_CERT", &format!("{API_DIR}/tls.crt")),
			env("RPROXY_TLS_KEY", &format!("{API_DIR}/tls.key")),
		]),
		ports: Some(vec![ContainerPort { name: Some("api".into()), container_port: API_PORT.into(), ..Default::default() }]),
		liveness_probe: Some(probe("/healthz")),
		readiness_probe: Some(probe("/healthz")),
		volume_mounts: Some(vec![mount("api", API_DIR), mount("certs", CERT_DIR)]),
		security_context: Some(restricted.clone()),
		..Default::default()
	};
	let certsync = Container {
		name: "certsync".into(),
		image: Some(m.controller_image.clone()),
		image_pull_policy: Some(m.pull_policy.clone()),
		args: Some(vec!["certsync".into(), "--dir".into(), CERT_DIR.into(), "--listen".into(), format!("0.0.0.0:{CERTSYNC_PORT}")]),
		ports: Some(vec![ContainerPort { name: Some("certsync".into()), container_port: CERTSYNC_PORT.into(), ..Default::default() }]),
		// listen on the pod's IP only
		env: Some(vec![EnvVar {
			name: "POD_IP".into(),
			value_from: Some(k8s_openapi::api::core::v1::EnvVarSource {
				field_ref: Some(k8s_openapi::api::core::v1::ObjectFieldSelector {
					field_path: "status.podIP".into(),
					..Default::default()
				}),
				..Default::default()
			}),
			..Default::default()
		}]),
		volume_mounts: Some(vec![mount("certs", CERT_DIR)]),
		security_context: Some(restricted),
		..Default::default()
	};
	let item = |k: &str| KeyToPath { key: k.into(), path: k.into(), ..Default::default() };
	let mut pod_meta = t.meta(&name, labels(&t.id));
	pod_meta.annotations.get_or_insert_default().insert(ANNOTATION_API.into(), api_hash.into());
	Deployment {
		metadata: t.meta(&name, labels(&t.id)),
		spec: Some(DeploymentSpec {
			replicas: Some(m.replicas),
			selector: LabelSelector { match_labels: Some(labels(&t.id)), ..Default::default() },
			strategy: Some(DeploymentStrategy { type_: Some("RollingUpdate".into()), ..Default::default() }),
			template: PodTemplateSpec {
				metadata: Some(ObjectMeta { labels: pod_meta.labels, annotations: pod_meta.annotations, ..Default::default() }),
				spec: Some(PodSpec {
					service_account_name: Some(name.clone()),
					// rproxy and certsync do not use the Kubernetes API
					automount_service_account_token: Some(false),
					security_context: Some(PodSecurityContext {
						run_as_non_root: Some(true),
						run_as_user: Some(65532),
						run_as_group: Some(65532),
						fs_group: Some(65532),
						seccomp_profile: Some(k8s_openapi::api::core::v1::SeccompProfile {
							type_: "RuntimeDefault".into(),
							..Default::default()
						}),
						// listen on ports below 1024 without root (a namespaced, safe sysctl)
						sysctls: Some(vec![Sysctl { name: "net.ipv4.ip_unprivileged_port_start".into(), value: "0".into() }]),
						..Default::default()
					}),
					containers: vec![rproxy, certsync],
					volumes: Some(vec![
						secret_volume("api", &api_secret_name(&t.id), Some(vec![item("tokens.yaml"), item("tls.crt"), item("tls.key")])),
						secret_volume("certs", &certs_secret_name(&t.id), None),
					]),
					..Default::default()
				}),
			},
			..Default::default()
		}),
		..Default::default()
	}
}

pub fn service(t: &Target, m: &Managed) -> Service {
	let ports = t
		.plan
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
		metadata: {
			let mut meta = t.meta(&object_name(&t.id), labels(&t.id));
			if let Some(a) = meta.annotations.as_mut() {
				a.retain(|k, _| k == ANNOTATION_GATEWAY || service_annotation_allowed(k, &t.service_annotations));
			}
			meta
		},
		spec: Some(ServiceSpec {
			type_: Some(m.service_type.clone()),
			selector: Some(labels(&t.id)),
			ports: Some(ports),
			external_traffic_policy: (m.service_type == "LoadBalancer").then(|| "Local".into()),
			external_ips: (!t.addresses.is_empty()).then(|| t.addresses.clone()),
			..Default::default()
		}),
		..Default::default()
	}
}

/// Who may reach a Gateway's rproxy pods: anyone on the listener ports; on the control
/// API and certsync ports only the controller's pods (`controller_ns`, `app.kubernetes.io/name: rproxy-gateway`).
pub fn network_policy(t: &Target, controller_ns: &str) -> k8s_openapi::api::networking::v1::NetworkPolicy {
	use k8s_openapi::api::networking::v1::{
		NetworkPolicy, NetworkPolicyIngressRule, NetworkPolicyPeer, NetworkPolicyPort, NetworkPolicySpec,
	};
	let port = |p: u16, proto: &str| NetworkPolicyPort {
		port: Some(IntOrString::Int(p.into())),
		protocol: Some(proto.into()),
		..Default::default()
	};
	let controller = NetworkPolicyPeer {
		namespace_selector: Some(LabelSelector {
			match_labels: Some([("kubernetes.io/metadata.name".to_string(), controller_ns.to_string())].into()),
			..Default::default()
		}),
		pod_selector: Some(LabelSelector {
			match_labels: Some([("app.kubernetes.io/name".to_string(), "rproxy-gateway".to_string())].into()),
			..Default::default()
		}),
		..Default::default()
	};
	let mut rules = vec![NetworkPolicyIngressRule {
		from: Some(vec![controller]),
		ports: Some(vec![port(API_PORT, "TCP"), port(CERTSYNC_PORT, "TCP")]),
	}];
	let listeners: Vec<NetworkPolicyPort> = t.plan.ports().into_iter().map(|(proto, p)| port(p, proto.k8s())).collect();
	if !listeners.is_empty() {
		rules.push(NetworkPolicyIngressRule { from: None, ports: Some(listeners) });
	}
	NetworkPolicy {
		metadata: t.meta(&object_name(&t.id), labels(&t.id)),
		spec: Some(NetworkPolicySpec {
			pod_selector: Some(LabelSelector { match_labels: Some(labels(&t.id)), ..Default::default() }),
			policy_types: Some(vec!["Ingress".into()]),
			ingress: Some(rules),
			..Default::default()
		}),
	}
}

fn pp() -> PatchParams {
	PatchParams::apply(MANAGER).force()
}

/// Writes a certificate Secret (`wanted` and the files still lingering) when it
/// changed; returns the hash of what it holds. `make` builds the Secret from its data.
pub async fn apply_certs(
	client: &kube::Client,
	ns: &str,
	name: &str,
	wanted: &BTreeMap<String, Vec<u8>>,
	absent: &mut BTreeMap<String, Instant>,
	make: impl Fn(&BTreeMap<String, Vec<u8>>) -> Secret,
) -> anyhow::Result<String> {
	let api = Api::<Secret>::namespaced(client.clone(), ns);
	let current = api.get_opt(name).await?;
	let have = data_of(current.as_ref());
	let data = with_linger(wanted, &have, absent, Instant::now());
	let s = make(&data);
	if current.as_ref().is_none_or(|c| have != data || !same_meta(&c.metadata, &s.metadata)) {
		api.patch(name, &pp(), &Patch::Apply(&s)).await?;
	}
	Ok(content_hash(&data))
}

/// Whether `have` already carries `want`'s labels, annotations and owners.
fn same_meta(have: &ObjectMeta, want: &ObjectMeta) -> bool {
	let sub = |h: &Option<BTreeMap<String, String>>, w: &Option<BTreeMap<String, String>>| {
		w.iter().flatten().all(|(k, v)| h.as_ref().and_then(|h| h.get(k)) == Some(v))
	};
	sub(&have.labels, &want.labels)
		&& sub(&have.annotations, &want.annotations)
		&& want.owner_references.iter().flatten().all(|o| have.owner_references.iter().flatten().any(|h| h.uid == o.uid))
}

/// Sets `ANNOTATION_CERTS` on pods that do not have `hash` yet: the kubelet then
/// updates their mounted Secret at once.
pub async fn touch_pods(client: &kube::Client, pods: &[Endpoint], hash: &str) {
	for p in pods.iter().filter(|p| p.certs.as_deref() != Some(hash)) {
		let api = Api::<Pod>::namespaced(client.clone(), &p.namespace);
		let patch = serde_json::json!({"metadata": {"annotations": {ANNOTATION_CERTS: hash}}});
		if let Err(e) = api.patch(&p.pod, &PatchParams::default(), &Patch::Merge(&patch)).await {
			tracing::debug!(pod = p.pod, error = %e, "cannot annotate the pod (its certificate files update at the kubelet's next sync)");
		}
	}
}

/// What `apply_managed` made.
pub struct Applied {
	/// The Service (with its status), when there is a port to serve.
	pub service: Option<Service>,
	/// The hash of the certificate Secret's content.
	pub certs: String,
}

/// Applies a Gateway's Secrets, ServiceAccount, Deployment and Service.
/// `current_api`: the control API Secret as it is (from the cache).
pub async fn apply_managed(
	client: &kube::Client,
	t: &Target<'_>,
	m: &Managed,
	boot: &bootstrap::Bootstrap,
	current_api: Option<&Secret>,
	absent: &mut BTreeMap<String, Instant>,
) -> anyhow::Result<Applied> {
	let ns = t.ns();
	let certs = apply_certs(client, ns, &certs_secret_name(&t.id), &t.plan.files, absent, |data| {
		secret(t.meta(&certs_secret_name(&t.id), t.secret_labels()), data)
	})
	.await?;
	let api = api_secret(t, current_api, boot)?;
	if current_api.is_none_or(|c| data_of(Some(c)) != data_of(Some(&api)) || !same_meta(&c.metadata, &api.metadata)) {
		Api::<Secret>::namespaced(client.clone(), ns).patch(&api_secret_name(&t.id), &pp(), &Patch::Apply(&api)).await?;
	}
	let name = object_name(&t.id);
	if let Some(cns) = &m.network_policy {
		let np = network_policy(t, cns);
		Api::<k8s_openapi::api::networking::v1::NetworkPolicy>::namespaced(client.clone(), ns)
			.patch(&name, &pp(), &Patch::Apply(&np))
			.await?;
	}
	Api::<ServiceAccount>::namespaced(client.clone(), ns).patch(&name, &pp(), &Patch::Apply(&service_account(t))).await?;
	let api_hash = content_hash(&data_of(Some(&api)));
	Api::<Deployment>::namespaced(client.clone(), ns).patch(&name, &pp(), &Patch::Apply(&deployment(t, m, &api_hash))).await?;
	let svc_api = Api::<Service>::namespaced(client.clone(), ns);
	if t.plan.ports().is_empty() {
		// a Service needs a port; none until a listener is valid
		let _ = svc_api.delete(&name, &DeleteParams::default()).await;
		return Ok(Applied { service: None, certs });
	}
	let service = svc_api.patch(&name, &pp(), &Patch::Apply(&service(t, m))).await?;
	Ok(Applied { service: Some(service), certs })
}

/// Deletes managed objects of Gateways that are gone or no longer ours (`keep`:
/// ids still in use), in every namespace. (Deleting a Gateway deletes them as
/// well: the Gateway owns them.)
pub async fn collect_garbage(client: &kube::Client, keep: &[String], scope: &crate::controller::cache::Scope) -> anyhow::Result<()> {
	let nss: Vec<Option<String>> = match scope {
		None => vec![None],
		Some(n) => n.iter().cloned().map(Some).collect(),
	};
	let lp = ListParams::default().labels(&format!("app.kubernetes.io/managed-by={MANAGER},{LABEL_GATEWAY}"));
	let gone = |m: &ObjectMeta| m.labels.as_ref().and_then(|l| l.get(LABEL_GATEWAY)).is_some_and(|id| !keep.contains(id));
	let dp = DeleteParams::default();
	let ns_name = |m: &ObjectMeta| (m.namespace.clone().unwrap_or_default(), m.name.clone().unwrap_or_default());
	for d in list_in::<Deployment>(client, &nss, &lp).await? {
		if gone(&d.metadata) {
			let (ns, name) = ns_name(&d.metadata);
			info!(deployment = format!("{ns}/{name}"), "deleting rproxy of a Gateway that is gone");
			Api::<Deployment>::namespaced(client.clone(), &ns).delete(&name, &dp).await?;
		}
	}
	for s in list_in::<Service>(client, &nss, &lp).await? {
		if gone(&s.metadata) {
			let (ns, name) = ns_name(&s.metadata);
			Api::<Service>::namespaced(client.clone(), &ns).delete(&name, &dp).await?;
		}
	}
	if let Ok(list) = list_in::<k8s_openapi::api::networking::v1::NetworkPolicy>(client, &nss, &lp).await {
		for np in list {
			if gone(&np.metadata) {
				let (ns, name) = ns_name(&np.metadata);
				Api::<k8s_openapi::api::networking::v1::NetworkPolicy>::namespaced(client.clone(), &ns).delete(&name, &dp).await?;
			}
		}
	}
	for s in list_in::<ServiceAccount>(client, &nss, &lp).await? {
		if gone(&s.metadata) {
			let (ns, name) = ns_name(&s.metadata);
			Api::<ServiceAccount>::namespaced(client.clone(), &ns).delete(&name, &dp).await?;
		}
	}
	for s in list_in::<Secret>(client, &nss, &lp).await? {
		if gone(&s.metadata) {
			let (ns, name) = ns_name(&s.metadata);
			Api::<Secret>::namespaced(client.clone(), &ns).delete(&name, &dp).await?;
		}
	}
	Ok(())
}

/// Lists `K` in each namespace (`None`: all namespaces).
async fn list_in<K>(client: &kube::Client, nss: &[Option<String>], lp: &ListParams) -> anyhow::Result<Vec<K>>
where
	K: kube::Resource<Scope = k8s_openapi::NamespaceResourceScope> + Clone + serde::de::DeserializeOwned + std::fmt::Debug,
	K::DynamicType: Default,
{
	let mut out = vec![];
	for ns in nss {
		let api: Api<K> = match ns {
			Some(ns) => Api::namespaced(client.clone(), ns),
			None => Api::all(client.clone()),
		};
		out.extend(api.list(lp).await?.items);
	}
	Ok(out)
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
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Endpoint {
	pub namespace: String,
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
	/// A managed Gateway's id (its own credentials); `None` for fleet pods.
	pub target: Option<String>,
}

/// Running pods in `ns` matching `labels` (`key=value,...`).
pub fn pods(store: &[std::sync::Arc<Pod>], ns: &str, selector: &BTreeMap<String, String>) -> Vec<Endpoint> {
	let mut out: Vec<Endpoint> = store
		.iter()
		.filter(|p| p.metadata.namespace.as_deref() == Some(ns))
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
			let labels = p.metadata.labels.clone().unwrap_or_default();
			let managed = labels.get("app.kubernetes.io/managed-by").map(String::as_str) == Some(MANAGER);
			// a managed pod belongs to its Gateway's Deployment (a ReplicaSet `rproxy-<id>-...`):
			// a pod someone else made with the same labels is not talked to
			if managed {
				let id = labels.get(LABEL_GATEWAY)?;
				let prefix = format!("{}-", object_name(id));
				let owned = p
					.metadata
					.owner_references
					.iter()
					.flatten()
					.any(|o| o.kind == "ReplicaSet" && o.controller == Some(true) && o.name.starts_with(&prefix));
				if !owned {
					return None;
				}
			}
			Some(Endpoint {
				namespace: ns.to_string(),
				pod: p.metadata.name.clone().unwrap_or_default(),
				uid: p.metadata.uid.clone().unwrap_or_default(),
				ip: st.pod_ip.clone()?,
				host_ip: st.host_ip.clone(),
				api_port: API_PORT,
				certsync_port: CERTSYNC_PORT,
				certs: p.metadata.annotations.as_ref().and_then(|a| a.get(ANNOTATION_CERTS)).cloned(),
				target: if managed { labels.get(LABEL_GATEWAY).cloned() } else { None },
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

	fn boot() -> bootstrap::Bootstrap {
		let (ca, key) = bootstrap::new_ca().unwrap();
		bootstrap::Bootstrap { ca_pem: ca.into_bytes(), ca_key_pem: key, token: "master".into() }
	}

	#[test]
	fn ids_are_short_and_stable() {
		let id = gateway_id("default", "web", "u1");
		assert!(id.starts_with("default-web-"), "{id}");
		assert_eq!(id, gateway_id("default", "web", "u1"));
		assert_ne!(id, gateway_id("default-web", "", "u1"));
		assert_ne!(id, gateway_id("default", "web", "u2"), "created again: new objects");
		assert!(gateway_id(&"n".repeat(63), &"g".repeat(253), "u").len() <= 47);
	}

	#[test]
	fn objects() {
		let m = Managed {
			rproxy_image: "rproxy:1".into(),
			controller_image: "gw:1".into(),
			replicas: 2,
			service_type: "LoadBalancer".into(),
			pull_policy: "IfNotPresent".into(),
			network_policy: Some("rproxy-gateway-system".into()),
		};
		let np = network_policy(&Target::new(&plan()), "rproxy-gateway-system");
		let rules = np.spec.unwrap().ingress.unwrap();
		assert_eq!(rules[0].ports.as_ref().unwrap().len(), 2, "the control API and certsync: the controller only");
		assert!(rules[1].from.is_none(), "the listeners: anyone");
		let p = plan();
		let mut t = Target::new(&p);
		t.labels = [("team".to_string(), "a".to_string()), ("app.kubernetes.io/name".to_string(), "mine".to_string())].into();
		t.annotations = [("note".to_string(), "x".to_string())].into();
		t.owner = Some(owner("gateway.networking.k8s.io/v1", "web", "uid-1"));
		t.addresses = vec!["192.0.2.10".into()];
		let id = t.id.clone();
		let d = deployment(&t, &m, "h");
		assert_eq!(d.metadata.namespace.as_deref(), Some("default"), "next to the Gateway");
		assert_eq!(d.metadata.owner_references.as_ref().unwrap()[0].uid, "uid-1");
		let spec = d.spec.unwrap();
		assert_eq!(spec.replicas, Some(2));
		let tmpl = spec.template.metadata.unwrap();
		let l = tmpl.labels.unwrap();
		assert_eq!(l["team"], "a", "infrastructure labels");
		assert_eq!(l["app.kubernetes.io/name"], "rproxy", "ours win: they select the pods");
		assert_eq!(l[LABEL_GATEWAY_NAME], "web");
		assert_eq!(tmpl.annotations.unwrap()["note"], "x");
		let pod = spec.template.spec.unwrap();
		assert_eq!(pod.containers.len(), 2);
		assert_eq!(pod.automount_service_account_token, Some(false));
		assert_eq!(pod.service_account_name, Some(object_name(&id)));
		let vols = pod.volumes.unwrap();
		let certs = vols.iter().find(|v| v.name == "certs").unwrap().secret.clone().unwrap();
		assert_eq!(certs.secret_name, Some(certs_secret_name(&id)));
		assert_eq!(certs.default_mode, Some(0o440));
		let api = vols.iter().find(|v| v.name == "api").unwrap().secret.clone().unwrap();
		assert!(api.items.unwrap().iter().all(|i| i.key != "ca.crt"));
		assert!(pod.containers.iter().all(|c| c.volume_mounts.iter().flatten().all(|v| v.read_only == Some(true))));
		t.annotations.insert("metallb.universe.tf/loadBalancerIPs".into(), "10.96.0.10".into());
		let s = service(&t, &m);
		assert!(
			!s.metadata.annotations.as_ref().unwrap().contains_key("metallb.universe.tf/loadBalancerIPs"),
			"address steering stays off the Service"
		);
		assert!(service_annotation_allowed("metallb.universe.tf/x", &["metallb.universe.tf/".to_string()]));
		assert_eq!(s.metadata.labels.as_ref().unwrap()["team"], "a");
		assert_eq!(s.metadata.annotations.as_ref().unwrap()["note"], "x");
		let spec = s.spec.unwrap();
		assert_eq!(spec.external_ips, Some(vec!["192.0.2.10".to_string()]));
		let ports = spec.ports.unwrap();
		assert_eq!((ports[0].port, ports[0].protocol.as_deref()), (443, Some("TCP")));
		let sa = service_account(&t);
		assert_eq!(sa.automount_service_account_token, Some(false));
		assert_eq!(sa.metadata.labels.unwrap()["team"], "a");
		let c = fleet_certs_secret("ns", &p.files);
		let l = c.metadata.labels.unwrap();
		assert_eq!(l[LABEL_CERTS_FOR], "fleet");
		assert!(!l.contains_key(LABEL_GATEWAY), "the fleet Secret is not one Gateway's");
		assert_eq!(parse_selector("a=b, c=d"), [("a".to_string(), "b".to_string()), ("c".to_string(), "d".to_string())].into());
	}

	#[test]
	fn api_secrets_keep_their_certificate() {
		let b = boot();
		let p = plan();
		let t = Target::new(&p);
		let first = api_secret(&t, None, &b).unwrap();
		let d = data_of(Some(&first));
		let token = bootstrap::derive_token("master", &t.id);
		assert!(String::from_utf8_lossy(&d["tokens.yaml"]).contains(&crate::pem::sha256_hex(token.as_bytes())));
		assert!(!String::from_utf8_lossy(&d["tokens.yaml"]).contains(&crate::pem::sha256_hex(b"master")), "not the master token");
		crate::pem::check_pair(&d["tls.crt"], &d["tls.key"]).unwrap();
		let again = api_secret(&t, Some(&first), &b).unwrap();
		assert_eq!(data_of(Some(&again)), d, "the certificate is kept");
		// near its end: issued again
		let mut old = first.clone();
		old.metadata.annotations.as_mut().unwrap().insert(bootstrap::ANNOTATION_NOT_AFTER.into(), "2001-01-01T00:00:00Z".into());
		assert_ne!(data_of(Some(&api_secret(&t, Some(&old), &b).unwrap()))["tls.crt"], d["tls.crt"]);
		assert!(!bootstrap::renew(Some(&bootstrap::not_after(bootstrap::LEAF_DAYS).to_string())));
		// another CA: issued again
		let other = boot();
		let renewed = api_secret(&t, Some(&first), &other).unwrap();
		assert_ne!(data_of(Some(&renewed))["tls.crt"], d["tls.crt"]);
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
