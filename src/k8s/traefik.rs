//! The Traefik CRDs read for migration (`traefik.io/v1alpha1`, also the older
//! `traefik.containo.us/v1alpha1`): IngressRoute, IngressRouteTCP,
//! IngressRouteUDP, Middleware, TLSOption. Read only; nothing is written back.

use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use serde::Deserialize;
use serde_json::{Map, Value};

pub const GROUPS: &[&str] = &["traefik.io", "traefik.containo.us"];

/// A reference to another Traefik object (`name`, optional `namespace`).
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ObjectRef {
	pub name: String,
	#[serde(default)]
	pub namespace: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct IngressRoute {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: IngressRouteSpec,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressRouteSpec {
	#[serde(default)]
	pub entry_points: Vec<String>,
	#[serde(default)]
	pub routes: Vec<HttpRouteEntry>,
	#[serde(default)]
	pub tls: Option<RouteTls>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HttpRouteEntry {
	#[serde(rename = "match", default)]
	pub rule: String,
	#[serde(default)]
	pub priority: Option<i64>,
	#[serde(default)]
	pub middlewares: Vec<ObjectRef>,
	#[serde(default)]
	pub services: Vec<ServiceRef>,
}

/// A backend of a Traefik route.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRef {
	pub name: String,
	#[serde(default)]
	pub namespace: Option<String>,
	/// `Service` (default) or `TraefikService`.
	#[serde(default)]
	pub kind: Option<String>,
	/// A Service port number or name.
	#[serde(default)]
	pub port: Option<Value>,
	#[serde(default)]
	pub weight: Option<i64>,
	#[serde(default)]
	pub scheme: Option<String>,
	#[serde(default)]
	pub pass_host_header: Option<bool>,
	#[serde(default)]
	pub proxy_protocol: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteTls {
	#[serde(default)]
	pub secret_name: Option<String>,
	#[serde(default)]
	pub options: Option<ObjectRef>,
	#[serde(default)]
	pub cert_resolver: Option<String>,
	#[serde(default)]
	pub domains: Vec<Domain>,
	/// IngressRouteTCP only.
	#[serde(default)]
	pub passthrough: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Domain {
	#[serde(default)]
	pub main: Option<String>,
	#[serde(default)]
	pub sans: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct IngressRouteTcp {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: IngressRouteTcpSpec,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressRouteTcpSpec {
	#[serde(default)]
	pub entry_points: Vec<String>,
	#[serde(default)]
	pub routes: Vec<TcpRouteEntry>,
	#[serde(default)]
	pub tls: Option<RouteTls>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TcpRouteEntry {
	#[serde(rename = "match", default)]
	pub rule: String,
	#[serde(default)]
	pub services: Vec<ServiceRef>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct IngressRouteUdp {
	#[serde(default)]
	pub metadata: ObjectMeta,
	pub spec: IngressRouteUdpSpec,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IngressRouteUdpSpec {
	#[serde(default)]
	pub entry_points: Vec<String>,
	#[serde(default)]
	pub routes: Vec<UdpRouteEntry>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct UdpRouteEntry {
	#[serde(default)]
	pub services: Vec<ServiceRef>,
}

/// A Middleware: `spec` is `{<type>: {settings}}` as in Traefik's dynamic configuration.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Middleware {
	#[serde(default)]
	pub metadata: ObjectMeta,
	#[serde(default)]
	pub spec: Map<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct TlsOption {
	#[serde(default)]
	pub metadata: ObjectMeta,
	#[serde(default)]
	pub spec: Map<String, Value>,
}
