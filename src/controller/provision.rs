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
use std::time::{Duration, Instant};

use k8s_openapi::ByteString;
use k8s_openapi::api::apps::v1::{Deployment, DeploymentSpec, DeploymentStrategy, RollingUpdateDeployment};
use k8s_openapi::api::core::v1::{
	Capabilities, Container, ContainerPort, EnvVar, ExecAction, HTTPGetAction, KeyToPath, Lifecycle, LifecycleHandler, Pod,
	PodReadinessGate, PodSecurityContext, PodSpec, PodTemplateSpec, Probe, Secret, SecretVolumeSource, SecurityContext, Service,
	ServiceAccount, ServicePort, ServiceSpec, SleepAction, Sysctl, TopologySpreadConstraint, Volume, VolumeMount,
};
use k8s_openapi::api::policy::v1::{PodDisruptionBudget, PodDisruptionBudgetSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta, OwnerReference};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::Api;
use kube::api::{DeleteParams, ListParams, Patch, PatchParams};
use tracing::info;

use crate::controller::bootstrap;
use crate::k8s::params::RproxyGatewayParametersSpec as Params;
use crate::render::GatewayPlan;
use crate::render::params::{object, objects};
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
/// The readiness gate of rproxy pods: `True` once the controller has applied the pod's rule
/// set(s) since rproxy (the container) last started; the pod takes traffic only then.
pub const CONDITION_RULESET: &str = "rproxy.max3584.net/ruleset-applied";
/// Seconds rproxy has, after its shutdown delay and drain, before the kubelet kills it.
pub const STOP_GRACE: i64 = 5;
/// Seconds an rproxy without a graceful shutdown has after its preStop (it stops at once on SIGTERM).
pub const OLD_STOP_GRACE: i64 = 15;
/// The preStop of an rproxy without a graceful shutdown when `--pre-stop-secs` is unset.
pub const PRE_STOP_SECS: u32 = 15;
/// The rproxy image this controller deploys by default (`--rproxy-image`): it has a graceful shutdown
/// and reads a changed token file again (rproxy v0.4.2).
pub const RPROXY_IMAGE: &str = "ghcr.io/max3584/rproxy-gateway/rproxy:0.4.2";
const API_DIR: &str = "/etc/rproxy-gateway/api";

/// How rproxy is deployed.
// one value, made at start
#[allow(clippy::large_enum_variant)]
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
	/// `externalTrafficPolicy` of the Service: `Local` (clients' addresses kept; only nodes with a
	/// ready rproxy pod take traffic) or `Cluster`. `None`: `Local` for LoadBalancer, `Cluster` for NodePort.
	pub external_traffic_policy: Option<String>,
	/// LoadBalancer Services get node ports (`allocateLoadBalancerNodePorts`; MetalLB does not need them).
	pub allocate_node_ports: bool,
	/// Seconds a stopping rproxy pod keeps serving (preStop) while load balancers and
	/// kube-proxy take it out (0: none). `None`: none with a graceful shutdown (`Target::graceful`; the
	/// shutdown delay does it), `PRE_STOP_SECS` without.
	pub pre_stop_secs: Option<u32>,
	/// The preStop is the kubelet's `sleep` action (Kubernetes 1.30 and later), else `exec sleep`.
	pub native_sleep: bool,
	/// How rproxy stops on SIGTERM (`RPROXY_SHUTDOWN_DELAY`, `RPROXY_SHUTDOWN_DRAIN`, rproxy v0.4.1):
	/// the default of the parameters' `rproxy.shutdown`.
	pub shutdown_delay: Duration,
	pub shutdown_drain: Duration,
	/// rproxy's readiness probe: how soon a hung rproxy leaves the Service's endpoints.
	pub readiness: ProbeTiming,
	/// Its path with a graceful shutdown: `/readyz` (also not ready while starting and draining) or
	/// `/healthz` (up). Older images: `/healthz`.
	pub readiness_path: String,
	/// rproxy's liveness probe (`/healthz`): when the kubelet restarts it (kept slow).
	pub liveness: ProbeTiming,
}

/// A probe's timing (the fields of a Kubernetes probe, in seconds).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeTiming {
	pub period: i32,
	pub timeout: i32,
	pub failure: i32,
	pub success: i32,
	pub initial_delay: i32,
}

impl ProbeTiming {
	/// Readiness: a hung rproxy is out of the endpoints after ~4 s.
	pub const READINESS: ProbeTiming = ProbeTiming { period: 2, timeout: 1, failure: 2, success: 1, initial_delay: 0 };
	/// Liveness: restarted after ~15 s without an answer.
	pub const LIVENESS: ProbeTiming = ProbeTiming { period: 5, timeout: 1, failure: 3, success: 1, initial_delay: 0 };

	/// `periodSeconds=2,timeoutSeconds=1,failureThreshold=2,successThreshold=1,initialDelaySeconds=0`
	/// (any of them; the others from `base`).
	pub fn parse(s: &str, base: ProbeTiming) -> Result<ProbeTiming, String> {
		let mut t = base;
		for kv in s.split(',').map(str::trim).filter(|kv| !kv.is_empty()) {
			let (k, v) = kv.split_once('=').ok_or_else(|| format!("{kv}: key=value"))?;
			let v: i32 = v.trim().parse().map_err(|_| format!("{kv}: not a number"))?;
			let min = if k.trim() == "initialDelaySeconds" { 0 } else { 1 };
			if v < min {
				return Err(format!("{kv}: at least {min}"));
			}
			match k.trim() {
				"periodSeconds" => t.period = v,
				"timeoutSeconds" => t.timeout = v,
				"failureThreshold" => t.failure = v,
				"successThreshold" => t.success = v,
				"initialDelaySeconds" => t.initial_delay = v,
				other => return Err(format!("{other}: not a probe setting")),
			}
		}
		Ok(t)
	}
}

impl Default for Managed {
	fn default() -> Self {
		Managed {
			rproxy_image: String::new(),
			controller_image: String::new(),
			replicas: 1,
			service_type: "LoadBalancer".into(),
			pull_policy: "IfNotPresent".into(),
			network_policy: None,
			external_traffic_policy: None,
			allocate_node_ports: true,
			pre_stop_secs: None,
			native_sleep: true,
			shutdown_delay: Duration::from_secs(15),
			shutdown_drain: Duration::from_secs(25),
			readiness: ProbeTiming::READINESS,
			readiness_path: "/readyz".into(),
			liveness: ProbeTiming::LIVENESS,
		}
	}
}

/// Where the UI reads rproxy (`--ui-namespace`, docs/DESIGN-v0.4.x.md 4.): the UI's namespace and the
/// labels of its pods (let through the NetworkPolicy to the control API).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiAccess {
	pub namespace: String,
	pub pod_selector: BTreeMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct Fleet {
	/// Label selector of the rproxy pods (in the controller's namespace).
	pub selector: String,
	/// Addresses written to Gateway status (else the pods' host IPs).
	pub addresses: Vec<String>,
	/// VIPs the fleet's `vip` sidecars hold (canonical addresses): the fleet's addresses instead.
	pub vips: Vec<String>,
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
	/// The GatewayClass's and the Gateway's RproxyGatewayParameters, merged (checked by `render::params`).
	pub params: Params,
	/// The rproxy image has a graceful shutdown (`features.graceful_shutdown`, rproxy v0.4.1):
	/// `RPROXY_SHUTDOWN_*` and the readiness path instead of the preStop (`graceful`).
	pub graceful: bool,
	/// The rproxy image reads a changed token file again (`features.tokens_reload`, rproxy v0.4.2):
	/// adding or removing the UI's token does not roll the pods (`api_hash`).
	pub tokens_reload: bool,
	/// The UI reads this Gateway's rproxy (`--ui-namespace` set and the parameters' `ui.visible`
	/// not false): its read-only token in the token file (rproxy before v0.4.2 reads the file at
	/// start: the pods roll), the UI's pods let through the NetworkPolicy.
	pub ui: Option<UiAccess>,
}

pub use crate::render::params::{SERVICE_ANNOTATION_DENY, service_annotation_allowed};

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
			params: Params::default(),
			graceful: false,
			tokens_reload: false,
			ui: None,
		}
	}

	/// The rproxy image: the parameters', else `--rproxy-image`.
	pub fn rproxy_image(&self, m: &Managed) -> String {
		self.params.rproxy.as_ref().and_then(|r| r.image.clone()).unwrap_or_else(|| m.rproxy_image.clone())
	}

	/// rproxy pods: the parameters' `replicas`, else `--replicas`.
	pub fn replicas(&self, m: &Managed) -> i32 {
		self.params.replicas.unwrap_or(m.replicas)
	}

	/// The Service's type: the parameters', else `--service-type`.
	pub fn service_type(&self, m: &Managed) -> String {
		self.params.service.as_ref().and_then(|s| s.type_).map_or_else(|| m.service_type.clone(), |t| t.as_str().to_string())
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
	let mut tokens = bootstrap::token_file(&token, &t.plan.ruleset);
	if t.ui.is_some() {
		tokens.push_str(&bootstrap::ui_token_entry(&bootstrap::derive_ui_token(&boot.token, &t.id)));
	}
	let data: BTreeMap<String, Vec<u8>> = [
		("tls.crt".to_string(), crt),
		("tls.key".to_string(), key),
		("ca.crt".to_string(), boot.ca_pem.clone()),
		("tokens.yaml".to_string(), tokens.into_bytes()),
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
	let probe = |path: &str, t: &ProbeTiming| Probe {
		http_get: Some(HTTPGetAction {
			path: Some(path.into()),
			port: IntOrString::Int(API_PORT.into()),
			scheme: Some("HTTPS".into()),
			..Default::default()
		}),
		period_seconds: Some(t.period),
		timeout_seconds: Some(t.timeout),
		failure_threshold: Some(t.failure),
		success_threshold: Some(t.success),
		initial_delay_seconds: Some(t.initial_delay),
		..Default::default()
	};
	let pp = t.params.pod.clone().unwrap_or_default();
	let resources = pp.resources.clone().unwrap_or_default();
	let replicas = t.replicas(m);
	let mut rproxy_env = vec![
		env("RPROXY_API_ADDR", "0.0.0.0"),
		env("RPROXY_API_PORT", &API_PORT.to_string()),
		env("RPROXY_TOKEN_FILE", &format!("{API_DIR}/tokens.yaml")),
		env("RPROXY_TLS_CERT", &format!("{API_DIR}/tls.crt")),
		env("RPROXY_TLS_KEY", &format!("{API_DIR}/tls.key")),
		// the kubelet mounts Secrets as root (0440, group fsGroup): rproxy may use root's files there
		env("RPROXY_FILES_TRUSTED_DIRS", &format!("{CERT_DIR},{API_DIR}")),
	];
	// SIGTERM: keep accepting for delay (GET /readyz: draining), then let connections end for drain
	// (rproxy v0.4.1). Else the preStop keeps an older rproxy serving, which stops at once on SIGTERM
	let (delay, drain) = if t.graceful { shutdown(t, m) } else { (Duration::ZERO, Duration::ZERO) };
	if t.graceful {
		rproxy_env.push(env("RPROXY_SHUTDOWN_DELAY", &duration_env(delay)));
		rproxy_env.push(env("RPROXY_SHUTDOWN_DRAIN", &duration_env(drain)));
	}
	let pre_stop_secs = m.pre_stop_secs.unwrap_or(if t.graceful { 0 } else { PRE_STOP_SECS });
	// the parameters' logLevel, performance and extraEnv (names the controller sets are refused there)
	rproxy_env.extend(crate::render::params::rproxy_env(&t.params));
	let rproxy = Container {
		name: "rproxy".into(),
		image: Some(t.rproxy_image(m)),
		image_pull_policy: Some(m.pull_policy.clone()),
		env: Some(rproxy_env),
		resources: object(resources.rproxy.as_ref()),
		ports: Some(vec![ContainerPort { name: Some("api".into()), container_port: API_PORT.into(), ..Default::default() }]),
		// liveness needs successThreshold 1
		liveness_probe: Some(probe("/healthz", &ProbeTiming { success: 1, ..m.liveness })),
		// /readyz goes draining at SIGTERM: the endpoint stops serving while rproxy still accepts
		// (older images: /healthz)
		readiness_probe: Some(probe(if t.graceful { &m.readiness_path } else { "/healthz" }, &m.readiness)),
		volume_mounts: Some(vec![mount("api", API_DIR), mount("certs", CERT_DIR)]),
		security_context: Some(restricted.clone()),
		lifecycle: pre_stop(m, pre_stop_secs),
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
		resources: object(resources.certsync.as_ref()),
		..Default::default()
	};
	let item = |k: &str| KeyToPath { key: k.into(), path: k.into(), ..Default::default() };
	let mut pod_meta = t.meta(&name, labels(&t.id));
	pod_meta.annotations.get_or_insert_default().insert(ANNOTATION_API.into(), api_hash.into());
	// the parameters' pod labels and annotations: under the infrastructure's and ours
	under(&mut pod_meta.labels, pp.labels.as_ref());
	under(&mut pod_meta.annotations, pp.annotations.as_ref());
	let selector = || LabelSelector { match_labels: Some(labels(&t.id)), ..Default::default() };
	let spread = match objects::<TopologySpreadConstraint>(pp.topology_spread_constraints.as_ref()) {
		// a constraint without a selector spreads the Gateway's own pods
		Some(list) => Some(
			list.into_iter()
				.map(|c| TopologySpreadConstraint { label_selector: c.label_selector.or_else(|| Some(selector())), ..c })
				.collect(),
		),
		// replicas on different nodes when they can be
		None => (replicas >= 2).then(|| {
			vec![TopologySpreadConstraint {
				max_skew: 1,
				topology_key: "kubernetes.io/hostname".into(),
				when_unsatisfiable: "ScheduleAnyway".into(),
				label_selector: Some(selector()),
				..Default::default()
			}]
		}),
	};
	Deployment {
		metadata: t.meta(&name, labels(&t.id)),
		spec: Some(DeploymentSpec {
			replicas: Some(replicas),
			selector: LabelSelector { match_labels: Some(labels(&t.id)), ..Default::default() },
			// a new pod takes traffic (its readiness gate) before an old one stops
			strategy: Some(DeploymentStrategy {
				type_: Some("RollingUpdate".into()),
				rolling_update: Some(RollingUpdateDeployment { max_unavailable: Some(IntOrString::Int(0)), ..Default::default() }),
			}),
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
					readiness_gates: Some(vec![PodReadinessGate { condition_type: CONDITION_RULESET.into() }]),
					termination_grace_period_seconds: grace_period(pre_stop_secs, delay, drain, t.graceful),
					topology_spread_constraints: spread,
					node_selector: pp.node_selector.clone(),
					tolerations: objects(pp.tolerations.as_ref()),
					affinity: object(pp.affinity.as_ref()),
					priority_class_name: pp.priority_class_name.clone(),
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

/// The parameters' `rproxy.shutdown` (each value), else the controller's.
pub fn shutdown(t: &Target, m: &Managed) -> (Duration, Duration) {
	let sd = t.params.rproxy.as_ref().and_then(|r| r.shutdown.as_ref());
	// validated with the parameters
	let get = |v: Option<&String>, default: Duration| v.and_then(|v| crate::render::params::duration(v)).unwrap_or(default);
	(get(sd.and_then(|s| s.delay.as_ref()), m.shutdown_delay), get(sd.and_then(|s| s.drain.as_ref()), m.shutdown_drain))
}

/// A duration as rproxy reads it (`25s`, `500ms`).
pub fn duration_env(d: Duration) -> String {
	if d.subsec_millis() == 0 { format!("{}s", d.as_secs()) } else { format!("{}ms", d.as_millis()) }
}

/// `terminationGracePeriodSeconds`: the preStop, rproxy's delay and drain (rounded up), and
/// `STOP_GRACE` for it to stop (`OLD_STOP_GRACE` after an older rproxy's preStop, as before).
/// `None` (the kubelet's 30 s) when it stops at once.
pub fn grace_period(pre_stop_secs: u32, delay: Duration, drain: Duration, graceful: bool) -> Option<i64> {
	let secs = |d: Duration| i64::try_from(d.as_millis().div_ceil(1000)).unwrap_or(i64::MAX);
	let total = i64::from(pre_stop_secs) + secs(delay) + secs(drain);
	(total > 0).then_some(total + if graceful { STOP_GRACE } else { OLD_STOP_GRACE })
}

/// rproxy's preStop: it keeps serving for `pre_stop_secs` after the pod is marked for deletion,
/// while the Service's endpoints, kube-proxy and load balancers (MetalLB moving its announcement)
/// take it out. rproxy stops at once on SIGTERM, which comes after.
fn pre_stop(m: &Managed, secs: u32) -> Option<Lifecycle> {
	if secs == 0 {
		return None;
	}
	let handler = if m.native_sleep {
		LifecycleHandler { sleep: Some(SleepAction { seconds: secs.into() }), ..Default::default() }
	} else {
		LifecycleHandler { exec: Some(ExecAction { command: Some(vec!["sleep".into(), secs.to_string()]) }), ..Default::default() }
	};
	Some(Lifecycle { pre_stop: Some(handler), ..Default::default() })
}

/// A Gateway's PodDisruptionBudget: the parameters', else (with 2 or more replicas) one pod at a
/// time goes in a drain. `None`: none.
pub fn pod_disruption_budget(t: &Target, m: &Managed) -> Option<PodDisruptionBudget> {
	let (min_available, max_unavailable) = match &t.params.pod_disruption_budget {
		Some(p) => (p.min_available.clone().map(|v| v.0), p.max_unavailable.clone().map(|v| v.0)),
		None if t.replicas(m) >= 2 => (None, Some(IntOrString::Int(1))),
		None => return None,
	};
	Some(PodDisruptionBudget {
		metadata: t.meta(&object_name(&t.id), labels(&t.id)),
		spec: Some(PodDisruptionBudgetSpec {
			min_available,
			max_unavailable,
			selector: Some(LabelSelector { match_labels: Some(labels(&t.id)), ..Default::default() }),
			// a pod that is not ready (its rule set never applied) does not block a drain
			unhealthy_pod_eviction_policy: Some("AlwaysAllow".into()),
		}),
		..Default::default()
	})
}

/// Adds `extra` to `to` where `to` has no such key (the parameters' labels and annotations lose
/// to the infrastructure's and the controller's).
fn under(to: &mut Option<BTreeMap<String, String>>, extra: Option<&BTreeMap<String, String>>) {
	let to = to.get_or_insert_default();
	for (k, v) in extra.into_iter().flatten() {
		to.entry(k.clone()).or_insert_with(|| v.clone());
	}
}

/// The Service's `externalTrafficPolicy` (none for ClusterIP): the parameters', else the flag's,
/// else `Local` for LoadBalancer.
fn external_traffic_policy(t: &Target, m: &Managed) -> Option<String> {
	let set = t.params.service.as_ref().and_then(|s| s.external_traffic_policy).map(|p| p.as_str().to_string());
	match (t.service_type(m).as_str(), set.or_else(|| m.external_traffic_policy.clone())) {
		("LoadBalancer" | "NodePort", Some(p)) => Some(p),
		("LoadBalancer", None) => Some("Local".into()),
		_ => None,
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
	let sp = t.params.service.clone().unwrap_or_default();
	let service_type = t.service_type(m);
	Service {
		metadata: {
			let mut meta = t.meta(&object_name(&t.id), labels(&t.id));
			if let Some(a) = meta.annotations.as_mut() {
				a.retain(|k, _| k == ANNOTATION_GATEWAY || service_annotation_allowed(k, &t.service_annotations));
			}
			// the parameters' (a Gateway's load balancer annotations were checked against the allow list)
			under(&mut meta.labels, sp.labels.as_ref());
			under(&mut meta.annotations, sp.annotations.as_ref());
			meta
		},
		spec: Some(ServiceSpec {
			selector: Some(labels(&t.id)),
			ports: Some(ports),
			external_traffic_policy: external_traffic_policy(t, m),
			allocate_load_balancer_node_ports: (service_type == "LoadBalancer").then_some(m.allocate_node_ports),
			external_ips: (!t.addresses.is_empty()).then(|| t.addresses.clone()),
			load_balancer_class: sp.load_balancer_class.clone().filter(|_| service_type == "LoadBalancer"),
			load_balancer_source_ranges: sp.load_balancer_source_ranges.clone(),
			ip_family_policy: sp.ip_family_policy.map(|p| p.as_str().to_string()),
			type_: Some(service_type),
			..Default::default()
		}),
		..Default::default()
	}
}

/// Who may reach a Gateway's rproxy pods: anyone on the listener ports; on the control
/// API and certsync ports only the controller's pods (`controller_ns`, `app.kubernetes.io/name: rproxy-gateway`);
/// on the control API also the UI's pods when the UI reads the Gateway (`Target::ui`).
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
	if let Some(ui) = &t.ui {
		rules.push(NetworkPolicyIngressRule {
			from: Some(vec![NetworkPolicyPeer {
				namespace_selector: Some(LabelSelector {
					match_labels: Some([("kubernetes.io/metadata.name".to_string(), ui.namespace.clone())].into()),
					..Default::default()
				}),
				pod_selector: Some(LabelSelector { match_labels: Some(ui.pod_selector.clone()), ..Default::default() }),
				..Default::default()
			}]),
			// the control API only (not certsync)
			ports: Some(vec![port(API_PORT, "TCP")]),
		});
	}
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

/// The rproxy container's restart count.
fn rproxy_restarts(pod: &Pod) -> i32 {
	pod.status
		.as_ref()
		.and_then(|s| s.container_statuses.as_ref())
		.and_then(|cs| cs.iter().find(|c| c.name == "rproxy"))
		.map_or(0, |c| c.restart_count)
}

/// The message of a `True` readiness gate condition: the rproxy restart count it was set for.
fn gate_message(restarts: i32) -> String {
	format!("rule sets applied (rproxy restarts: {restarts})")
}

/// Whether the controller has applied the rule sets to the pod's rproxy since it last
/// started: the readiness gate is `True` for its current restart count (the `vip`
/// sidecar holds VIPs only then).
pub fn gate_applied(pod: &Pod) -> bool {
	let cond = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).and_then(|c| c.iter().find(|c| c.type_ == CONDITION_RULESET));
	cond.is_some_and(|c| c.status == "True" && c.message.as_deref() == Some(gate_message(rproxy_restarts(pod)).as_str()))
}

/// What the pod's readiness gate (`CONDITION_RULESET`) should become, if it must change.
/// `done`: whether this pass brought every rule set of the pod up to date (`None`: not
/// asked). The gate turns `True` once the sets are applied and stays so (a later update
/// waiting for files does not take a serving pod out); it turns `False` when rproxy has
/// restarted since (its sets live in memory) until they are applied again.
pub fn gate_change(pod: &Pod, done: Option<bool>) -> Option<bool> {
	let gated =
		pod.spec.as_ref().and_then(|s| s.readiness_gates.as_ref()).is_some_and(|g| g.iter().any(|g| g.condition_type == CONDITION_RULESET));
	if !gated || pod.metadata.deletion_timestamp.is_some() {
		return None;
	}
	let cond = pod.status.as_ref().and_then(|s| s.conditions.as_ref()).and_then(|c| c.iter().find(|c| c.type_ == CONDITION_RULESET));
	let is_true = cond.is_some_and(|c| c.status == "True");
	let restarts = rproxy_restarts(pod);
	let current = is_true && cond.and_then(|c| c.message.as_deref()) == Some(gate_message(restarts).as_str());
	match done {
		Some(true) if !current => Some(true),
		_ if is_true && !current => Some(false),
		_ => None,
	}
}

/// Sets the pod's readiness gate condition.
pub async fn set_gate(client: &kube::Client, pod: &Pod, ready: bool) {
	let (ns, name) = (pod.metadata.namespace.clone().unwrap_or_default(), pod.metadata.name.clone().unwrap_or_default());
	let restarts = rproxy_restarts(pod);
	let (reason, message) = if ready {
		("RuleSetsApplied", gate_message(restarts))
	} else {
		("RproxyRestarted", format!("rproxy restarted (restarts: {restarts}); waiting for its rule sets"))
	};
	let patch = serde_json::json!({"status": {"conditions": [{
		"type": CONDITION_RULESET,
		"status": if ready { "True" } else { "False" },
		"reason": reason,
		"message": message,
		"lastTransitionTime": crate::render::status::now(),
	}]}});
	match Api::<Pod>::namespaced(client.clone(), &ns).patch_status(&name, &PatchParams::default(), &Patch::Strategic(&patch)).await {
		Ok(_) => info!(pod = format!("{ns}/{name}"), ready, "readiness gate"),
		Err(e) => tracing::warn!(pod = format!("{ns}/{name}"), error = %e, "cannot set the readiness gate (RBAC: pods/status patch)"),
	}
}

/// What `apply_managed` made.
pub struct Applied {
	/// The Service (with its status), when there is a port to serve.
	pub service: Option<Service>,
	/// The hash of the certificate Secret's content.
	pub certs: String,
	/// Whether the Gateway has a PodDisruptionBudget (else `collect_garbage` deletes it).
	pub pdb: bool,
	/// The hash of the control API Secret's content (the pod template's `ANNOTATION_API`).
	pub api: String,
}

/// Writes a Gateway's certificate and control API Secrets; returns the hashes of their content.
pub async fn apply_secrets(
	client: &kube::Client,
	t: &Target<'_>,
	boot: &bootstrap::Bootstrap,
	current_api: Option<&Secret>,
	absent: &mut BTreeMap<String, Instant>,
) -> anyhow::Result<(String, String)> {
	let ns = t.ns();
	let certs = apply_certs(client, ns, &certs_secret_name(&t.id), &t.plan.files, absent, |data| {
		secret(t.meta(&certs_secret_name(&t.id), t.secret_labels()), data)
	})
	.await?;
	let api = api_secret(t, current_api, boot)?;
	if current_api.is_none_or(|c| data_of(Some(c)) != data_of(Some(&api)) || !same_meta(&c.metadata, &api.metadata)) {
		Api::<Secret>::namespaced(client.clone(), ns).patch(&api_secret_name(&t.id), &pp(), &Patch::Apply(&api)).await?;
	}
	Ok((certs, api_hash(&data_of(Some(&api)), t.tokens_reload)))
}

/// The hash of a control API Secret's content in the pod template (`ANNOTATION_API`): a change rolls
/// the pods. rproxy before v0.4.2 reads its token file only at start (and on SIGHUP), so the UI's token
/// is part of it (adding or removing it rolls the pods once); an rproxy that reads a changed token file
/// again (`tokens_reload`, max3584/rproxy-api#253) takes it without a roll.
pub fn api_hash(data: &BTreeMap<String, Vec<u8>>, tokens_reload: bool) -> String {
	if !tokens_reload {
		return content_hash(data);
	}
	let mut d = data.clone();
	if let Some(f) = d.get_mut("tokens.yaml") {
		*f = bootstrap::without_ui_entry(&String::from_utf8_lossy(f)).as_bytes().to_vec();
	}
	content_hash(&d)
}

/// Applies a Gateway's Secrets, ServiceAccount, Deployment, PodDisruptionBudget and Service.
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
	let (certs, api_hash) = apply_secrets(client, t, boot, current_api, absent).await?;
	let name = object_name(&t.id);
	if let Some(cns) = &m.network_policy {
		let np = network_policy(t, cns);
		Api::<k8s_openapi::api::networking::v1::NetworkPolicy>::namespaced(client.clone(), ns)
			.patch(&name, &pp(), &Patch::Apply(&np))
			.await?;
	}
	Api::<ServiceAccount>::namespaced(client.clone(), ns).patch(&name, &pp(), &Patch::Apply(&service_account(t))).await?;
	Api::<Deployment>::namespaced(client.clone(), ns).patch(&name, &pp(), &Patch::Apply(&deployment(t, m, &api_hash))).await?;
	// none wanted: `collect_garbage` deletes it
	let pdb = pod_disruption_budget(t, m);
	if let Some(want) = &pdb {
		let api = Api::<PodDisruptionBudget>::namespaced(client.clone(), ns);
		if let Err(e) = api.patch(&name, &pp(), &Patch::Apply(want)).await {
			tracing::warn!(gateway = %t.plan.ruleset, error = %e, "cannot write the PodDisruptionBudget");
		}
	}
	let svc_api = Api::<Service>::namespaced(client.clone(), ns);
	if t.plan.ports().is_empty() {
		// a Service needs a port; none until a listener is valid
		let _ = svc_api.delete(&name, &DeleteParams::default()).await;
		return Ok(Applied { service: None, certs, pdb: pdb.is_some(), api: api_hash });
	}
	let service = svc_api.patch(&name, &pp(), &Patch::Apply(&service(t, m))).await?;
	Ok(Applied { service: Some(service), certs, pdb: pdb.is_some(), api: api_hash })
}

/// Whether a Gateway's rproxy Deployment exists (a Gateway kept in its last good shape when its
/// parameters become invalid).
pub async fn deployed(client: &kube::Client, ns: &str, id: &str) -> anyhow::Result<bool> {
	Ok(Api::<Deployment>::namespaced(client.clone(), ns).get_opt(&object_name(id)).await?.is_some())
}

/// Deletes managed objects of Gateways that are gone or no longer ours (`keep`:
/// ids still in use), in every namespace. (Deleting a Gateway deletes them as
/// well: the Gateway owns them.) PodDisruptionBudgets go as well unless their
/// Gateway is in `keep_pdb` (it wants one, or is kept in its last good shape).
pub async fn collect_garbage(
	client: &kube::Client,
	keep: &[String],
	keep_pdb: &[String],
	scope: &crate::controller::cache::Scope,
) -> anyhow::Result<()> {
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
	if let Ok(list) = list_in::<PodDisruptionBudget>(client, &nss, &lp).await {
		for pdb in list {
			let id = pdb.metadata.labels.as_ref().and_then(|l| l.get(LABEL_GATEWAY));
			if gone(&pdb.metadata) || id.is_none_or(|id| !keep_pdb.contains(id)) {
				let (ns, name) = ns_name(&pdb.metadata);
				Api::<PodDisruptionBudget>::namespaced(client.clone(), &ns).delete(&name, &dp).await?;
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
	/// The image of the pod's `rproxy` container.
	pub rproxy_image: Option<String>,
}

/// Running pods in `ns` matching `labels` (`key=value,...`).
pub fn pods(store: &[std::sync::Arc<Pod>], ns: &str, selector: &BTreeMap<String, String>) -> Vec<Endpoint> {
	pods_of(store, ns, selector, false)
}

fn pods_of(store: &[std::sync::Arc<Pod>], ns: &str, selector: &BTreeMap<String, String>, terminating: bool) -> Vec<Endpoint> {
	let mut out: Vec<Endpoint> = store
		.iter()
		.filter(|p| p.metadata.namespace.as_deref() == Some(ns))
		.filter(|p| {
			let l = p.metadata.labels.clone().unwrap_or_default();
			selector.iter().all(|(k, v)| l.get(k) == Some(v))
		})
		.filter(|p| terminating || p.metadata.deletion_timestamp.is_none())
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
				rproxy_image: p.spec.as_ref().and_then(|s| s.containers.iter().find(|c| c.name == "rproxy")).and_then(|c| c.image.clone()),
			})
		})
		.collect();
	out.sort_by(|a, b| a.pod.cmp(&b.pod));
	out
}

/// The pods the UI may be sent to: running, not being deleted, Ready (with the readiness gate
/// `CONDITION_RULESET` True when the pod has it) and, with `api_hash`, made from the pod template that
/// has the current control API Secret (`ANNOTATION_API`): with rproxy v0.4.1 (which reads its token file at
/// start) only those have the UI's token. A pod that would answer the UI's token with 401 is not listed
/// (rproxy locks a client out after repeated failures): `ui::accepting` then asks each pod (rproxy v0.4.2
/// reads a changed token file again, without a new pod).
pub fn ui_pods(store: &[std::sync::Arc<Pod>], ns: &str, selector: &BTreeMap<String, String>, api_hash: Option<&str>) -> Vec<Endpoint> {
	let accepts: std::collections::BTreeSet<String> = store
		.iter()
		.filter(|p| p.metadata.namespace.as_deref() == Some(ns) && p.metadata.deletion_timestamp.is_none())
		.filter(|p| {
			let conds = p.status.as_ref().and_then(|s| s.conditions.as_ref());
			let is_true = |t: &str| conds.into_iter().flatten().any(|c| c.type_ == t && c.status == "True");
			let gated = p
				.spec
				.as_ref()
				.and_then(|s| s.readiness_gates.as_ref())
				.into_iter()
				.flatten()
				.any(|g| g.condition_type == CONDITION_RULESET);
			is_true("Ready") && (!gated || is_true(CONDITION_RULESET))
		})
		.filter(|p| {
			api_hash.is_none_or(|h| p.metadata.annotations.as_ref().and_then(|a| a.get(ANNOTATION_API)).map(String::as_str) == Some(h))
		})
		.filter_map(|p| p.metadata.name.clone())
		.collect();
	pods_of(store, ns, selector, false).into_iter().filter(|e| accepts.contains(&e.pod)).collect()
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
			ruleset: "k8s/default/web".into(),
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
			..Default::default()
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
	fn graceful_shutdown() {
		let p = plan();
		let mut t = Target::new(&p);
		let env = |pod: &PodSpec, n: &str| pod.containers[0].env.iter().flatten().find(|e| e.name == n).and_then(|e| e.value.clone());
		let path = |probe: &Option<Probe>| probe.as_ref().unwrap().http_get.as_ref().unwrap().path.clone().unwrap();
		// an rproxy not known to have a graceful shutdown: the preStop and /healthz as before, no RPROXY_SHUTDOWN_*
		let pod = deployment(&t, &Managed::default(), "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DELAY"), None);
		assert_eq!(path(&pod.containers[0].readiness_probe), "/healthz");
		assert!(pod.containers[0].lifecycle.is_some());
		// with one (the defaults): delay 15 s replaces the preStop, /readyz, delay + drain + 5 s
		t.graceful = true;
		let pod = deployment(&t, &Managed::default(), "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DELAY").as_deref(), Some("15s"));
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DRAIN").as_deref(), Some("25s"));
		assert!(pod.containers[0].lifecycle.is_none(), "no preStop");
		assert_eq!(pod.termination_grace_period_seconds, Some(45), "15 s + 25 s + 5 s");
		assert_eq!(path(&pod.containers[0].readiness_probe), "/readyz");
		assert_eq!(path(&pod.containers[0].liveness_probe), "/healthz", "draining is not dead");
		// the flags
		let m = Managed {
			shutdown_delay: Duration::from_secs(10),
			shutdown_drain: Duration::from_millis(1500),
			readiness_path: "/healthz".into(),
			..Default::default()
		};
		let pod = deployment(&t, &m, "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DELAY").as_deref(), Some("10s"));
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DRAIN").as_deref(), Some("1500ms"));
		assert_eq!(pod.termination_grace_period_seconds, Some(17), "10 s + 2 s (rounded up) + 5 s");
		assert_eq!(path(&pod.containers[0].readiness_probe), "/healthz");
		// the parameters' rproxy.shutdown, each value over the flags'
		t.params.rproxy = Some(crate::k8s::params::RproxyParams {
			shutdown: Some(crate::k8s::params::Shutdown { delay: Some("5s".into()), drain: None }),
			..Default::default()
		});
		let pod = deployment(&t, &Managed::default(), "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DELAY").as_deref(), Some("5s"));
		assert_eq!(env(&pod, "RPROXY_SHUTDOWN_DRAIN").as_deref(), Some("25s"));
		assert_eq!(pod.termination_grace_period_seconds, Some(35));
		// stopping at once: the kubelet's default
		t.params.rproxy = Some(crate::k8s::params::RproxyParams {
			shutdown: Some(crate::k8s::params::Shutdown { delay: Some("0s".into()), drain: Some("0".into()) }),
			..Default::default()
		});
		let pod = deployment(&t, &Managed::default(), "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(pod.termination_grace_period_seconds, None);
		assert_eq!(duration_env(Duration::from_secs(60)), "60s");
		// --pre-stop-secs set (an install that sets managed.preStopSeconds): that preStop, then the delay and drain
		t.params.rproxy = None;
		let pod = deployment(&t, &Managed { pre_stop_secs: Some(10), ..Default::default() }, "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(pod.containers[0].lifecycle.as_ref().unwrap().pre_stop.as_ref().unwrap().sleep.as_ref().unwrap().seconds, 10);
		assert_eq!(pod.termination_grace_period_seconds, Some(55), "10 + 15 + 25 + 5");
	}

	#[test]
	fn high_availability() {
		let p = plan();
		let t = Target::new(&p);
		let m = Managed { replicas: 2, ..Default::default() };
		let pod = deployment(&t, &m, "h").spec.unwrap();
		let rolling = pod.strategy.unwrap().rolling_update.unwrap();
		assert_eq!(rolling.max_unavailable, Some(IntOrString::Int(0)), "a new pod is ready before an old one stops");
		let pod = pod.template.spec.unwrap();
		assert_eq!(pod.readiness_gates.unwrap()[0].condition_type, CONDITION_RULESET);
		assert_eq!(pod.termination_grace_period_seconds, Some(30), "an rproxy without a graceful shutdown: preStop 15 s + 15 s");
		let rproxy = &pod.containers[0];
		assert_eq!(rproxy.lifecycle.as_ref().unwrap().pre_stop.as_ref().unwrap().sleep.as_ref().unwrap().seconds, 15);
		let r = rproxy.readiness_probe.as_ref().unwrap();
		assert_eq!((r.period_seconds, r.failure_threshold, r.timeout_seconds), (Some(2), Some(2), Some(1)));
		let l = rproxy.liveness_probe.as_ref().unwrap();
		assert_eq!((l.period_seconds, l.failure_threshold), (Some(5), Some(3)));
		assert!(pod.containers[1].lifecycle.is_none(), "certsync stops at once");
		let spread = pod.topology_spread_constraints.unwrap();
		assert_eq!(spread[0].topology_key, "kubernetes.io/hostname");
		assert_eq!(spread[0].when_unsatisfiable, "ScheduleAnyway");
		assert_eq!(spread[0].label_selector.as_ref().unwrap().match_labels, Some(labels(&t.id)));
		// an older cluster: exec; no preStop at 0; one replica: no spreading
		let old = Managed { native_sleep: false, pre_stop_secs: Some(5), ..Default::default() };
		let pod = deployment(&t, &old, "h").spec.unwrap().template.spec.unwrap();
		let exec = pod.containers[0].lifecycle.clone().unwrap().pre_stop.unwrap().exec.unwrap();
		assert_eq!(exec.command, Some(vec!["sleep".to_string(), "5".to_string()]));
		assert_eq!(pod.termination_grace_period_seconds, Some(20));
		assert!(pod.topology_spread_constraints.is_none());
		let none = Managed { pre_stop_secs: Some(0), ..Default::default() };
		let pod = deployment(&t, &none, "h").spec.unwrap().template.spec.unwrap();
		assert!(pod.containers[0].lifecycle.is_none());
		assert_eq!(pod.termination_grace_period_seconds, None);
		// the PodDisruptionBudget
		assert!(pod_disruption_budget(&t, &Managed::default()).is_none(), "one replica: none");
		let pdb = pod_disruption_budget(&t, &m).unwrap();
		assert_eq!(pdb.metadata.name, Some(object_name(&t.id)));
		assert_eq!(pdb.metadata.labels.as_ref().unwrap()[LABEL_GATEWAY], t.id, "collected with the Gateway's objects");
		let spec = pdb.spec.unwrap();
		assert_eq!(spec.max_unavailable, Some(IntOrString::Int(1)));
		assert_eq!(spec.selector.unwrap().match_labels, Some(labels(&t.id)));
		assert_eq!(spec.unhealthy_pod_eviction_policy.as_deref(), Some("AlwaysAllow"));
	}

	#[test]
	fn parameters() {
		use crate::k8s::params::RproxyGatewayParametersSpec;
		let p = plan();
		let mut t = Target::new(&p);
		t.labels = [("team".to_string(), "infra".to_string())].into();
		t.annotations = [("metallb.universe.tf/loadBalancerIPs".to_string(), "10.0.0.1".to_string())].into();
		t.params = serde_json::from_value::<RproxyGatewayParametersSpec>(serde_json::json!({
			"replicas": 3,
			"podDisruptionBudget": {"minAvailable": 2},
			"pod": {
				"labels": {"team": "params", "cost": "a"},
				"annotations": {"prometheus.io/scrape": "true"},
				"resources": {"rproxy": {"requests": {"cpu": "500m"}}, "certsync": {"limits": {"memory": "32Mi"}}},
				"topologySpreadConstraints": [{"maxSkew": 1, "topologyKey": "topology.kubernetes.io/zone", "whenUnsatisfiable": "DoNotSchedule"}],
				"nodeSelector": {"pool": "edge"},
				"tolerations": [{"key": "edge", "operator": "Exists"}],
				"priorityClassName": "high"
			},
			"service": {
				"type": "NodePort", "externalTrafficPolicy": "Local", "ipFamilyPolicy": "PreferDualStack",
				"loadBalancerSourceRanges": ["203.0.113.0/24"], "labels": {"svc": "x"},
				"annotations": {"metallb.universe.tf/address-pool": "edge"}
			},
			"rproxy": {"image": "example.com/rproxy:9", "logLevel": "debug", "performance": {"workers": 4}}
		}))
		.unwrap();
		let m = Managed { replicas: 1, rproxy_image: "rproxy:1".into(), ..Default::default() };
		let d = deployment(&t, &m, "h").spec.unwrap();
		assert_eq!(d.replicas, Some(3));
		let tmpl = d.template.metadata.unwrap();
		let l = tmpl.labels.unwrap();
		assert_eq!((l["team"].as_str(), l["cost"].as_str()), ("infra", "a"), "the infrastructure's labels win over the parameters'");
		assert_eq!(l["app.kubernetes.io/name"], "rproxy");
		assert_eq!(tmpl.annotations.unwrap()["prometheus.io/scrape"], "true");
		let pod = d.template.spec.unwrap();
		let rproxy = &pod.containers[0];
		assert_eq!(rproxy.image.as_deref(), Some("example.com/rproxy:9"));
		assert_eq!(rproxy.resources.as_ref().unwrap().requests.as_ref().unwrap()["cpu"].0, "500m");
		assert_eq!(pod.containers[1].resources.as_ref().unwrap().limits.as_ref().unwrap()["memory"].0, "32Mi");
		let env: BTreeMap<String, String> =
			rproxy.env.iter().flatten().map(|e| (e.name.clone(), e.value.clone().unwrap_or_default())).collect();
		assert_eq!((env["RPROXY_LOG_LEVEL"].as_str(), env["RPROXY_WORKERS"].as_str()), ("debug", "4"));
		assert_eq!(env["RPROXY_API_PORT"], "9443", "the controller's own settings stay");
		let spread = pod.topology_spread_constraints.unwrap();
		assert_eq!(spread.len(), 1, "the parameters' constraints replace the default");
		assert_eq!(spread[0].topology_key, "topology.kubernetes.io/zone");
		assert_eq!(spread[0].label_selector.as_ref().unwrap().match_labels, Some(labels(&t.id)), "the Gateway's pods");
		assert_eq!(pod.node_selector.unwrap()["pool"], "edge");
		assert_eq!(pod.tolerations.unwrap()[0].key.as_deref(), Some("edge"));
		assert_eq!(pod.priority_class_name.as_deref(), Some("high"));
		assert_eq!(pod.readiness_gates.unwrap().len(), 1, "the readiness gate stays");
		let pdb = pod_disruption_budget(&t, &m).unwrap().spec.unwrap();
		assert_eq!((pdb.min_available, pdb.max_unavailable), (Some(IntOrString::Int(2)), None));
		let s = service(&t, &m);
		let a = s.metadata.annotations.unwrap();
		assert!(!a.contains_key("metallb.universe.tf/loadBalancerIPs"), "the infrastructure's stay filtered");
		assert_eq!(a["metallb.universe.tf/address-pool"], "edge", "the parameters' were checked before");
		assert_eq!(s.metadata.labels.unwrap()["svc"], "x");
		let spec = s.spec.unwrap();
		assert_eq!(spec.type_.as_deref(), Some("NodePort"));
		assert_eq!(spec.external_traffic_policy.as_deref(), Some("Local"));
		assert_eq!(spec.ip_family_policy.as_deref(), Some("PreferDualStack"));
		assert_eq!(spec.load_balancer_source_ranges, Some(vec!["203.0.113.0/24".to_string()]));
		assert_eq!(spec.allocate_load_balancer_node_ports, None, "NodePort");
		// without parameters: the flags, as in v0.4.1
		let t = Target::new(&p);
		let m = Managed { replicas: 2, ..Default::default() };
		assert_eq!(deployment(&t, &m, "h").spec.unwrap().replicas, Some(2));
		assert_eq!(pod_disruption_budget(&t, &m).unwrap().spec.unwrap().max_unavailable, Some(IntOrString::Int(1)));
	}

	#[test]
	fn external_traffic_policies() {
		let p = plan();
		let t = Target::new(&p);
		let etp = |service_type: &str, policy: Option<&str>| {
			let m = Managed { service_type: service_type.into(), external_traffic_policy: policy.map(Into::into), ..Default::default() };
			service(&t, &m).spec.unwrap().external_traffic_policy
		};
		assert_eq!(etp("LoadBalancer", None).as_deref(), Some("Local"), "clients' addresses kept by default");
		assert_eq!(etp("LoadBalancer", Some("Cluster")).as_deref(), Some("Cluster"));
		assert_eq!(etp("NodePort", None), None, "NodePort as before (Cluster)");
		assert_eq!(etp("NodePort", Some("Local")).as_deref(), Some("Local"));
		assert_eq!(etp("ClusterIP", Some("Local")), None);
		let m = Managed { allocate_node_ports: false, ..Default::default() };
		assert_eq!(service(&t, &m).spec.unwrap().allocate_load_balancer_node_ports, Some(false));
		let m = Managed { service_type: "ClusterIP".into(), ..Default::default() };
		assert_eq!(service(&t, &m).spec.unwrap().allocate_load_balancer_node_ports, None);
	}

	#[test]
	fn probe_timing() {
		let r = ProbeTiming::READINESS;
		assert_eq!(ProbeTiming::parse("", r), Ok(r));
		let t = ProbeTiming::parse("periodSeconds=1, failureThreshold=3,initialDelaySeconds=0", r).unwrap();
		assert_eq!((t.period, t.failure, t.timeout, t.initial_delay), (1, 3, 1, 0));
		assert!(ProbeTiming::parse("periodSeconds=0", r).is_err());
		assert!(ProbeTiming::parse("period=1", r).is_err());
		assert!(ProbeTiming::parse("periodSeconds", r).is_err());
		let p = plan();
		let m = Managed { liveness: ProbeTiming { success: 3, ..ProbeTiming::LIVENESS }, ..Default::default() };
		let pod = deployment(&Target::new(&p), &m, "h").spec.unwrap().template.spec.unwrap();
		assert_eq!(pod.containers[0].liveness_probe.as_ref().unwrap().success_threshold, Some(1), "liveness takes 1 only");
	}

	#[test]
	fn readiness_gate() {
		use k8s_openapi::api::core::v1::{ContainerStatus, PodCondition, PodStatus};
		let pod = |gated: bool, cond: Option<(&str, String)>, restarts: i32| Pod {
			spec: Some(PodSpec {
				readiness_gates: gated.then(|| vec![PodReadinessGate { condition_type: CONDITION_RULESET.into() }]),
				..Default::default()
			}),
			status: Some(PodStatus {
				conditions: cond.map(|(status, message)| {
					vec![PodCondition {
						type_: CONDITION_RULESET.into(),
						status: status.into(),
						message: Some(message),
						..Default::default()
					}]
				}),
				container_statuses: Some(vec![ContainerStatus { name: "rproxy".into(), restart_count: restarts, ..Default::default() }]),
				..Default::default()
			}),
			..Default::default()
		};
		assert_eq!(gate_change(&pod(false, None, 0), Some(true)), None, "no gate: not touched");
		assert_eq!(gate_change(&pod(true, None, 0), Some(true)), Some(true), "applied: ready");
		assert_eq!(gate_change(&pod(true, None, 0), Some(false)), None);
		assert_eq!(gate_change(&pod(true, None, 0), None), None);
		let set = Some(("True", gate_message(0)));
		assert_eq!(gate_change(&pod(true, set.clone(), 0), Some(true)), None, "already");
		assert_eq!(gate_change(&pod(true, set.clone(), 0), Some(false)), None, "a later update waiting for files: stays ready");
		assert_eq!(gate_change(&pod(true, set.clone(), 1), None), Some(false), "rproxy restarted: out until applied");
		assert_eq!(gate_change(&pod(true, set.clone(), 1), Some(true)), Some(true), "applied again");
		assert_eq!(gate_change(&pod(true, Some(("False", "x".into())), 1), Some(true)), Some(true));
		let mut gone = pod(true, set, 1);
		gone.metadata.deletion_timestamp = Some(k8s_openapi::apimachinery::pkg::apis::meta::v1::Time(Default::default()));
		assert_eq!(gate_change(&gone, None), None, "a pod being deleted is left alone");
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
		assert!(String::from_utf8_lossy(&d["tokens.yaml"]).contains("allow_rulesets: [\"k8s/default/web\"]"), "its own rule set only");
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
	fn the_ui_reads_with_its_own_token() {
		let b = boot();
		let p = plan();
		let mut t = Target::new(&p);
		let off = api_secret(&t, None, &b).unwrap();
		let ui = UiAccess {
			namespace: "rproxy-ui".into(),
			pod_selector: [("app.kubernetes.io/name".to_string(), "rproxy-ui".to_string())].into(),
		};
		t.ui = Some(ui.clone());
		let on = api_secret(&t, Some(&off), &b).unwrap();
		let (d_off, d_on) = (data_of(Some(&off)), data_of(Some(&on)));
		let tokens = String::from_utf8_lossy(&d_on["tokens.yaml"]).into_owned();
		let ui_token = bootstrap::derive_ui_token("master", &t.id);
		assert!(tokens.contains(&crate::pem::sha256_hex(ui_token.as_bytes())), "{tokens}");
		assert!(tokens.ends_with("scopes: [rules:read, metrics:read]\n"));
		assert!(!String::from_utf8_lossy(&d_off["tokens.yaml"]).contains("rproxy-ui"), "off: as before");
		// rproxy v0.4.1 reads its token file at start (and on SIGHUP): the pods roll once for the UI's token
		assert_ne!(content_hash(&d_on), content_hash(&d_off));
		assert_ne!(api_hash(&d_on, false), api_hash(&d_off, false));
		// rproxy v0.4.2 reads it again when it changes: no roll
		assert_eq!(api_hash(&d_on, true), api_hash(&d_off, true));
		assert_eq!(api_hash(&d_off, true), content_hash(&d_off), "without the UI: the same hash as before");
		let mut stripped = d_on.clone();
		stripped
			.insert("tokens.yaml".into(), bootstrap::without_ui_entry(&String::from_utf8_lossy(&d_on["tokens.yaml"])).as_bytes().to_vec());
		assert_eq!(content_hash(&stripped), content_hash(&d_off), "off: the same Secret (and hash) as before");
		// the NetworkPolicy lets the UI's pods reach the control API only
		let rules = network_policy(&t, "rproxy-gateway-system").spec.unwrap().ingress.unwrap();
		let r = rules
			.iter()
			.find(|r| {
				r.from.as_ref().is_some_and(|f| f[0].pod_selector.as_ref().and_then(|s| s.match_labels.as_ref()) == Some(&ui.pod_selector))
			})
			.unwrap();
		assert_eq!(r.ports.as_ref().unwrap().len(), 1);
		assert_eq!(r.ports.as_ref().unwrap()[0].port, Some(IntOrString::Int(API_PORT.into())), "not certsync");
		let from = &r.from.as_ref().unwrap()[0];
		assert_eq!(from.namespace_selector.as_ref().unwrap().match_labels.as_ref().unwrap()["kubernetes.io/metadata.name"], "rproxy-ui");
		t.ui = None;
		assert_eq!(network_policy(&t, "rproxy-gateway-system").spec.unwrap().ingress.unwrap().len(), rules.len() - 1);
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

	#[test]
	fn the_ui_gets_only_pods_that_take_its_token() {
		// name, Ready, ruleset-applied, the pod template's api hash, being deleted
		let pod = |name: &str, ready: bool, gate: bool, api: &str, deleting: bool| {
			let mut v = serde_json::json!({
				"metadata": {
					"name": name, "namespace": "web", "uid": format!("uid-{name}"),
					"labels": {LABEL_GATEWAY: "web-gw-1", "app.kubernetes.io/managed-by": MANAGER},
					"annotations": {ANNOTATION_API: api},
					"ownerReferences": [{"apiVersion": "apps/v1", "kind": "ReplicaSet", "name": format!("{}-abc", object_name("web-gw-1")), "uid": "rs", "controller": true}],
				},
				"spec": {"containers": [{"name": "rproxy"}], "readinessGates": [{"conditionType": CONDITION_RULESET}]},
				"status": {"phase": "Running", "podIP": format!("10.0.0.{}", name.len()), "conditions": [
					{"type": "Ready", "status": if ready { "True" } else { "False" }},
					{"type": CONDITION_RULESET, "status": if gate { "True" } else { "False" }},
				]},
			});
			if deleting {
				v["metadata"]["deletionTimestamp"] = serde_json::json!("2026-10-08T00:00:00Z");
			}
			std::sync::Arc::new(serde_json::from_value::<Pod>(v).unwrap())
		};
		let store = vec![
			pod("new", true, true, "h2", false),
			pod("old-template", true, true, "h1", false),
			pod("starting", false, false, "h2", false),
			pod("no-rule-set-yet", true, false, "h2", false),
			pod("stopping", true, true, "h2", true),
		];
		let sel: BTreeMap<String, String> = [(LABEL_GATEWAY.to_string(), "web-gw-1".to_string())].into();
		let names = |eps: Vec<Endpoint>| eps.into_iter().map(|e| e.pod).collect::<Vec<_>>();
		assert_eq!(names(ui_pods(&store, "web", &sel, Some("h2"))), ["new"]);
		// without the hash (fleet; a pass that could not write the Secrets): Ready and not being deleted
		assert_eq!(names(ui_pods(&store, "web", &sel, None)), ["new", "old-template"]);
	}
}
