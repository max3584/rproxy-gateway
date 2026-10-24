//! rproxy's own checks of what one route adds to a port, made while rendering (#92).
//!
//! All HTTPRoutes on a port go into one rproxy rule. rproxy refuses a rule with a bad
//! `match` expression, regular expression or header, and the rule carries every
//! tenant's routes: a route that rproxy would refuse is left out here (with
//! `Accepted: False`) so the others on the port keep running. The checks follow
//! rproxy-api's (`src/l7/matcher.rs`, `src/l7/mod.rs` `MiddlewareSpec::validate`,
//! `src/l7/middleware/mod.rs` `Middleware::compile`, `src/l7/middleware/cors.rs`) with
//! the same crates (`regex` with its default size limits, `http`'s header names and
//! values). What cannot be checked here is caught when rproxy refuses the rule: the
//! controller then leaves out the route rproxy names (`controller::isolate`).

use http::{HeaderName, HeaderValue};
use serde_json::Value;

/// rproxy's limit on nested parentheses and `!` (`matcher::MAX_DEPTH`).
pub const MAX_DEPTH: usize = 32;
/// rproxy's limit on matchers in one expression (`matcher::MAX_TERMS`).
pub const MAX_TERMS: usize = 256;

#[derive(Debug, PartialEq)]
enum Token {
	Ident(String),
	Str(String),
	LParen,
	RParen,
	Comma,
	And,
	Or,
	Not,
}

fn tokenize(src: &str) -> Result<Vec<Token>, String> {
	let mut out = vec![];
	let mut chars = src.char_indices().peekable();
	while let Some((i, c)) = chars.next() {
		match c {
			c if c.is_whitespace() => {}
			'(' => out.push(Token::LParen),
			')' => out.push(Token::RParen),
			',' => out.push(Token::Comma),
			'!' => out.push(Token::Not),
			'&' | '|' => match chars.next() {
				Some((_, n)) if n == c => out.push(if c == '&' { Token::And } else { Token::Or }),
				_ => return Err(format!("expected {c}{c} at {i}")),
			},
			'`' | '"' => {
				let mut s = String::new();
				loop {
					match chars.next() {
						Some((_, q)) if q == c => break,
						Some((_, ch)) => s.push(ch),
						None => return Err(format!("unterminated string starting at {i}")),
					}
				}
				out.push(Token::Str(s));
			}
			c if c.is_ascii_alphabetic() => {
				let mut s = c.to_string();
				while let Some(&(_, n)) = chars.peek() {
					if n.is_ascii_alphanumeric() {
						s.push(n);
						chars.next();
					} else {
						break;
					}
				}
				out.push(Token::Ident(s));
			}
			c => return Err(format!("unexpected '{c}' at {i}")),
		}
	}
	Ok(out)
}

struct Parser {
	tokens: Vec<Token>,
	pos: usize,
	depth: usize,
	terms: usize,
}

impl Parser {
	fn peek(&self) -> Option<&Token> {
		self.tokens.get(self.pos)
	}

	fn next(&mut self) -> Option<&Token> {
		self.pos += 1;
		self.tokens.get(self.pos - 1)
	}

	fn or(&mut self) -> Result<(), String> {
		self.and()?;
		while self.peek() == Some(&Token::Or) {
			self.pos += 1;
			self.and()?;
		}
		Ok(())
	}

	fn and(&mut self) -> Result<(), String> {
		self.unary()?;
		while self.peek() == Some(&Token::And) {
			self.pos += 1;
			self.unary()?;
		}
		Ok(())
	}

	fn unary(&mut self) -> Result<(), String> {
		match self.next() {
			Some(Token::Not) => self.nested(|p| p.unary()),
			Some(Token::LParen) => self.nested(|p| {
				p.or()?;
				match p.next() {
					Some(Token::RParen) => Ok(()),
					_ => Err("expected )".into()),
				}
			}),
			Some(Token::Ident(name)) => {
				let name = name.clone();
				self.terms += 1;
				if self.terms > MAX_TERMS {
					return Err(format!("more than {MAX_TERMS} matchers in one expression"));
				}
				self.call(&name)
			}
			other => Err(format!("expected a matcher, got {other:?}")),
		}
	}

	fn nested(&mut self, f: impl FnOnce(&mut Self) -> Result<(), String>) -> Result<(), String> {
		if self.depth >= MAX_DEPTH {
			return Err(format!("parentheses and ! nested deeper than {MAX_DEPTH} levels"));
		}
		self.depth += 1;
		let r = f(self);
		self.depth -= 1;
		r
	}

	fn call(&mut self, name: &str) -> Result<(), String> {
		if self.next() != Some(&Token::LParen) {
			return Err(format!("expected ( after {name}"));
		}
		let mut args = vec![];
		loop {
			match self.next() {
				Some(Token::Str(s)) => args.push(s.clone()),
				Some(Token::RParen) if args.is_empty() => break,
				other => return Err(format!("{name}: expected a quoted argument, got {other:?}")),
			}
			match self.next() {
				Some(Token::Comma) => continue,
				Some(Token::RParen) => break,
				other => return Err(format!("{name}: expected , or ), got {other:?}")),
			}
		}
		matcher(name, &args)
	}
}

/// A regular expression as rproxy compiles it (`regex::Regex::new`, the crate's default size limits).
pub fn regex(what: &str, pattern: &str) -> Result<(), String> {
	regex::Regex::new(pattern).map(|_| ()).map_err(|e| format!("{what}: {e}"))
}

fn matcher(name: &str, args: &[String]) -> Result<(), String> {
	let count = |min: usize, max: usize| {
		if (min..=max).contains(&args.len()) {
			Ok(())
		} else {
			Err(format!(
				"{name} takes {} argument(s), got {}",
				if min == max { min.to_string() } else { format!("{min} or more") },
				args.len()
			))
		}
	};
	match name {
		"Host" | "Method" | "ClientIP" => count(1, usize::MAX)?,
		"HostRegexp" | "PathRegexp" => {
			count(1, 1)?;
			regex(name, &args[0])?;
		}
		"Path" | "PathPrefix" => {
			count(1, usize::MAX)?;
			if let Some(p) = args.iter().find(|p| !p.starts_with('/')) {
				return Err(format!("{name}: {p:?} must start with /"));
			}
		}
		"Header" => count(2, 2)?,
		"HeaderRegexp" | "QueryRegexp" => {
			count(2, 2)?;
			regex(name, &args[1])?;
		}
		"Query" => count(1, 2)?,
		other => return Err(format!("unknown matcher {other}")),
	}
	if name == "ClientIP" {
		for c in args {
			cidr(c).map_err(|e| format!("ClientIP: {e}"))?;
		}
	}
	Ok(())
}

/// An address or CIDR as rproxy reads it (`net::cidr::Cidr`).
fn cidr(s: &str) -> Result<(), String> {
	let bad = || format!("not an address or CIDR: {s}");
	let (addr, prefix) = match s.trim().split_once('/') {
		Some((a, p)) => (a, Some(p.parse::<u8>().map_err(|_| bad())?)),
		None => (s.trim(), None),
	};
	let ip: std::net::IpAddr = addr.trim_matches(|c| c == '[' || c == ']').parse().map_err(|_| bad())?;
	let max = if ip.is_ipv4() || ip.to_canonical().is_ipv4() { 32 } else { 128 };
	if prefix.unwrap_or(max) > max {
		return Err(format!("prefix /{} is too long for {addr}", prefix.unwrap_or(max)));
	}
	Ok(())
}

/// A route's `match` expression, checked as rproxy parses it (`Matcher::parse`).
pub fn expression(src: &str) -> Result<(), String> {
	let mut p = Parser { tokens: tokenize(src)?, pos: 0, depth: 0, terms: 0 };
	if p.tokens.is_empty() {
		return Err("empty match".into());
	}
	p.or()?;
	if p.pos != p.tokens.len() {
		return Err(format!("unexpected {:?} after the expression", p.tokens[p.pos]));
	}
	Ok(())
}

/// A header name as rproxy takes it (`http::HeaderName::from_bytes`).
pub fn header_name(n: &str) -> Result<(), String> {
	HeaderName::from_bytes(n.as_bytes()).map(|_| ()).map_err(|_| format!("{n:?} is not a valid header name"))
}

/// A header value as rproxy takes it (`http::HeaderValue::from_str`: no CR, LF, NUL or other control characters).
pub fn header_value(v: &str) -> Result<(), String> {
	HeaderValue::from_str(v).map(|_| ()).map_err(|_| format!("{v:?} is not a valid header value"))
}

/// A field's type in rproxy's `MiddlewareSpec` (serde).
#[derive(Clone, Copy)]
enum T {
	Str,
	Bool,
	/// An unsigned integer of at most this value (`u8`, `u16`, `u32`, `u64`).
	Uint(u64),
	Strs,
	U32s,
	StrMap,
	/// A nested struct (`deny_unknown_fields`).
	Obj(&'static [F]),
	/// A unit enum (`rename_all = "snake_case"`).
	OneOf(&'static [&'static str]),
}

/// How a field may be left out: required, `#[serde(default)]` (absent only), `Option` (absent or null).
#[derive(Clone, Copy, PartialEq)]
enum P {
	Req,
	Def,
	Opt,
}

struct F(&'static str, T, P);

const U8: T = T::Uint(u8::MAX as u64);
const U16: T = T::Uint(u16::MAX as u64);
const U32: T = T::Uint(u32::MAX as u64);
const U64: T = T::Uint(u64::MAX);

const HEADER_OPS: &[F] = &[F("set", T::StrMap, P::Def), F("remove", T::Strs, P::Def), F("add", T::StrMap, P::Def)];
const HSTS: &[F] = &[F("max_age", U64, P::Req), F("include_subdomains", T::Bool, P::Def), F("preload", T::Bool, P::Def)];
const HEADERS_CORS: &[F] = &[
	F("allow_origins", T::Strs, P::Req),
	F("allow_methods", T::Strs, P::Def),
	F("allow_headers", T::Strs, P::Def),
	F("allow_credentials", T::Bool, P::Def),
	F("max_age", U64, P::Opt),
];
const FORWARD_BODY: &[F] = &[F("max_size", U64, P::Req)];
const FRACTION: &[F] = &[F("numerator", U32, P::Req), F("denominator", U32, P::Def)];

/// rproxy-api's `MiddlewareSpec` (src/l7/mod.rs): each kind's fields.
const KINDS: &[(&str, &[F])] = &[
	("redirect_scheme", &[F("scheme", T::Str, P::Def), F("port", U16, P::Opt), F("permanent", T::Bool, P::Def), F("status", U16, P::Opt)]),
	(
		"redirect_regex",
		&[F("regex", T::Str, P::Req), F("replacement", T::Str, P::Req), F("permanent", T::Bool, P::Def), F("status", U16, P::Opt)],
	),
	("rate_limit", &[F("average", U64, P::Req), F("period", T::Str, P::Def), F("burst", U64, P::Opt), F("source", T::Str, P::Def)]),
	("in_flight", &[F("amount", U64, P::Req)]),
	("crowdsec", &[F("appsec", T::Bool, P::Def), F("on_error", T::Str, P::Def)]),
	("ip_allow", &[F("source_range", T::Strs, P::Req)]),
	(
		"headers",
		&[
			F("request", T::Obj(HEADER_OPS), P::Opt),
			F("response", T::Obj(HEADER_OPS), P::Opt),
			F("hsts", T::Obj(HSTS), P::Opt),
			F("frame_deny", T::Bool, P::Def),
			F("content_type_nosniff", T::Bool, P::Def),
			F("referrer_policy", T::Str, P::Opt),
			F("csp", T::Str, P::Opt),
			F("cors", T::Obj(HEADERS_CORS), P::Opt),
		],
	),
	(
		"forward_auth",
		&[
			F("address", T::Str, P::Def),
			F("response_headers", T::Strs, P::Def),
			F("trust_forward_header", T::Bool, P::Def),
			F("request_headers", T::Strs, P::Def),
			F("timeout", T::Str, P::Opt),
			F("service", T::Str, P::Opt),
			F("path", T::Str, P::Opt),
			F("protocol", T::OneOf(&["http", "grpc"]), P::Def),
			F("client_request", T::Bool, P::Def),
			F("allow_status", T::Strs, P::Def),
			F("forward_body", T::Obj(FORWARD_BODY), P::Opt),
		],
	),
	(
		"oidc",
		&[
			F("issuer", T::Str, P::Req),
			F("client_id", T::Str, P::Req),
			F("client_secret_file", T::Str, P::Req),
			F("scopes", T::Strs, P::Def),
			F("cookie_secret_file", T::Str, P::Req),
			F("ca_file", T::Str, P::Opt),
			F("callback_path", T::Str, P::Opt),
			F("logout_path", T::Str, P::Opt),
			F("cookie_name", T::Str, P::Opt),
			F("groups_claim", T::Str, P::Opt),
		],
	),
	(
		"basic_auth",
		&[
			F("users_file", T::Str, P::Req),
			F("realm", T::Str, P::Opt),
			F("keep_authorization", T::Bool, P::Def),
			F("user_header", T::Str, P::Opt),
		],
	),
	("strip_prefix", &[F("prefixes", T::Strs, P::Req)]),
	("add_prefix", &[F("prefix", T::Str, P::Req)]),
	("replace_path", &[F("path", T::Str, P::Req)]),
	("replace_path_regex", &[F("regex", T::Str, P::Req), F("replacement", T::Str, P::Req)]),
	("compress", &[F("encodings", T::Strs, P::Def), F("min_size", U64, P::Opt)]),
	("buffering", &[F("max_request_body", U64, P::Req)]),
	("retry", &[F("attempts", U32, P::Req), F("initial_interval", T::Str, P::Opt), F("status", T::Strs, P::Def)]),
	("circuit_breaker", &[F("failure_percent", U8, P::Req), F("window", T::Str, P::Req), F("recovery", T::Str, P::Req)]),
	("errors", &[F("status", T::Strs, P::Req), F("service", T::Str, P::Req), F("path", T::Str, P::Req)]),
	("respond", &[F("status", U16, P::Req), F("body", T::Str, P::Opt), F("content_type", T::Str, P::Opt)]),
	(
		"geoip",
		&[
			F("allow_countries", T::Strs, P::Def),
			F("deny_countries", T::Strs, P::Def),
			F("allow_asns", T::U32s, P::Def),
			F("deny_asns", T::U32s, P::Def),
			F("unknown", T::OneOf(&["allow", "deny"]), P::Def),
		],
	),
	(
		"cors",
		&[
			F("allow_origins", T::Strs, P::Req),
			F("allow_methods", T::Strs, P::Def),
			F("allow_headers", T::Strs, P::Def),
			F("expose_headers", T::Strs, P::Def),
			F("allow_credentials", T::Bool, P::Def),
			F("max_age", U64, P::Opt),
		],
	),
	("mirror", &[F("service", T::Str, P::Req), F("percent", U8, P::Opt), F("fraction", T::Obj(FRACTION), P::Opt)]),
	("replace_host", &[F("host", T::Str, P::Req)]),
];

fn type_ok(v: &Value, t: T, at: &str) -> Result<(), String> {
	let all = |f: fn(&Value) -> bool| v.as_array().is_some_and(|a| a.iter().all(f));
	let ok = match t {
		T::Str => v.is_string(),
		T::Bool => v.is_boolean(),
		T::Uint(max) => v.as_u64().is_some_and(|n| n <= max),
		T::Strs => all(Value::is_string),
		T::U32s => all(|x| x.as_u64().is_some_and(|n| n <= u64::from(u32::MAX))),
		T::StrMap => v.as_object().is_some_and(|m| m.values().all(Value::is_string)),
		T::Obj(fields) => return shape(v, fields, at),
		T::OneOf(names) => v.as_str().is_some_and(|s| names.contains(&s)),
	};
	if ok { Ok(()) } else { Err(format!("{at}: {v} is not of the type rproxy reads")) }
}

/// The fields of a struct as serde reads it with `deny_unknown_fields`.
fn shape(v: &Value, fields: &[F], at: &str) -> Result<(), String> {
	let Some(obj) = v.as_object() else {
		return Err(format!("{at}: not an object"));
	};
	if let Some(k) = obj.keys().find(|k| !fields.iter().any(|f| f.0 == k.as_str())) {
		return Err(format!("{at}: unknown field {k:?}"));
	}
	for F(name, t, p) in fields {
		match obj.get(*name) {
			None if *p == P::Req => return Err(format!("{at}: missing field {name:?}")),
			None => {}
			Some(Value::Null) if *p == P::Opt => {}
			Some(x) => type_ok(x, *t, &format!("{at}.{name}"))?,
		}
	}
	Ok(())
}

fn strs(v: &Value) -> impl Iterator<Item = &str> {
	v.as_array().into_iter().flatten().filter_map(Value::as_str)
}

fn header_ops(ops: &Value) -> Result<(), String> {
	for key in ["set", "add"] {
		if let Some(m) = ops.get(key).and_then(Value::as_object) {
			for (n, v) in m {
				header_name(n)?;
				header_value(v.as_str().unwrap_or_default())?;
			}
		}
	}
	for n in strs(&ops["remove"]) {
		header_name(n)?;
	}
	Ok(())
}

/// The `cors` middleware's settings as rproxy checks them (`CorsFilterSpec::validate`).
fn cors(c: &Value) -> Result<(), String> {
	if strs(&c["allow_origins"]).next().is_none() {
		return Err("allow_origins is empty".into());
	}
	for o in strs(&c["allow_origins"]) {
		if o != "*" && !(o.starts_with("http://") || o.starts_with("https://")) {
			return Err(format!("allow_origins: {o:?} must be * or start with http:// or https://"));
		}
		if o != "*" && o.contains('*') {
			let rest = o.split_once("://").map(|(_, r)| r).unwrap_or("");
			let ok = rest.strip_prefix("*.").is_some_and(|host| !host.is_empty() && !host.contains('*') && !host.starts_with('.'));
			if !ok {
				return Err(format!("allow_origins: {o:?}: a wildcard must be the first label (https://*.example.com)"));
			}
		}
	}
	for key in ["allow_origins", "allow_methods", "allow_headers", "expose_headers"] {
		for v in strs(&c[key]) {
			if HeaderValue::from_str(v).is_err() || v.contains(',') {
				return Err(format!("{v:?} is not a valid header value"));
			}
		}
	}
	Ok(())
}

/// rproxy's `parse_duration`: `10s`, `500ms`, `1m`, `2h`, at most 365 days.
fn duration(s: &str) -> Result<(), String> {
	let bad = || format!("{s:?} is not a duration (e.g. 10s, 500ms, 1m)");
	let split = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
	let (num, unit) = s.split_at(split);
	let n: u64 = num.parse().map_err(|_| bad())?;
	// milliseconds; `m` and `h` overflow as rproxy's seconds do
	let ms: Option<u128> = match unit {
		"ms" => Some(u128::from(n)),
		"s" => Some(u128::from(n) * 1000),
		"m" => n.checked_mul(60).map(|s| u128::from(s) * 1000),
		"h" => n.checked_mul(3600).map(|s| u128::from(s) * 1000),
		_ => return Err(bad()),
	};
	match ms {
		Some(ms) if ms <= 365 * 24 * 3600 * 1000 => Ok(()),
		_ => Err(format!("{s:?} is longer than 365 days (8760h)")),
	}
}

/// rproxy's `parse_status_range`: `404` or `500-599`.
fn status_range(s: &str) -> Result<(), String> {
	let (a, b) = s.split_once('-').unwrap_or((s, s));
	let parse = |v: &str| v.trim().parse::<u16>().ok().filter(|n| (100..=599).contains(n));
	match (parse(a), parse(b)) {
		(Some(a), Some(b)) if a <= b => Ok(()),
		_ => Err(format!("{s:?} is not a status or a range like 500-599")),
	}
}

/// rproxy's `check_url`.
fn url(u: &str) -> Result<(), String> {
	let rest = u
		.strip_prefix("http://")
		.or_else(|| u.strip_prefix("https://"))
		.ok_or_else(|| format!("{u:?} must start with http:// or https://"))?;
	if rest.is_empty() || rest.starts_with('/') {
		return Err(format!("{u:?} has no host"));
	}
	Ok(())
}

fn cookie_name(c: &str) -> bool {
	!c.is_empty() && c.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
}

/// rproxy's most entries in `ip_allow.source_range` (`net::cidr::MAX_ALLOW_FROM`).
pub const MAX_ALLOW_FROM: usize = 64;

/// One middleware as rproxy reads, checks and compiles it, where that needs nothing but the
/// middleware: its shape (exactly one known kind, each field of the type serde reads, no unknown
/// field: rproxy refuses a whole set whose body it cannot read), and the values rproxy's
/// `MiddlewareSpec::validate` and `Middleware::compile` check (regular expressions, header names and
/// values, paths, CIDRs, statuses, durations, URLs). Whether a named `service` exists and whether a
/// file can be read are left to rproxy (the rule).
pub fn middleware(v: &Value) -> Result<(), String> {
	let Some(obj) = v.as_object() else {
		return Err("a middleware is an object with one kind".into());
	};
	let mut kinds = obj.iter();
	let (Some((kind, spec)), None) = (kinds.next(), kinds.next()) else {
		return Err(format!("a middleware has exactly one kind (got {})", obj.keys().cloned().collect::<Vec<_>>().join(", ")));
	};
	let Some((_, fields)) = KINDS.iter().find(|(k, _)| k == kind) else {
		return Err(format!("unknown middleware kind {kind:?}"));
	};
	shape(spec, fields, kind)?;
	let s = |f: &str| spec[f].as_str();
	let n = |f: &str| spec[f].as_u64();
	let bad = |m: String| Err(format!("{kind}: {m}"));
	let status_ok = |s: Option<u64>| s.is_none_or(|s| [301, 302, 303, 307, 308].contains(&s));
	match kind.as_str() {
		"redirect_regex" | "replace_path_regex" => {
			if !status_ok(n("status")) {
				return bad("status must be 301, 302, 303, 307 or 308".into());
			}
			regex(kind, s("regex").unwrap_or_default())?;
		}
		"redirect_scheme" => {
			if let Some(sc) = s("scheme").filter(|sc| *sc != "http" && *sc != "https") {
				return bad(format!("scheme {sc:?} must be http or https"));
			}
			if !status_ok(n("status")) {
				return bad("status must be 301, 302, 303, 307 or 308".into());
			}
		}
		"rate_limit" => {
			if n("average") == Some(0) {
				return bad("average must be at least 1".into());
			}
			if let Some(p) = s("period") {
				duration(p).map_err(|e| format!("{kind}: period: {e}"))?;
			}
			if let Some(src) = s("source").filter(|src| *src != "ip") {
				let name = src.strip_prefix("header:").map(str::trim).filter(|n| !n.is_empty());
				match name {
					Some(h) => header_name(h).map_err(|e| format!("{kind}: source: {e}"))?,
					None => return bad(format!("source {src:?} must be ip or header:<name>")),
				}
			}
		}
		"in_flight" if n("amount") == Some(0) => return bad("amount must be at least 1".into()),
		"buffering" if n("max_request_body") == Some(0) => return bad("max_request_body must be at least 1".into()),
		"crowdsec" => {
			if let Some(o) = s("on_error").filter(|o| *o != "allow" && *o != "block") {
				return bad(format!("on_error {o:?} must be allow or block"));
			}
		}
		"headers" => {
			for side in ["request", "response"] {
				header_ops(&spec[side]).map_err(|e| format!("headers {side}: {e}"))?;
			}
			for key in ["referrer_policy", "csp"] {
				if let Some(p) = s(key) {
					header_value(p).map_err(|e| format!("headers {key}: {e}"))?;
				}
			}
			// rproxy only takes these as header values (not the `cors` middleware's rules)
			for key in ["allow_origins", "allow_methods", "allow_headers"] {
				for v in strs(&spec["cors"][key]) {
					header_value(v).map_err(|e| format!("headers cors: {e}"))?;
				}
			}
		}
		"cors" => cors(spec).map_err(|e| format!("cors: {e}"))?,
		"respond" => {
			if let Some(st) = n("status").filter(|st| !(100..=599).contains(st)) {
				return bad(format!("status {st} is not an HTTP status"));
			}
			if let Some(t) = s("content_type") {
				header_value(t).map_err(|e| format!("respond content_type: {e}"))?;
			}
		}
		"ip_allow" => {
			let list: Vec<&str> = strs(&spec["source_range"]).collect();
			if list.is_empty() {
				return bad("source_range is empty".into());
			}
			if list.len() > MAX_ALLOW_FROM {
				return bad(format!("source_range may hold at most {MAX_ALLOW_FROM} entries"));
			}
			for c in list {
				cidr(c).map_err(|e| format!("ip_allow: {e}"))?;
			}
		}
		"strip_prefix" => {
			if strs(&spec["prefixes"]).any(|p| !p.starts_with('/')) {
				return bad("prefixes must start with /".into());
			}
		}
		"add_prefix" | "replace_path" => {
			let field = if kind == "add_prefix" { "prefix" } else { "path" };
			if let Some(p) = s(field).filter(|p| !p.starts_with('/')) {
				return bad(format!("{p:?} must start with /"));
			}
		}
		"replace_host" => {
			let host = s("host").unwrap_or_default();
			let ok = !host.is_empty() && host.parse::<http::uri::Authority>().is_ok_and(|a| a.as_str() == host && !host.contains('@'));
			if !ok || header_value(host).is_err() {
				return bad(format!("host {host:?} must be host or host:port"));
			}
		}
		"retry" => {
			if n("attempts") == Some(0) {
				return bad("attempts must be at least 1".into());
			}
			for st in strs(&spec["status"]) {
				status_range(st).map_err(|e| format!("{kind}: status: {e}"))?;
			}
			if let Some(d) = s("initial_interval") {
				duration(d).map_err(|e| format!("{kind}: initial_interval: {e}"))?;
			}
		}
		"circuit_breaker" => {
			if !n("failure_percent").is_some_and(|p| (1..=100).contains(&p)) {
				return bad("failure_percent must be 1-100".into());
			}
			for f in ["window", "recovery"] {
				duration(s(f).unwrap_or_default()).map_err(|e| format!("{kind}: {e}"))?;
			}
		}
		"errors" => {
			let list: Vec<&str> = strs(&spec["status"]).collect();
			if list.is_empty() {
				return bad("status is empty".into());
			}
			for st in list {
				status_range(st).map_err(|e| format!("{kind}: {e}"))?;
			}
			if !s("path").unwrap_or_default().starts_with('/') {
				return bad(format!("path {:?} must start with /", s("path").unwrap_or_default()));
			}
		}
		"compress" => {
			if let Some(e) = strs(&spec["encodings"]).find(|e| !["br", "zstd", "gzip"].contains(&e.trim().to_ascii_lowercase().as_str())) {
				return bad(format!("encoding {e:?} must be br, zstd or gzip"));
			}
		}
		"mirror" => {
			if spec.get("percent").is_some_and(|p| !p.is_null()) && spec.get("fraction").is_some_and(|f| !f.is_null()) {
				return bad("give percent or fraction, not both".into());
			}
			if n("percent").is_some_and(|p| p > 100) {
				return bad("percent must be 0-100".into());
			}
			let f = &spec["fraction"];
			if f.is_object() {
				let (num, den) = (f["numerator"].as_u64().unwrap_or(0), f["denominator"].as_u64().unwrap_or(100));
				if den == 0 || num > den {
					return bad("fraction: denominator must be at least 1 and numerator at most denominator".into());
				}
			}
		}
		"geoip" => {
			let list = |f: &str| strs(&spec[f]).collect::<Vec<_>>();
			let asns = |f: &str| spec[f].as_array().map(|a| a.iter().filter_map(Value::as_u64).collect::<Vec<_>>()).unwrap_or_default();
			let (ac, dc, aa, da) = (list("allow_countries"), list("deny_countries"), asns("allow_asns"), asns("deny_asns"));
			if ac.is_empty() && dc.is_empty() && aa.is_empty() && da.is_empty() {
				return bad("needs a list (allow_countries, deny_countries, allow_asns or deny_asns)".into());
			}
			if let Some(c) = ac.iter().chain(&dc).find(|c| c.len() != 2 || !c.bytes().all(|b| b.is_ascii_uppercase())) {
				return bad(format!("{c:?} is not a country code (ISO 3166-1 alpha-2, e.g. JP)"));
			}
			if ac.iter().any(|c| dc.contains(c)) || aa.iter().any(|a| da.contains(a)) {
				return bad("a country or ASN is in both the allow and the deny list".into());
			}
			if aa.iter().chain(&da).any(|a| *a == 0) {
				return bad("ASNs are 1-4294967295".into());
			}
		}
		"forward_auth" => {
			let (address, service, path) = (s("address").unwrap_or_default(), s("service"), s("path"));
			match (address.is_empty(), service) {
				(false, None) => url(address).map_err(|e| format!("{kind}: {e}"))?,
				(true, Some(_)) => {}
				_ => return bad("give address or service (exactly one)".into()),
			}
			if let Some(p) = path {
				if service.is_none() || !p.starts_with('/') || p.parse::<http::uri::PathAndQuery>().is_err() {
					return bad(format!("path {p:?}: only with service, and must start with /"));
				}
			}
			if let Some(t) = s("timeout") {
				duration(t).map_err(|e| format!("{kind}: timeout: {e}"))?;
			}
			for st in strs(&spec["allow_status"]) {
				status_range(st).map_err(|e| format!("{kind}: allow_status: {e}"))?;
			}
			if spec["forward_body"]["max_size"].as_u64() == Some(0) {
				return bad("forward_body.max_size must be at least 1".into());
			}
			let response: Vec<&str> = strs(&spec["response_headers"]).collect();
			if s("protocol") == Some("grpc") {
				if spec["client_request"] == true || strs(&spec["allow_status"]).next().is_some() || path.is_some() || !response.is_empty()
				{
					return bad("protocol grpc takes no client_request, allow_status, path or response_headers".into());
				}
				if address.starts_with("https://") {
					return bad("protocol grpc with address needs http:// (h2c)".into());
				}
			}
			if response.contains(&"*") && response.len() > 1 {
				return bad("response_headers: \"*\" (all) stands alone".into());
			}
			for (key, list) in [("request_headers", strs(&spec["request_headers"]).collect::<Vec<_>>()), ("response_headers", response)] {
				for h in list.into_iter().filter(|h| *h != "*") {
					header_name(h).map_err(|e| format!("{kind} {key}: {e}"))?;
				}
			}
		}
		"oidc" => {
			url(s("issuer").unwrap_or_default()).map_err(|e| format!("{kind}: issuer: {e}"))?;
			if s("client_id").unwrap_or_default().is_empty() {
				return bad("client_id is empty".into());
			}
			let paths = [s("callback_path").unwrap_or("/_rproxy/oidc/callback"), s("logout_path").unwrap_or("/_rproxy/oidc/logout")];
			if paths.iter().any(|p| !p.starts_with('/')) || paths[0] == paths[1] {
				return bad("callback_path and logout_path must start with / and differ".into());
			}
			if s("cookie_name").is_some_and(|c| !cookie_name(c)) {
				return bad("cookie_name is not a cookie name".into());
			}
		}
		"basic_auth" => {
			if let Some(h) = s("user_header") {
				header_name(h).map_err(|e| format!("{kind}: user_header: {e}"))?;
			}
			if let Some(r) = s("realm") {
				if r.contains('"') || header_value(r).is_err() {
					return bad(format!("realm {r:?}"));
				}
			}
		}
		_ => {}
	}
	Ok(())
}

/// Whether the rproxy pods take what an RproxyMiddleware uses, by `GET /capabilities` (an
/// older rproxy reads a field it does not know as an unknown field and refuses the whole set,
/// with no `rules[i]`): the kinds and fields added after rproxy v0.4.0 (`HttpSpec::options_used`,
/// `forward_auth_used`).
pub fn supported(v: &Value, f: &crate::render::Features) -> Result<(), String> {
	let Some((kind, spec)) = v.as_object().and_then(|o| o.iter().next()) else { return Ok(()) };
	let has = |k: &str| spec.get(k).is_some_and(|x| !x.is_null());
	let nonempty = |k: &str| spec.get(k).and_then(Value::as_array).is_some_and(|a| !a.is_empty());
	let mut wanted: Vec<(bool, &str, &str)> = vec![];
	match kind.as_str() {
		"cors" => wanted.push((f.cors, "the cors middleware", "middlewares cors")),
		"mirror" => wanted.push((f.mirror, "the mirror middleware", "middlewares mirror")),
		"replace_host" => wanted.push((f.replace_host, "the replace_host middleware", "middlewares replace_host")),
		"headers" => {
			let add = ["request", "response"].iter().any(|s| spec[*s]["add"].as_object().is_some_and(|m| !m.is_empty()));
			wanted.push((!add || f.headers_add, "headers add", "http_options headers_add"));
		}
		"redirect_scheme" | "redirect_regex" => {
			wanted.push((!has("status") || f.redirect_status, "a redirect's status", "http_options redirect_status"))
		}
		"retry" => wanted.push((!nonempty("status") || f.retry_status, "retry status", "http_options retry_status")),
		"forward_auth" => {
			let all = strs(&spec["response_headers"]).any(|h| h == "*");
			wanted.extend([
				(!(has("service") || has("path")) || f.fa_service, "forward_auth service", "forward_auth service"),
				(spec["protocol"] != "grpc" || f.fa_grpc, "forward_auth protocol grpc", "forward_auth grpc"),
				(spec["client_request"] != true || f.fa_client_request, "forward_auth client_request", "forward_auth client_request"),
				(!nonempty("allow_status") || f.fa_allow_status, "forward_auth allow_status", "forward_auth allow_status"),
				(!has("forward_body") || f.ext_auth_body, "forward_auth forward_body", "forward_auth forward_body"),
				(!all || f.fa_all_response_headers, "forward_auth response_headers [\"*\"]", "forward_auth all_response_headers"),
			]);
		}
		_ => {}
	}
	for (ok, what, rproxy) in wanted {
		crate::render::needs(ok, what, rproxy)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn expressions() {
		assert!(expression("Host(`a.example`) && (PathPrefix(`/api/`) || Method(`POST`)) && !ClientIP(`10.0.0.0/8`)").is_ok());
		assert!(expression("PathRegexp(`^(?:/a)$`)").is_ok());
		let e = expression("PathRegexp(`^(?:/a($`)").unwrap_err();
		assert!(e.starts_with("PathRegexp: regex parse error"), "{e}");
		assert!(expression("HeaderRegexp(`x`, `(`)").is_err());
		assert!(expression("QueryRegexp(`q`, `[`)").is_err());
		assert!(expression("Path(`a`)").is_err());
		assert!(expression("ClientIP(`10.0.0.0/33`)").is_err());
		assert!(expression("Nope(`a`)").is_err());
		assert!(expression("Host(`a`) &&").is_err());
		assert!(expression("").is_err());
		// rproxy's limits
		let chain = vec!["Host(`b.example`)"; MAX_TERMS].join(" && ");
		assert!(expression(&chain).is_ok());
		let chain = vec!["Host(`b.example`)"; MAX_TERMS + 1].join(" && ");
		assert!(expression(&chain).unwrap_err().contains("more than 256"));
		assert!(expression(&("!".repeat(MAX_DEPTH) + "Host(`a`)")).is_ok());
		assert!(expression(&("!".repeat(MAX_DEPTH + 1) + "Host(`a`)")).is_err());
		// a regular expression larger than the regex crate's default size limit
		let big = format!("PathRegexp(`{}`)", "\\w{100}".repeat(200));
		assert!(expression(&big).unwrap_err().contains("size limit"));
	}

	#[test]
	fn headers() {
		assert!(header_name("X-Ok").is_ok());
		assert!(header_name("bad name").is_err());
		assert!(header_name("").is_err());
		assert!(header_value("a\tb").is_ok());
		for bad in ["a\r\nX-Injected: 1", "a\nb", "a\0b"] {
			assert!(header_value(bad).is_err(), "{bad:?}");
		}
	}

	#[test]
	fn middlewares() {
		assert!(middleware(&json!({"headers": {"request": {"set": {"X-A": "1"}, "remove": ["X-B"]}}})).is_ok());
		assert!(middleware(&json!({"headers": {"request": {"set": {"X-A": "1\r\nX-Evil: 1"}}}})).is_err());
		assert!(middleware(&json!({"headers": {"response": {"add": {"bad name": "1"}}}})).is_err());
		assert!(middleware(&json!({"headers": {"request": {"remove": ["bad name"]}}})).is_err());
		assert!(middleware(&json!({"replace_path_regex": {"regex": "^/a(/.*)$", "replacement": "${1}"}})).is_ok());
		assert!(middleware(&json!({"replace_path_regex": {"regex": "(", "replacement": "x"}})).is_err());
		assert!(middleware(&json!({"redirect_regex": {"regex": "^/(.*)", "replacement": "/x", "status": 305}})).is_err());
		assert!(middleware(&json!({"cors": {"allow_origins": ["https://*bar.com"]}})).is_err());
		assert!(middleware(&json!({"cors": {"allow_origins": ["https://*.bar.com"], "allow_methods": ["GET"]}})).is_ok());
		assert!(middleware(&json!({"ip_allow": {"source_range": ["10.0.0.0/8", "::1"]}})).is_ok());
		assert!(middleware(&json!({"ip_allow": {"source_range": ["nope"]}})).is_err());
		assert!(middleware(&json!({"replace_host": {"host": "a.example:8080"}})).is_ok());
		assert!(middleware(&json!({"replace_host": {"host": "a b"}})).is_err());
		assert!(middleware(&json!({"add_prefix": {"prefix": "x"}})).is_err());
		// the shape of an RproxyMiddleware
		assert!(middleware(&json!({})).is_err());
		assert!(middleware(&json!({"headers": {}, "retry": {"attempts": 2}})).is_err());
		assert!(middleware(&json!({"no_such_kind": {}})).is_err());
		assert!(middleware(&json!("headers")).is_err());
	}

	/// What rproxy's serde would refuse (`deny_unknown_fields`, types): rproxy then refuses the whole
	/// body, with no `rules[i]` to leave out.
	#[test]
	fn middleware_shapes() {
		for bad in [
			json!({"retry": {"attempts": 2, "bogus": true}}),
			json!({"retry": {"attempts": "2"}}),
			json!({"retry": {}}),
			json!({"retry": {"attempts": -1}}),
			json!({"circuit_breaker": {"failure_percent": 300, "window": "10s", "recovery": "10s"}}),
			json!({"headers": {"request": {"set": {"x": 1}}}}),
			json!({"headers": {"request": {"sett": {}}}}),
			json!({"headers": {"frame_deny": null}}),
			json!({"headers": {"hsts": {}}}),
			json!({"forward_auth": {"service": "s", "protocol": "smtp"}}),
			json!({"geoip": {"allow_asns": [5000000000u64]}}),
			json!({"cors": {"allow_origins": ["*"], "max_age": 1.5}}),
			json!({"mirror": {"service": "s", "fraction": {"numerator": 1, "extra": 2}}}),
		] {
			assert!(middleware(&bad).is_err(), "{bad}");
		}
		for good in [
			json!({"retry": {"attempts": 2, "initial_interval": null}}),
			json!({"headers": {"frame_deny": true, "hsts": {"max_age": 60}}}),
			json!({"forward_auth": {"service": "s", "protocol": "grpc"}}),
			json!({"geoip": {"deny_countries": ["JP"], "unknown": "deny"}}),
			json!({"compress": {}}),
		] {
			assert!(middleware(&good).is_ok(), "{good}: {:?}", middleware(&good));
		}
	}

	/// The values rproxy's `MiddlewareSpec::validate` and `Middleware::compile` check.
	#[test]
	fn middleware_values() {
		let many: Vec<String> = (0..=MAX_ALLOW_FROM).map(|i| format!("10.0.0.{i}")).collect();
		for bad in [
			json!({"ip_allow": {"source_range": many}}),
			json!({"cors": {"allow_origins": []}}),
			json!({"rate_limit": {"average": 0}}),
			json!({"rate_limit": {"average": 1, "source": "cookie"}}),
			json!({"rate_limit": {"average": 1, "period": "1d"}}),
			json!({"in_flight": {"amount": 0}}),
			json!({"buffering": {"max_request_body": 0}}),
			json!({"crowdsec": {"on_error": "maybe"}}),
			json!({"retry": {"attempts": 0}}),
			json!({"retry": {"attempts": 1, "status": ["600"]}}),
			json!({"circuit_breaker": {"failure_percent": 0, "window": "10s", "recovery": "10s"}}),
			json!({"errors": {"status": ["500-599"], "service": "s", "path": "x"}}),
			json!({"compress": {"encodings": ["deflate"]}}),
			json!({"mirror": {"service": "s", "percent": 50, "fraction": {"numerator": 1}}}),
			json!({"geoip": {}}),
			json!({"geoip": {"allow_countries": ["jp"]}}),
			json!({"forward_auth": {"address": "auth:80"}}),
			json!({"forward_auth": {"address": "http://a", "service": "s"}}),
			json!({"forward_auth": {"service": "s", "timeout": "soon"}}),
			json!({"forward_auth": {"service": "s", "response_headers": ["*", "x"]}}),
			json!({"oidc": {"issuer": "http://idp", "client_id": "", "client_secret_file": "a", "cookie_secret_file": "b"}}),
			json!({"basic_auth": {"users_file": "u", "realm": "a\"b"}}),
			json!({"respond": {"status": 99}}),
		] {
			assert!(middleware(&bad).is_err(), "{bad}");
		}
		assert!(middleware(&json!({"rate_limit": {"average": 5, "period": "1m", "source": "header:X-Key"}})).is_ok());
		assert!(middleware(&json!({"retry": {"attempts": 3, "status": ["500", "502-504"], "initial_interval": "100ms"}})).is_ok());
		assert!(duration("8760h").is_ok() && duration("8761h").is_err() && duration("1x").is_err());
	}

	/// `headers.cors` is not the `cors` middleware: rproxy only takes its values as header values.
	#[test]
	fn headers_cors_is_looser() {
		assert!(middleware(&json!({"headers": {"cors": {"allow_origins": ["null", "example.com"]}}})).is_ok());
		assert!(middleware(&json!({"headers": {"cors": {"allow_origins": ["a\nb"]}}})).is_err());
		assert!(middleware(&json!({"headers": {"cors": {"allow_origins": ["*"], "expose_headers": []}}})).is_err());
	}
}
