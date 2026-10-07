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
	/// What the rproxy pods take.
	pub features: &'a crate::render::Features,
	/// `HTTPRoute` or `GRPCRoute` (ReferenceGrants name the route's kind).
	pub kind: &'static str,
	/// A GRPCRoute: backends speak HTTP/2 (h2c).
	pub grpc: bool,
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
	/// `ResolvedRefs` of the route (the first problem).
	pub resolved: Option<Cond>,
	/// Why the route cannot be accepted (an unsupported filter or value).
	pub unsupported: Option<String>,
	/// rproxy service name → the Services it sends to (for RproxyPolicy).
	pub backends: BTreeMap<String, Vec<crate::render::world::Key>>,
}

impl Output {
	/// Prefixes the names of everything it adds (routes, services, middlewares; the service and
	/// middleware references follow), so two routes of different kinds with the same name do not clash.
	pub fn prefix_names(&mut self, prefix: &str) {
		let p = |n: &str| if n == RESPOND_500 { n.to_string() } else { format!("{prefix}{n}") };
		for e in &mut self.entries {
			e.route.name = p(&e.route.name);
			e.route.service = e.route.service.as_deref().map(p);
			e.route.middlewares = e.route.middlewares.iter().map(|m| p(m)).collect();
		}
		self.services = std::mem::take(&mut self.services)
			.into_iter()
			.map(|(k, mut v)| {
				for srv in &mut v.servers {
					srv.middlewares = srv.middlewares.iter().map(|m| p(m)).collect();
				}
				(p(&k), v)
			})
			.collect();
		self.middlewares = std::mem::take(&mut self.middlewares)
			.into_iter()
			.map(|(k, mut v)| {
				// a mirror names its service
				if let Some(svc) = v.pointer_mut("/mirror/service") {
					if let Some(s) = svc.as_str() {
						*svc = Value::String(p(s));
					}
				}
				(p(&k), v)
			})
			.collect();
		self.backends = std::mem::take(&mut self.backends).into_iter().map(|(k, v)| (p(&k), v)).collect();
	}
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

fn header_ops(ctx: &Ctx, m: &HeaderModifier) -> Result<Value, String> {
	let map = |hs: &[crate::k8s::gateway::HttpHeader]| -> serde_json::Map<String, Value> {
		hs.iter().map(|h| (h.name.clone(), Value::String(h.value.clone()))).collect()
	};
	let mut out = serde_json::Map::new();
	if !m.set.is_empty() {
		out.insert("set".into(), Value::Object(map(&m.set)));
	}
	if !m.add.is_empty() {
		// rproxy's `add` appends to a header already there (`a` → `a,v`)
		crate::render::needs(ctx.features.headers_add, "HeaderModifier add", "http_options headers_add")?;
		out.insert("add".into(), Value::Object(map(&m.add)));
	}
	if !m.remove.is_empty() {
		out.insert("remove".into(), json!(m.remove));
	}
	Ok(Value::Object(out))
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
		301 | 308 => true,
		302 | 303 | 307 => false,
		other => return Err(format!("RequestRedirect statusCode {other} is not supported")),
	};
	if !matches!(status, 301 | 302) {
		crate::render::needs(
			ctx.features.redirect_status,
			&format!("RequestRedirect statusCode {status}"),
			"http_options redirect_status",
		)?;
	}
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
	let with_status = ctx.features.redirect_status;
	let mw = |regex: String, replacement: String| {
		let mut v = json!({"regex": regex, "replacement": replacement, "permanent": permanent});
		if with_status {
			// the exact code (otherwise rproxy picks 307 / 308 for methods other than GET and HEAD)
			v["status"] = json!(status);
		}
		json!({ "redirect_regex": v })
	};
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

fn rewrite(ctx: &Ctx, f: &crate::k8s::gateway::UrlRewrite, m: &HttpRouteMatch) -> Result<Vec<Value>, String> {
	let mut out = vec![];
	if let Some(h) = &f.hostname {
		crate::render::needs(ctx.features.replace_host, "URLRewrite hostname", "middlewares replace_host")?;
		out.push(json!({"replace_host": {"host": h}}));
	}
	match &f.path {
		None => {}
		Some(PathModifier { kind, replace_full_path: Some(p), .. }) if kind == "ReplaceFullPath" => {
			out.push(json!({"replace_path": {"path": p}}));
		}
		Some(PathModifier { kind, replace_prefix_match: Some(np), .. }) if kind == "ReplacePrefixMatch" => {
			let prefix = regex_escape(&matched_prefix(m));
			let np = np.trim_end_matches('/');
			if np.is_empty() {
				out.push(json!({"replace_path_regex": {"regex": format!("^{prefix}(/.*)$"), "replacement": "${1}"}}));
				out.push(json!({"replace_path_regex": {"regex": format!("^{prefix}$"), "replacement": "/"}}));
			} else {
				out.push(
					json!({"replace_path_regex": {"regex": format!("^{prefix}(/.*)?$"), "replacement": format!("{}${{1}}", replacement_literal(np))}}),
				);
			}
		}
		Some(p) => return Err(format!("URLRewrite path type {} is not supported", p.kind)),
	}
	Ok(out)
}

fn cors(ctx: &Ctx, c: &crate::k8s::gateway::CorsFilter) -> Result<Value, String> {
	crate::render::needs(ctx.features.cors, "the CORS filter", "middlewares cors")?;
	let mut v = json!({
		"allow_origins": c.allow_origins,
		"allow_methods": c.allow_methods,
		"allow_headers": c.allow_headers,
		"expose_headers": c.expose_headers,
	});
	if c.allow_credentials == Some(true) {
		v["allow_credentials"] = json!(true);
	}
	// Gateway API's default is 5 seconds
	v["max_age"] = json!(c.max_age.unwrap_or(5));
	Ok(json!({ "cors": v }))
}

/// What a filter gives.
enum Filtered {
	/// Middlewares (several for prefix replacements).
	Middlewares(Vec<Value>),
	/// An ExtensionRef that does not resolve (the rule answers 500).
	Missing,
}

/// The middlewares of one filter, or why it cannot be used. `base`: names for
/// what the filter adds (a mirror's service).
fn filter(ctx: &Ctx, ns: &str, f: &HttpRouteFilter, m: &HttpRouteMatch, base: &str, out: &mut Output) -> Result<Filtered, String> {
	Ok(Filtered::Middlewares(match f.kind.as_str() {
		"RequestHeaderModifier" => {
			vec![
				json!({"headers": {"request": header_ops(ctx, f.request_header_modifier.as_ref().ok_or("requestHeaderModifier is missing")?)?}}),
			]
		}
		"ResponseHeaderModifier" => {
			vec![
				json!({"headers": {"response": header_ops(ctx, f.response_header_modifier.as_ref().ok_or("responseHeaderModifier is missing")?)?}}),
			]
		}
		"RequestRedirect" => redirect(ctx, f.request_redirect.as_ref().ok_or("requestRedirect is missing")?, m)?,
		"URLRewrite" => rewrite(ctx, f.url_rewrite.as_ref().ok_or("urlRewrite is missing")?, m)?,
		"CORS" => vec![cors(ctx, f.cors.as_ref().ok_or("cors is missing")?)?],
		"RequestMirror" => {
			crate::render::needs(ctx.features.mirror, "the RequestMirror filter", "middlewares mirror")?;
			let mirror = f.request_mirror.as_ref().ok_or("requestMirror is missing")?;
			let eps = match backends::resolve(ctx.world, ctx.kind, ns, &mirror.backend_ref) {
				Ok(eps) => eps,
				Err(e) => {
					out.resolved.get_or_insert(e);
					return Ok(Filtered::Middlewares(vec![]));
				}
			};
			if eps.is_empty() {
				// nothing to mirror to (no ready pod): requests go on unmirrored
				return Ok(Filtered::Middlewares(vec![]));
			}
			let name = format!("{base}/mirror");
			let servers = backends::spread(&[(1, eps)])
				.into_iter()
				.map(|(ep, weight)| rp::Server { url: format!("http://{}", ep.authority()), weight, ..Default::default() })
				.collect();
			out.services.insert(name.clone(), rp::Service { servers, ..Default::default() });
			let mut v = json!({ "service": name });
			if let Some(fr) = &mirror.fraction {
				v["fraction"] = json!({"numerator": fr.numerator, "denominator": fr.denominator.unwrap_or(100)});
			} else if let Some(p) = mirror.percent {
				v["percent"] = json!(p);
			}
			vec![json!({ "mirror": v })]
		}
		"ExtensionRef" => {
			let r = f.extension_ref.as_ref().ok_or("extensionRef is missing")?;
			if r.group != RPROXY_GROUP || r.kind != "RproxyMiddleware" {
				return Err(format!("ExtensionRef {}/{} is not supported (RproxyMiddleware only)", r.group, r.kind));
			}
			match ctx.world.middlewares.get(&(ns.to_string(), r.name.clone())) {
				Some(mw) => vec![Value::Object(mw.spec.0.0.clone())],
				None => return Ok(Filtered::Missing),
			}
		}
		other => return Err(format!("filter {other} is not supported")),
	}))
}

/// The middlewares of a backendRef's filters (rproxy's per-server middlewares:
/// header changes, the host and the full path).
fn backend_filter(ctx: &Ctx, f: &HttpRouteFilter, matches: &[HttpRouteMatch]) -> Result<Vec<Value>, String> {
	crate::render::needs(ctx.features.server_middlewares, "filters on backendRefs", "http_options server_middlewares")?;
	match f.kind.as_str() {
		"RequestHeaderModifier" | "ResponseHeaderModifier" => {
			let mut dummy = Output::default();
			match filter(ctx, "", f, &HttpRouteMatch::default(), "", &mut dummy)? {
				Filtered::Middlewares(m) => Ok(m),
				Filtered::Missing => Err("unreachable".into()),
			}
		}
		"URLRewrite" => {
			let r = f.url_rewrite.as_ref().ok_or("urlRewrite is missing")?;
			let prefix = r.path.as_ref().is_some_and(|p| p.kind == "ReplacePrefixMatch");
			// a prefix replacement depends on the match; a server serves all of the rule's matches
			let distinct: std::collections::BTreeSet<String> = matches.iter().map(matched_prefix).collect();
			if prefix && distinct.len() > 1 {
				return Err("URLRewrite ReplacePrefixMatch on a backendRef of a rule with several path prefixes is not supported".into());
			}
			rewrite(ctx, r, matches.first().unwrap_or(&HttpRouteMatch::default()))
		}
		other => Err(format!("filter {other} on a backendRef is not supported")),
	}
}

/// rproxy's `retry` middleware for a rule's `retry`.
fn retry(ctx: &Ctx, r: &crate::k8s::gateway::HttpRouteRetry) -> Result<Option<Value>, String> {
	// Gateway API counts the retries, rproxy the attempts (the first included)
	let retries = r.attempts.unwrap_or(1);
	if retries <= 0 {
		return Ok(None);
	}
	let mut v = json!({ "attempts": retries + 1 });
	if !r.codes.is_empty() {
		crate::render::needs(ctx.features.retry_status, "retry codes", "http_options retry_status")?;
		v["status"] = json!(r.codes.iter().map(|c| c.to_string()).collect::<Vec<_>>());
	}
	if let Some(b) = &r.backoff {
		if crate::render::duration_ms(b).is_none() {
			return Err(format!("retry backoff {b:?} is not a duration"));
		}
		v["initial_interval"] = json!(b);
	}
	Ok(Some(json!({ "retry": v })))
}

/// The route timeouts of a rule (`Ok(None)`: none), or where the rproxy lacks
/// them the backend request timeout as the service's response timeout.
fn timeouts(ctx: &Ctx, t: &crate::k8s::gateway::HttpRouteTimeouts) -> Result<(Option<Value>, Option<Value>), String> {
	let parse = |d: &Option<String>| -> Result<Option<u64>, String> {
		match d.as_deref() {
			None => Ok(None),
			Some(d) => match crate::render::duration_ms(d) {
				Some(0) => Ok(None),
				Some(ms) => Ok(Some(ms)),
				None => Err(format!("timeout {d:?} is not a duration")),
			},
		}
	};
	let (request, backend) = (parse(&t.request)?, parse(&t.backend_request)?);
	if ctx.features.route_timeouts {
		let mut v = serde_json::Map::new();
		if let Some(ms) = request {
			v.insert("request".into(), json!(format!("{ms}ms")));
		}
		if let Some(ms) = backend {
			v.insert("backend_request".into(), json!(format!("{ms}ms")));
		}
		return Ok(((!v.is_empty()).then_some(Value::Object(v)), None));
	}
	if request.is_some() && backend.is_none() {
		return Err("timeouts.request needs a newer rproxy (GET /capabilities: http_options route_timeouts)".into());
	}
	Ok((None, backend.map(|ms| json!({"response": format!("{ms}ms")}))))
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
	for (i, rule) in route.spec.rules.iter().enumerate() {
		let base = format!("{ns}/{name}/r{i}");
		let default_match = [HttpRouteMatch::default()];
		let matches: &[HttpRouteMatch] = if rule.matches.is_empty() { &default_match } else { &rule.matches };
		// backends: (weight, endpoints, the backendRef's middlewares); an invalid backendRef
		// gets its share answered with 500 where rproxy can (`servers[].status`)
		let mut weighted: Vec<(u32, Vec<Endpoint>)> = vec![];
		let mut server_mws: Vec<Vec<String>> = vec![];
		let mut fixed_500: Vec<bool> = vec![];
		let mut protocols: Vec<Option<String>> = vec![];
		let mut any_valid = false;
		for (bi, b) in rule.backend_refs.iter().enumerate() {
			let weight = b.backend.weight.unwrap_or(1).max(0) as u32;
			match backends::resolve(ctx.world, ctx.kind, &ns, &b.backend) {
				Ok(eps) => {
					let svc = (b.backend.namespace.clone().unwrap_or_else(|| ns.clone()), b.backend.name.clone());
					out.backends.entry(base.clone()).or_default().push(svc);
					let mut names = vec![];
					for (k, f) in b.filters.iter().enumerate() {
						match backend_filter(ctx, f, matches) {
							Ok(mws) => {
								for (x, mw) in mws.into_iter().enumerate() {
									let n = format!("{base}/b{bi}/f{k}.{x}");
									out.middlewares.insert(n.clone(), mw);
									names.push(n);
								}
							}
							Err(e) => {
								out.unsupported.get_or_insert(format!("rule {i}: {e}"));
							}
						}
					}
					protocols.push(backends::app_protocol(ctx.world, &ns, &b.backend));
					any_valid = true;
					weighted.push((weight, eps));
					server_mws.push(names);
					fixed_500.push(false);
				}
				Err(e) => {
					out.resolved.get_or_insert(e);
					if ctx.features.server_status && weight > 0 {
						weighted.push((weight, vec![Endpoint { addr: String::new(), port: 0 }]));
						server_mws.push(vec![]);
						fixed_500.push(true);
					}
				}
			}
		}
		let servers: Vec<rp::Server> = backends::spread_indexed(&weighted)
			.into_iter()
			.map(|(g, ep, weight)| {
				if fixed_500[g] {
					rp::Server { status: Some(500), weight, ..Default::default() }
				} else {
					rp::Server {
						url: format!("http://{}", ep.authority()),
						weight,
						middlewares: server_mws[g].clone(),
						..Default::default()
					}
				}
			})
			.collect();
		let mut route_timeouts = None;
		let service = if servers.is_empty() || !any_valid {
			None
		} else {
			let mut svc = rp::Service { servers, ..Default::default() };
			if let Some(t) = &rule.timeouts {
				match timeouts(ctx, t) {
					Ok((on_route, on_service)) => {
						route_timeouts = on_route;
						svc.timeouts = on_service;
					}
					Err(e) => {
						out.unsupported.get_or_insert(format!("rule {i}: {e}"));
					}
				}
			}
			// the backends' protocol: gRPC is HTTP/2 without TLS; an HTTPRoute's backends say so with appProtocol
			let h2c = |p: &Option<String>| p.as_deref() == Some("kubernetes.io/h2c");
			let wanted =
				if ctx.grpc && protocols.iter().all(|p| p.is_none() || h2c(p)) || !protocols.is_empty() && protocols.iter().all(h2c) {
					Some("h2c")
				} else {
					None
				};
			if let Some(p) = wanted {
				match crate::render::needs(ctx.features.service_protocol, "HTTP/2 to backends (h2c)", "services protocol") {
					Ok(()) => svc.protocol = Some(p),
					Err(e) => {
						out.unsupported.get_or_insert(format!("rule {i}: {e}"));
					}
				}
			}
			out.services.insert(base.clone(), svc);
			Some(base.clone())
		};
		let retry_mw = match rule.retry.as_ref().map(|r| retry(ctx, r)) {
			Some(Ok(mw)) => mw,
			Some(Err(e)) => {
				out.unsupported.get_or_insert(format!("rule {i}: {e}"));
				None
			}
			None => None,
		};
		for (j, m) in matches.iter().enumerate() {
			let mut chain = vec![];
			let mut answers = false;
			let mut broken = false;
			for (k, f) in rule.filters.iter().enumerate() {
				match filter(ctx, &ns, f, m, &format!("{base}/f{k}"), &mut out) {
					Ok(Filtered::Middlewares(mws)) => {
						for (x, mw) in mws.into_iter().enumerate() {
							answers |= mw.get("redirect_regex").is_some() || mw.get("respond").is_some();
							let n = format!("{base}/m{j}/f{k}.{x}");
							out.middlewares.insert(n.clone(), mw);
							chain.push(n);
						}
					}
					Ok(Filtered::Missing) => {
						broken = true;
						out.resolved.get_or_insert(Cond::new(
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
			if service.is_some() && !answers {
				if let Some(mw) = &retry_mw {
					let n = format!("{base}/retry");
					out.middlewares.insert(n.clone(), mw.clone());
					chain.push(n);
				}
			}
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
							timeouts: if service.is_some() { route_timeouts.clone() } else { None },
						},
					}),
					Err(e) => {
						out.unsupported.get_or_insert(format!("rule {i}: {e}"));
					}
				}
			}
		}
	}
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
		let features = crate::render::Features::default();
		let ctx = Ctx { world: &world, scheme: "http", port: 80, features: &features, kind: "HTTPRoute", grpc: false };
		let r = RequestRedirect { scheme: Some("https".into()), status_code: Some(301), ..Default::default() };
		let v = redirect(&ctx, &r, &m(None)).unwrap();
		assert_eq!(v[0]["redirect_regex"]["replacement"], "https://${1}${2}${3}");
		assert_eq!(v[0]["redirect_regex"]["permanent"], true);
		let r = RequestRedirect {
			hostname: Some("example.org".into()),
			path: Some(PathModifier { kind: "ReplacePrefixMatch".into(), replace_prefix_match: Some("/".into()), replace_full_path: None }),
			..Default::default()
		};
		let ctx8080 = Ctx { world: &world, scheme: "http", port: 8080, features: &features, kind: "HTTPRoute", grpc: false };
		let v = redirect(&ctx8080, &r, &m(Some(("PathPrefix", "/old/")))).unwrap();
		assert_eq!(v.len(), 2);
		assert_eq!(v[0]["redirect_regex"]["regex"], "^[a-z]+://([^/?]*?)(?::[0-9]+)?/old(/[^?]*)(\\?.*)?$");
		assert_eq!(v[0]["redirect_regex"]["replacement"], "http://example.org:8080${2}${3}");
		assert_eq!(v[1]["redirect_regex"]["replacement"], "http://example.org:8080/${2}");
		let v = redirect(&ctx, &RequestRedirect { status_code: Some(307), ..Default::default() }, &m(None)).unwrap();
		assert_eq!(v[0]["redirect_regex"]["status"], 307);
		assert_eq!(v[0]["redirect_regex"]["permanent"], false);
		// an older rproxy: 301 and 302 only
		let old = crate::render::Features { redirect_status: false, ..Default::default() };
		let ctx_old = Ctx { world: &world, scheme: "http", port: 80, features: &old, kind: "HTTPRoute", grpc: false };
		let e = redirect(&ctx_old, &RequestRedirect { status_code: Some(303), ..Default::default() }, &m(None)).unwrap_err();
		assert!(e.contains("redirect_status"), "{e}");
		let v = redirect(&ctx_old, &RequestRedirect { status_code: Some(301), ..Default::default() }, &m(None)).unwrap();
		assert!(v[0]["redirect_regex"].get("status").is_none());
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
		let world = World::default();
		let features = crate::render::Features::default();
		let ctx = Ctx { world: &world, scheme: "http", port: 80, features: &features, kind: "HTTPRoute", grpc: false };
		let v = rewrite(&ctx, &f, &m(Some(("PathPrefix", "/old")))).unwrap();
		assert_eq!(v, vec![json!({"replace_path_regex": {"regex": "^/old(/.*)?$", "replacement": "/new${1}"}})]);
		let host = crate::k8s::gateway::UrlRewrite { hostname: Some("one.example.org".into()), path: None };
		assert_eq!(rewrite(&ctx, &host, &m(None)).unwrap(), vec![json!({"replace_host": {"host": "one.example.org"}})]);
		let old = crate::render::Features { replace_host: false, ..Default::default() };
		let ctx_old = Ctx { world: &world, scheme: "http", port: 80, features: &old, kind: "HTTPRoute", grpc: false };
		assert!(rewrite(&ctx_old, &host, &m(None)).is_err());
		assert_eq!(regex_escape("/a.b$"), "/a\\.b\\$");
	}
}
