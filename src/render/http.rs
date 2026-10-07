//! HTTPRoute → rproxy `http` routes, services and middlewares.
//!
//! One rproxy route per (route rule, match, host name). Gateway API's
//! precedence (most specific host name, then exact path, longest prefix,
//! method, most header matches, most query matches, oldest route, namespace and
//! name, rule order) is turned into explicit `priority` values once all routes
//! on a port are known (`assign_priorities`).

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::k8s::crd::GROUP as RPROXY_GROUP;
use crate::k8s::gateway::{HeaderModifier, HttpRoute, HttpRouteFilter, HttpRouteMatch, PathModifier, RequestRedirect};
use crate::render::backends::{self, Endpoint};
use crate::render::hostname;
use crate::render::status::Cond;
use crate::render::world::World;
use crate::rproxy::model as rp;

/// What a request that reaches a rule without usable backends gets (Gateway API: 500).
pub const RESPOND_500: &str = "rproxy-gateway/500";

/// Where the routes are served.
pub struct Ctx<'a> {
	pub world: &'a World,
	/// `http` or `https`.
	pub scheme: &'static str,
	pub port: u16,
}

/// One rproxy route before priorities are given.
#[derive(Clone, Debug)]
pub struct Entry {
	pub key: SortKey,
	pub route: rp::HttpRoute,
}

/// Gateway API precedence, compared so that the first entry wins.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct SortKey {
	/// Reversed: larger is more specific.
	pub host: std::cmp::Reverse<(u8, usize)>,
	pub path_kind: std::cmp::Reverse<u8>,
	pub path_len: std::cmp::Reverse<usize>,
	pub method: std::cmp::Reverse<bool>,
	pub headers: std::cmp::Reverse<usize>,
	pub query: std::cmp::Reverse<usize>,
	/// The route's creation time (RFC 3339 sorts as text).
	pub created: String,
	pub namespace: String,
	pub name: String,
	pub rule: usize,
	pub matched: usize,
	pub host_index: usize,
}

/// Gives each entry a priority so rproxy tries them in precedence order.
pub fn assign_priorities(entries: &mut [Entry]) -> Vec<rp::HttpRoute> {
	entries.sort_by(|a, b| a.key.cmp(&b.key));
	let n = entries.len() as i64;
	entries
		.iter()
		.enumerate()
		.map(|(i, e)| {
			let mut r = e.route.clone();
			r.priority = Some(n - i as i64);
			r
		})
		.collect()
}

/// The routes, services and middlewares one HTTPRoute adds to a port.
#[derive(Debug, Default)]
pub struct Output {
	pub entries: Vec<Entry>,
	pub services: BTreeMap<String, rp::Service>,
	pub middlewares: BTreeMap<String, Value>,
	/// `ResolvedRefs` of the route.
	pub resolved: Option<Cond>,
	/// Why the route cannot be accepted (an unsupported filter or value).
	pub unsupported: Option<String>,
}

/// Quotes an argument of a `match` expression (backticks, or double quotes for a value with a backtick).
fn quote(s: &str) -> Option<String> {
	if !s.contains('`') {
		Some(format!("`{s}`"))
	} else if !s.contains('"') {
		Some(format!("\"{s}\""))
	} else {
		None
	}
}

/// A regular expression matching `s` literally.
pub fn regex_escape(s: &str) -> String {
	let mut out = String::with_capacity(s.len());
	for c in s.chars() {
		if "\\.+*?()|[]{}^$#&-~".contains(c) {
			out.push('\\');
		}
		out.push(c);
	}
	out
}

/// `$` written literally in a regex replacement.
fn replacement_literal(s: &str) -> String {
	s.replace('$', "$$")
}

/// The `match` expression of one Gateway API match on the given host names.
fn expression(m: &HttpRouteMatch, host: Option<&str>, exclusions: &[String]) -> Result<String, String> {
	let q = |s: &str| quote(s).ok_or_else(|| format!("{s:?}: a value cannot hold both ` and \""));
	let mut parts = vec![];
	if let Some(h) = host {
		parts.push(format!("Host({})", q(&hostname::to_rproxy(h))?));
	}
	for x in exclusions {
		if host.is_none_or(|h| hostname::intersect(h, x).is_some()) {
			parts.push(format!("!Host({})", q(&hostname::to_rproxy(x))?));
		}
	}
	let (kind, value) = path_of(m);
	match kind {
		"Exact" => parts.push(format!("Path({})", q(&value)?)),
		"RegularExpression" => parts.push(format!("PathRegexp({})", q(&format!("^(?:{value})$"))?)),
		_ => {
			let p = value.trim_end_matches('/');
			if !p.is_empty() {
				parts.push(format!("(Path({}) || PathPrefix({}))", q(p)?, q(&format!("{p}/"))?));
			}
		}
	}
	if let Some(method) = &m.method {
		parts.push(format!("Method({})", q(method)?));
	}
	for h in &m.headers {
		match h.kind.as_deref().unwrap_or("Exact") {
			"Exact" => parts.push(format!("Header({}, {})", q(&h.name)?, q(&h.value)?)),
			"RegularExpression" => parts.push(format!("HeaderRegexp({}, {})", q(&h.name)?, q(&h.value)?)),
			other => return Err(format!("header match type {other} is not supported")),
		}
	}
	for p in &m.query_params {
		match p.kind.as_deref().unwrap_or("Exact") {
			"Exact" => parts.push(format!("Query({}, {})", q(&p.name)?, q(&p.value)?)),
			"RegularExpression" => parts.push(format!("QueryRegexp({}, {})", q(&p.name)?, q(&p.value)?)),
			other => return Err(format!("query match type {other} is not supported")),
		}
	}
	if parts.is_empty() {
		parts.push("PathPrefix(`/`)".into());
	}
	Ok(parts.join(" && "))
}

/// The path match: (type, value), defaulting to `PathPrefix /`.
fn path_of(m: &HttpRouteMatch) -> (&str, String) {
	match &m.path {
		Some(p) => (p.kind.as_deref().unwrap_or("PathPrefix"), p.value.clone().unwrap_or_else(|| "/".into())),
		None => ("PathPrefix", "/".into()),
	}
}

fn sort_key(route: &HttpRoute, m: &HttpRouteMatch, host: Option<&str>, rule: usize, matched: usize, host_index: usize) -> SortKey {
	use std::cmp::Reverse;
	let (kind, value) = path_of(m);
	let (path_kind, path_len) = match kind {
		"Exact" => (3, value.len()),
		"RegularExpression" => (1, value.len()),
		_ => (2, value.trim_end_matches('/').len()),
	};
	SortKey {
		host: Reverse(hostname::specificity(host)),
		path_kind: Reverse(path_kind),
		path_len: Reverse(path_len),
		method: Reverse(m.method.is_some()),
		headers: Reverse(m.headers.len()),
		query: Reverse(m.query_params.len()),
		created: route.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()).unwrap_or_default(),
		namespace: route.metadata.namespace.clone().unwrap_or_default(),
		name: route.metadata.name.clone().unwrap_or_default(),
		rule,
		matched,
		host_index,
	}
}

fn header_ops(m: &HeaderModifier) -> Value {
	let mut set = serde_json::Map::new();
	// rproxy's headers middleware has set and remove; add is applied as set
	for h in m.set.iter().chain(&m.add) {
		set.insert(h.name.clone(), Value::String(h.value.clone()));
	}
	let mut out = serde_json::Map::new();
	if !set.is_empty() {
		out.insert("set".into(), Value::Object(set));
	}
	if !m.remove.is_empty() {
		out.insert("remove".into(), json!(m.remove));
	}
	Value::Object(out)
}

/// The path prefix a `ReplacePrefixMatch` replaces: the match's prefix without a trailing slash.
fn matched_prefix(m: &HttpRouteMatch) -> String {
	let (kind, value) = path_of(m);
	if kind == "PathPrefix" { value.trim_end_matches('/').to_string() } else { String::new() }
}

const URL_HEAD: &str = "^[a-z]+://([^/?]*?)(?::[0-9]+)?";

fn redirect(ctx: &Ctx, r: &RequestRedirect, m: &HttpRouteMatch) -> Result<Vec<Value>, String> {
	let status = r.status_code.unwrap_or(302);
	let permanent = match status {
		301 => true,
		302 => false,
		other => return Err(format!("RequestRedirect statusCode {other} is not supported (301 and 302)")),
	};
	let scheme = r.scheme.clone().unwrap_or_else(|| ctx.scheme.to_string()).to_ascii_lowercase();
	let default_port = |s: &str| if s == "https" { 443 } else { 80 };
	let port = match r.port {
		Some(p) if p == default_port(&scheme) => String::new(),
		Some(p) => format!(":{p}"),
		None if r.scheme.is_some() => String::new(),
		None if i32::from(ctx.port) == default_port(&scheme) => String::new(),
		None => format!(":{}", ctx.port),
	};
	let host = match &r.hostname {
		Some(h) => replacement_literal(h),
		None => "${1}".into(),
	};
	let head = format!("{}://{host}{port}", replacement_literal(&scheme));
	let mw = |regex: String, replacement: String| json!({"redirect_regex": {"regex": regex, "replacement": replacement, "permanent": permanent}});
	match &r.path {
		None => Ok(vec![mw(format!("{URL_HEAD}(/[^?]*)?(\\?.*)?$"), format!("{head}${{2}}${{3}}"))]),
		Some(PathModifier { kind, replace_full_path: Some(p), .. }) if kind == "ReplaceFullPath" => {
			Ok(vec![mw(format!("{URL_HEAD}(/[^?]*)?(\\?.*)?$"), format!("{head}{}${{3}}", replacement_literal(p)))])
		}
		Some(PathModifier { kind, replace_prefix_match: Some(np), .. }) if kind == "ReplacePrefixMatch" => {
			let prefix = regex_escape(&matched_prefix(m));
			let np = np.trim_end_matches('/');
			if np.is_empty() {
				Ok(vec![
					mw(format!("{URL_HEAD}{prefix}(/[^?]*)(\\?.*)?$"), format!("{head}${{2}}${{3}}")),
					mw(format!("{URL_HEAD}{prefix}(\\?.*)?$"), format!("{head}/${{2}}")),
				])
			} else {
				Ok(vec![mw(format!("{URL_HEAD}{prefix}(/[^?]*)?(\\?.*)?$"), format!("{head}{}${{2}}${{3}}", replacement_literal(np)))])
			}
		}
		Some(p) => Err(format!("RequestRedirect path type {} is not supported", p.kind)),
	}
}

fn rewrite(f: &crate::k8s::gateway::UrlRewrite, m: &HttpRouteMatch) -> Result<Vec<Value>, String> {
	if f.hostname.is_some() {
		return Err("URLRewrite hostname is not supported (rproxy sets Host from the client or the backend URL)".into());
	}
	match &f.path {
		None => Ok(vec![]),
		Some(PathModifier { kind, replace_full_path: Some(p), .. }) if kind == "ReplaceFullPath" => {
			Ok(vec![json!({"replace_path": {"path": p}})])
		}
		Some(PathModifier { kind, replace_prefix_match: Some(np), .. }) if kind == "ReplacePrefixMatch" => {
			let prefix = regex_escape(&matched_prefix(m));
			let np = np.trim_end_matches('/');
			if np.is_empty() {
				Ok(vec![
					json!({"replace_path_regex": {"regex": format!("^{prefix}(/.*)$"), "replacement": "${1}"}}),
					json!({"replace_path_regex": {"regex": format!("^{prefix}$"), "replacement": "/"}}),
				])
			} else {
				Ok(vec![
					json!({"replace_path_regex": {"regex": format!("^{prefix}(/.*)?$"), "replacement": format!("{}${{1}}", replacement_literal(np))}}),
				])
			}
		}
		Some(p) => Err(format!("URLRewrite path type {} is not supported", p.kind)),
	}
}

/// The middlewares of one filter (several for prefix replacements), or why it cannot be used.
/// `Ok(None)`: an ExtensionRef that does not resolve (the rule answers 500).
fn filter(ctx: &Ctx, ns: &str, f: &HttpRouteFilter, m: &HttpRouteMatch) -> Result<Option<Vec<Value>>, String> {
	Ok(Some(match f.kind.as_str() {
		"RequestHeaderModifier" => {
			vec![json!({"headers": {"request": header_ops(f.request_header_modifier.as_ref().ok_or("requestHeaderModifier is missing")?)}})]
		}
		"ResponseHeaderModifier" => {
			vec![
				json!({"headers": {"response": header_ops(f.response_header_modifier.as_ref().ok_or("responseHeaderModifier is missing")?)}}),
			]
		}
		"RequestRedirect" => redirect(ctx, f.request_redirect.as_ref().ok_or("requestRedirect is missing")?, m)?,
		"URLRewrite" => rewrite(f.url_rewrite.as_ref().ok_or("urlRewrite is missing")?, m)?,
		"ExtensionRef" => {
			let r = f.extension_ref.as_ref().ok_or("extensionRef is missing")?;
			if r.group != RPROXY_GROUP || r.kind != "RproxyMiddleware" {
				return Err(format!("ExtensionRef {}/{} is not supported (RproxyMiddleware only)", r.group, r.kind));
			}
			match ctx.world.middlewares.get(&(ns.to_string(), r.name.clone())) {
				Some(mw) => vec![Value::Object(mw.spec.0.0.clone())],
				None => return Ok(None),
			}
		}
		other => return Err(format!("filter {other} is not supported")),
	}))
}

/// Builds the routes of `route` served on a listener: `hosts` are the route's
/// effective host names there (`None`: any), `exclusions` the host names more
/// specific listeners on the same port take.
pub fn build(ctx: &Ctx, route: &HttpRoute, hosts: Option<&[String]>, exclusions: &[String]) -> Output {
	let mut out = Output::default();
	let ns = route.metadata.namespace.clone().unwrap_or_default();
	let name = route.metadata.name.clone().unwrap_or_default();
	let host_list: Vec<Option<&str>> = match hosts {
		Some(h) => h.iter().map(|s| Some(s.as_str())).collect(),
		None => vec![None],
	};
	let mut resolved: Option<Cond> = None;
	for (i, rule) in route.spec.rules.iter().enumerate() {
		let base = format!("{ns}/{name}/r{i}");
		// backends
		let mut weighted: Vec<(u32, Vec<Endpoint>)> = vec![];
		let mut any_invalid = false;
		for b in &rule.backend_refs {
			if !b.filters.is_empty() {
				out.unsupported.get_or_insert_with(|| "filters on backendRefs are not supported".into());
			}
			match backends::resolve(ctx.world, "HTTPRoute", &ns, &b.backend) {
				Ok(eps) => weighted.push((b.backend.weight.unwrap_or(1).max(0) as u32, eps)),
				Err(e) => {
					any_invalid = true;
					resolved.get_or_insert(e);
				}
			}
		}
		let servers: Vec<rp::Server> = backends::spread(&weighted)
			.into_iter()
			.map(|(ep, weight)| rp::Server { url: format!("http://{}", ep.authority()), weight })
			.collect();
		let service = if servers.is_empty() {
			None
		} else {
			let mut svc = rp::Service { servers, ..Default::default() };
			if let Some(t) = &rule.timeouts {
				if let Some(d) = t.backend_request.as_deref().or(t.request.as_deref()) {
					match crate::render::duration_ms(d) {
						Some(0) => {}
						Some(ms) => svc.timeouts = Some(json!({"response": format!("{ms}ms")})),
						None => {
							out.unsupported.get_or_insert_with(|| format!("timeout {d:?} is not a duration"));
						}
					}
				}
			}
			out.services.insert(base.clone(), svc);
			Some(base.clone())
		};
		let _ = any_invalid;
		let default_match = [HttpRouteMatch::default()];
		let matches: &[HttpRouteMatch] = if rule.matches.is_empty() { &default_match } else { &rule.matches };
		for (j, m) in matches.iter().enumerate() {
			let mut chain = vec![];
			let mut answers = false;
			let mut broken = false;
			for (k, f) in rule.filters.iter().enumerate() {
				match filter(ctx, &ns, f, m) {
					Ok(Some(mws)) => {
						for (x, mw) in mws.into_iter().enumerate() {
							answers |= mw.get("redirect_regex").is_some() || mw.get("respond").is_some();
							let n = format!("{base}/m{j}/f{k}.{x}");
							out.middlewares.insert(n.clone(), mw);
							chain.push(n);
						}
					}
					Ok(None) => {
						broken = true;
						resolved.get_or_insert(Cond::new(
							"ResolvedRefs",
							false,
							"BackendNotFound",
							format!("rule {i}: the ExtensionRef filter's RproxyMiddleware is not found"),
						));
					}
					Err(e) => {
						out.unsupported.get_or_insert(format!("rule {i}: {e}"));
					}
				}
			}
			let service = if broken { None } else { service.clone() };
			if service.is_none() && !answers {
				out.middlewares.insert(RESPOND_500.into(), json!({"respond": {"status": 500, "body": "500 Internal Server Error\n"}}));
				chain.push(RESPOND_500.into());
			}
			for (x, host) in host_list.iter().enumerate() {
				match expression(m, *host, exclusions) {
					Ok(rule_expr) => out.entries.push(Entry {
						key: sort_key(route, m, *host, i, j, x),
						route: rp::HttpRoute {
							name: format!("{base}/m{j}/h{x}"),
							rule: rule_expr,
							priority: None,
							service: service.clone(),
							middlewares: chain.clone(),
						},
					}),
					Err(e) => {
						out.unsupported.get_or_insert(format!("rule {i}: {e}"));
					}
				}
			}
		}
	}
	out.resolved = resolved;
	out
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::k8s::gateway::{HttpPathMatch, ValueMatch};

	fn m(path: Option<(&str, &str)>) -> HttpRouteMatch {
		HttpRouteMatch { path: path.map(|(k, v)| HttpPathMatch { kind: Some(k.into()), value: Some(v.into()) }), ..Default::default() }
	}

	#[test]
	fn expressions() {
		assert_eq!(expression(&m(None), None, &[]).unwrap(), "PathPrefix(`/`)");
		assert_eq!(expression(&m(Some(("Exact", "/a"))), Some("a.example.com"), &[]).unwrap(), "Host(`a.example.com`) && Path(`/a`)");
		assert_eq!(
			expression(&m(Some(("PathPrefix", "/v2/"))), Some("*.example.com"), &["a.example.com".into(), "b.other.net".into()]).unwrap(),
			"Host(`**.example.com`) && !Host(`a.example.com`) && (Path(`/v2`) || PathPrefix(`/v2/`))"
		);
		let mut hm = m(None);
		hm.method = Some("POST".into());
		hm.headers = vec![ValueMatch { kind: None, name: "version".into(), value: "two".into() }];
		hm.query_params = vec![ValueMatch { kind: Some("RegularExpression".into()), name: "q".into(), value: "^a`b".into() }];
		assert_eq!(expression(&hm, None, &[]).unwrap(), "Method(`POST`) && Header(`version`, `two`) && QueryRegexp(`q`, \"^a`b\")");
		assert_eq!(expression(&m(Some(("RegularExpression", "/r/[0-9]+"))), None, &[]).unwrap(), "PathRegexp(`^(?:/r/[0-9]+)$`)");
	}

	#[test]
	fn precedence() {
		let route = HttpRoute::default();
		let exact = sort_key(&route, &m(Some(("Exact", "/a"))), None, 0, 0, 0);
		let long = sort_key(&route, &m(Some(("PathPrefix", "/abc"))), None, 0, 1, 0);
		let short = sort_key(&route, &m(Some(("PathPrefix", "/a"))), None, 0, 2, 0);
		let host = sort_key(&route, &m(Some(("PathPrefix", "/"))), Some("a.example.com"), 1, 0, 0);
		let mut keys = [short.clone(), exact.clone(), long.clone(), host.clone()];
		keys.sort();
		assert_eq!(keys, [host, exact, long, short]);
	}

	#[test]
	fn redirects() {
		let world = World::default();
		let ctx = Ctx { world: &world, scheme: "http", port: 80 };
		let r = RequestRedirect { scheme: Some("https".into()), status_code: Some(301), ..Default::default() };
		let v = redirect(&ctx, &r, &m(None)).unwrap();
		assert_eq!(v[0]["redirect_regex"]["replacement"], "https://${1}${2}${3}");
		assert_eq!(v[0]["redirect_regex"]["permanent"], true);
		let r = RequestRedirect {
			hostname: Some("example.org".into()),
			path: Some(PathModifier { kind: "ReplacePrefixMatch".into(), replace_prefix_match: Some("/".into()), replace_full_path: None }),
			..Default::default()
		};
		let ctx8080 = Ctx { world: &world, scheme: "http", port: 8080 };
		let v = redirect(&ctx8080, &r, &m(Some(("PathPrefix", "/old/")))).unwrap();
		assert_eq!(v.len(), 2);
		assert_eq!(v[0]["redirect_regex"]["regex"], "^[a-z]+://([^/?]*?)(?::[0-9]+)?/old(/[^?]*)(\\?.*)?$");
		assert_eq!(v[0]["redirect_regex"]["replacement"], "http://example.org:8080${2}${3}");
		assert_eq!(v[1]["redirect_regex"]["replacement"], "http://example.org:8080/${2}");
		assert!(redirect(&ctx, &RequestRedirect { status_code: Some(307), ..Default::default() }, &m(None)).is_err());
	}

	#[test]
	fn rewrites() {
		let f = crate::k8s::gateway::UrlRewrite {
			hostname: None,
			path: Some(PathModifier {
				kind: "ReplacePrefixMatch".into(),
				replace_prefix_match: Some("/new".into()),
				replace_full_path: None,
			}),
		};
		let v = rewrite(&f, &m(Some(("PathPrefix", "/old")))).unwrap();
		assert_eq!(v, vec![json!({"replace_path_regex": {"regex": "^/old(/.*)?$", "replacement": "/new${1}"}})]);
		assert_eq!(regex_escape("/a.b$"), "/a\\.b\\$");
	}
}
