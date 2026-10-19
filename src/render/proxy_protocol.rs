//! `RproxyGatewayParameters` `proxyProtocol`: rproxy's `proxy_protocol` (rproxy v0.4.13,
//! `features.proxy_protocol_in`) on the Gateway's TCP rules, so the client address in the PROXY
//! header the load balancer in front sends is the one rproxy uses (docs/DESIGN-v0.4.x.md, A).
//!
//! - Without `listeners`: every TCP rule of the Gateway's set (HTTP, HTTPS, TLS and TCP ports,
//!   the ListenerSets' and migrated Ingress / Traefik ports too: the load balancer sends the header
//!   to the Service port, whichever listener it is for). RproxyRules are left as they are written.
//! - With `listeners`: the rules of those listeners of the Gateway. One port is one rule, so the other
//!   listeners on that port read the header too.
//! - UDP rules never get it (rproxy reads PROXY headers on TCP only); a UDP or unknown listener in
//!   `listeners` is reported in the Gateway's condition, the others still get it.
//! - An rproxy without `features.proxy_protocol_in` gets nothing (it would refuse the rule set);
//!   the condition says so.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::k8s::params::ProxyProtocolParams;
use crate::render::status::Cond;
use crate::rproxy::model as rp;

/// The Gateway condition this writes (only while `proxyProtocol` is set).
pub const CONDITION: &str = "rproxy.max3584.net/ProxyProtocol";

/// One of the Gateway's own listeners.
pub struct ListenerInfo<'a> {
	pub name: &'a str,
	pub udp: bool,
	/// The rule serving it (none when it is not accepted or has nothing to serve yet).
	pub rule_key: Option<&'a str>,
}

/// rproxy's `proxy_protocol` value.
pub fn value(pp: &ProxyProtocolParams) -> Value {
	json!({"from": pp.from, "required": pp.required.unwrap_or(true)})
}

/// Sets `proxy_protocol` on the rules `pp` names; returns the Gateway's condition.
pub fn apply(rules: &mut [rp::Rule], listeners: &[ListenerInfo], pp: &ProxyProtocolParams, supported: bool) -> Cond {
	if !supported {
		return Cond::new(
			CONDITION,
			false,
			"UnsupportedRproxy",
			"proxyProtocol needs a newer rproxy (GET /capabilities: proxy_protocol_in, rproxy v0.4.13): PROXY headers are not read",
		);
	}
	let mut unknown = vec![];
	let mut udp = vec![];
	let keys: BTreeSet<String> = match &pp.listeners {
		None => rules.iter().filter(|r| r.protocol == rp::Protocol::Tcp).map(|r| r.key()).collect(),
		Some(names) => {
			let mut keys = BTreeSet::new();
			for n in names {
				match listeners.iter().find(|l| l.name == n) {
					None => unknown.push(n.as_str()),
					Some(l) if l.udp => udp.push(n.as_str()),
					Some(l) => keys.extend(l.rule_key.map(str::to_string)),
				}
			}
			keys
		}
	};
	let v = value(pp);
	let mut ports: Vec<u16> = vec![];
	for r in rules.iter_mut().filter(|r| r.protocol == rp::Protocol::Tcp && keys.contains(&r.key())) {
		r.extra.insert("proxy_protocol".into(), v.clone());
		ports.push(r.listen_port);
	}
	ports.sort_unstable();
	ports.dedup();
	let required = if pp.required.unwrap_or(true) { "required" } else { "optional" };
	let applied = if ports.is_empty() {
		"no TCP port reads PROXY headers yet".to_string()
	} else {
		format!(
			"TCP ports {} read PROXY headers from {} ({required})",
			ports.iter().map(u16::to_string).collect::<Vec<_>>().join(", "),
			pp.from.join(", ")
		)
	};
	let mut problems = vec![];
	if !unknown.is_empty() {
		problems.push(format!("no listener {}", unknown.join(", ")));
	}
	if !udp.is_empty() {
		problems.push(format!("{}: UDP (PROXY headers are read on TCP only)", udp.join(", ")));
	}
	if problems.is_empty() {
		Cond::new(CONDITION, true, "Applied", applied)
	} else {
		Cond::new(CONDITION, false, "InvalidListeners", format!("spec.proxyProtocol.listeners: {}; {applied}", problems.join("; ")))
	}
}
