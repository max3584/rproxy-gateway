//! `RproxyPolicy` (policy attachment, GEP-713) and `RproxyRule` (a rule verbatim).
//!
//! A policy's `targetRefs` name, in its own namespace, a Gateway (every rule of
//! its set), a Gateway listener (`sectionName`: that listener's rule) or a
//! Service (the L4 rules sending to it; for `outlier_detection`, also the
//! `http` services sending to it). When several policies set the same key on a
//! rule, the oldest wins.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::k8s::crd::{FreeForm, RproxyPolicy};
use crate::k8s::gateway::{GROUP, Gateway};
use crate::render::status::Cond;
use crate::render::world::{Key, World};
use crate::rproxy::model as rp;

/// A policy's status for one Gateway (`status.ancestors[]`).
#[derive(Clone, Debug)]
pub struct PolicyStatus {
	pub namespace: String,
	pub name: String,
	pub generation: i64,
	pub ancestor: Value,
	pub conds: Vec<Cond>,
}

/// An RproxyRule's status.
#[derive(Clone, Debug)]
pub struct RawStatus {
	pub namespace: String,
	pub name: String,
	pub generation: i64,
	/// The rule's key in rproxy, once accepted into the set.
	pub rule_key: Option<String>,
	pub conds: Vec<Cond>,
}

/// What a policy needs to know about the rendered rules.
pub struct Targets<'a> {
	/// listener name → rule key
	pub listeners: &'a BTreeMap<String, String>,
	/// L4 rule key → Services it sends to
	pub l4_services: &'a BTreeMap<String, Vec<Key>>,
	/// (rule key, http service name) → Services it sends to
	pub http_services: &'a BTreeMap<(String, String), Vec<Key>>,
}

fn settings(p: &RproxyPolicy) -> Vec<(&'static str, Value)> {
	let s = &p.spec;
	let mut out = vec![];
	let ff = |f: &Option<FreeForm>| f.as_ref().map(|f| Value::Object(f.0.clone()));
	if let Some(v) = ff(&s.limits) {
		out.push(("limits", v));
	}
	if let Some(v) = ff(&s.bandwidth) {
		out.push(("bandwidth", v));
	}
	if let Some(v) = ff(&s.geoip) {
		out.push(("geoip", v));
	}
	if let Some(v) = ff(&s.outlier_detection) {
		out.push(("outlier_detection", v));
	}
	if let Some(v) = &s.allow_from {
		out.push(("allow_from", json!(v)));
	}
	if let Some(v) = s.crowdsec {
		out.push(("crowdsec", json!(v)));
	}
	out
}

/// A policy's timeouts, checked.
#[derive(Default)]
struct Timeouts {
	connect: Option<String>,
	response: Option<String>,
	/// L4 tcp rules' `idle_timeout` and `half_close_timeout` (rproxy v0.4.20), as rproxy writes them
	/// (`0s`, `90s`, `1500ms`).
	idle: Option<String>,
	half_close: Option<String>,
}

impl Timeouts {
	fn relay(&self) -> bool {
		self.idle.is_some() || self.half_close.is_some()
	}
}

/// The longest relay timeout (rproxy's longest duration: 365 days).
const MAX_RELAY_MS: u64 = 365 * 24 * 3_600_000;

/// A policy's `connectTimeout`, `responseTimeout`, `idleTimeout` and `halfCloseTimeout`, checked (`Err`: why not).
fn timeouts(p: &RproxyPolicy) -> Result<Timeouts, String> {
	let check = |what: &str, v: &Option<String>, min: u64, max: u64| -> Result<Option<String>, String> {
		match v.as_deref().map(str::trim) {
			None => Ok(None),
			Some(d) => match crate::render::duration_ms(d) {
				Some(ms) if (min..=max).contains(&ms) => Ok(Some(d.to_string())),
				_ => Err(format!("{what} {d:?}: a duration from {}", if min == 100 { "100ms to 10m" } else { "1ms to 1h" })),
			},
		}
	};
	// `0s` (no limit) or 1s to 365 days (rproxy's `validate_relay_timeout`), in one unit as rproxy reads it
	let relay = |what: &str, v: &Option<String>| -> Result<Option<String>, String> {
		match v.as_deref().map(str::trim) {
			None => Ok(None),
			Some(d) => match crate::render::duration_ms(d) {
				Some(0) => Ok(Some("0s".into())),
				Some(ms) if (1000..=MAX_RELAY_MS).contains(&ms) => {
					Ok(Some(if ms % 1000 == 0 { format!("{}s", ms / 1000) } else { format!("{ms}ms") }))
				}
				_ => Err(format!("{what} {d:?}: 0s (no limit) or a duration from 1s to 365 days (8760h)")),
			},
		}
	};
	Ok(Timeouts {
		connect: check("connectTimeout", &p.spec.connect_timeout, 100, 600_000)?,
		response: check("responseTimeout", &p.spec.response_timeout, 1, 3_600_000)?,
		idle: relay("idleTimeout", &p.spec.idle_timeout)?,
		half_close: relay("halfCloseTimeout", &p.spec.half_close_timeout)?,
	})
}

/// Puts a policy's timeouts on a rule: an L4 tcp rule's `connect_timeout` (and with `relay`, its
/// `idle_timeout` and `half_close_timeout`), or the `http` services `service` picks: `timeouts.connect`,
/// and `timeouts.response` unless the route rule set its own. What is set already (an older policy)
/// stays. Returns whether an L4 tcp rule got a relay timeout of this policy.
fn put_timeouts(r: &mut rp::Rule, t: &Timeouts, relay: bool, service: impl Fn(&str) -> bool) -> bool {
	match r.http.as_mut() {
		Some(http) => {
			for (_, svc) in http.services.iter_mut().filter(|(n, _)| service(n)) {
				if let Some(c) = &t.connect {
					crate::render::set_timeout(svc, "connect", c);
				}
				if let (Some(t), false) = (&t.response, svc.route_timeouts) {
					crate::render::set_timeout(svc, "response", t);
				}
			}
			false
		}
		None if r.protocol == rp::Protocol::Tcp => {
			if let Some(c) = &t.connect {
				set_once(&mut r.extra, "connect_timeout", &Value::String(c.clone()));
			}
			if !relay {
				return false;
			}
			// an older policy's value stays: this one is applied only where it set one
			let mut set = false;
			for (k, v) in [("idle_timeout", &t.idle), ("half_close_timeout", &t.half_close)] {
				if let Some(v) = v {
					set |= set_once(&mut r.extra, k, &Value::String(v.clone()));
				}
			}
			set
		}
		None => false,
	}
}

/// The policy condition about `idleTimeout` / `halfCloseTimeout` (only while the policy sets one and
/// reaches an L4 tcp rule).
pub const RELAY_CONDITION: &str = "rproxy.max3584.net/RelayTimeouts";

/// Sets `key` unless it is set; whether it was.
fn set_once(map: &mut serde_json::Map<String, Value>, key: &str, value: &Value) -> bool {
	if map.contains_key(key) {
		return false;
	}
	map.insert(key.to_string(), value.clone());
	true
}

/// Applies the policies of `gw`'s namespace to its rules; returns their status for this Gateway.
/// `features`: what every pod of the Gateway takes (`tcp_idle_timeout` for the relay timeouts).
pub fn apply(
	world: &World,
	gw: &Gateway,
	rules: &mut [rp::Rule],
	targets: &Targets,
	features: &crate::render::Features,
) -> Vec<PolicyStatus> {
	let gw_ns = gw.metadata.namespace.clone().unwrap_or_default();
	let gw_name = gw.metadata.name.clone().unwrap_or_default();
	let mut policies: Vec<&RproxyPolicy> =
		world.policies.iter().filter(|p| p.metadata.namespace.as_deref() == Some(gw_ns.as_str())).collect();
	// oldest first: it wins when two set the same key
	policies.sort_by_key(|p| (p.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()), p.metadata.name.clone()));
	let mut out = vec![];
	for p in policies {
		let mut touched = false;
		let mut problems = vec![];
		// L4 tcp rules the relay timeouts reached (or would have, on an rproxy without them)
		let mut relayed = false;
		let relay = features.tcp_idle_timeout;
		let times = match timeouts(p) {
			Ok(t) => t,
			Err(e) => {
				// a wrong timeout: the policy is not used (its other settings neither)
				out.push(PolicyStatus {
					namespace: gw_ns.clone(),
					name: p.metadata.name.clone().unwrap_or_default(),
					generation: p.metadata.generation.unwrap_or(0),
					ancestor: json!({"group": GROUP, "kind": "Gateway", "namespace": gw_ns, "name": gw_name}),
					conds: vec![Cond::new("Accepted", false, "Invalid", e)],
				});
				continue;
			}
		};
		for t in &p.spec.target_refs {
			let is_gateway = t.group == GROUP && t.kind == "Gateway";
			let is_service = t.group.is_empty() && t.kind == "Service";
			if is_gateway && t.name == gw_name {
				touched = true;
				let keys: Vec<String> = match &t.section_name {
					Some(s) => match targets.listeners.get(s) {
						Some(k) => vec![k.clone()],
						None => {
							problems.push(format!("Gateway {gw_name} has no listener {s} with a rule"));
							continue;
						}
					},
					None => rules.iter().map(|r| r.key()).collect(),
				};
				for r in rules.iter_mut().filter(|r| keys.contains(&r.key())) {
					for (k, v) in settings(p) {
						if let (true, Some(http)) = (k == "outlier_detection", r.http.as_mut()) {
							// L7: the rule's services
							for svc in http.services.values_mut() {
								svc.outlier_detection.get_or_insert_with(|| v.clone());
							}
							continue;
						}
						set_once(&mut r.extra, k, &v);
					}
					relayed |= put_timeouts(r, &times, relay, |_| true) || (!relay && times.relay() && l4_tcp(r));
				}
			} else if is_service {
				let svc: Key = (gw_ns.clone(), t.name.clone());
				for r in rules.iter_mut() {
					let key = r.key();
					let l4 = targets.l4_services.get(&key).is_some_and(|s| s.contains(&svc));
					if l4 {
						touched = true;
						for (k, v) in settings(p) {
							set_once(&mut r.extra, k, &v);
						}
						relayed |= put_timeouts(r, &times, relay, |_| true) || (!relay && times.relay() && l4_tcp(r));
					}
					if let Some(http) = r.http.as_mut() {
						for (name, s) in http.services.iter_mut() {
							if targets.http_services.get(&(key.clone(), name.clone())).is_some_and(|l| l.contains(&svc)) {
								touched = true;
								for (k, v) in settings(p) {
									if k == "outlier_detection" {
										s.outlier_detection.get_or_insert_with(|| v.clone());
									}
								}
							}
						}
						let picked: Vec<String> = http
							.services
							.keys()
							.filter(|name| targets.http_services.get(&(key.clone(), (*name).clone())).is_some_and(|l| l.contains(&svc)))
							.cloned()
							.collect();
						if !picked.is_empty() {
							put_timeouts(r, &times, relay, |n| picked.iter().any(|p| p == n));
						}
					}
				}
			}
		}
		if !touched && problems.is_empty() {
			continue;
		}
		let cond = if problems.is_empty() {
			Cond::ok("Accepted", "Accepted")
		} else {
			Cond::new("Accepted", false, "TargetNotFound", problems.join("; "))
		};
		let mut conds = vec![cond];
		if relayed {
			conds.push(if relay {
				Cond::new(RELAY_CONDITION, true, "Applied", "idleTimeout / halfCloseTimeout are set on the L4 TCP rules")
			} else {
				Cond::new(
					RELAY_CONDITION,
					false,
					"UnsupportedRproxy",
					"idleTimeout / halfCloseTimeout need a newer rproxy (GET /capabilities: tcp_idle_timeout, rproxy v0.4.20): not set, rproxy's own defaults apply",
				)
			});
		}
		out.push(PolicyStatus {
			namespace: gw_ns.clone(),
			name: p.metadata.name.clone().unwrap_or_default(),
			generation: p.metadata.generation.unwrap_or(0),
			ancestor: json!({"group": GROUP, "kind": "Gateway", "namespace": gw_ns, "name": gw_name}),
			conds,
		});
	}
	out
}

/// An L4 tcp rule (where `idle_timeout` / `half_close_timeout` go).
fn l4_tcp(r: &rp::Rule) -> bool {
	r.http.is_none() && r.protocol == rp::Protocol::Tcp
}

/// The RproxyRules naming `gw`: rules to add (JSON) and their status.
/// What RproxyRules may do in a Gateway's set.
pub struct RawLimits<'a> {
	/// Why they are not read (`None`: they are): fleet mode unless asked for, the class's
	/// `policy.rproxyRules: false`, or the class's parameters invalid (fail closed).
	pub off: Option<String>,
	/// Files they may name (`*_file`): the Gateway's own, in the certificate directory.
	pub cert_dir: &'a str,
	pub files: &'a std::collections::BTreeSet<String>,
}

pub fn raw_rules(world: &World, gw: &Gateway, taken: &[String], limits: &RawLimits) -> (Vec<Value>, Vec<RawStatus>) {
	let gw_ns = gw.metadata.namespace.clone().unwrap_or_default();
	let gw_name = gw.metadata.name.clone().unwrap_or_default();
	let mut rules = vec![];
	let mut status = vec![];
	let mut keys: Vec<String> = taken.to_vec();
	let mut raws: Vec<_> = world
		.raw_rules
		.iter()
		.filter(|r| {
			let ns = r.metadata.namespace.clone().unwrap_or_default();
			r.spec.parent_ref.name == gw_name && r.spec.parent_ref.namespace.clone().unwrap_or(ns) == gw_ns
		})
		.collect();
	raws.sort_by_key(|r| {
		(r.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()), r.metadata.namespace.clone(), r.metadata.name.clone())
	});
	for r in raws {
		let rns = r.metadata.namespace.clone().unwrap_or_default();
		let mut st = RawStatus {
			namespace: rns.clone(),
			name: r.metadata.name.clone().unwrap_or_default(),
			generation: r.metadata.generation.unwrap_or(0),
			rule_key: None,
			conds: vec![],
		};
		// a rule from another namespace needs the Gateway's namespace to allow it
		if rns != gw_ns && !world.granted((crate::k8s::crd::GROUP, "RproxyRule", &rns), (GROUP, "Gateway", &gw_ns, &gw_name)) {
			st.conds.push(Cond::new(
				"Accepted",
				false,
				"RefNotPermitted",
				"a ReferenceGrant in the Gateway's namespace must allow RproxyRules from this namespace",
			));
			status.push(st);
			continue;
		}
		if let Some(why) = &limits.off {
			st.conds.push(Cond::new("Accepted", false, "NotAllowed", why.clone()));
			status.push(st);
			continue;
		}
		let rule = Value::Object(r.spec.rule.0.clone());
		// files on the rproxy host: only the Gateway's own certificate files
		if let Some(path) = crate::render::foreign_file(&rule, limits.cert_dir, limits.files) {
			st.conds.push(Cond::new("Accepted", false, "Invalid", format!("{path}: rules may only name this Gateway's certificate files")));
			status.push(st);
			continue;
		}
		let protocol = match rule["protocol"].as_str().map(str::to_ascii_lowercase).as_deref() {
			Some("tcp") => rp::Protocol::Tcp,
			Some("udp") => rp::Protocol::Udp,
			_ => {
				st.conds.push(Cond::new("Accepted", false, "Invalid", "rule.protocol must be tcp or udp"));
				status.push(st);
				continue;
			}
		};
		let (Some(addr), Some(port)) = (rule["listen_addr"].as_str(), rule["listen_port"].as_u64().and_then(|p| u16::try_from(p).ok()))
		else {
			st.conds.push(Cond::new("Accepted", false, "Invalid", "rule.listen_addr and rule.listen_port are required"));
			status.push(st);
			continue;
		};
		let key = rp::rule_key(protocol, addr, port);
		if keys.contains(&key) {
			st.conds.push(Cond::new("Accepted", false, "Conflicted", format!("{key} is already taken in this Gateway's rule set")));
			status.push(st);
			continue;
		}
		keys.push(key.clone());
		st.rule_key = Some(key);
		st.conds.push(Cond::ok("Accepted", "Accepted"));
		rules.push(rule);
		status.push(st);
	}
	(rules, status)
}
