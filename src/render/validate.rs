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

/// rproxy's middleware kinds (`MiddlewareSpec::kind`).
const KINDS: &[&str] = &[
	"redirect_scheme",
	"redirect_regex",
	"rate_limit",
	"in_flight",
	"crowdsec",
	"ip_allow",
	"headers",
	"forward_auth",
	"oidc",
	"basic_auth",
	"strip_prefix",
	"add_prefix",
	"replace_path",
	"replace_path_regex",
	"compress",
	"buffering",
	"retry",
	"circuit_breaker",
	"errors",
	"respond",
	"geoip",
	"cors",
	"mirror",
	"replace_host",
];

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

/// The CORS settings as rproxy checks them (`CorsFilterSpec::validate`).
fn cors(c: &Value) -> Result<(), String> {
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

/// One middleware as rproxy would check and compile it, where that needs nothing but the
/// middleware (its shape: one known kind; regular expressions, header names and values,
/// paths, CIDRs, statuses). Fields rproxy reads with serde are left to rproxy.
pub fn middleware(v: &Value) -> Result<(), String> {
	let Some(obj) = v.as_object() else {
		return Err("a middleware is an object with one kind".into());
	};
	let mut kinds = obj.iter();
	let (Some((kind, spec)), None) = (kinds.next(), kinds.next()) else {
		return Err(format!("a middleware has exactly one kind (got {})", obj.keys().cloned().collect::<Vec<_>>().join(", ")));
	};
	if !KINDS.contains(&kind.as_str()) {
		return Err(format!("unknown middleware kind {kind:?}"));
	}
	let status_ok = |s: &Value| s.as_u64().is_none_or(|s| [301, 302, 303, 307, 308].contains(&s));
	match kind.as_str() {
		"redirect_regex" | "replace_path_regex" => {
			if kind == "redirect_regex" && !status_ok(&spec["status"]) {
				return Err(format!("{kind}: status must be 301, 302, 303, 307 or 308"));
			}
			regex(kind, spec["regex"].as_str().unwrap_or_default())?;
		}
		"redirect_scheme" => {
			if let Some(s) = spec["scheme"].as_str().filter(|s| *s != "http" && *s != "https") {
				return Err(format!("redirect_scheme: scheme {s:?} must be http or https"));
			}
			if !status_ok(&spec["status"]) {
				return Err("redirect_scheme: status must be 301, 302, 303, 307 or 308".into());
			}
		}
		"headers" => {
			for side in ["request", "response"] {
				header_ops(&spec[side]).map_err(|e| format!("headers {side}: {e}"))?;
			}
			for key in ["referrer_policy", "csp"] {
				if let Some(p) = spec[key].as_str() {
					header_value(p).map_err(|e| format!("headers {key}: {e}"))?;
				}
			}
			if spec["cors"].is_object() {
				cors(&spec["cors"]).map_err(|e| format!("headers cors: {e}"))?;
			}
		}
		"cors" => cors(spec).map_err(|e| format!("cors: {e}"))?,
		"respond" => {
			if let Some(s) = spec["status"].as_u64().filter(|s| !(100..=599).contains(s)) {
				return Err(format!("respond: status {s} is not an HTTP status"));
			}
			if let Some(t) = spec["content_type"].as_str() {
				header_value(t).map_err(|e| format!("respond content_type: {e}"))?;
			}
		}
		"ip_allow" => {
			let list: Vec<&str> = strs(&spec["source_range"]).collect();
			if list.is_empty() {
				return Err("ip_allow: source_range is empty".into());
			}
			for c in list {
				cidr(c).map_err(|e| format!("ip_allow: {e}"))?;
			}
		}
		"strip_prefix" => {
			if strs(&spec["prefixes"]).any(|p| !p.starts_with('/')) {
				return Err("strip_prefix: prefixes must start with /".into());
			}
		}
		"add_prefix" | "replace_path" => {
			let field = if kind == "add_prefix" { "prefix" } else { "path" };
			if let Some(p) = spec[field].as_str().filter(|p| !p.starts_with('/')) {
				return Err(format!("{kind}: {p:?} must start with /"));
			}
		}
		"replace_host" => {
			let host = spec["host"].as_str().unwrap_or_default();
			let ok = !host.is_empty() && host.parse::<http::uri::Authority>().is_ok_and(|a| a.as_str() == host && !host.contains('@'));
			if !ok {
				return Err(format!("replace_host: host {host:?} must be host or host:port"));
			}
		}
		"forward_auth" => {
			for key in ["request_headers", "response_headers"] {
				for n in strs(&spec[key]).filter(|n| *n != "*") {
					header_name(n).map_err(|e| format!("forward_auth {key}: {e}"))?;
				}
			}
		}
		_ => {}
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
}
