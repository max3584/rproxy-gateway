//! When rproxy refuses an HTTP rule (`rules[i]: ...`), leave out only the routes it is
//! about, not the whole port (#92): every tenant's HTTPRoutes on a port share one rule.
//!
//! The routes are found by the names in rproxy's message (`route <name>`, `middleware
//! <name>`, `service <name>`); when the message names none, by halving the routes with dry
//! runs (`PUT /rulesets/{name}?dry_run=true`). A route's "owner" is the object it comes
//! from: an HTTPRoute (`<ns>/<name>`), a GRPCRoute (`grpc:<ns>/<name>`) or a migrated
//! IngressRoute / Ingress (`traefik/<ns>/<name>`, `ingress/<ns>/<name>`); all of an owner's
//! routes on the rule go together (its `Accepted` is one condition).

use std::collections::BTreeSet;
use std::net::SocketAddr;

use serde_json::Value;

use crate::render::http::RESPOND_500;
use crate::rproxy::client::{ApiError, Client};

/// The owner of an rproxy route name (see the module); `None` for names no route of ours has.
pub fn owner(name: &str) -> Option<String> {
	if name == RESPOND_500 {
		return None;
	}
	let (prefix, rest) = match name.strip_prefix("grpc:") {
		Some(r) => ("grpc:", r),
		None => ("", name),
	};
	let parts: Vec<&str> = rest.splitn(4, '/').collect();
	match parts.as_slice() {
		[kind @ ("traefik" | "ingress"), ns, n, _] => Some(format!("{prefix}{kind}/{ns}/{n}")),
		[ns, n, _, ..] => Some(format!("{prefix}{ns}/{n}")),
		_ => None,
	}
}

fn routes(rule: &Value) -> impl Iterator<Item = &Value> {
	rule["http"]["routes"].as_array().into_iter().flatten()
}

/// The owners of the rule's HTTP routes, in their order.
pub fn owners(rule: &Value) -> Vec<String> {
	let mut out: Vec<String> = vec![];
	for o in routes(rule).filter_map(|r| r["name"].as_str().and_then(owner)) {
		if !out.contains(&o) {
			out.push(o);
		}
	}
	out
}

/// The services and middlewares the given routes use (directly, through a service's servers,
/// or a middleware's `service`: mirror, forward_auth, errors).
fn used<'a>(http: &Value, routes: impl Iterator<Item = &'a Value>) -> BTreeSet<String> {
	let mut out = BTreeSet::new();
	let mut todo: Vec<String> = vec![];
	for r in routes {
		todo.extend(r["service"].as_str().map(str::to_string));
		todo.extend(r["middlewares"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(str::to_string)));
	}
	while let Some(n) = todo.pop() {
		if !out.insert(n.clone()) {
			continue;
		}
		if let Some(svc) = http["services"].get(&n) {
			for s in svc["servers"].as_array().into_iter().flatten() {
				todo.extend(s["middlewares"].as_array().into_iter().flatten().filter_map(|m| m.as_str().map(str::to_string)));
			}
		}
		if let Some(mw) = http["middlewares"].get(&n).and_then(Value::as_object) {
			todo.extend(mw.values().filter_map(|spec| spec["service"].as_str().map(str::to_string)));
		}
	}
	out
}

/// Whether `name` appears in `message` as a whole name (not as the start of a longer one).
fn mentions(message: &str, name: &str) -> bool {
	message.match_indices(name).any(|(i, _)| {
		let after = message[i + name.len()..].chars().next();
		let before = message[..i].chars().next_back();
		after.is_none_or(|c| matches!(c, ':' | '"' | ' ' | ',' | ')' | ';')) && before.is_none_or(|c| matches!(c, ' ' | '"' | '(' | '['))
	})
}

/// The owners whose routes use what rproxy's message names (empty: it names nothing of a route).
pub fn named(rule: &Value, message: &str) -> Vec<String> {
	let http = &rule["http"];
	let mut names: Vec<&str> = routes(rule).filter_map(|r| r["name"].as_str()).collect();
	for part in ["services", "middlewares"] {
		names.extend(http[part].as_object().into_iter().flat_map(|m| m.keys().map(String::as_str)));
	}
	// the longest first: `a/r0/m0/h1` is not taken for `a/r0/m0/h10`
	names.sort_by_key(|n| std::cmp::Reverse(n.len()));
	let Some(hit) = names.into_iter().find(|n| *n != RESPOND_500 && mentions(message, n)) else {
		return vec![];
	};
	let mut out: Vec<String> = vec![];
	for r in routes(rule) {
		let name = r["name"].as_str().unwrap_or_default();
		if (name == hit || used(http, std::iter::once(r)).contains(hit))
			&& let Some(o) = owner(name)
		{
			if !out.contains(&o) {
				out.push(o);
			}
		}
	}
	out
}

/// The rule without the routes of `owners`, and without the services and middlewares only those
/// routes used.
pub fn without(rule: &Value, owners: &BTreeSet<String>) -> Value {
	let mut out = rule.clone();
	let Some(list) = rule["http"]["routes"].as_array() else { return out };
	let gone = |r: &&Value| r["name"].as_str().and_then(owner).is_some_and(|o| owners.contains(&o));
	let (dropped, kept): (Vec<&Value>, Vec<&Value>) = list.iter().partition(gone);
	let http = &rule["http"];
	let still = used(http, kept.iter().copied().chain(http.get("default")));
	let unused: BTreeSet<String> = used(http, dropped.into_iter()).difference(&still).cloned().collect();
	out["http"]["routes"] = Value::Array(kept.into_iter().cloned().collect());
	for part in ["services", "middlewares"] {
		if let Some(m) = out.get_mut("http").and_then(|h| h.get_mut(part)).and_then(Value::as_object_mut) {
			m.retain(|k, _| !unused.contains(k));
		}
	}
	out
}

/// rproxy's message without the `rules[i]: ` it starts with.
pub fn message(m: &str) -> &str {
	m.strip_prefix("rules[").and_then(|r| r.split_once("]: ")).map(|(_, rest)| rest).unwrap_or(m)
}

/// Where a refused rule's set is tried (dry runs).
pub struct Target<'a> {
	pub rp: &'a Client,
	pub addr: SocketAddr,
	pub set: &'a str,
	pub generation: i64,
}

/// The owners to leave out of `rules[i]`, which rproxy refused with `message`; `None`: the
/// fault is not in one route (the rule is left out as a whole).
pub async fn find(t: &Target<'_>, rules: &[Value], i: usize, message: &str) -> Option<Vec<String>> {
	let all = owners(&rules[i]);
	if all.is_empty() {
		return None;
	}
	let hit = named(&rules[i], message);
	if !hit.is_empty() {
		return Some(hit);
	}
	// does the rule pass with only the routes of `keep`? (`None`: rproxy did not answer)
	let fails = async |keep: &[String]| -> Option<bool> {
		let out: BTreeSet<String> = all.iter().filter(|o| !keep.contains(o)).cloned().collect();
		let mut trial = rules.to_vec();
		trial[i] = without(&rules[i], &out);
		match t.rp.put_ruleset_dry_run(t.addr, t.set, t.generation, &trial).await {
			Ok(()) => Some(false),
			Err(e) => match e.downcast_ref::<ApiError>() {
				Some(a) if a.status.is_client_error() => Some(super::refused_index(&a.message) == Some(i)),
				_ => None,
			},
		}
	};
	// the rule without any of its routes: still refused, so not a route's fault
	if fails(&[]).await? {
		return None;
	}
	let mut c = all.clone();
	while c.len() > 1 {
		let (a, b) = c.split_at(c.len() / 2);
		if fails(a).await? {
			c = a.to_vec();
		} else if fails(b).await? {
			c = b.to_vec();
		} else {
			// only some routes together are refused (names clashing): not one route's fault
			return None;
		}
	}
	Some(c)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn rule() -> Value {
		json!({"protocol": "tcp", "listen_addr": "0.0.0.0", "listen_port": 443, "http": {
			"routes": [
				{"name": "a/web/r0/m0/h1", "match": "PathPrefix(`/`)", "service": "a/web/r0", "middlewares": ["a/web/r0/m0/f0.0"]},
				{"name": "a/web/r0/m0/h10", "match": "PathPrefix(`/`)", "service": "a/web/r0"},
				{"name": "grpc:a/web/r0/m0/h0", "match": "PathPrefix(`/`)", "middlewares": [RESPOND_500]},
				{"name": "b/api/r0/m0/h0@https", "match": "PathPrefix(`/`)", "service": "b/api/r0", "middlewares": ["traefik/b/shared"]},
				{"name": "traefik/b/ir/r0", "match": "PathPrefix(`/`)", "service": "traefik/b/ir/r0", "middlewares": ["traefik/b/shared"]}
			],
			"services": {
				"a/web/r0": {"servers": [{"url": "http://10.0.0.1:80"}]},
				"b/api/r0": {"servers": [{"url": "http://10.0.0.2:80", "middlewares": ["b/api/r0/b0/f0.0"]}]},
				"b/api/r0/b0/f0/mirror": {"servers": [{"url": "http://10.0.0.3:80"}]},
				"traefik/b/ir/r0": {"servers": [{"url": "http://10.0.0.4:80"}]}
			},
			"middlewares": {
				"a/web/r0/m0/f0.0": {"headers": {"request": {"set": {"x": "1"}}}},
				"b/api/r0/b0/f0.0": {"mirror": {"service": "b/api/r0/b0/f0/mirror"}},
				"traefik/b/shared": {"headers": {}},
				RESPOND_500: {"respond": {"status": 500}}
			}
		}})
	}

	#[test]
	fn owners_of_names() {
		assert_eq!(owner("a/web/r0/m0/h0@https").as_deref(), Some("a/web"));
		assert_eq!(owner("grpc:a/web/r0/m0/h0").as_deref(), Some("grpc:a/web"));
		assert_eq!(owner("traefik/ns/ir/r3").as_deref(), Some("traefik/ns/ir"));
		assert_eq!(owner("ingress/ns/ing/r0/p1").as_deref(), Some("ingress/ns/ing"));
		assert_eq!(owner(RESPOND_500), None);
		assert_eq!(owners(&rule()), ["a/web", "grpc:a/web", "b/api", "traefik/b/ir"]);
	}

	#[test]
	fn names_in_messages() {
		let r = rule();
		assert_eq!(named(&r, "route a/web/r0/m0/h10: match: PathRegexp: regex parse error"), ["a/web"]);
		assert_eq!(named(&r, "route grpc:a/web/r0/m0/h0: match: bad"), ["grpc:a/web"]);
		// a middleware: the routes using it (through a service's servers and a mirror too)
		assert_eq!(named(&r, "middleware b/api/r0/b0/f0.0: \"a\\r\" is not a valid header value"), ["b/api"]);
		assert_eq!(named(&r, "service \"b/api/r0/b0/f0/mirror\" is not defined"), ["b/api"]);
		// shared by several owners: all of them
		assert_eq!(named(&r, "middleware traefik/b/shared: bad"), ["b/api", "traefik/b/ir"]);
		assert!(named(&r, "middleware rproxy-gateway/500: bad").is_empty());
		assert!(named(&r, "tls: no certificate").is_empty());
		assert_eq!(message("rules[3]: route x: bad"), "route x: bad");
	}

	#[test]
	fn leaving_out() {
		let r = rule();
		let out = without(&r, &["b/api".to_string()].into());
		let names: Vec<&str> = out["http"]["routes"].as_array().unwrap().iter().map(|r| r["name"].as_str().unwrap()).collect();
		assert_eq!(names, ["a/web/r0/m0/h1", "a/web/r0/m0/h10", "grpc:a/web/r0/m0/h0", "traefik/b/ir/r0"]);
		let services = out["http"]["services"].as_object().unwrap();
		assert!(!services.contains_key("b/api/r0") && !services.contains_key("b/api/r0/b0/f0/mirror"));
		let mws = out["http"]["middlewares"].as_object().unwrap();
		assert!(!mws.contains_key("b/api/r0/b0/f0.0"));
		// still used by the IngressRoute
		assert!(mws.contains_key("traefik/b/shared"));
		let out = without(&r, &["grpc:a/web".to_string()].into());
		assert!(out["http"]["middlewares"].get(RESPOND_500).is_none());
		// nothing added that was not there
		let bare = json!({"http": {"routes": [{"name": "a/web/r0/m0/h0", "match": "PathPrefix(`/`)"}]}});
		assert_eq!(without(&bare, &["a/web".to_string()].into()), json!({"http": {"routes": []}}));
	}
}
