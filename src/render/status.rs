//! Conditions (Gateway API status), kept independent of time so rendering is pure;
//! `to_k8s` adds `observedGeneration` and `lastTransitionTime`.

use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Cond {
	pub kind: String,
	pub status: bool,
	pub reason: String,
	pub message: String,
}

impl Cond {
	pub fn new(kind: &str, status: bool, reason: &str, message: impl Into<String>) -> Cond {
		Cond { kind: kind.into(), status, reason: reason.into(), message: message.into() }
	}

	pub fn ok(kind: &str, reason: &str) -> Cond {
		Cond::new(kind, true, reason, "")
	}
}

/// Sets `c` in `list`, replacing one of the same type.
pub fn set(list: &mut Vec<Cond>, c: Cond) {
	match list.iter_mut().find(|x| x.kind == c.kind) {
		Some(x) => *x = c,
		None => list.push(c),
	}
}

pub fn get<'a>(list: &'a [Cond], kind: &str) -> Option<&'a Cond> {
	list.iter().find(|c| c.kind == kind)
}

/// Kubernetes conditions. `previous` (the conditions in the current status) keeps
/// `lastTransitionTime` of a condition whose status did not change.
pub fn to_k8s(conds: &[Cond], generation: i64, previous: Option<&Value>, now: &str) -> Vec<Value> {
	conds
		.iter()
		.map(|c| {
			let status = if c.status { "True" } else { "False" };
			let since = previous
				.and_then(Value::as_array)
				.and_then(|a| a.iter().find(|p| p["type"] == c.kind.as_str() && p["status"] == status))
				.and_then(|p| p["lastTransitionTime"].as_str())
				.unwrap_or(now);
			let message: String = c.message.chars().take(32_000).collect();
			json!({
				"type": c.kind, "status": status, "reason": c.reason, "message": message,
				"observedGeneration": generation, "lastTransitionTime": since,
			})
		})
		.collect()
}

/// The current time as a Kubernetes timestamp (seconds, UTC).
pub fn now() -> String {
	let ts = jiff::Timestamp::now();
	ts.strftime("%Y-%m-%dT%H:%M:%SZ").to_string()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn transition_times_are_kept() {
		let prev = json!([{"type": "Accepted", "status": "True", "lastTransitionTime": "2026-01-01T00:00:00Z"}]);
		let out = to_k8s(&[Cond::ok("Accepted", "Accepted"), Cond::new("Programmed", false, "Pending", "x")], 3, Some(&prev), "NOW");
		assert_eq!(out[0]["lastTransitionTime"], "2026-01-01T00:00:00Z");
		assert_eq!(out[0]["observedGeneration"], 3);
		assert_eq!(out[1]["lastTransitionTime"], "NOW");
		assert_eq!(out[1]["status"], "False");
		let mut list = vec![Cond::ok("A", "A")];
		set(&mut list, Cond::new("A", false, "B", ""));
		assert_eq!(list.len(), 1);
		assert!(!get(&list, "A").unwrap().status);
		assert!(now().ends_with('Z'));
	}
}
