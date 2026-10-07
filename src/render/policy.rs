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

fn set_once(map: &mut serde_json::Map<String, Value>, key: &str, value: &Value) {
	if !map.contains_key(key) {
		map.insert(key.to_string(), value.clone());
	}
}

/// Applies the policies of `gw`'s namespace to its rules; returns their status for this Gateway.
pub fn apply(world: &World, gw: &Gateway, rules: &mut [rp::Rule], targets: &Targets) -> Vec<PolicyStatus> {
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
		out.push(PolicyStatus {
			namespace: gw_ns.clone(),
			name: p.metadata.name.clone().unwrap_or_default(),
			generation: p.metadata.generation.unwrap_or(0),
			ancestor: json!({"group": GROUP, "kind": "Gateway", "namespace": gw_ns, "name": gw_name}),
			conds: vec![cond],
		});
	}
	out
}

/// The RproxyRules naming `gw`: rules to add (JSON) and their status.
/// What RproxyRules may do in a Gateway's set.
pub struct RawLimits<'a> {
	/// Whether they are read at all (fleet mode: off unless asked for).
	pub enabled: bool,
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
		if !limits.enabled {
			st.conds.push(Cond::new(
				"Accepted",
				false,
				"NotAllowed",
				"RproxyRules are off for this Gateway (fleet mode: --fleet-rproxy-rules)",
			));
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
