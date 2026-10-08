//! `RproxyGatewayParameters` (`rproxy.max3584.net/v1alpha1`): how a managed Gateway's
//! rproxy Deployment, Service and pods are made (docs/DESIGN-v0.4.x.md, A).
//!
//! Named by a GatewayClass's `spec.parametersRef` (the class's defaults, in the
//! controller's namespace; it may carry `policy`) and by a Gateway's
//! `spec.infrastructure.parametersRef` (that Gateway's overrides, in its namespace).
//! Fields of Kubernetes shapes (resources, tolerations, affinity, topology spread
//! constraints, environment variables) are kept free-form in the CRD (to keep it
//! small) and read by the controller (`render::params`).

use std::borrow::Cow;
use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use kube::CustomResource;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};

use crate::k8s::crd::FreeForm;

/// An integer or a string (`x-kubernetes-int-or-string`): `1`, `"25%"`, `"auto"`, `"64KiB"`.
#[derive(Clone, Debug, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct IntOrStr(pub IntOrString);

impl Default for IntOrStr {
	fn default() -> Self {
		IntOrStr(IntOrString::Int(0))
	}
}

impl JsonSchema for IntOrStr {
	fn schema_name() -> Cow<'static, str> {
		"IntOrStr".into()
	}

	fn inline_schema() -> bool {
		true
	}

	fn json_schema(_: &mut SchemaGenerator) -> Schema {
		json_schema!({"x-kubernetes-int-or-string": true})
	}
}

/// How a managed Gateway's rproxy is made: replicas, the PodDisruptionBudget, the pods, the
/// Service and rproxy's settings. Every field may be left out (the controller's flags then).
#[derive(CustomResource, Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[kube(
	group = "rproxy.max3584.net",
	version = "v1alpha1",
	kind = "RproxyGatewayParameters",
	plural = "rproxygatewayparameters",
	namespaced,
	shortname = "rpgwp"
)]
#[kube(
	doc = "How a managed Gateway's rproxy Deployment, Service and pods are made; named by a GatewayClass's parametersRef (in the controller's namespace) or a Gateway's spec.infrastructure.parametersRef"
)]
#[kube(printcolumn = r#"{"name":"Replicas","type":"integer","jsonPath":".spec.replicas"}"#)]
#[serde(rename_all = "camelCase")]
pub struct RproxyGatewayParametersSpec {
	/// rproxy pods per Gateway (1 to policy.maxReplicas, 10 by default).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[schemars(range(min = 1))]
	pub replicas: Option<i32>,
	/// The Gateway's PodDisruptionBudget (default: maxUnavailable 1 with 2 or more replicas, none with 1).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pod_disruption_budget: Option<PdbParams>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pod: Option<PodParams>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub service: Option<ServiceParams>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rproxy: Option<RproxyParams>,
	/// What the UI is shown (docs/DESIGN-v0.4.x.md, C).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ui: Option<UiParams>,
	/// What Gateways' own parameters may set: only in the GatewayClass's parameters.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub policy: Option<ParamsPolicy>,
}

/// One of `minAvailable` and `maxUnavailable`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
#[schemars(extend("x-kubernetes-validations" = [{"rule": "!(has(self.minAvailable) && has(self.maxUnavailable))", "message": "one of minAvailable and maxUnavailable"}]))]
pub struct PdbParams {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub min_available: Option<IntOrStr>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub max_unavailable: Option<IntOrStr>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PodParams {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub labels: Option<BTreeMap<String, String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub annotations: Option<BTreeMap<String, String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub resources: Option<ContainerResources>,
	/// Kubernetes TopologySpreadConstraints; without a labelSelector, the Gateway's pods are selected.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub topology_spread_constraints: Option<Vec<FreeForm>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub node_selector: Option<BTreeMap<String, String>>,
	/// Kubernetes Tolerations.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub tolerations: Option<Vec<FreeForm>>,
	/// A Kubernetes Affinity.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub affinity: Option<FreeForm>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub priority_class_name: Option<String>,
}

/// Kubernetes ResourceRequirements of each container.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ContainerResources {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub rproxy: Option<FreeForm>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub certsync: Option<FreeForm>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum ServiceType {
	LoadBalancer,
	NodePort,
	ClusterIP,
}

impl ServiceType {
	pub fn as_str(self) -> &'static str {
		match self {
			ServiceType::LoadBalancer => "LoadBalancer",
			ServiceType::NodePort => "NodePort",
			ServiceType::ClusterIP => "ClusterIP",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum TrafficPolicy {
	Local,
	Cluster,
}

impl TrafficPolicy {
	pub fn as_str(self) -> &'static str {
		match self {
			TrafficPolicy::Local => "Local",
			TrafficPolicy::Cluster => "Cluster",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
pub enum IpFamilyPolicy {
	SingleStack,
	PreferDualStack,
	RequireDualStack,
}

impl IpFamilyPolicy {
	pub fn as_str(self) -> &'static str {
		match self {
			IpFamilyPolicy::SingleStack => "SingleStack",
			IpFamilyPolicy::PreferDualStack => "PreferDualStack",
			IpFamilyPolicy::RequireDualStack => "RequireDualStack",
		}
	}
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ServiceParams {
	#[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
	pub type_: Option<ServiceType>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub external_traffic_policy: Option<TrafficPolicy>,
	/// Cannot change once the Service exists (create the Gateway again).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub load_balancer_class: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub load_balancer_source_ranges: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub ip_family_policy: Option<IpFamilyPolicy>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub labels: Option<BTreeMap<String, String>>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub annotations: Option<BTreeMap<String, String>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum LogLevel {
	Error,
	Warn,
	Info,
	Debug,
	Trace,
}

impl LogLevel {
	pub fn as_str(self) -> &'static str {
		match self {
			LogLevel::Error => "error",
			LogLevel::Warn => "warn",
			LogLevel::Info => "info",
			LogLevel::Debug => "debug",
			LogLevel::Trace => "trace",
		}
	}
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct RproxyParams {
	/// The rproxy image (`repo:tag` or `repo@sha256:...`): only in the GatewayClass's parameters.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub image: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub log_level: Option<LogLevel>,
	/// rproxy's global.performance, passed as environment variables.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub performance: Option<Performance>,
	/// How rproxy stops on SIGTERM (RPROXY_SHUTDOWN_DELAY, RPROXY_SHUTDOWN_DRAIN; terminationGracePeriodSeconds
	/// is both and 5 s more): keep accepting for `delay` while /readyz says draining, then let
	/// connections end for up to `drain`. Default: the controller's (15s, 25s). Only for rproxy with
	/// features.graceful_shutdown (v0.4.1); older rproxy keeps a preStop (docs/DESIGN-v0.4.x.md, E).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub shutdown: Option<Shutdown>,
	/// More RPROXY_* environment variables of the rproxy container (Kubernetes EnvVars): only in
	/// the GatewayClass's parameters; names the controller sets are refused.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub extra_env: Option<Vec<FreeForm>>,
}

/// rproxy's `global.performance` (rproxy-api docs/DESIGN-v0.4.md 12.).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Performance {
	/// RPROXY_WORKERS: tokio worker threads.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[schemars(range(min = 1, max = 1024))]
	pub workers: Option<u32>,
	/// RPROXY_UDP_SHARDS: 1 to 64, or auto.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub udp_shards: Option<IntOrStr>,
	/// RPROXY_CPU_AFFINITY: none, auto or a CPU list (0-3,6).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub cpu_affinity: Option<String>,
	/// RPROXY_BUSY_POLL_USECS: 0 (off) to 1000.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[schemars(range(max = 1000))]
	pub busy_poll_usecs: Option<u32>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub splice: Option<Splice>,
}

/// splice(2) for plain L4 TCP (RPROXY_SPLICE*).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Splice {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub enabled: Option<bool>,
	/// Bytes relayed in user space first (a number or "64KiB").
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub after: Option<IntOrStr>,
	/// 0 to 64.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[schemars(range(max = 64))]
	pub full_reads: Option<u32>,
	/// 0 (the kernel's) or 4KiB to 16MiB.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub pipe_size: Option<IntOrStr>,
}

/// Durations such as `5s`, `250ms`, `1m` (0s to 10m).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct Shutdown {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub delay: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub drain: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct UiParams {
	/// Listed for the UI (a Gateway can only turn the class's true into false).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub visible: Option<bool>,
}

/// What Gateways' own parameters may set (the GatewayClass's parameters only).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct ParamsPolicy {
	/// The fields a Gateway's parameters may set (default: replicas, podDisruptionBudget,
	/// pod.labels, pod.annotations, pod.resources, pod.topologySpreadConstraints,
	/// service.externalTrafficPolicy, service.loadBalancerSourceRanges, service.ipFamilyPolicy,
	/// service.labels, service.annotations, rproxy.logLevel, rproxy.performance, rproxy.shutdown, ui).
	/// rproxy.image, rproxy.extraEnv and policy cannot be listed.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub gateway_overrides: Option<Vec<String>>,
	/// The most replicas a Gateway's parameters may ask for (10 by default).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	#[schemars(range(min = 1))]
	pub max_replicas: Option<i32>,
	/// priorityClassName values a Gateway's parameters may use (with pod.priorityClassName allowed).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub allowed_priority_classes: Option<Vec<String>>,
	/// loadBalancerClass values a Gateway's parameters may use (with service.loadBalancerClass allowed).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub allowed_load_balancer_classes: Option<Vec<String>>,
}
