//! The rule shape the controller sends (rproxy-api docs/openapi.json `RuleRequest`),
//! and the parts of rproxy's answers it reads.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// One rule of a rule set (`RuleRequest`).
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Rule {
	pub protocol: Protocol,
	pub listen_addr: String,
	pub listen_port: u16,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub extra_listen_addrs: Vec<String>,
	/// Listen while the addresses are not on the node yet (fleet VIPs; rproxy v0.4.3).
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub listen_freebind: bool,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub targets: Vec<Target>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tls: Option<Tls>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub http: Option<Http>,
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub labels: BTreeMap<String, String>,
	/// Settings from RproxyPolicy (`limits`, `bandwidth`, ...), merged as they are.
	#[serde(flatten)]
	pub extra: Map<String, Value>,
}

impl Rule {
	/// rproxy's name of the rule: `tcp/0.0.0.0:443`.
	pub fn key(&self) -> String {
		rule_key(self.protocol, &self.listen_addr, self.listen_port)
	}

	/// Whether rproxy would refuse the two side by side (`409 already_exists`): the same protocol,
	/// overlapping ports, and an address of one equal to, or covered by a wildcard of, the other's
	/// (rproxy-api `net::listen::clash`: `::` without other addresses also takes IPv4).
	pub fn overlaps(&self, other: &Rule) -> bool {
		let ports = |r: &Rule| {
			let end = r.extra.get("listen_port_end").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).unwrap_or(r.listen_port);
			(r.listen_port, end.max(r.listen_port))
		};
		let ((a0, a1), (b0, b1)) = (ports(self), ports(other));
		if self.protocol != other.protocol || a0 > b1 || b0 > a1 {
			return false;
		}
		let ips = |r: &Rule| -> Vec<std::net::IpAddr> {
			std::iter::once(&r.listen_addr).chain(&r.extra_listen_addrs).filter_map(|a| a.trim_matches(['[', ']']).parse().ok()).collect()
		};
		let covers = |wild: std::net::IpAddr, v6only: bool, ip: std::net::IpAddr| match wild {
			std::net::IpAddr::V4(w) if w.is_unspecified() => ip.is_ipv4(),
			std::net::IpAddr::V6(w) if w.is_unspecified() => !v6only || ip.is_ipv6(),
			_ => false,
		};
		let (av, bv) = (!self.extra_listen_addrs.is_empty(), !other.extra_listen_addrs.is_empty());
		let bs = ips(other);
		ips(self).into_iter().any(|x| bs.iter().any(|y| x == *y || covers(x, av, *y) || covers(*y, bv, x)))
	}
}

pub fn rule_key(protocol: Protocol, addr: &str, port: u16) -> String {
	if addr.contains(':') { format!("{}/[{addr}]:{port}", protocol.as_str()) } else { format!("{}/{addr}:{port}", protocol.as_str()) }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
	#[default]
	Tcp,
	Udp,
}

impl Protocol {
	pub fn as_str(self) -> &'static str {
		match self {
			Protocol::Tcp => "tcp",
			Protocol::Udp => "udp",
		}
	}

	/// The Service port protocol.
	pub fn k8s(self) -> &'static str {
		match self {
			Protocol::Tcp => "TCP",
			Protocol::Udp => "UDP",
		}
	}
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Target {
	pub addr: String,
	pub port: u16,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub weight: Option<u32>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Tls {
	/// `passthrough`, `sni` or `terminate`.
	pub mode: &'static str,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub routes: Vec<TlsRoute>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub certificates: Vec<Certificate>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub unmatched: Option<&'static str>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub options: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub client_auth: Option<Value>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub alpn: Vec<String>,
	/// 421 for a request whose Host is another listener's than the SNI's (rproxy v0.4.3).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub misdirected: Option<Misdirected>,
}

/// `tls.misdirected`: groups of server name patterns.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Misdirected {
	pub groups: Vec<Vec<String>>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct TlsRoute {
	pub server_names: Vec<String>,
	/// One destination (`remote_addr` / `remote_port`), or several (`targets`, `features.tls_route_targets`).
	#[serde(skip_serializing_if = "String::is_empty")]
	pub remote_addr: String,
	#[serde(skip_serializing_if = "is_zero")]
	pub remote_port: u16,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub targets: Vec<Target>,
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub passthrough: bool,
}

fn is_zero(p: &u16) -> bool {
	*p == 0
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Certificate {
	#[serde(skip_serializing_if = "Option::is_none")]
	pub cert_file: Option<String>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub key_file: Option<String>,
	/// An ACME resolver of rproxy's settings file (Traefik `certResolver`).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub acme: Option<String>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub domains: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Http {
	pub routes: Vec<HttpRoute>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub default: Option<HttpDefault>,
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub services: BTreeMap<String, Service>,
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub middlewares: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HttpRoute {
	pub name: String,
	#[serde(rename = "match")]
	pub rule: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub priority: Option<i64>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub service: Option<String>,
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub middlewares: Vec<String>,
	/// `request` / `backend_request` (`features.http_options` `route_timeouts`).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub timeouts: Option<Value>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct HttpDefault {
	pub status: u16,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub service: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Service {
	pub servers: Vec<Server>,
	/// `http1`, `h2`, `h2c`, `auto` (`features.services` `protocol`).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub protocol: Option<&'static str>,
	/// TLS to the `https://` servers (`features.services` `tls`).
	#[serde(skip_serializing_if = "Option::is_none")]
	pub tls: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub pass_host_header: Option<bool>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub timeouts: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub outlier_detection: Option<Value>,
	/// Not sent: the route rule set `timeouts` (HTTPRoute), so no RproxyPolicy or default response
	/// timeout replaces them.
	#[serde(skip)]
	pub route_timeouts: bool,
	/// Not sent: the controller's default response timeout applies (an HTTPRoute rule's main service
	/// without `timeouts`; not GRPCRoute, whose streams may hold their headers back).
	#[serde(skip)]
	pub default_response: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Server {
	/// The backend's URL, or (`status`) none: rproxy answers with that status.
	#[serde(skip_serializing_if = "String::is_empty")]
	pub url: String,
	/// A fixed status (`features.http_options` `server_status`): Gateway API's 500 for an invalid backendRef.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub status: Option<u16>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub weight: Option<u32>,
	/// Middlewares for requests to this server only (`features.http_options` `server_middlewares`).
	#[serde(skip_serializing_if = "Vec::is_empty")]
	pub middlewares: Vec<String>,
}

/// The body of `PUT /rulesets/{name}`.
#[derive(Clone, Debug, Serialize)]
pub struct RulesetRequest<'a> {
	pub generation: i64,
	pub rules: &'a [Value],
}

/// One entry of a rule's `conditions` (rproxy v0.4).
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
pub struct RuleCondition {
	#[serde(rename = "type")]
	pub kind: String,
	pub status: String,
	#[serde(default)]
	pub reason: String,
	#[serde(default)]
	pub message: String,
}

/// The parts of a rule view (`GET /rulesets/{name}` `rules[]`) the controller reads.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RuleView {
	pub protocol: Protocol,
	pub listen_addr: String,
	pub listen_port: u16,
	#[serde(default)]
	pub state: String,
	#[serde(default)]
	pub error: Option<String>,
	#[serde(default)]
	pub conditions: Vec<RuleCondition>,
}

impl RuleView {
	pub fn key(&self) -> String {
		rule_key(self.protocol, &self.listen_addr, self.listen_port)
	}

	/// The condition of this type: rproxy's own, or (before `features.conditions`) made from `state`.
	pub fn condition(&self, kind: &str) -> RuleCondition {
		if let Some(c) = self.conditions.iter().find(|c| c.kind == kind) {
			return c.clone();
		}
		let failed = self.state == "failed";
		let message = self.error.clone().unwrap_or_default();
		let (status, reason) = match kind {
			"Programmed" if self.state == "running" => ("True", "Listening"),
			"Programmed" if failed => ("False", "Failed"),
			"Programmed" => ("False", "Pending"),
			_ => ("True", kind),
		};
		RuleCondition { kind: kind.into(), status: status.into(), reason: reason.into(), message }
	}
}

/// `GET /rulesets/{name}`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Ruleset {
	#[serde(default)]
	pub generation: i64,
	#[serde(default)]
	pub etag: String,
	#[serde(default)]
	pub rules: Vec<RuleView>,
}

/// `GET /rulesets` entries.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RulesetSummary {
	pub name: String,
	#[serde(default)]
	pub generation: i64,
	#[serde(default)]
	pub etag: String,
}

/// One result of `PUT /rulesets/{name}`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct ApplyResult {
	pub rule: String,
	#[serde(default)]
	pub action: String,
	#[serde(default)]
	pub state: Option<String>,
	#[serde(default)]
	pub error: Option<String>,
}

/// The answer of `PUT /rulesets/{name}`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct RulesetApplied {
	#[serde(default)]
	pub generation: i64,
	#[serde(default)]
	pub etag: String,
	#[serde(default)]
	pub results: Vec<ApplyResult>,
}

/// The parts of `GET /capabilities` the controller reads.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Capabilities {
	#[serde(default)]
	pub version: String,
	#[serde(default)]
	pub features: Map<String, Value>,
}

impl Capabilities {
	pub fn feature(&self, name: &str) -> bool {
		self.features.get(name).and_then(Value::as_bool).unwrap_or(false)
	}

	/// A name in a list-shaped feature (`middlewares`, `services`).
	pub fn lists(&self, feature: &str, name: &str) -> bool {
		self.features.get(feature).and_then(Value::as_array).is_some_and(|a| a.iter().any(|v| v.as_str() == Some(name)))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keys_and_shape() {
		let r = Rule {
			protocol: Protocol::Tcp,
			listen_addr: "0.0.0.0".into(),
			listen_port: 80,
			targets: vec![Target { addr: "10.0.0.1".into(), port: 8080, weight: None }],
			..Default::default()
		};
		assert_eq!(r.key(), "tcp/0.0.0.0:80");
		assert_eq!(rule_key(Protocol::Udp, "::", 53), "udp/[::]:53");
		let v = serde_json::to_value(&r).unwrap();
		assert_eq!(
			v,
			serde_json::json!({"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": 80, "targets": [{"addr": "10.0.0.1", "port": 8080}]})
		);
	}

	#[test]
	fn conditions_fall_back_to_state() {
		let v: RuleView = serde_json::from_value(serde_json::json!({
			"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": 80, "state": "failed", "error": "bind: in use"
		}))
		.unwrap();
		let p = v.condition("Programmed");
		assert_eq!((p.status.as_str(), p.reason.as_str(), p.message.as_str()), ("False", "Failed", "bind: in use"));
		assert_eq!(v.condition("Accepted").status, "True");
	}

	#[test]
	fn rules_overlap_like_rproxy() {
		let r = |p: Protocol, addr: &str, extra: &[&str], port: u16| Rule {
			protocol: p,
			listen_addr: addr.into(),
			listen_port: port,
			extra_listen_addrs: extra.iter().map(|s| s.to_string()).collect(),
			..Default::default()
		};
		let t = Protocol::Tcp;
		assert!(!r(t, "192.0.2.10", &[], 443).overlaps(&r(t, "192.0.2.11", &[], 443)), "two VIPs share a port");
		assert!(r(t, "192.0.2.10", &[], 443).overlaps(&r(t, "192.0.2.10", &[], 443)));
		assert!(!r(t, "192.0.2.10", &[], 443).overlaps(&r(Protocol::Udp, "192.0.2.10", &[], 443)), "tcp and udp");
		assert!(!r(t, "192.0.2.10", &[], 443).overlaps(&r(t, "192.0.2.10", &[], 80)));
		assert!(r(t, "0.0.0.0", &[], 443).overlaps(&r(t, "192.0.2.10", &[], 443)), "a wildcard takes every address");
		assert!(r(t, "192.0.2.10", &[], 443).overlaps(&r(t, "0.0.0.0", &[], 443)));
		assert!(r(t, "::", &[], 443).overlaps(&r(t, "192.0.2.10", &[], 443)), "a dual-stack :: takes IPv4");
		assert!(!r(t, "0.0.0.0", &["::"], 443).overlaps(&r(t, "2001:db8::10", &["192.0.2.10"], 80)));
		assert!(r(t, "192.0.2.11", &["2001:db8::10"], 443).overlaps(&r(t, "192.0.2.10", &["2001:db8::10"], 443)), "an extra address");
		assert!(r(t, "192.0.2.10", &["2001:db8::10"], 443).overlaps(&r(t, "0.0.0.0", &[], 443)));
	}
}
