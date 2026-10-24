//! What a tenant's middleware may reach (#94). One port is one rproxy rule holding every route's
//! services and middlewares, so a middleware written by a tenant (an RproxyMiddleware, a Traefik
//! Middleware migrated from an IngressRoute) could otherwise name another tenant's service
//! (`errors.service`, `mirror.service`, `forward_auth.service`) or make the Gateway's pods call any
//! URL (`forward_auth.address`, `oidc.issuer`).
//!
//! - The services an RproxyMiddleware names are its route's own rules: a rule's `name` or `r<i>`
//!   (or the rule's full rproxy name), rewritten to the route's rproxy service. Anything else
//!   makes the route `Accepted: False` (it never reaches rproxy).
//! - URLs out and kinds: what the GatewayClass's parameters allow (`policy.middlewares`; by
//!   default every kind and no URL out).

use serde_json::Value;

use crate::k8s::gateway::HttpRouteRule;
use crate::k8s::params::MiddlewarePolicy;

/// Where a middleware names a service of the rule set.
pub const SERVICE_FIELDS: &[&str] = &["/errors/service", "/mirror/service", "/forward_auth/service"];

/// Where a middleware names a URL rproxy calls out to.
const OUTBOUND_FIELDS: &[&str] = &["/forward_auth/address", "/oidc/issuer"];

/// `/errors/service` → `errors.service`.
fn field(at: &str) -> String {
	at.trim_start_matches('/').replace('/', ".")
}

/// A tenant's middleware against the class's policy: its kind, and the URLs it calls out to.
/// `None`: the class's parameters are invalid, and no middleware of a tenant is taken until they
/// are fixed (a typo elsewhere must not open what the policy closes).
pub fn check(v: &Value, policy: Option<&MiddlewarePolicy>) -> Result<(), String> {
	let Some(policy) = policy else {
		return Err("the GatewayClass's parameters are invalid: tenants' middlewares are refused until they are fixed".into());
	};
	if let Some(allowed) = &policy.allowed_kinds {
		if let Some(kind) = v.as_object().and_then(|o| o.keys().find(|k| !allowed.contains(k))) {
			return Err(format!("the middleware kind {kind} is not allowed by the GatewayClass (policy.middlewares.allowedKinds)"));
		}
	}
	let allow = policy.outbound_hosts.as_deref().unwrap_or_default();
	for at in OUTBOUND_FIELDS {
		let Some(u) = v.pointer(at).and_then(Value::as_str).filter(|u| !u.is_empty()) else { continue };
		let host = url_host(u, *at == "/oidc/issuer").map_err(|e| format!("{} {u:?}: {e}", field(at)))?;
		if !outbound_allowed(&host, allow) {
			return Err(format!(
				"{} {u:?}: {host} is not allowed by the GatewayClass (policy.middlewares.outboundHosts; by default no URL out)",
				field(at)
			));
		}
	}
	Ok(())
}

/// The host of an `http://` / `https://` URL, lower case, read as strictly as rproxy reads the URLs
/// it connects to (rproxy-api's `net::url`, #324), so that the controller gives the error and no
/// URL parser reads another host than this one: visible ASCII only, no `\` or `#`, no user info,
/// a host name of letters, digits, `-`, `_` and `.` (no empty label, none starting or ending with
/// `-`, one trailing `.`), IPv4 as `a.b.c.d` only (`127.1` is not), IPv6 in brackets without a zone,
/// a port of 1-65535. `base`: a URL rproxy appends paths to (`oidc.issuer`) takes no query either.
fn url_host(u: &str, base: bool) -> Result<String, String> {
	let rest = u.strip_prefix("http://").or_else(|| u.strip_prefix("https://")).ok_or("must start with http:// or https://")?;
	if !u.bytes().all(|b| b.is_ascii_graphic()) || u.contains(['\\', '#']) {
		return Err("only visible ASCII, without \\ or #".into());
	}
	if base && u.contains('?') {
		return Err("takes no query (?)".into());
	}
	let authority = rest.split(['/', '?']).next().unwrap_or_default();
	let plain = || format!("{authority:?} is not a plain host[:port]");
	let (host, port, v6) = match authority.strip_prefix('[') {
		Some(inner) => {
			let (h, after) = inner.split_once(']').ok_or_else(plain)?;
			match after {
				"" => (h, None, true),
				_ => (h, Some(after.strip_prefix(':').ok_or_else(plain)?), true),
			}
		}
		None => match authority.split_once(':') {
			Some((h, p)) => (h, Some(p), false),
			None => (authority, None, false),
		},
	};
	if let Some(p) = port {
		if !p.bytes().all(|b| b.is_ascii_digit()) || !p.parse::<u32>().is_ok_and(|n| (1..=65535).contains(&n)) {
			return Err(format!("{authority:?}: the port must be 1-65535"));
		}
	}
	if v6 {
		host.parse::<std::net::Ipv6Addr>().map_err(|_| plain())?;
		return Ok(host.to_ascii_lowercase());
	}
	let name = host.strip_suffix('.').unwrap_or(host);
	let labels: Vec<&str> = name.split('.').collect();
	let label_ok = |l: &&str| {
		!l.is_empty() && !l.starts_with('-') && !l.ends_with('-') && l.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
	};
	if name.is_empty() || !labels.iter().all(label_ok) {
		return Err(plain());
	}
	// a last label of digits is an IPv4 address, in the a.b.c.d form only (`127.1`, `010.0.0.1` are not)
	if labels.last().is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit())) && name.parse::<std::net::Ipv4Addr>().is_err() {
		return Err(format!("{authority:?}: an IPv4 address is a.b.c.d"));
	}
	Ok(name.to_ascii_lowercase())
}

/// Whether `host` (from [`url_host`]) is in the allow list.
fn outbound_allowed(host: &str, allow: &[String]) -> bool {
	let ip = host.parse::<std::net::IpAddr>().ok();
	allow.iter().any(|entry| {
		let entry = entry.trim().trim_end_matches('.').to_ascii_lowercase();
		if entry == "*" {
			return true;
		}
		if let Some(suffix) = entry.strip_prefix('*') {
			return suffix.starts_with('.') && host.len() > suffix.len() && host.ends_with(suffix);
		}
		match entry.parse::<crate::render::Cidr>() {
			// an IP range matches IP addresses only (a host name may resolve anywhere)
			Ok(c) => ip.is_some_and(|ip| c.contains(ip)),
			Err(_) => entry == host,
		}
	})
}

/// The class's `policy.middlewares` (an error names the field).
pub fn check_policy(p: &MiddlewarePolicy) -> Result<(), String> {
	let kinds = crate::render::validate::kind_names();
	for k in p.allowed_kinds.iter().flatten() {
		if !kinds.contains(&k.as_str()) {
			return Err(format!("spec.policy.middlewares.allowedKinds: {k}: not an rproxy middleware kind (one of {})", kinds.join(", ")));
		}
	}
	for h in p.outbound_hosts.iter().flatten() {
		let name = h.strip_prefix("*.").unwrap_or(h);
		let ok = h == "*"
			|| h.parse::<crate::render::Cidr>().is_ok()
			|| (!name.is_empty() && !name.starts_with('.') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b"-.".contains(&b)));
		if !ok {
			return Err(format!("spec.policy.middlewares.outboundHosts: {h:?}: not *, a host name, *.<domain> or an IP range"));
		}
	}
	Ok(())
}

/// The services an RproxyMiddleware names, as its route's own: a rule's `name`, `r<i>`, or the
/// route's full rproxy name of a rule (`<namespace>/<route>/r<i>`, with `grpc:` for a GRPCRoute),
/// rewritten to `<namespace>/<route>/r<i>` (a GRPCRoute's names get `grpc:` afterwards, with the
/// rest of its output). A rule's `name` comes first: a rule named like `r1` is that rule, not the
/// rule of index 1. Anything else (another route's service) is refused.
pub fn own_services(v: &mut Value, ns: &str, route: &str, rules: &[HttpRouteRule], grpc: bool) -> Result<(), String> {
	for at in SERVICE_FIELDS {
		let Some(slot) = v.pointer_mut(at) else { continue };
		let Some(s) = slot.as_str() else { continue };
		let i = rule_index(s, ns, route, rules, grpc).ok_or_else(|| {
			format!(
				"{} {s:?}: only this route's own rules may be named (a rule's name, or r0{})",
				field(at),
				if rules.len() > 1 { format!(" to r{}", rules.len() - 1) } else { String::new() }
			)
		})?;
		*slot = Value::String(format!("{ns}/{route}/r{i}"));
	}
	Ok(())
}

fn rule_index(s: &str, ns: &str, route: &str, rules: &[HttpRouteRule], grpc: bool) -> Option<usize> {
	if let Some(i) = rules.iter().position(|r| r.name.as_deref() == Some(s)) {
		return Some(i);
	}
	let full = format!("{ns}/{route}/");
	// a GRPCRoute's rproxy names carry `grpc:` (`Output::prefix_names`)
	let s = if grpc { s.strip_prefix("grpc:").filter(|r| r.starts_with(&full)).unwrap_or(s) } else { s };
	let short = s.strip_prefix(&full).unwrap_or(s);
	let i: usize = short.strip_prefix('r')?.parse().ok()?;
	(i < rules.len() && short == format!("r{i}")).then_some(i)
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	/// `check` with the class's parameters valid.
	fn check(v: &Value, p: &MiddlewarePolicy) -> Result<(), String> {
		super::check(v, Some(p))
	}

	fn rules(names: &[Option<&str>]) -> Vec<HttpRouteRule> {
		names.iter().map(|n| HttpRouteRule { name: n.map(String::from), ..Default::default() }).collect()
	}

	#[test]
	fn services_stay_in_the_route() {
		let rs = rules(&[None, Some("pages")]);
		let own = |v: Value| {
			let mut v = v;
			own_services(&mut v, "a", "web", &rs, false).map(|()| v)
		};
		// a rule's index, its name, or its full name: the route's own service
		assert_eq!(own(json!({"errors": {"status": ["500"], "service": "r1", "path": "/e"}})).unwrap()["errors"]["service"], "a/web/r1");
		assert_eq!(own(json!({"mirror": {"service": "pages"}})).unwrap()["mirror"]["service"], "a/web/r1");
		assert_eq!(own(json!({"forward_auth": {"service": "a/web/r0"}})).unwrap()["forward_auth"]["service"], "a/web/r0");
		// another tenant's route, another route of the namespace, a rule that does not exist
		for bad in ["b/other/r0", "a/other/r0", "r2", "r01", "x", "a/web/r9", ""] {
			let e = own(json!({"mirror": {"service": bad}})).unwrap_err();
			assert!(e.contains("only this route's own rules") && e.contains("r0 to r1"), "{bad}: {e}");
		}
		// no service named: unchanged
		assert_eq!(own(json!({"forward_auth": {"address": "http://x"}})).unwrap(), json!({"forward_auth": {"address": "http://x"}}));
		assert_eq!(own(json!({"forward_auth": {"service": null}})).unwrap(), json!({"forward_auth": {"service": null}}));
	}

	#[test]
	fn outbound_urls() {
		let fa = |u: &str| json!({"forward_auth": {"address": u}});
		let oidc = |u: &str| json!({"oidc": {"issuer": u, "client_id": "c"}});
		let none = MiddlewarePolicy::DEFAULT;
		// by default, no URL out
		let e = check(&fa("http://auth.example.com/check"), &none).unwrap_err();
		assert!(e.contains("forward_auth.address") && e.contains("outboundHosts"), "{e}");
		assert!(check(&oidc("https://id.example.com"), &none).unwrap_err().contains("oidc.issuer"));
		// what needs no URL out is untouched
		assert!(check(&json!({"forward_auth": {"service": "r0"}}), &none).is_ok());
		assert!(check(&json!({"headers": {"frame_deny": true}}), &none).is_ok());
		let p =
			|hosts: &[&str]| MiddlewarePolicy { outbound_hosts: Some(hosts.iter().map(|h| h.to_string()).collect()), ..Default::default() };
		assert!(check(&fa("http://auth.example.com:9091/x"), &p(&["auth.example.com"])).is_ok());
		assert!(check(&fa("http://AUTH.example.com./x"), &p(&["auth.example.com"])).is_ok());
		assert!(check(&fa("http://other.example.com/x"), &p(&["auth.example.com"])).is_err());
		assert!(check(&oidc("https://id.example.com"), &p(&["*.example.com"])).is_ok());
		assert!(check(&oidc("https://example.com"), &p(&["*.example.com"])).is_err());
		assert!(check(&oidc("https://evilexample.com"), &p(&["*.example.com"])).is_err());
		assert!(check(&fa("http://10.1.2.3:8080"), &p(&["10.0.0.0/8"])).is_ok());
		assert!(check(&fa("http://[fd00::1]:8080"), &p(&["fd00::/8"])).is_ok());
		assert!(check(&fa("http://169.254.169.254/latest"), &p(&["10.0.0.0/8"])).is_err());
		// an IP range never matches a host name
		assert!(check(&fa("http://ten.example.com"), &p(&["0.0.0.0/0"])).is_err());
		assert!(check(&fa("http://anything.internal"), &p(&["*"])).is_ok());
		// hosts another parser could read differently
		for u in [
			"http://auth.example.com@169.254.169.254/",
			"http://auth.example.com%2f@x/",
			"http://auth.example.com\\@x/",
			"http://auth.example.com:80:80/",
			"http://auth.example.com:x/",
			"http://[::1/",
			// as rproxy reads the URLs it connects to (rproxy-api #324)
			"http://auth.example.com:0/",
			"http://auth.example.com:65536/",
			"http://auth.example.com:/",
			"http://auth.example.com/#x",
			"http://auth.example.com/a b",
			"http://[fe80::1%25eth0]/",
		] {
			assert!(check(&fa(u), &p(&["auth.example.com"])).is_err(), "{u}");
		}
	}

	#[test]
	fn strict_hosts() {
		let any = MiddlewarePolicy { outbound_hosts: Some(vec!["*".into()]), ..Default::default() };
		let fa = |u: &str| check(&json!({"forward_auth": {"address": u}}), &any);
		for bad in [
			"http://127.1/",
			"http://010.0.0.1/",
			"http://0x7f.0.0.1/",
			"http://a..b/",
			"http://-a.example/",
			"http://a-.example/",
			"http://./",
		] {
			assert!(fa(bad).is_err(), "{bad}");
		}
		for good in ["http://127.0.0.1:65535/x?y=1", "http://auth_1.example./", "https://[2001:db8::1]:8443/p"] {
			assert!(fa(good).is_ok(), "{good}: {:?}", fa(good));
		}
		// a URL rproxy appends paths to takes no query
		let e = check(&json!({"oidc": {"issuer": "https://id.example/realms/x?a=1"}}), &any).unwrap_err();
		assert!(e.contains("query"), "{e}");
		assert!(check(&json!({"oidc": {"issuer": "https://id.example/realms/x"}}), &any).is_ok());
	}

	/// The class's parameters invalid: nothing is taken (a typo must not open what the policy closes).
	#[test]
	fn invalid_class_parameters_refuse_everything() {
		let e = super::check(&json!({"headers": {"frame_deny": true}}), None).unwrap_err();
		assert!(e.contains("parameters are invalid"), "{e}");
	}

	#[test]
	fn grpc_names() {
		let rs = rules(&[None, None]);
		let own = |s: &str, grpc: bool| {
			let mut v = json!({"mirror": {"service": s}});
			own_services(&mut v, "a", "svc", &rs, grpc).map(|()| v["mirror"]["service"].clone())
		};
		assert_eq!(own("grpc:a/svc/r1", true).unwrap(), "a/svc/r1");
		assert_eq!(own("r1", true).unwrap(), "a/svc/r1");
		assert!(own("grpc:a/svc/r1", false).is_err());
		assert!(own("grpc:b/svc/r0", true).is_err());
		// a rule's name first: a rule named like an index is that rule
		let named = rules(&[Some("r1"), None]);
		let mut v = json!({"mirror": {"service": "r1"}});
		own_services(&mut v, "a", "svc", &named, false).unwrap();
		assert_eq!(v["mirror"]["service"], "a/svc/r0");
	}

	#[test]
	fn kinds() {
		let p = MiddlewarePolicy { allowed_kinds: Some(vec!["headers".into()]), ..Default::default() };
		assert!(check(&json!({"headers": {}}), &p).is_ok());
		assert!(check(&json!({"rate_limit": {"average": 1}}), &p).unwrap_err().contains("allowedKinds"));
	}

	#[test]
	fn policy() {
		let ok = |k: &[&str], h: &[&str]| {
			check_policy(&MiddlewarePolicy {
				allowed_kinds: Some(k.iter().map(|s| s.to_string()).collect()),
				outbound_hosts: Some(h.iter().map(|s| s.to_string()).collect()),
			})
		};
		assert!(ok(&["headers", "forward_auth"], &["*", "a.example.com", "*.example.com", "10.0.0.0/8", "fd00::/8"]).is_ok());
		assert!(ok(&["nope"], &[]).unwrap_err().contains("allowedKinds: nope"));
		assert!(ok(&[], &["http://a"]).unwrap_err().contains("outboundHosts"));
		assert!(ok(&[], &["*."]).is_err());
		assert!(ok(&[], &["10.0.0.0/99"]).is_err());
	}
}
