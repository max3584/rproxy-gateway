//! Status written back: Gateway `status` (addresses, conditions, listeners),
//! route `status.parents[]` and GatewayClass `status`, from the rendered plan
//! and the rules' `conditions` rproxy answers (rproxy-api docs/DESIGN-v0.4.md 3.2).

use serde_json::{Value, json};

use crate::render::status::{self, Cond, to_k8s};
use crate::render::{GatewayPlan, ParentStatus};
use crate::rproxy::model::RuleView;

/// What happened on one rproxy pod.
#[derive(Clone, Debug)]
pub enum PodSync {
	/// The set is applied; rproxy's views of its rules.
	Synced(Vec<RuleView>),
	/// Not ready or not reachable.
	NotReady(String),
	/// Waiting (certificate files not written yet).
	Pending(String),
	/// rproxy refused the set.
	Rejected(String),
}

/// The rule's condition `kind` across pods: the first that is not `True`, else `True`.
fn rule_cond(pods: &[PodSync], key: &str, kind: &str) -> Option<crate::rproxy::model::RuleCondition> {
	let mut out = None;
	for p in pods {
		if let PodSync::Synced(views) = p {
			let Some(v) = views.iter().find(|v| v.key() == key) else { continue };
			let c = v.condition(kind);
			if c.status != "True" {
				return Some(c);
			}
			out.get_or_insert(c);
		}
	}
	out
}

fn synced(pods: &[PodSync]) -> usize {
	pods.iter().filter(|p| matches!(p, PodSync::Synced(_))).count()
}

/// Why nothing is applied yet (the first pod's reason).
fn waiting(pods: &[PodSync]) -> (bool, String) {
	for p in pods {
		match p {
			PodSync::Rejected(m) => return (true, format!("rproxy refused the rule set: {m}")),
			PodSync::Pending(m) | PodSync::NotReady(m) => return (false, m.clone()),
			PodSync::Synced(_) => {}
		}
	}
	(false, "no rproxy pod is running".into())
}

/// The listener conditions after rproxy's answer.
pub fn listener_conds(plan: &GatewayPlan, i: usize, pods: &[PodSync]) -> Vec<Cond> {
	let l = &plan.listeners[i];
	let mut conds = l.conds.clone();
	let accepted = status::get(&conds, "Accepted").is_some_and(|c| c.status);
	let programmed = match (&l.rule_key, accepted && l.servable) {
		(Some(key), true) if synced(pods) > 0 => match rule_cond(pods, key, "Programmed") {
			Some(c) if c.status == "True" => Cond::ok("Programmed", "Programmed"),
			Some(c) => {
				let reason = if c.reason == "Pending" { "Pending" } else { "Invalid" };
				Cond::new("Programmed", false, reason, format!("rproxy: {} {}", c.reason, c.message).trim_end().to_string())
			}
			None => Cond::new("Programmed", false, "Pending", "the rule is not in rproxy yet"),
		},
		(Some(_), true) => {
			let (rejected, m) = waiting(pods);
			Cond::new("Programmed", false, if rejected { "Invalid" } else { "Pending" }, m)
		}
		// accepted, but nothing attached yet that needs a rule (TLS, TCP, UDP)
		(None, true) => Cond::ok("Programmed", "Programmed"),
		_ if accepted => Cond::new("Programmed", false, "Invalid", "no usable certificate"),
		_ => Cond::new("Programmed", false, "Invalid", "the listener is not accepted"),
	};
	if let Some(key) = &l.rule_key {
		if let Some(c) = rule_cond(pods, key, "ResolvedRefs") {
			if c.status != "True" && status::get(&conds, "ResolvedRefs").is_some_and(|r| r.status) {
				status::set(
					&mut conds,
					Cond::new("ResolvedRefs", false, "InvalidCertificateRef", format!("rproxy: {} {}", c.reason, c.message)),
				);
			}
		}
	}
	status::set(&mut conds, programmed);
	conds
}

/// Gateway `status`.
pub fn gateway_status(plan: &GatewayPlan, addresses: &[(String, String)], pods: &[PodSync], previous: Option<&Value>, now: &str) -> Value {
	let generation = plan.generation;
	let prev = |path: &str| previous.and_then(|p| p.pointer(path));
	let mut conds = plan.conds.clone();
	let programmed = if addresses.is_empty() {
		Cond::new("Programmed", false, "AddressNotAssigned", "waiting for an address (the rproxy Service)")
	} else if synced(pods) == 0 {
		let (rejected, m) = waiting(pods);
		Cond::new("Programmed", false, if rejected { "Invalid" } else { "Pending" }, m)
	} else {
		Cond::ok("Programmed", "Programmed")
	};
	status::set(&mut conds, programmed);
	let listeners: Vec<Value> = plan
		.listeners
		.iter()
		.enumerate()
		.map(|(i, l)| {
			let prev_conds = previous
				.and_then(|p| p["listeners"].as_array())
				.and_then(|a| a.iter().find(|x| x["name"] == l.name.as_str()))
				.map(|x| &x["conditions"]);
			json!({
				"name": l.name,
				"supportedKinds": l.supported_kinds,
				"attachedRoutes": l.attached,
				"conditions": to_k8s(&listener_conds(plan, i, pods), generation, prev_conds, now),
			})
		})
		.collect();
	json!({
		"addresses": addresses.iter().map(|(t, v)| json!({"type": t, "value": v})).collect::<Vec<_>>(),
		"conditions": to_k8s(&conds, generation, prev("/conditions"), now),
		"listeners": listeners,
	})
}

/// A route's conditions for one parent after rproxy's answer.
pub fn parent_conds(p: &ParentStatus, pods: &[PodSync]) -> Vec<Cond> {
	let mut conds = p.conds.clone();
	for key in &p.rule_keys {
		if let Some(c) = rule_cond(pods, key, "Accepted") {
			if c.status != "True" && status::get(&conds, "Accepted").is_some_and(|a| a.status) {
				status::set(
					&mut conds,
					Cond::new("Accepted", false, "UnsupportedValue", format!("rproxy: {key}: {} {}", c.reason, c.message)),
				);
			}
		}
		if let Some(c) = rule_cond(pods, key, "ResolvedRefs") {
			if c.status != "True" && status::get(&conds, "ResolvedRefs").is_some_and(|a| a.status) {
				status::set(
					&mut conds,
					Cond::new("ResolvedRefs", false, "BackendNotFound", format!("rproxy: {key}: {} {}", c.reason, c.message)),
				);
			}
		}
	}
	conds
}

/// A route's `status.parents[]` entry.
pub fn parent_entry(p: &ParentStatus, conds: &[Cond], controller: &str, previous: Option<&Value>, now: &str) -> Value {
	let prev = previous
		.and_then(|s| s["parents"].as_array())
		.and_then(|a| a.iter().find(|e| e["controllerName"] == controller && same_parent(&e["parentRef"], &p.parent_ref)))
		.map(|e| &e["conditions"]);
	json!({
		"parentRef": p.parent_ref,
		"controllerName": controller,
		"conditions": to_k8s(conds, p.generation, prev, now),
	})
}

fn same_parent(v: &Value, p: &crate::k8s::gateway::ParentReference) -> bool {
	serde_json::from_value::<crate::k8s::gateway::ParentReference>(v.clone()).is_ok_and(|x| &x == p)
}

/// The route's new `status.parents`: other controllers' entries kept, ours replaced.
pub fn route_parents(previous: Option<&Value>, ours: Vec<Value>, controller: &str) -> Vec<Value> {
	let mut out: Vec<Value> = previous
		.and_then(|s| s["parents"].as_array())
		.into_iter()
		.flatten()
		.filter(|e| e["controllerName"] != controller)
		.cloned()
		.collect();
	out.extend(ours);
	out
}

/// An RproxyRule's `status` (its rule's conditions from rproxy).
pub fn raw_status(st: &crate::render::policy::RawStatus, pods: &[PodSync], previous: Option<&Value>, now: &str) -> Value {
	let mut conds = st.conds.clone();
	if let (Some(key), true) = (&st.rule_key, status::get(&conds, "Accepted").is_some_and(|c| c.status)) {
		if synced(pods) == 0 {
			let (_, m) = waiting(pods);
			status::set(&mut conds, Cond::new("Programmed", false, "Pending", m));
		}
		for kind in ["Accepted", "Programmed", "ResolvedRefs", "BackendsHealthy"] {
			if let Some(c) = rule_cond(pods, key, kind) {
				status::set(&mut conds, Cond::new(kind, c.status == "True", &c.reason, c.message));
			}
		}
	}
	json!({ "conditions": to_k8s(&conds, st.generation, previous.map(|p| &p["conditions"]), now) })
}

/// An RproxyPolicy's `status.ancestors[]` entry.
pub fn policy_entry(p: &crate::render::policy::PolicyStatus, controller: &str, previous: Option<&Value>, now: &str) -> Value {
	let prev = previous
		.and_then(|s| s["ancestors"].as_array())
		.and_then(|a| a.iter().find(|e| e["controllerName"] == controller && e["ancestorRef"] == p.ancestor))
		.map(|e| &e["conditions"]);
	json!({
		"ancestorRef": p.ancestor,
		"controllerName": controller,
		"conditions": to_k8s(&p.conds, p.generation, prev, now),
	})
}

/// The policy's new `status.ancestors`: other controllers' entries kept, ours replaced.
pub fn policy_ancestors(previous: Option<&Value>, ours: Vec<Value>, controller: &str) -> Vec<Value> {
	let mut out: Vec<Value> = previous
		.and_then(|s| s["ancestors"].as_array())
		.into_iter()
		.flatten()
		.filter(|e| e["controllerName"] != controller)
		.cloned()
		.collect();
	out.extend(ours);
	out
}

/// GatewayClass `status`.
pub fn class_status(generation: i64, features: &[&str], previous: Option<&Value>, now: &str) -> Value {
	let conds = [Cond::ok("Accepted", "Accepted"), Cond::ok("SupportedVersion", "SupportedVersion")];
	let mut features: Vec<&str> = features.to_vec();
	features.sort();
	json!({
		"conditions": to_k8s(&conds, generation, previous.map(|p| &p["conditions"]), now),
		"supportedFeatures": features.iter().map(|f| json!({"name": f})).collect::<Vec<_>>(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::render::ListenerPlan;

	fn view(key_port: u16, programmed: &str) -> RuleView {
		serde_json::from_value(json!({
			"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": key_port, "state": "running",
			"conditions": [{"type": "Programmed", "status": programmed, "reason": if programmed == "True" {"Listening"} else {"BindFailed"}, "message": "in use"}]
		}))
		.unwrap()
	}

	fn plan() -> GatewayPlan {
		GatewayPlan {
			generation: 4,
			conds: vec![Cond::ok("Accepted", "Accepted")],
			listeners: vec![ListenerPlan {
				name: "http".into(),
				supported_kinds: vec![],
				attached: 1,
				conds: vec![Cond::ok("Accepted", "Accepted"), Cond::ok("ResolvedRefs", "ResolvedRefs")],
				rule_key: Some("tcp/0.0.0.0:80".into()),
				servable: true,
			}],
			..Default::default()
		}
	}

	#[test]
	fn gateway_status_from_rproxy() {
		let p = plan();
		let addrs = vec![("IPAddress".to_string(), "10.96.0.10".to_string())];
		let s = gateway_status(&p, &addrs, &[PodSync::Synced(vec![view(80, "True")])], None, "T");
		assert_eq!(s["addresses"][0]["value"], "10.96.0.10");
		assert_eq!(s["conditions"][1]["type"], "Programmed");
		assert_eq!(s["conditions"][1]["status"], "True");
		assert_eq!(s["listeners"][0]["conditions"][2]["status"], "True");
		assert_eq!(s["listeners"][0]["attachedRoutes"], 1);
		let s = gateway_status(&p, &addrs, &[PodSync::Synced(vec![view(80, "False")])], None, "T");
		let c = &s["listeners"][0]["conditions"][2];
		assert_eq!((c["status"].as_str(), c["reason"].as_str()), (Some("False"), Some("Invalid")));
		let s = gateway_status(&p, &[], &[PodSync::NotReady("starting".into())], None, "T");
		assert_eq!(s["conditions"][1]["reason"], "AddressNotAssigned");
		assert_eq!(s["listeners"][0]["conditions"][2]["reason"], "Pending");
	}

	#[test]
	fn route_parents_keep_other_controllers() {
		let prev = json!({"parents": [
			{"parentRef": {"name": "other"}, "controllerName": "example.com/other", "conditions": []},
			{"parentRef": {"name": "gw"}, "controllerName": "me", "conditions": [{"type": "Accepted", "status": "True", "lastTransitionTime": "OLD"}]}
		]});
		let p = ParentStatus {
			kind: crate::render::RouteKind::Http,
			namespace: "default".into(),
			name: "r".into(),
			generation: 2,
			parent_ref: crate::k8s::gateway::ParentReference { name: "gw".into(), ..Default::default() },
			conds: vec![Cond::ok("Accepted", "Accepted")],
			rule_keys: Default::default(),
		};
		let e = parent_entry(&p, &p.conds, "me", Some(&prev), "NOW");
		assert_eq!(e["conditions"][0]["lastTransitionTime"], "OLD");
		let parents = route_parents(Some(&prev), vec![e], "me");
		assert_eq!(parents.len(), 2);
		assert_eq!(parents[0]["controllerName"], "example.com/other");
		let c = class_status(1, &["HTTPRoute", "Gateway"], None, "T");
		assert_eq!(c["supportedFeatures"][0]["name"], "Gateway");
	}
}
