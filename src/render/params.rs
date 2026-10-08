//! `RproxyGatewayParameters`: finding the GatewayClass's and the Gateway's, checking them
//! and merging them into what a managed Gateway's rproxy is made of (docs/DESIGN-v0.4.x.md, A).
//!
//! - The GatewayClass's `parametersRef` must be in the controller's namespace; it is the
//!   administrator's and may carry `policy` (what Gateways may set).
//! - A Gateway's `spec.infrastructure.parametersRef` is in its own namespace (Gateway API's
//!   `LocalParametersReference`); it may set only what the class's `policy` allows.
//! - Merging (later wins): the controller's flags, the class's, the Gateway's. Scalars
//!   replace, maps merge by key, lists and objects replace as a whole (GEP-1867).
//!
//! Any error makes the Gateway `Accepted: False` (`InvalidParameters`) with a message naming
//! the field.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{Affinity, EnvVar, ResourceRequirements, Toleration, TopologySpreadConstraint};
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
use serde_json::Value;

use crate::k8s::crd::{FreeForm, GROUP};
use crate::k8s::gateway::{Gateway, GatewayClass};
use crate::k8s::params::{IntOrStr, Performance, RproxyGatewayParametersSpec as Spec};
use crate::render::world::World;

pub const KIND: &str = "RproxyGatewayParameters";

/// What resolving parameters needs from the controller's settings.
#[derive(Clone, Debug)]
pub struct ParamsOptions {
	/// The controller's namespace: the only one a GatewayClass's parametersRef may name.
	pub controller_namespace: String,
	/// Fleet mode: Gateways' parameters are not used (the fleet's pods come from the chart).
	pub fleet: bool,
	/// `--replicas` (replicas when no parameters set them).
	pub default_replicas: i32,
	/// `--service-annotation-prefix`: load balancer annotation prefixes Gateways may use.
	pub service_annotations: Vec<String>,
}

impl Default for ParamsOptions {
	fn default() -> Self {
		ParamsOptions {
			controller_namespace: "rproxy-gateway-system".into(),
			fleet: false,
			default_replicas: 1,
			service_annotations: vec![],
		}
	}
}

/// What a Gateway's parameters may set unless the class's `policy.gatewayOverrides` says otherwise.
pub const DEFAULT_OVERRIDES: &[&str] = &[
	"replicas",
	"podDisruptionBudget",
	"pod.labels",
	"pod.annotations",
	"pod.resources",
	"pod.topologySpreadConstraints",
	"service.externalTrafficPolicy",
	"service.loadBalancerSourceRanges",
	"service.ipFamilyPolicy",
	"service.labels",
	"service.annotations",
	"rproxy.logLevel",
	"rproxy.performance",
	"rproxy.shutdown",
	"ui",
];

/// What the class's `policy.gatewayOverrides` may open besides the defaults.
pub const OPENABLE: &[&str] =
	&["service.type", "service.loadBalancerClass", "pod.nodeSelector", "pod.tolerations", "pod.affinity", "pod.priorityClassName"];

/// What only the GatewayClass's parameters may set (never opened to Gateways).
pub const CLASS_ONLY: &[&str] = &["rproxy.image", "rproxy.extraEnv", "policy"];

/// The default of `policy.maxReplicas`.
pub const MAX_REPLICAS: i32 = 10;

/// Label and annotation prefixes the controller uses (refused in parameters).
const RESERVED_PREFIXES: &[&str] = &["rproxy.max3584.net/", "app.kubernetes.io/", "gateway.networking.k8s.io/"];

/// Environment variables of rproxy the controller sets (refused in `rproxy.extraEnv`): exact names,
/// and prefixes ending in `_` or `*`.
const RESERVED_ENV: &[&str] = &[
	"RPROXY_API_",
	"RPROXY_TOKEN_FILE",
	"RPROXY_TLS_",
	"RPROXY_FILES_",
	"RPROXY_CONFIG",
	"RPROXY_DATABASE_URL",
	"RPROXY_UPDATE*",
	"RPROXY_HANDOFF*",
	"RPROXY_STATIC_RULES",
	"RPROXY_SHUTDOWN_",
	"RPROXY_LOG_LEVEL",
	"RPROXY_WORKERS",
	"RPROXY_UDP_SHARDS",
	"RPROXY_CPU_AFFINITY",
	"RPROXY_BUSY_POLL_USECS",
	"RPROXY_SPLICE*",
];

/// Annotation prefixes of `spec.infrastructure` (and of Gateways' parameters) kept off the
/// Service: they ask load balancers for addresses (a tenant could take an address that is not theirs).
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

/// Whether a tenant's annotation may go onto the Service.
pub fn service_annotation_allowed(key: &str, allow: &[String]) -> bool {
	allow.iter().any(|p| key.starts_with(p.as_str())) || !SERVICE_ANNOTATION_DENY.iter().any(|p| key.starts_with(p))
}

fn lookup<'a>(world: &'a World, ns: &str, name: &str) -> Option<&'a Spec> {
	world.gateway_parameters.get(&(ns.to_string(), name.to_string())).map(|p| &p.spec)
}

fn check_kind(group: &str, kind: &str, name: &str) -> Result<(), String> {
	if group == GROUP && kind == KIND {
		Ok(())
	} else {
		Err(format!("parametersRef {group}/{kind} {name}: not supported (only {GROUP}/{KIND})"))
	}
}

/// The GatewayClass's parameters (`Ok(None)` without a `parametersRef`).
pub fn class_parameters(world: &World, class: &GatewayClass, o: &ParamsOptions) -> Result<Option<Spec>, String> {
	let Some(r) = &class.spec.parameters_ref else { return Ok(None) };
	check_kind(&r.group, &r.kind, &r.name)?;
	let ns = r.namespace.as_deref().unwrap_or_default();
	if ns != o.controller_namespace {
		return Err(format!(
			"parametersRef {KIND} {}: the namespace must be the controller's ({}), not {:?}",
			r.name, o.controller_namespace, ns
		));
	}
	let spec = lookup(world, ns, &r.name).ok_or_else(|| format!("{KIND} {ns}/{}: not found", r.name))?;
	let at = |e: String| format!("{KIND} {ns}/{}: {e}", r.name);
	check_policy(spec).map_err(at)?;
	check_values(spec).map_err(at)?;
	Ok(Some(spec.clone()))
}

/// A Gateway's parameters: its class's merged with its own (`Ok(None)` when neither has any).
pub fn gateway_parameters(world: &World, gw: &Gateway, o: &ParamsOptions) -> Result<Option<Spec>, String> {
	let class_name = &gw.spec.gateway_class_name;
	let class = match world.classes.iter().find(|c| c.metadata.name.as_deref() == Some(class_name.as_str())) {
		Some(c) => class_parameters(world, c, o).map_err(|e| format!("GatewayClass {class_name}: {e}"))?,
		None => None,
	};
	let own = match gw.spec.infrastructure.as_ref().and_then(|i| i.parameters_ref.as_ref()) {
		None => None,
		Some(r) => {
			check_kind(&r.group, &r.kind, &r.name)?;
			if o.fleet {
				return Err(format!("parametersRef {KIND} {}: not used in fleet mode (the fleet's pods are set in the chart)", r.name));
			}
			let ns = gw.metadata.namespace.as_deref().unwrap_or_default();
			let spec = lookup(world, ns, &r.name).ok_or_else(|| format!("{KIND} {ns}/{}: not found", r.name))?;
			let at = |e: String| format!("{KIND} {ns}/{}: {e}", r.name);
			check_gateway(spec, class.as_ref(), o).map_err(at)?;
			check_values(spec).map_err(at)?;
			Some(spec)
		}
	};
	let merged = match (class, own) {
		(None, None) => return Ok(None),
		(Some(c), None) => c,
		(None, Some(g)) => g.clone(),
		(Some(c), Some(g)) => merge(&c, g),
	};
	check_merged(&merged, o)?;
	Ok(Some(merged))
}

/// The fields `spec` sets, by their names in `policy.gatewayOverrides`.
pub fn set_paths(spec: &Spec) -> Vec<&'static str> {
	let mut out = vec![];
	let mut add = |set: bool, path: &'static str| {
		if set {
			out.push(path);
		}
	};
	add(spec.replicas.is_some(), "replicas");
	add(spec.pod_disruption_budget.is_some(), "podDisruptionBudget");
	if let Some(p) = &spec.pod {
		add(p.labels.is_some(), "pod.labels");
		add(p.annotations.is_some(), "pod.annotations");
		add(p.resources.is_some(), "pod.resources");
		add(p.topology_spread_constraints.is_some(), "pod.topologySpreadConstraints");
		add(p.node_selector.is_some(), "pod.nodeSelector");
		add(p.tolerations.is_some(), "pod.tolerations");
		add(p.affinity.is_some(), "pod.affinity");
		add(p.priority_class_name.is_some(), "pod.priorityClassName");
	}
	if let Some(s) = &spec.service {
		add(s.type_.is_some(), "service.type");
		add(s.external_traffic_policy.is_some(), "service.externalTrafficPolicy");
		add(s.load_balancer_class.is_some(), "service.loadBalancerClass");
		add(s.load_balancer_source_ranges.is_some(), "service.loadBalancerSourceRanges");
		add(s.ip_family_policy.is_some(), "service.ipFamilyPolicy");
		add(s.labels.is_some(), "service.labels");
		add(s.annotations.is_some(), "service.annotations");
	}
	if let Some(r) = &spec.rproxy {
		add(r.image.is_some(), "rproxy.image");
		add(r.log_level.is_some(), "rproxy.logLevel");
		add(r.performance.is_some(), "rproxy.performance");
		add(r.shutdown.is_some(), "rproxy.shutdown");
		add(r.extra_env.is_some(), "rproxy.extraEnv");
	}
	add(spec.ui.is_some(), "ui");
	add(spec.policy.is_some(), "policy");
	out
}

/// The class's `policy`: only fields that can be opened.
fn check_policy(spec: &Spec) -> Result<(), String> {
	let Some(p) = &spec.policy else { return Ok(()) };
	for f in p.gateway_overrides.iter().flatten() {
		if CLASS_ONLY.contains(&f.as_str()) {
			return Err(format!("spec.policy.gatewayOverrides: {f} cannot be opened to Gateways"));
		}
		if !DEFAULT_OVERRIDES.contains(&f.as_str()) && !OPENABLE.contains(&f.as_str()) {
			return Err(format!(
				"spec.policy.gatewayOverrides: {f}: not a field (one of {})",
				[DEFAULT_OVERRIDES, OPENABLE].concat().join(", ")
			));
		}
	}
	Ok(())
}

/// A Gateway's own parameters against what its class allows.
fn check_gateway(own: &Spec, class: Option<&Spec>, o: &ParamsOptions) -> Result<(), String> {
	let policy = class.and_then(|c| c.policy.clone()).unwrap_or_default();
	let allowed: Vec<String> =
		policy.gateway_overrides.clone().unwrap_or_else(|| DEFAULT_OVERRIDES.iter().map(|s| s.to_string()).collect());
	for path in set_paths(own) {
		if CLASS_ONLY.contains(&path) {
			return Err(format!("spec.{path}: only in the GatewayClass's parameters"));
		}
		if !allowed.iter().any(|a| a == path) {
			return Err(format!("spec.{path}: not allowed by the GatewayClass (policy.gatewayOverrides)"));
		}
	}
	if let Some(n) = own.replicas {
		let max = policy.max_replicas.unwrap_or(MAX_REPLICAS);
		if n > max {
			return Err(format!("spec.replicas: {n} is over the GatewayClass's policy.maxReplicas ({max})"));
		}
	}
	if let Some(pc) = own.pod.as_ref().and_then(|p| p.priority_class_name.as_ref()) {
		if !policy.allowed_priority_classes.iter().flatten().any(|a| a == pc) {
			return Err(format!("spec.pod.priorityClassName: {pc} is not in the GatewayClass's policy.allowedPriorityClasses"));
		}
	}
	if let Some(lbc) = own.service.as_ref().and_then(|s| s.load_balancer_class.as_ref()) {
		if !policy.allowed_load_balancer_classes.iter().flatten().any(|a| a == lbc) {
			return Err(format!("spec.service.loadBalancerClass: {lbc} is not in the GatewayClass's policy.allowedLoadBalancerClasses"));
		}
	}
	for k in own.service.as_ref().and_then(|s| s.annotations.as_ref()).into_iter().flatten().map(|(k, _)| k) {
		if !service_annotation_allowed(k, &o.service_annotations) {
			return Err(format!(
				"spec.service.annotations: {k} picks load balancer addresses; allowed only with the controller's --service-annotation-prefix"
			));
		}
	}
	let class_hides = class.and_then(|c| c.ui.as_ref()).and_then(|u| u.visible) == Some(false);
	if class_hides && own.ui.as_ref().and_then(|u| u.visible) == Some(true) {
		return Err("spec.ui.visible: the GatewayClass hides its Gateways from the UI (a Gateway can only set false)".into());
	}
	Ok(())
}

/// What the merged parameters must hold together (the PodDisruptionBudget against the replicas).
fn check_merged(spec: &Spec, o: &ParamsOptions) -> Result<(), String> {
	let replicas = spec.replicas.unwrap_or(o.default_replicas);
	if let Some(p) = &spec.pod_disruption_budget {
		let blocks = |v: &IntOrStr, min: bool| -> Result<bool, String> {
			Ok(match &v.0 {
				IntOrString::Int(n) => {
					if *n < 0 {
						return Err(format!("{n}: not negative"));
					}
					if min { *n >= replicas } else { *n == 0 }
				}
				IntOrString::String(s) => {
					let pct = percent(s).ok_or_else(|| format!("{s:?}: a number or a percentage"))?;
					if min { pct >= 100 } else { pct == 0 }
				}
			})
		};
		if let Some(v) = &p.min_available {
			if blocks(v, true).map_err(|e| format!("spec.podDisruptionBudget.minAvailable: {e}"))? {
				return Err(format!("spec.podDisruptionBudget.minAvailable: {} with {replicas} replicas would block every drain", show(v)));
			}
		}
		if let Some(v) = &p.max_unavailable {
			if blocks(v, false).map_err(|e| format!("spec.podDisruptionBudget.maxUnavailable: {e}"))? {
				return Err(format!("spec.podDisruptionBudget.maxUnavailable: {} would block every drain", show(v)));
			}
		}
	}
	Ok(())
}

fn show(v: &IntOrStr) -> String {
	match &v.0 {
		IntOrString::Int(n) => n.to_string(),
		IntOrString::String(s) => s.clone(),
	}
}

/// `25%` → 25.
fn percent(s: &str) -> Option<u32> {
	s.strip_suffix('%')?.parse().ok().filter(|p| *p <= 100)
}

/// Checks every value (both the class's and Gateways' parameters).
fn check_values(spec: &Spec) -> Result<(), String> {
	if let Some(p) = &spec.pod {
		labels("spec.pod.labels", p.labels.as_ref(), true, true)?;
		labels("spec.pod.annotations", p.annotations.as_ref(), false, true)?;
		if let Some(r) = &p.resources {
			for (name, v) in [("rproxy", &r.rproxy), ("certsync", &r.certsync)] {
				if let Some(v) = v {
					let rr: ResourceRequirements = read(v).map_err(|e| format!("spec.pod.resources.{name}: {e}"))?;
					for (k, q) in rr.limits.iter().flatten().chain(rr.requests.iter().flatten()) {
						if !quantity(&q.0) {
							return Err(format!("spec.pod.resources.{name}: {k}: {:?} is not a quantity", q.0));
						}
					}
				}
			}
		}
		for (i, c) in p.topology_spread_constraints.iter().flatten().enumerate() {
			let t: TopologySpreadConstraint = read(c).map_err(|e| format!("spec.pod.topologySpreadConstraints[{i}]: {e}"))?;
			if !["DoNotSchedule", "ScheduleAnyway"].contains(&t.when_unsatisfiable.as_str()) {
				return Err(format!("spec.pod.topologySpreadConstraints[{i}].whenUnsatisfiable: DoNotSchedule or ScheduleAnyway"));
			}
			if t.max_skew < 1 {
				return Err(format!("spec.pod.topologySpreadConstraints[{i}].maxSkew: at least 1"));
			}
		}
		labels("spec.pod.nodeSelector", p.node_selector.as_ref(), true, false)?;
		for (i, t) in p.tolerations.iter().flatten().enumerate() {
			let t: Toleration = read(t).map_err(|e| format!("spec.pod.tolerations[{i}]: {e}"))?;
			if !matches!(t.operator.as_deref(), None | Some("Exists" | "Equal")) {
				return Err(format!("spec.pod.tolerations[{i}].operator: Exists or Equal"));
			}
			if !matches!(t.effect.as_deref(), None | Some("" | "NoSchedule" | "PreferNoSchedule" | "NoExecute")) {
				return Err(format!("spec.pod.tolerations[{i}].effect: NoSchedule, PreferNoSchedule or NoExecute"));
			}
		}
		if let Some(a) = &p.affinity {
			let _: Affinity = read(a).map_err(|e| format!("spec.pod.affinity: {e}"))?;
		}
		if let Some(pc) = &p.priority_class_name {
			if !dns_subdomain(pc) {
				return Err(format!("spec.pod.priorityClassName: {pc:?} is not a name"));
			}
		}
	}
	if let Some(s) = &spec.service {
		labels("spec.service.labels", s.labels.as_ref(), true, true)?;
		labels("spec.service.annotations", s.annotations.as_ref(), false, true)?;
		if let Some(c) = &s.load_balancer_class {
			if !label_key(c) {
				return Err(format!("spec.service.loadBalancerClass: {c:?} is not a class name (such as example.com/lb)"));
			}
		}
		for r in s.load_balancer_source_ranges.iter().flatten() {
			if !r.contains('/') || r.parse::<crate::render::Cidr>().is_err() {
				return Err(format!("spec.service.loadBalancerSourceRanges: {r:?} is not a CIDR"));
			}
		}
	}
	if let Some(r) = &spec.rproxy {
		if let Some(i) = &r.image {
			image_ref(i).map_err(|e| format!("spec.rproxy.image: {e}"))?;
		}
		if let Some(p) = &r.performance {
			performance_env(p).map_err(|e| format!("spec.rproxy.performance.{e}"))?;
		}
		if let Some(sd) = &r.shutdown {
			for (name, v) in [("delay", &sd.delay), ("drain", &sd.drain)] {
				if let Some(v) = v {
					let d = duration(v).ok_or_else(|| format!("spec.rproxy.shutdown.{name}: {v:?} is not a duration (5s, 250ms, 1m)"))?;
					if d > std::time::Duration::from_secs(600) {
						return Err(format!("spec.rproxy.shutdown.{name}: at most 10m"));
					}
				}
			}
		}
		for (i, e) in r.extra_env.iter().flatten().enumerate() {
			let e: EnvVar = read(e).map_err(|e| format!("spec.rproxy.extraEnv[{i}]: {e}"))?;
			if !e.name.starts_with("RPROXY_") {
				return Err(format!("spec.rproxy.extraEnv[{i}].name: {}: only RPROXY_* variables", e.name));
			}
			if reserved_env(&e.name) {
				return Err(format!("spec.rproxy.extraEnv[{i}].name: {} is set by the controller", e.name));
			}
		}
	}
	Ok(())
}

fn read<T: serde::de::DeserializeOwned>(v: &FreeForm) -> Result<T, String> {
	serde_json::from_value(Value::Object(v.0.clone())).map_err(|e| e.to_string())
}

fn reserved_env(name: &str) -> bool {
	RESERVED_ENV.iter().any(|r| match r.strip_suffix('*') {
		Some(prefix) => name.starts_with(prefix),
		None if r.ends_with('_') => name.starts_with(r),
		None => name == *r,
	})
}

/// Label (or annotation) keys and values; `values`: label values are checked too; `reserved`:
/// the controller's prefixes are refused.
fn labels(field: &str, m: Option<&BTreeMap<String, String>>, values: bool, reserved: bool) -> Result<(), String> {
	for (k, v) in m.into_iter().flatten() {
		if reserved && RESERVED_PREFIXES.iter().any(|p| k.starts_with(p)) {
			return Err(format!("{field}: {k}: the controller's prefixes ({}) are not allowed", RESERVED_PREFIXES.join(", ")));
		}
		if !label_key(k) {
			return Err(format!("{field}: {k:?} is not a key"));
		}
		if values && !(v.is_empty() || (v.len() <= 63 && label_name(v))) {
			return Err(format!("{field}: {k}: {v:?} is not a label value"));
		}
	}
	Ok(())
}

/// `[prefix/]name`: a DNS subdomain prefix and a name of up to 63 characters.
fn label_key(k: &str) -> bool {
	let (prefix, name) = match k.rsplit_once('/') {
		Some((p, n)) => (Some(p), n),
		None => (None, k),
	};
	prefix.is_none_or(dns_subdomain) && !name.is_empty() && name.len() <= 63 && label_name(name)
}

fn label_name(s: &str) -> bool {
	let b = s.as_bytes();
	!b.is_empty()
		&& b[0].is_ascii_alphanumeric()
		&& b[b.len() - 1].is_ascii_alphanumeric()
		&& b.iter().all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
}

fn dns_subdomain(s: &str) -> bool {
	!s.is_empty()
		&& s.len() <= 253
		&& s.split('.').all(|p| {
			let b = p.as_bytes();
			!b.is_empty()
				&& b[0].is_ascii_alphanumeric()
				&& b[b.len() - 1].is_ascii_alphanumeric()
				&& b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
		})
}

/// A Kubernetes quantity: `500m`, `128Mi`, `1.5`, `2e3`.
fn quantity(s: &str) -> bool {
	let s = s.strip_prefix(['+', '-']).unwrap_or(s);
	let num_end = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
	let (num, suffix) = s.split_at(num_end);
	let num_ok = !num.is_empty() && num != "." && num.matches('.').count() <= 1;
	let suffix_ok = matches!(suffix, "" | "Ki" | "Mi" | "Gi" | "Ti" | "Pi" | "Ei" | "n" | "u" | "m" | "k" | "M" | "G" | "T" | "P" | "E")
		|| suffix.strip_prefix(['e', 'E']).is_some_and(|e| {
			let e = e.strip_prefix(['+', '-']).unwrap_or(e);
			!e.is_empty() && e.chars().all(|c| c.is_ascii_digit())
		});
	num_ok && suffix_ok
}

/// `repo:tag` or `repo@sha256:<64 hex>`.
fn image_ref(s: &str) -> Result<(), String> {
	if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c.is_control()) {
		return Err(format!("{s:?} is not an image reference"));
	}
	if let Some((repo, digest)) = s.split_once('@') {
		let hex = digest.strip_prefix("sha256:").unwrap_or("");
		if repo.is_empty() || hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
			return Err(format!("{s:?}: repo@sha256:<64 hex digits>"));
		}
		return Ok(());
	}
	let last = s.rsplit('/').next().unwrap_or(s);
	match last.split_once(':') {
		Some((name, tag)) if !name.is_empty() && !tag.is_empty() => Ok(()),
		_ => Err(format!("{s:?}: repo:tag or repo@sha256:...")),
	}
}

/// `5s`, `250ms`, `1m`, `0`.
pub fn duration(s: &str) -> Option<std::time::Duration> {
	let s = s.trim();
	if s == "0" {
		return Some(std::time::Duration::ZERO);
	}
	let (n, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit())?);
	let n: u64 = n.parse().ok()?;
	Some(match unit {
		"ms" => std::time::Duration::from_millis(n),
		"s" => std::time::Duration::from_secs(n),
		"m" => std::time::Duration::from_secs(n.checked_mul(60)?),
		_ => return None,
	})
}

/// A size in bytes: a number, or `64KiB`, `1MiB`, `16MiB`.
fn bytes(v: &IntOrStr) -> Result<u64, String> {
	match &v.0 {
		IntOrString::Int(n) => u64::try_from(*n).map_err(|_| format!("{n}: not negative")),
		IntOrString::String(s) => {
			let t = s.trim();
			let end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
			let (n, unit) = t.split_at(end);
			let n: u64 = n.parse().map_err(|_| format!("{s:?}: a number of bytes or 64KiB"))?;
			let mul = match unit.trim() {
				"" | "B" => 1,
				"KiB" | "K" | "k" => 1 << 10,
				"MiB" | "M" => 1 << 20,
				"GiB" | "G" => 1 << 30,
				_ => return Err(format!("{s:?}: a number of bytes or 64KiB")),
			};
			n.checked_mul(mul).ok_or_else(|| format!("{s:?}: too large"))
		}
	}
}

/// rproxy's environment variables for `performance` (an error names the field).
pub fn performance_env(p: &Performance) -> Result<Vec<(String, String)>, String> {
	let mut out = vec![];
	if let Some(w) = p.workers {
		if w == 0 {
			return Err("workers: at least 1".into());
		}
		out.push(("RPROXY_WORKERS".into(), w.to_string()));
	}
	if let Some(u) = &p.udp_shards {
		let v = match &u.0 {
			IntOrString::Int(n) if (1..=64).contains(n) => n.to_string(),
			IntOrString::String(s) if s == "auto" => s.clone(),
			IntOrString::String(s) if s.parse::<i32>().is_ok_and(|n| (1..=64).contains(&n)) => s.clone(),
			_ => return Err(format!("udpShards: {}: 1 to 64 or auto", show(u))),
		};
		out.push(("RPROXY_UDP_SHARDS".into(), v));
	}
	if let Some(c) = &p.cpu_affinity {
		let list = |s: &str| {
			!s.is_empty()
				&& s.split(',').all(|part| {
					let mut ends = part.splitn(2, '-');
					let a = ends.next().unwrap_or("");
					let b = ends.next();
					!a.is_empty()
						&& a.chars().all(|c| c.is_ascii_digit())
						&& b.is_none_or(|b| !b.is_empty() && b.chars().all(|c| c.is_ascii_digit()))
				})
		};
		if !(c == "none" || c == "auto" || list(c)) {
			return Err(format!("cpuAffinity: {c:?}: none, auto or a CPU list such as 0-3,6"));
		}
		out.push(("RPROXY_CPU_AFFINITY".into(), c.clone()));
	}
	if let Some(b) = p.busy_poll_usecs {
		if b > 1000 {
			return Err("busyPollUsecs: 0 to 1000".into());
		}
		out.push(("RPROXY_BUSY_POLL_USECS".into(), b.to_string()));
	}
	if let Some(s) = &p.splice {
		if let Some(e) = s.enabled {
			out.push(("RPROXY_SPLICE".into(), if e { "1" } else { "0" }.into()));
		}
		if let Some(a) = &s.after {
			out.push(("RPROXY_SPLICE_AFTER".into(), bytes(a).map_err(|e| format!("splice.after: {e}"))?.to_string()));
		}
		if let Some(f) = s.full_reads {
			if f > 64 {
				return Err("splice.fullReads: 0 to 64".into());
			}
			out.push(("RPROXY_SPLICE_FULL_READS".into(), f.to_string()));
		}
		if let Some(ps) = &s.pipe_size {
			let n = bytes(ps).map_err(|e| format!("splice.pipeSize: {e}"))?;
			if n != 0 && !(4 << 10..=16 << 20).contains(&n) {
				return Err("splice.pipeSize: 0 or 4KiB to 16MiB".into());
			}
			out.push(("RPROXY_SPLICE_PIPE_SIZE".into(), n.to_string()));
		}
	}
	Ok(out)
}

/// rproxy's extra environment variables from `spec` (logLevel, performance, extraEnv), checked before.
pub fn rproxy_env(spec: &Spec) -> Vec<EnvVar> {
	let mut out = vec![];
	let Some(r) = &spec.rproxy else { return out };
	let env = |n: &str, v: &str| EnvVar { name: n.into(), value: Some(v.into()), ..Default::default() };
	if let Some(l) = r.log_level {
		out.push(env("RPROXY_LOG_LEVEL", l.as_str()));
	}
	if let Some(p) = &r.performance {
		out.extend(performance_env(p).unwrap_or_default().iter().map(|(n, v)| env(n, v)));
	}
	out.extend(r.extra_env.iter().flatten().filter_map(|e| read::<EnvVar>(e).ok()));
	out
}

fn merge_map(base: &Option<BTreeMap<String, String>>, over: &Option<BTreeMap<String, String>>) -> Option<BTreeMap<String, String>> {
	match (base, over) {
		(Some(b), Some(o)) => {
			let mut m = b.clone();
			m.extend(o.clone());
			Some(m)
		}
		(b, o) => o.clone().or_else(|| b.clone()),
	}
}

/// `over` on top of `base`: scalars replace, maps merge by key, lists and objects replace.
/// `policy` stays the class's (`base`).
pub fn merge(base: &Spec, over: &Spec) -> Spec {
	let pod = match (&base.pod, &over.pod) {
		(Some(b), Some(o)) => Some(crate::k8s::params::PodParams {
			labels: merge_map(&b.labels, &o.labels),
			annotations: merge_map(&b.annotations, &o.annotations),
			resources: match (&b.resources, &o.resources) {
				(Some(br), Some(or)) => Some(crate::k8s::params::ContainerResources {
					rproxy: or.rproxy.clone().or_else(|| br.rproxy.clone()),
					certsync: or.certsync.clone().or_else(|| br.certsync.clone()),
				}),
				(b, o) => o.clone().or_else(|| b.clone()),
			},
			topology_spread_constraints: o.topology_spread_constraints.clone().or_else(|| b.topology_spread_constraints.clone()),
			node_selector: merge_map(&b.node_selector, &o.node_selector),
			tolerations: o.tolerations.clone().or_else(|| b.tolerations.clone()),
			affinity: o.affinity.clone().or_else(|| b.affinity.clone()),
			priority_class_name: o.priority_class_name.clone().or_else(|| b.priority_class_name.clone()),
		}),
		(b, o) => o.clone().or_else(|| b.clone()),
	};
	let service = match (&base.service, &over.service) {
		(Some(b), Some(o)) => Some(crate::k8s::params::ServiceParams {
			type_: o.type_.or(b.type_),
			external_traffic_policy: o.external_traffic_policy.or(b.external_traffic_policy),
			load_balancer_class: o.load_balancer_class.clone().or_else(|| b.load_balancer_class.clone()),
			load_balancer_source_ranges: o.load_balancer_source_ranges.clone().or_else(|| b.load_balancer_source_ranges.clone()),
			ip_family_policy: o.ip_family_policy.or(b.ip_family_policy),
			labels: merge_map(&b.labels, &o.labels),
			annotations: merge_map(&b.annotations, &o.annotations),
		}),
		(b, o) => o.clone().or_else(|| b.clone()),
	};
	let rproxy = match (&base.rproxy, &over.rproxy) {
		(Some(b), Some(o)) => Some(crate::k8s::params::RproxyParams {
			image: o.image.clone().or_else(|| b.image.clone()),
			log_level: o.log_level.or(b.log_level),
			performance: o.performance.clone().or_else(|| b.performance.clone()),
			shutdown: o.shutdown.clone().or_else(|| b.shutdown.clone()),
			extra_env: o.extra_env.clone().or_else(|| b.extra_env.clone()),
		}),
		(b, o) => o.clone().or_else(|| b.clone()),
	};
	Spec {
		replicas: over.replicas.or(base.replicas),
		pod_disruption_budget: over.pod_disruption_budget.clone().or_else(|| base.pod_disruption_budget.clone()),
		pod,
		service,
		rproxy,
		ui: match (&base.ui, &over.ui) {
			(Some(b), Some(o)) => Some(crate::k8s::params::UiParams { visible: o.visible.or(b.visible) }),
			(b, o) => o.clone().or_else(|| b.clone()),
		},
		policy: base.policy.clone(),
	}
}

/// Kubernetes objects from the free-form fields (checked before; unreadable ones are left out).
pub fn objects<T: serde::de::DeserializeOwned>(list: Option<&Vec<FreeForm>>) -> Option<Vec<T>> {
	list.map(|l| l.iter().filter_map(|v| read(v).ok()).collect())
}

pub fn object<T: serde::de::DeserializeOwned>(v: Option<&FreeForm>) -> Option<T> {
	v.and_then(|v| read(v).ok())
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::render::tests::world;

	const CLASS: &str = r#"
apiVersion: gateway.networking.k8s.io/v1
kind: GatewayClass
metadata: {name: rproxy}
spec:
  controllerName: rproxy.max3584.net/gateway-controller
  parametersRef: {group: rproxy.max3584.net, kind: RproxyGatewayParameters, name: rproxy-default, namespace: rproxy-gateway-system}
"#;

	fn gateway(params: &str) -> String {
		format!(
			r#"
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {{name: web, namespace: team-a}}
spec:
  gatewayClassName: rproxy
  infrastructure: {{parametersRef: {{group: rproxy.max3584.net, kind: RproxyGatewayParameters, name: web}}}}
  listeners: [{{name: http, port: 80, protocol: HTTP}}]
---
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyGatewayParameters
metadata: {{name: web, namespace: team-a}}
spec:
{params}
"#
		)
	}

	fn class_params(spec: &str) -> String {
		format!(
			"\n---\napiVersion: rproxy.max3584.net/v1alpha1\nkind: RproxyGatewayParameters\nmetadata: {{name: rproxy-default, namespace: rproxy-gateway-system}}\nspec:\n{spec}\n"
		)
	}

	fn resolve(yaml: &str) -> Result<Option<Spec>, String> {
		let w = world(yaml);
		gateway_parameters(&w, &w.gateways[0], &ParamsOptions::default())
	}

	#[test]
	fn merging() {
		let yaml = format!(
			"{CLASS}{}{}",
			class_params(
				"  replicas: 2\n  pod: {labels: {a: class, b: class}, tolerations: [{key: x, operator: Exists}], resources: {rproxy: {requests: {cpu: 100m}}, certsync: {requests: {cpu: 5m}}}}\n  service: {type: NodePort, annotations: {note: class}}\n  rproxy: {logLevel: debug, performance: {workers: 2, busyPollUsecs: 50}}\n  ui: {visible: true}"
			),
			gateway(
				"  replicas: 3\n  pod: {labels: {b: gw, c: gw}, resources: {rproxy: {limits: {memory: 512Mi}}}}\n  service: {annotations: {other: gw}}\n  rproxy: {performance: {workers: 4}}"
			)
		);
		let p = resolve(&yaml).unwrap().unwrap();
		assert_eq!(p.replicas, Some(3), "the Gateway's scalar wins");
		let pod = p.pod.unwrap();
		let l = pod.labels.unwrap();
		assert_eq!((l["a"].as_str(), l["b"].as_str(), l["c"].as_str()), ("class", "gw", "gw"), "maps merge by key");
		assert_eq!(pod.tolerations.unwrap().len(), 1, "the class's list stays when the Gateway sets none");
		let r = pod.resources.unwrap();
		assert!(r.rproxy.unwrap().0.get("requests").is_none(), "objects replace as a whole");
		assert!(r.certsync.is_some());
		let svc = p.service.unwrap();
		assert_eq!(svc.type_.map(|t| t.as_str()), Some("NodePort"));
		assert_eq!(svc.annotations.unwrap().len(), 2);
		let rp = p.rproxy.unwrap();
		assert_eq!(rp.log_level.map(|l| l.as_str()), Some("debug"));
		let perf = rp.performance.unwrap();
		assert_eq!((perf.workers, perf.busy_poll_usecs), (Some(4), None), "performance replaces as a whole");
		// no references: nothing
		let w = world(
			&gateway("  replicas: 2")
				.replace("  infrastructure: {parametersRef: {group: rproxy.max3584.net, kind: RproxyGatewayParameters, name: web}}\n", ""),
		);
		assert_eq!(gateway_parameters(&w, &w.gateways[0], &ParamsOptions::default()), Ok(None));
	}

	#[test]
	fn references() {
		let err = |yaml: &str| resolve(yaml).unwrap_err();
		assert!(err(&gateway("  replicas: 2").replace("name: web}}", "name: missing}}")).contains("team-a/missing: not found"));
		assert!(
			err(&gateway("  replicas: 2").replace("kind: RproxyGatewayParameters, name: web", "kind: ConfigMap, name: web"))
				.contains("not supported")
		);
		// the class's reference: the controller's namespace only, and must exist
		let other_ns = CLASS.replace("namespace: rproxy-gateway-system", "namespace: team-a");
		assert!(err(&format!("{other_ns}{}", gateway("  replicas: 2"))).contains("must be the controller's"));
		assert!(
			err(&format!("{CLASS}{}", gateway("  replicas: 2")))
				.contains("GatewayClass rproxy: RproxyGatewayParameters rproxy-gateway-system/rproxy-default: not found")
		);
		// fleet: Gateways' parameters are not used
		let w = world(&gateway("  replicas: 2"));
		let fleet = ParamsOptions { fleet: true, ..Default::default() };
		assert!(gateway_parameters(&w, &w.gateways[0], &fleet).unwrap_err().contains("fleet"));
	}

	#[test]
	fn policy() {
		let err = |class: &str, gw: &str| resolve(&format!("{CLASS}{}{}", class_params(class), gateway(gw))).unwrap_err();
		let ok = |class: &str, gw: &str| resolve(&format!("{CLASS}{}{}", class_params(class), gateway(gw))).unwrap();
		// tenants: the defaults
		ok("  replicas: 1", "  replicas: 2\n  pod: {labels: {x: z}}\n  service: {externalTrafficPolicy: Cluster}");
		assert_eq!(
			err("  replicas: 1", "  pod: {tolerations: [{key: x, operator: Exists}]}"),
			"RproxyGatewayParameters team-a/web: spec.pod.tolerations: not allowed by the GatewayClass (policy.gatewayOverrides)"
		);
		assert!(err("  replicas: 1", "  service: {type: NodePort}").contains("spec.service.type: not allowed"));
		assert!(err("  replicas: 1", "  rproxy: {image: example.com/rproxy:1}").contains("only in the GatewayClass's parameters"));
		assert!(err("  replicas: 1", "  policy: {maxReplicas: 99}").contains("spec.policy: only in the GatewayClass's"));
		assert!(err("  replicas: 1", "  rproxy: {extraEnv: [{name: RPROXY_X, value: '1'}]}").contains("spec.rproxy.extraEnv: only"));
		// opened by the class
		let open = "  policy: {gatewayOverrides: [replicas, pod.tolerations, pod.priorityClassName, service.loadBalancerClass], maxReplicas: 3, allowedPriorityClasses: [high], allowedLoadBalancerClasses: [example.com/lb]}";
		ok(open, "  pod: {tolerations: [{key: x, operator: Exists}]}");
		assert!(err(open, "  pod: {labels: {a: b}}").contains("spec.pod.labels: not allowed"), "the list replaces the defaults");
		assert!(err(open, "  replicas: 4").contains("over the GatewayClass's policy.maxReplicas (3)"));
		ok(open, "  pod: {priorityClassName: high}");
		assert!(err(open, "  pod: {priorityClassName: system-node-critical}").contains("allowedPriorityClasses"));
		ok(open, "  service: {loadBalancerClass: example.com/lb}");
		assert!(err(open, "  service: {loadBalancerClass: other.io/lb}").contains("allowedLoadBalancerClasses"));
		// the class itself: what can be opened
		assert!(err("  policy: {gatewayOverrides: [rproxy.image]}", "  replicas: 1").contains("cannot be opened"));
		assert!(err("  policy: {gatewayOverrides: [pod.nothing]}", "  replicas: 1").contains("not a field"));
		// the class's own values are not bound by its policy
		ok(
			"  pod: {priorityClassName: system-cluster-critical}\n  rproxy: {image: 'example.com/rproxy@sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef'}",
			"  replicas: 1",
		);
		// load balancer annotations from tenants: only through the allow list
		assert!(
			err("  replicas: 1", "  service: {annotations: {metallb.universe.tf/loadBalancerIPs: 10.0.0.1}}")
				.contains("--service-annotation-prefix")
		);
		ok("  service: {annotations: {metallb.universe.tf/address-pool: a}}", "  service: {annotations: {note: x}}");
		let w = world(&format!(
			"{CLASS}{}{}",
			class_params("  replicas: 1"),
			gateway("  service: {annotations: {metallb.universe.tf/loadBalancerIPs: 10.0.0.1}}")
		));
		let o = ParamsOptions { service_annotations: vec!["metallb.universe.tf/".into()], ..Default::default() };
		assert!(gateway_parameters(&w, &w.gateways[0], &o).is_ok());
		// ui: a Gateway can only hide itself
		assert!(err("  ui: {visible: false}", "  ui: {visible: true}").contains("spec.ui.visible"));
		ok("  ui: {visible: true}", "  ui: {visible: false}");
	}

	#[test]
	fn values() {
		let err = |gw: &str| resolve(&gateway(gw)).unwrap_err();
		let ok = |gw: &str| resolve(&gateway(gw)).unwrap().unwrap();
		assert!(err("  pod: {labels: {app.kubernetes.io/name: x}}").contains("the controller's prefixes"));
		assert!(err("  service: {labels: {rproxy.max3584.net/gateway: x}}").contains("the controller's prefixes"));
		assert!(err("  pod: {labels: {'bad key!': x}}").contains("not a key"));
		assert!(err("  pod: {labels: {a: 'not a value'}}").contains("not a label value"));
		ok("  pod: {annotations: {note: 'any text at all'}}");
		assert!(err("  pod: {resources: {rproxy: {requests: {cpu: lots}}}}").contains("not a quantity"));
		ok("  pod: {resources: {rproxy: {requests: {cpu: 500m, memory: 128Mi}, limits: {memory: 1.5Gi}}}}");
		assert!(err("  pod: {topologySpreadConstraints: [{maxSkew: 1, topologyKey: zone}]}").contains("topologySpreadConstraints[0]"));
		assert!(
			err("  pod: {topologySpreadConstraints: [{maxSkew: 1, topologyKey: zone, whenUnsatisfiable: Maybe}]}")
				.contains("whenUnsatisfiable")
		);
		ok("  pod: {topologySpreadConstraints: [{maxSkew: 1, topologyKey: zone, whenUnsatisfiable: ScheduleAnyway}]}");
		assert!(err("  service: {loadBalancerSourceRanges: [10.0.0.1]}").contains("not a CIDR"));
		ok("  service: {loadBalancerSourceRanges: [10.0.0.0/8, '2001:db8::/32']}");
		assert!(err("  rproxy: {performance: {udpShards: 65}}").contains("udpShards"));
		assert!(err("  rproxy: {performance: {cpuAffinity: 'all'}}").contains("cpuAffinity"));
		assert!(err("  rproxy: {performance: {splice: {pipeSize: 1KiB}}}").contains("pipeSize"));
		assert!(err("  rproxy: {shutdown: {delay: '5'}}").contains("not a duration"));
		assert!(err("  rproxy: {shutdown: {drain: 11m}}").contains("at most 10m"));
		ok("  rproxy: {shutdown: {delay: 5s, drain: 25s}}");
		// the PodDisruptionBudget against the replicas
		assert!(err("  replicas: 2\n  podDisruptionBudget: {minAvailable: 2}").contains("would block every drain"));
		assert!(err("  replicas: 2\n  podDisruptionBudget: {maxUnavailable: 0}").contains("would block every drain"));
		assert!(err("  replicas: 3\n  podDisruptionBudget: {minAvailable: 100%}").contains("would block every drain"));
		ok("  replicas: 3\n  podDisruptionBudget: {minAvailable: 2}");
		ok("  replicas: 3\n  podDisruptionBudget: {maxUnavailable: 34%}");
		assert!(err("  podDisruptionBudget: {maxUnavailable: half}").contains("a number or a percentage"));
	}

	#[test]
	fn class_only_values() {
		let err = |class: &str| resolve(&format!("{CLASS}{}{}", class_params(class), gateway("  replicas: 1"))).unwrap_err();
		assert!(err("  rproxy: {image: 'rproxy latest'}").contains("spec.rproxy.image"));
		assert!(err("  rproxy: {image: example.com/rproxy}").contains("repo:tag"));
		assert!(err("  rproxy: {extraEnv: [{name: HOME, value: /}]}").contains("only RPROXY_*"));
		assert!(err("  rproxy: {extraEnv: [{name: RPROXY_TOKEN_FILE, value: /x}]}").contains("set by the controller"));
		assert!(err("  rproxy: {extraEnv: [{name: RPROXY_API_ADDR, value: 127.0.0.1}]}").contains("set by the controller"));
		assert!(err("  rproxy: {extraEnv: [{name: RPROXY_SPLICE_AFTER, value: '1'}]}").contains("set by the controller"));
		let p = resolve(&format!(
			"{CLASS}{}{}",
			class_params("  rproxy: {logLevel: debug, extraEnv: [{name: RPROXY_GEOIP_DB, value: /x}], performance: {workers: 4, udpShards: auto, cpuAffinity: '0-3,6', busyPollUsecs: 50, splice: {enabled: false, after: 64KiB, fullReads: 4, pipeSize: 1MiB}}}"),
			gateway("  replicas: 1")
		))
		.unwrap()
		.unwrap();
		let env: Vec<(String, String)> = rproxy_env(&p).into_iter().map(|e| (e.name, e.value.unwrap_or_default())).collect();
		let get = |n: &str| env.iter().find(|(k, _)| k == n).map(|(_, v)| v.as_str());
		assert_eq!(get("RPROXY_LOG_LEVEL"), Some("debug"));
		assert_eq!(get("RPROXY_WORKERS"), Some("4"));
		assert_eq!(get("RPROXY_UDP_SHARDS"), Some("auto"));
		assert_eq!(get("RPROXY_CPU_AFFINITY"), Some("0-3,6"));
		assert_eq!(get("RPROXY_SPLICE"), Some("0"));
		assert_eq!(get("RPROXY_SPLICE_AFTER"), Some("65536"), "rproxy's environment takes bytes");
		assert_eq!(get("RPROXY_SPLICE_PIPE_SIZE"), Some("1048576"));
		assert_eq!(get("RPROXY_GEOIP_DB"), Some("/x"));
	}

	#[test]
	fn helpers() {
		assert!(quantity("500m") && quantity("1.5Gi") && quantity("2e3") && quantity("128974848") && !quantity("1.2.3") && !quantity("Mi"));
		assert_eq!(duration("250ms"), Some(std::time::Duration::from_millis(250)));
		assert_eq!(duration("1m"), Some(std::time::Duration::from_secs(60)));
		assert_eq!(duration("0"), Some(std::time::Duration::ZERO));
		assert_eq!(duration("5"), None);
		assert!(label_key("example.com/a-b_c.d") && !label_key("Example.com/a") && !label_key("a/"));
		assert!(service_annotation_allowed("note", &[]));
		assert!(!service_annotation_allowed("metallb.universe.tf/loadBalancerIPs", &[]));
		assert_eq!(set_paths(&Spec { replicas: Some(1), ..Default::default() }), vec!["replicas"]);
	}
}
