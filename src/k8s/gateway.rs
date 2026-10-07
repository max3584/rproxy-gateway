//! The parts of Gateway API (gateway.networking.k8s.io, v1) the controller reads.
//!
//! Objects are watched as dynamic objects (the served version is found by
//! discovery) and read into these types; fields the controller does not use are
//! ignored. Status is written as JSON (`crate::status`).

use std::collections::BTreeMap;

use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};
use serde::{Deserialize, Serialize};

pub const GROUP: &str = "gateway.networking.k8s.io";

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GatewayClass {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: GatewayClassSpec,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayClassSpec {
	pub controller_name: String,
	#[serde(default)]
	pub parameters_ref: Option<ParametersReference>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParametersReference {
	#[serde(default)]
	pub group: String,
	#[serde(default)]
	pub kind: String,
	pub name: String,
	#[serde(default)]
	pub namespace: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Gateway {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: GatewaySpec,
	#[serde(default)]
	pub status: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewaySpec {
	pub gateway_class_name: String,
	#[serde(default)]
	pub listeners: Vec<Listener>,
	#[serde(default)]
	pub addresses: Vec<GatewayAddress>,
	#[serde(default)]
	pub infrastructure: Option<GatewayInfrastructure>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GatewayInfrastructure {
	#[serde(default)]
	pub labels: BTreeMap<String, String>,
	#[serde(default)]
	pub annotations: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct GatewayAddress {
	#[serde(default, rename = "type")]
	pub kind: Option<String>,
	pub value: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Listener {
	pub name: String,
	#[serde(default)]
	pub hostname: Option<String>,
	pub port: i32,
	pub protocol: String,
	#[serde(default)]
	pub tls: Option<ListenerTls>,
	#[serde(default)]
	pub allowed_routes: Option<AllowedRoutes>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListenerTls {
	#[serde(default)]
	pub mode: Option<String>,
	#[serde(default)]
	pub certificate_refs: Vec<SecretObjectReference>,
	#[serde(default)]
	pub options: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedRoutes {
	#[serde(default)]
	pub namespaces: Option<RouteNamespaces>,
	#[serde(default)]
	pub kinds: Option<Vec<RouteGroupKind>>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct RouteNamespaces {
	#[serde(default)]
	pub from: Option<String>,
	#[serde(default)]
	pub selector: Option<LabelSelector>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub struct RouteGroupKind {
	#[serde(default)]
	pub group: Option<String>,
	pub kind: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SecretObjectReference {
	#[serde(default)]
	pub group: Option<String>,
	#[serde(default)]
	pub kind: Option<String>,
	pub name: String,
	#[serde(default)]
	pub namespace: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ParentReference {
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub group: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub kind: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub namespace: Option<String>,
	pub name: String,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub section_name: Option<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub port: Option<i32>,
}

/// An HTTPRoute.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct HttpRoute {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: HttpRouteSpec,
	#[serde(default)]
	pub status: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteSpec {
	#[serde(default)]
	pub parent_refs: Vec<ParentReference>,
	#[serde(default)]
	pub hostnames: Vec<String>,
	#[serde(default)]
	pub rules: Vec<HttpRouteRule>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteRule {
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub matches: Vec<HttpRouteMatch>,
	#[serde(default)]
	pub filters: Vec<HttpRouteFilter>,
	#[serde(default)]
	pub backend_refs: Vec<HttpBackendRef>,
	#[serde(default)]
	pub timeouts: Option<HttpRouteTimeouts>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteTimeouts {
	#[serde(default)]
	pub request: Option<String>,
	#[serde(default)]
	pub backend_request: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteMatch {
	#[serde(default)]
	pub path: Option<HttpPathMatch>,
	#[serde(default)]
	pub headers: Vec<ValueMatch>,
	#[serde(default)]
	pub query_params: Vec<ValueMatch>,
	#[serde(default)]
	pub method: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct HttpPathMatch {
	/// `Exact`, `PathPrefix` (the default) or `RegularExpression`.
	#[serde(default, rename = "type")]
	pub kind: Option<String>,
	#[serde(default)]
	pub value: Option<String>,
}

/// A header or query parameter match.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ValueMatch {
	/// `Exact` (the default) or `RegularExpression`.
	#[serde(default, rename = "type")]
	pub kind: Option<String>,
	pub name: String,
	pub value: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteFilter {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub request_header_modifier: Option<HeaderModifier>,
	#[serde(default)]
	pub response_header_modifier: Option<HeaderModifier>,
	#[serde(default)]
	pub request_redirect: Option<RequestRedirect>,
	#[serde(default)]
	pub url_rewrite: Option<UrlRewrite>,
	#[serde(default)]
	pub extension_ref: Option<LocalObjectReference>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct HeaderModifier {
	#[serde(default)]
	pub set: Vec<HttpHeader>,
	#[serde(default)]
	pub add: Vec<HttpHeader>,
	#[serde(default)]
	pub remove: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct HttpHeader {
	pub name: String,
	pub value: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RequestRedirect {
	#[serde(default)]
	pub scheme: Option<String>,
	#[serde(default)]
	pub hostname: Option<String>,
	#[serde(default)]
	pub path: Option<PathModifier>,
	#[serde(default)]
	pub port: Option<i32>,
	#[serde(default)]
	pub status_code: Option<i32>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UrlRewrite {
	#[serde(default)]
	pub hostname: Option<String>,
	#[serde(default)]
	pub path: Option<PathModifier>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PathModifier {
	/// `ReplaceFullPath` or `ReplacePrefixMatch`.
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub replace_full_path: Option<String>,
	#[serde(default)]
	pub replace_prefix_match: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct LocalObjectReference {
	#[serde(default)]
	pub group: String,
	pub kind: String,
	pub name: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpBackendRef {
	#[serde(flatten)]
	pub backend: BackendRef,
	#[serde(default)]
	pub filters: Vec<HttpRouteFilter>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BackendRef {
	#[serde(default)]
	pub group: Option<String>,
	#[serde(default)]
	pub kind: Option<String>,
	pub name: String,
	#[serde(default)]
	pub namespace: Option<String>,
	#[serde(default)]
	pub port: Option<i32>,
	#[serde(default)]
	pub weight: Option<i32>,
}

/// A TLSRoute, TCPRoute or UDPRoute (the same shape; TCP and UDP routes have no hostnames).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct L4Route {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: L4RouteSpec,
	#[serde(default)]
	pub status: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L4RouteSpec {
	#[serde(default)]
	pub parent_refs: Vec<ParentReference>,
	#[serde(default)]
	pub hostnames: Vec<String>,
	#[serde(default)]
	pub rules: Vec<L4RouteRule>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct L4RouteRule {
	#[serde(default)]
	pub name: Option<String>,
	#[serde(default)]
	pub backend_refs: Vec<BackendRef>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReferenceGrant {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: ReferenceGrantSpec,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReferenceGrantSpec {
	#[serde(default)]
	pub from: Vec<ReferenceGrantFrom>,
	#[serde(default)]
	pub to: Vec<ReferenceGrantTo>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReferenceGrantFrom {
	#[serde(default)]
	pub group: String,
	pub kind: String,
	pub namespace: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct ReferenceGrantTo {
	#[serde(default)]
	pub group: String,
	pub kind: String,
	#[serde(default)]
	pub name: Option<String>,
}
