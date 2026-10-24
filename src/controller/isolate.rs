//! When rproxy refuses an HTTP rule (`rules[i]: ...`), leave out only the routes it is
//! about, not the whole port (#92): every tenant's HTTPRoutes on a port share one rule.
//!
//! The routes are found by the name at the head of rproxy's message (`rules[i]: route <name>:`,
//! `middleware <name>:`, `service <name>:`; nothing further in the message is trusted, as rproxy
//! echoes values a tenant wrote); when the head names nothing of ours, by halving the routes with
//! dry runs (`PUT /rulesets/{name}?dry_run=true`, at most `MAX_DRY_RUNS` a pass). A route's
//! "owner" is the object it comes from: an HTTPRoute (`<ns>/<name>`), a GRPCRoute
//! (`grpc:<ns>/<name>`) or a migrated IngressRoute / Ingress (`traefik:<ns>/<name>`,
//! `ingress:<ns>/<name>`; Kubernetes names have no `:`); all of an owner's routes on the rule go
//! together (its `Accepted` is one condition).

use std::collections::BTreeSet;
use std::net::SocketAddr;

use serde_json::Value;

use crate::render::http::RESPOND_500;
use crate::rproxy::client::{ApiError, Client};

/// The owner of an rproxy route name (see the module); `None` for names no route of ours has.
/// Told apart by the names' shapes: an HTTPRoute's `<ns>/<name>/r<i>/m<j>/h<k>[@<listener>]`, an
/// IngressRoute's `traefik/<ns>/<name>/r<i>`, an Ingress's `ingress/<ns>/<name>/r<i>/p<j>` (an
/// HTTPRoute in a namespace called `traefik` or `ingress` keeps its own shape).
pub fn owner(name: &str) -> Option<String> {
	if name == RESPOND_500 {
		return None;
	}
	let (prefix, rest) = match name.strip_prefix("grpc:") {
		Some(r) => ("grpc:", r),
		None => ("", name),
	};
	let index = |s: &str, c: char| {
		s.strip_prefix(c)
			.map(|d| d.split('@').next().unwrap_or_default())
			.is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
	};
	let segs: Vec<&str> = rest.split('/').collect();
	match segs.as_slice() {
		[ns, n, r, m, h] if index(r, 'r') && index(m, 'm') && index(h, 'h') => Some(format!("{prefix}{ns}/{n}")),
		["ingress", ns, n, r, p] if prefix.is_empty() && index(r, 'r') && index(p, 'p') => Some(format!("ingress:{ns}/{n}")),
		["traefik", ns, n, r] if prefix.is_empty() && index(r, 'r') => Some(format!("traefik:{ns}/{n}")),
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

/// The name at the head of rproxy's message for a rule (`route <name>: ...`, `middleware <name>:
/// ...`, `service <name>: ...`, the name maybe quoted), after `rules[i]: `.
fn head(message: &str) -> Option<&str> {
	let m = self::message(message);
	let rest = ["route ", "middleware ", "service "].iter().find_map(|k| m.strip_prefix(k))?;
	match rest.strip_prefix('"') {
		Some(q) => {
			let (name, after) = q.split_once('"')?;
			after.starts_with(':').then_some(name)
		}
		None => rest.split_once(": ").map(|(name, _)| name),
	}
}

/// The owners whose routes use what the head of rproxy's message names (empty: it names nothing of
/// a route). Only the head is read: the rest of the message can hold values a tenant wrote (an
/// address, a duration), which could name another tenant's route.
pub fn named(rule: &Value, message: &str) -> Vec<String> {
	let http = &rule["http"];
	let Some(hit) = head(message).filter(|h| *h != RESPOND_500) else {
		return vec![];
	};
	let known = routes(rule).any(|r| r["name"] == hit)
		|| ["services", "middlewares"].iter().any(|part| http[*part].as_object().is_some_and(|m| m.contains_key(hit)));
	if !known {
		return vec![];
	}
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

/// A digest of what `owner` puts into the rule (its routes and what only they use): an owner
/// left out stays left out while this is the same (`controller::sync_pod`).
pub fn content(rule: &Value, owner: &str) -> String {
	let mine: Vec<&Value> = routes(rule).filter(|r| r["name"].as_str().and_then(self::owner).as_deref() == Some(owner)).collect();
	let http = &rule["http"];
	let parts: Vec<(&String, &Value)> = used(http, mine.iter().copied())
		.iter()
		.filter_map(|n| {
			http["services"]
				.as_object()
				.and_then(|m| m.get_key_value(n))
				.or_else(|| http["middlewares"].as_object().and_then(|m| m.get_key_value(n)))
		})
		.collect();
	crate::pem::sha256_hex(&serde_json::to_vec(&(mine, parts)).unwrap_or_default())
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

/// Most dry runs one pass makes to find refused routes.
pub const MAX_DRY_RUNS: usize = 24;

/// The owners to leave out of `rules[i]`, which rproxy refused with `message`. `Ok(None)`: the
/// fault is not in one route, or finding it would take more than the dry runs left (`budget`):
/// the rule is left out as a whole. `Err`: a dry run got no answer (rproxy not reachable, a
/// timeout, 5xx): nothing is decided, the pass is tried again.
pub async fn find(t: &Target<'_>, rules: &[Value], i: usize, message: &str, budget: &mut usize) -> Result<Option<Vec<String>>, String> {
	let all = owners(&rules[i]);
	if all.is_empty() {
		return Ok(None);
	}
	let hit = named(&rules[i], message);
	if !hit.is_empty() {
		return Ok(Some(hit));
	}
	// is the rule refused with only the routes of `keep`? (`None`: no dry runs left)
	let fails = async |keep: &[String], budget: &mut usize| -> Result<Option<bool>, String> {
		if *budget == 0 {
			return Ok(None);
		}
		*budget -= 1;
		let out: BTreeSet<String> = all.iter().filter(|o| !keep.contains(o)).cloned().collect();
		let mut trial = rules.to_vec();
		trial[i] = without(&rules[i], &out);
		match t.rp.put_ruleset_dry_run(t.addr, t.set, t.generation, &trial).await {
			Ok(()) => Ok(Some(false)),
			Err(e) => match e.downcast_ref::<ApiError>() {
				Some(a) if a.status.is_client_error() => Ok(Some(super::refused_index(&a.message) == Some(i))),
				_ => Err(format!("dry run: {e:#}")),
			},
		}
	};
	// the rule without any of its routes: still refused, so not a route's fault
	match fails(&[], budget).await? {
		Some(false) => {}
		_ => return Ok(None),
	}
	let mut c = all.clone();
	while c.len() > 1 {
		let (a, b) = c.split_at(c.len() / 2);
		match fails(a, budget).await? {
			Some(true) => c = a.to_vec(),
			None => return Ok(None),
			Some(false) => match fails(b, budget).await? {
				Some(true) => c = b.to_vec(),
				// only some routes together are refused (names clashing): not one route's fault
				_ => return Ok(None),
			},
		}
	}
	Ok(Some(c))
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
		assert_eq!(owner("traefik/ns/ir/r3").as_deref(), Some("traefik:ns/ir"));
		assert_eq!(owner("ingress/ns/ing/r0/p1").as_deref(), Some("ingress:ns/ing"));
		// HTTPRoutes in namespaces called traefik / ingress are HTTPRoutes
		assert_eq!(owner("traefik/web/r0/m0/h0").as_deref(), Some("traefik/web"));
		assert_eq!(owner("ingress/web/r1/m0/h2@l").as_deref(), Some("ingress/web"));
		assert_eq!(owner(RESPOND_500), None);
		assert_eq!(owner("a/web/r0/mirror"), None);
		assert_eq!(owners(&rule()), ["a/web", "grpc:a/web", "b/api", "traefik:b/ir"]);
	}

	#[test]
	fn names_in_messages() {
		let r = rule();
		assert_eq!(named(&r, "rules[0]: route a/web/r0/m0/h10: match: PathRegexp: regex parse error"), ["a/web"]);
		assert_eq!(named(&r, "route grpc:a/web/r0/m0/h0: match: bad"), ["grpc:a/web"]);
		// a middleware: the routes using it (through a service's servers and a mirror too)
		assert_eq!(named(&r, "rules[2]: middleware b/api/r0/b0/f0.0: \"a\\r\" is not a valid header value"), ["b/api"]);
		assert_eq!(named(&r, "service \"b/api/r0/b0/f0/mirror\": servers is empty"), ["b/api"]);
		// shared by several owners: all of them
		assert_eq!(named(&r, "middleware traefik/b/shared: bad"), ["b/api", "traefik:b/ir"]);
		assert!(named(&r, "middleware rproxy-gateway/500: bad").is_empty());
		assert!(named(&r, "tls: no certificate").is_empty());
		assert!(named(&r, "route nobody/x/r0/m0/h0: bad").is_empty());
		assert_eq!(message("rules[3]: route x: bad"), "route x: bad");
	}

	/// rproxy echoes values a tenant wrote: a victim's route name in them must not get the victim
	/// left out (only the head of the message is read).
	#[test]
	fn a_name_in_a_value_is_not_trusted() {
		let r = rule();
		for m in [
			"rules[0]: middleware tenant/mw: address \"route a/web/r0/m0/h10: \" must start with http://",
			"rules[0]: tls: route a/web/r0/m0/h10: x",
			"rules[0]: crowdsec on_error \"a/web/r0/m0/h10\" must be allow or block",
			"rules[0]: period: \"route b/api/r0/m0/h0@https: 1x\" is not a duration",
		] {
			assert!(named(&r, m).is_empty(), "{m}");
		}
	}

	#[test]
	fn contents() {
		let r = rule();
		let before = content(&r, "b/api");
		let mut changed = r.clone();
		changed["http"]["middlewares"]["b/api/r0/b0/f0.0"] = json!({"mirror": {"service": "elsewhere"}});
		assert_ne!(content(&changed, "b/api"), before);
		// another owner's change does not touch it
		changed = r.clone();
		changed["http"]["services"]["a/web/r0"] = json!({"servers": [{"url": "http://10.9.9.9:80"}]});
		assert_eq!(content(&changed, "b/api"), before);
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
