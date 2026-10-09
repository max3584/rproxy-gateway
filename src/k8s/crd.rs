//! rproxy's own CRDs (`rproxy.max3584.net/v1beta1`; `v1alpha1` is served too, with the same
//! schema, and deprecated: docs/DESIGN-v0.4.x.md 11.4; rproxy-api docs/DESIGN-v0.4.md 3.3):
//! settings Gateway API lacks.
//!
//! - `RproxyMiddleware`: `spec` has the shape of `http.middlewares.<name>`; used
//!   through an HTTPRoute `ExtensionRef` filter.
//! - `RproxyPolicy`: `spec.targetRefs` (Gateway, a listener, a Service) plus a
//!   rule's `limits`, `bandwidth`, `geoip`, `outlier_detection`, `allow_from`,
//!   `crowdsec` (policy attachment, GEP-713).
//! - `RproxyRule`: a rule verbatim in `spec.rule`, added to a Gateway's rule set
//!   (the escape hatch for what Gateway API cannot express).

use std::borrow::Cow;

use kube::CustomResource;
use schemars::{JsonSchema, Schema, SchemaGenerator, json_schema};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const GROUP: &str = "rproxy.max3584.net";
/// The version stored and written (v0.4.5).
pub const VERSION: &str = "v1beta1";
/// The first version: still served (same schema, no conversion), deprecated.
pub const DEPRECATED_VERSION: &str = "v1alpha1";

/// A JSON object kept as it is (`x-kubernetes-preserve-unknown-fields`): rproxy validates it.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize)]
#[serde(transparent)]
pub struct FreeForm(pub Map<String, Value>);

impl JsonSchema for FreeForm {
	fn schema_name() -> Cow<'static, str> {
		"FreeForm".into()
	}

	fn inline_schema() -> bool {
		true
	}

	fn json_schema(_: &mut SchemaGenerator) -> Schema {
		json_schema!({"type": "object", "x-kubernetes-preserve-unknown-fields": true})
	}
}

/// `spec` is one middleware of rproxy's `http.middlewares`, e.g. `{"rate_limit": {"average": 10}}`.
#[derive(CustomResource, Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[kube(group = "rproxy.max3584.net", version = "v1beta1", kind = "RproxyMiddleware", namespaced, shortname = "rpmw")]
#[kube(doc = "One rproxy HTTP middleware (the shape of http.middlewares.<name>), used through an HTTPRoute ExtensionRef filter")]
#[serde(transparent)]
pub struct RproxyMiddlewareSpec(pub FreeForm);

/// What a policy attaches to (GEP-713).
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct PolicyTargetRef {
	/// `gateway.networking.k8s.io` for a Gateway, `""` for a Service.
	#[serde(default)]
	pub group: String,
	/// `Gateway` or `Service`.
	pub kind: String,
	pub name: String,
	/// A listener of the Gateway (its rule only).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub section_name: Option<String>,
}

/// A rule's settings attached to Gateways, listeners or Services.
#[derive(CustomResource, Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[kube(group = "rproxy.max3584.net", version = "v1beta1", kind = "RproxyPolicy", namespaced, shortname = "rppol")]
#[kube(status = "PolicyStatus")]
#[kube(
	doc = "rproxy rule settings (limits, bandwidth, geoip, outlier_detection, allow_from, crowdsec, connect and response timeouts) attached to Gateways, listeners or Services"
)]
#[serde(rename_all = "camelCase")]
pub struct RproxyPolicySpec {
	pub target_refs: Vec<PolicyTargetRef>,
	/// A rule's `limits` (rproxy #165).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub limits: Option<FreeForm>,
	/// A rule's `bandwidth` (rproxy #166).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub bandwidth: Option<FreeForm>,
	/// A rule's `geoip` (rproxy #168).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub geoip: Option<FreeForm>,
	/// L4: a rule's `outlier_detection`; on a Service used by HTTPRoutes, the service's (rproxy #170).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub outlier_detection: Option<FreeForm>,
	/// A rule's `allow_from` (CIDRs).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub allow_from: Option<Vec<String>>,
	/// A rule's `crowdsec`.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub crowdsec: Option<bool>,
	/// How long rproxy may take to connect to a backend pod (`10s`, `500ms`; 100ms-10m): an L4 tcp
	/// rule's `connect_timeout` (rproxy v0.4.3), an `http` service's `timeouts.connect`. Replaces the
	/// controller's default (`--backend-connect-timeout-*`).
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub connect_timeout: Option<String>,
	/// How long rproxy waits for a backend's response headers after sending the request (an `http`
	/// service's `timeouts.response`; HTTPRoute and GRPCRoute backends). Replaces the controller's
	/// default (`--backend-response-timeout`); an HTTPRoute rule's own `timeouts` still win.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub response_timeout: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct PolicyStatus {
	#[serde(default)]
	pub ancestors: Vec<FreeForm>,
}

/// The Gateway whose rule set gets the rule.
#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct GatewayRef {
	pub name: String,
	/// Defaults to the RproxyRule's namespace.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub namespace: Option<String>,
}

/// One rproxy rule verbatim (the body of `POST /rules`), added to a Gateway's rule set.
#[derive(CustomResource, Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
#[kube(group = "rproxy.max3584.net", version = "v1beta1", kind = "RproxyRule", namespaced, shortname = "rprule")]
#[kube(status = "RuleStatus")]
#[kube(doc = "One rproxy rule verbatim (the body of POST /rules), added to a Gateway's rule set")]
#[kube(printcolumn = r#"{"name":"Gateway","type":"string","jsonPath":".spec.parentRef.name"}"#)]
#[kube(printcolumn = r#"{"name":"Programmed","type":"string","jsonPath":".status.conditions[?(@.type==\"Programmed\")].status"}"#)]
#[serde(rename_all = "camelCase")]
pub struct RproxyRuleSpec {
	pub parent_ref: GatewayRef,
	pub rule: FreeForm,
}

#[derive(Clone, Debug, Default, PartialEq, Deserialize, Serialize, JsonSchema)]
pub struct RuleStatus {
	#[serde(default)]
	pub conditions: Vec<FreeForm>,
}

/// The CRDs as one YAML stream (`rproxy-gateway crds`, the Helm chart's `crds/`).
pub fn crds_yaml() -> String {
	use kube::CustomResourceExt;
	let mut out = String::from("# Generated by `rproxy-gateway crds`; do not edit.\n");
	for crd in [RproxyMiddleware::crd(), RproxyPolicy::crd(), RproxyRule::crd(), crate::k8s::params::RproxyGatewayParameters::crd()] {
		out.push_str("---\n");
		out.push_str(&serde_saphyr::to_string(&with_deprecated_version(crd)).expect("a CRD serializes"));
	}
	out
}

/// Serves `v1alpha1` next to `v1beta1` (stored): the same schema, so no conversion webhook
/// (`strategy: None` only rewrites `apiVersion`). Objects made with v0.4.4 and earlier keep working
/// under either version; `v1alpha1` tells clients it is deprecated.
fn with_deprecated_version(
	mut crd: k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition,
) -> k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition {
	use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceConversion;
	let kind = crd.spec.names.kind.clone();
	let mut old = crd.spec.versions[0].clone();
	old.name = DEPRECATED_VERSION.to_string();
	old.storage = false;
	old.served = true;
	old.deprecated = Some(true);
	old.deprecation_warning = Some(format!("{GROUP}/{DEPRECATED_VERSION} {kind} is deprecated; use {GROUP}/{VERSION}"));
	crd.spec.versions.push(old);
	crd.spec.conversion = Some(CustomResourceConversion { strategy: "None".into(), webhook: None });
	crd
}

#[cfg(test)]
mod tests {
	#[test]
	fn both_versions_are_served_with_one_schema() {
		use kube::CustomResourceExt;
		let crd = super::with_deprecated_version(super::RproxyRule::crd());
		let v = &crd.spec.versions;
		assert_eq!((v[0].name.as_str(), v[0].storage, v[0].deprecated), ("v1beta1", true, None));
		assert_eq!((v[1].name.as_str(), v[1].storage, v[1].served, v[1].deprecated), ("v1alpha1", false, true, Some(true)));
		assert_eq!(v[0].schema, v[1].schema, "no conversion is needed");
		assert_eq!(v[0].subresources, v[1].subresources);
		assert_eq!(crd.spec.conversion.as_ref().unwrap().strategy, "None");
	}

	#[test]
	fn the_chart_has_the_current_crds() {
		let chart = include_str!("../../charts/rproxy-gateway/crds/rproxy.max3584.net.yaml");
		assert!(chart == super::crds_yaml(), "run: cargo run -- crds > charts/rproxy-gateway/crds/rproxy.max3584.net.yaml");
	}
}
