//! Traefik middlewares → rproxy middlewares: the mapping of rproxy-api's
//! `contrib/traefik2rproxy.py` (`convert_middleware`, docs/MIGRATING-FROM-TRAEFIK.md),
//! for the `Middleware` CRD. What has no rproxy equivalent is left out with a note.

use serde_json::{Map, Value, json};

/// Case-insensitive lookup through nested maps.
pub fn g<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
	let mut cur = v;
	for key in keys {
		let obj = cur.as_object()?;
		cur = match obj.get(*key) {
			Some(x) => x,
			None => obj.iter().find(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, x)| x)?,
		};
	}
	if cur.is_null() { None } else { Some(cur) }
}

pub fn as_list(v: Option<&Value>) -> Vec<String> {
	match v {
		None => vec![],
		Some(Value::String(s)) => s.split(',').map(str::trim).filter(|s| !s.is_empty()).map(str::to_string).collect(),
		Some(Value::Array(a)) => a.iter().map(|x| x.as_str().map(str::to_string).unwrap_or_else(|| x.to_string())).collect(),
		Some(x) => vec![x.to_string()],
	}
}

pub fn as_bool(v: Option<&Value>, default: bool) -> bool {
	match v {
		None => default,
		Some(Value::Bool(b)) => *b,
		Some(Value::String(s)) => matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "yes"),
		Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
		Some(_) => default,
	}
}

pub fn as_int(v: Option<&Value>) -> Option<i64> {
	match v? {
		Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
		Value::String(s) => s.trim().parse::<f64>().ok().map(|f| f as i64),
		_ => None,
	}
}

/// A Go duration (or seconds as a number) as rproxy writes it: `500ms`, `10s`, `5m`, `1h`.
pub fn duration(v: Option<&Value>) -> Option<String> {
	let ms: f64 = match v? {
		Value::Number(n) => n.as_f64()? * 1000.0,
		Value::String(s) => {
			let s = s.trim();
			if let Ok(secs) = s.parse::<f64>() {
				secs * 1000.0
			} else {
				let mut total = 0.0;
				let mut rest = s;
				while !rest.is_empty() {
					let n = rest.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
					let num: f64 = rest[..n].parse().ok()?;
					rest = &rest[n..];
					let (unit, len) =
						[("ns", 1e-6), ("us", 1e-3), ("µs", 1e-3), ("ms", 1.0), ("s", 1000.0), ("m", 60_000.0), ("h", 3_600_000.0)]
							.iter()
							.find(|(u, _)| rest.starts_with(u))
							.map(|(u, m)| (*m, u.len()))?;
					total += num * unit;
					rest = &rest[len..];
				}
				total
			}
		}
		_ => return None,
	};
	let ms = ms.round() as u64;
	for (size, unit) in [(3_600_000, "h"), (60_000, "m"), (1000, "s")] {
		if ms > 0 && ms.is_multiple_of(size) {
			return Some(format!("{}{unit}", ms / size));
		}
	}
	Some(format!("{}ms", ms.max(1)))
}

/// What converting needs from outside: notes, and files (htpasswd) to write next to rproxy.
pub trait Sink {
	fn note(&mut self, text: String);
	/// Users of a basicAuth Secret written as a file; returns its path.
	fn users_file(&mut self, namespace: &str, secret: &str) -> Option<String>;
	/// A Service used by `errors`; returns the rproxy service name.
	fn errors_service(&mut self, namespace: &str, service: &Value) -> Option<String>;
}

fn headers(cfg: &Value, sink: &mut dyn Sink, name: &str) -> Value {
	let mut out = Map::new();
	let mut req = (Map::new(), vec![]);
	let mut resp = (Map::new(), vec![]);
	for (key, ops) in [("customRequestHeaders", &mut req), ("customResponseHeaders", &mut resp)] {
		if let Some(Value::Object(m)) = g(cfg, &[key]) {
			for (k, v) in m {
				match v {
					Value::Null => ops.1.push(k.clone()),
					Value::String(s) if s.is_empty() => ops.1.push(k.clone()),
					Value::String(s) => {
						ops.0.insert(k.clone(), Value::String(s.clone()));
					}
					other => {
						ops.0.insert(k.clone(), Value::String(other.to_string()));
					}
				}
			}
		}
	}
	if let Some(v) = g(cfg, &["customFrameOptionsValue"]).and_then(Value::as_str) {
		resp.0.insert("X-Frame-Options".into(), json!(v));
	}
	if as_bool(g(cfg, &["browserXssFilter"]), false) {
		resp.0.insert("X-XSS-Protection".into(), json!("1; mode=block"));
	}
	if let Some(v) = g(cfg, &["permissionsPolicy"]).and_then(Value::as_str) {
		resp.0.insert("Permissions-Policy".into(), json!(v));
	}
	for (side, (set, remove)) in [("request", req), ("response", resp)] {
		let mut entry = Map::new();
		if !set.is_empty() {
			entry.insert("set".into(), Value::Object(set));
		}
		if !remove.is_empty() {
			entry.insert("remove".into(), json!(remove));
		}
		if !entry.is_empty() {
			out.insert(side.into(), Value::Object(entry));
		}
	}
	let sts = as_int(g(cfg, &["stsSeconds"])).unwrap_or(0);
	if sts > 0 {
		out.insert(
			"hsts".into(),
			json!({"max_age": sts, "include_subdomains": as_bool(g(cfg, &["stsIncludeSubdomains"]), false), "preload": as_bool(g(cfg, &["stsPreload"]), false)}),
		);
		if as_bool(g(cfg, &["forceSTSHeader"]), false) {
			sink.note(format!("middleware {name}: forceSTSHeader: rproxy sends HSTS only on HTTPS"));
		}
	}
	if as_bool(g(cfg, &["frameDeny"]), false) {
		out.insert("frame_deny".into(), json!(true));
	}
	if as_bool(g(cfg, &["contentTypeNosniff"]), false) {
		out.insert("content_type_nosniff".into(), json!(true));
	}
	if let Some(v) = g(cfg, &["referrerPolicy"]).and_then(Value::as_str) {
		out.insert("referrer_policy".into(), json!(v));
	}
	if let Some(v) = g(cfg, &["contentSecurityPolicy"]).and_then(Value::as_str) {
		out.insert("csp".into(), json!(v));
	}
	let origins = as_list(g(cfg, &["accessControlAllowOriginList"]));
	if !origins.is_empty() {
		let mut cors = Map::new();
		cors.insert("allow_origins".into(), json!(origins));
		let methods = as_list(g(cfg, &["accessControlAllowMethods"]));
		if !methods.is_empty() {
			cors.insert("allow_methods".into(), json!(methods));
		}
		let hdrs = as_list(g(cfg, &["accessControlAllowHeaders"]));
		if !hdrs.is_empty() {
			cors.insert("allow_headers".into(), json!(hdrs));
		}
		if as_bool(g(cfg, &["accessControlAllowCredentials"]), false) {
			cors.insert("allow_credentials".into(), json!(true));
		}
		if let Some(age) = as_int(g(cfg, &["accessControlMaxAge"])) {
			cors.insert("max_age".into(), json!(age));
		}
		out.insert("cors".into(), Value::Object(cors));
	}
	const KNOWN: &[&str] = &[
		"customrequestheaders",
		"customresponseheaders",
		"customframeoptionsvalue",
		"browserxssfilter",
		"permissionspolicy",
		"stsseconds",
		"stsincludesubdomains",
		"stspreload",
		"forcestsheader",
		"framedeny",
		"contenttypenosniff",
		"referrerpolicy",
		"contentsecuritypolicy",
		"accesscontrolalloworiginlist",
		"accesscontrolallowmethods",
		"accesscontrolallowheaders",
		"accesscontrolallowcredentials",
		"accesscontrolmaxage",
		"addvaryheader",
	];
	for k in cfg.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
		if !KNOWN.contains(&k.to_ascii_lowercase().as_str()) {
			sink.note(format!("middleware {name}: headers.{k} is not converted"));
		}
	}
	json!({"headers": out})
}

/// One converted middleware.
pub enum Converted {
	/// An rproxy middleware.
	One(Value),
	/// A chain: the Middlewares it names (namespace, name).
	Chain(Vec<(Option<String>, String)>),
	/// Nothing (left out, with a note).
	None,
}

/// Converts the Traefik Middleware `spec` (`{type: settings}`) of `namespace/name`.
pub fn convert(namespace: &str, name: &str, spec: &Map<String, Value>, sink: &mut dyn Sink) -> Converted {
	let label = format!("{namespace}/{name}");
	if spec.len() != 1 {
		sink.note(format!("middleware {label}: not one middleware type; left out"));
		return Converted::None;
	}
	let (kind, cfg) = spec.iter().next().unwrap();
	let empty = Value::Object(Map::new());
	let cfg = if cfg.is_null() { &empty } else { cfg };
	let str_of = |keys: &[&str]| g(cfg, keys).and_then(Value::as_str).map(str::to_string);
	match kind.to_ascii_lowercase().as_str() {
		"chain" => {
			let refs = g(cfg, &["middlewares"]).and_then(Value::as_array).cloned().unwrap_or_default();
			Converted::Chain(
				refs.iter()
					.filter_map(|r| {
						let n = r["name"].as_str()?;
						Some((r["namespace"].as_str().map(str::to_string), n.split('@').next().unwrap_or(n).to_string()))
					})
					.collect(),
			)
		}
		"redirectscheme" => {
			let scheme = str_of(&["scheme"]).unwrap_or_else(|| "https".into());
			let mut out = json!({"scheme": scheme, "permanent": as_bool(g(cfg, &["permanent"]), false)});
			if let Some(port) = as_int(g(cfg, &["port"])) {
				if !((scheme == "https" && port == 443) || (scheme == "http" && port == 80)) {
					out["port"] = json!(port);
				}
			}
			Converted::One(json!({"redirect_scheme": out}))
		}
		"redirectregex" => Converted::One(json!({"redirect_regex": {
			"regex": str_of(&["regex"]).unwrap_or_default(),
			"replacement": str_of(&["replacement"]).unwrap_or_default(),
			"permanent": as_bool(g(cfg, &["permanent"]), false),
		}})),
		"stripprefix" => {
			if as_bool(g(cfg, &["forceSlash"]), false) {
				sink.note(format!("middleware {label}: stripPrefix.forceSlash is not converted"));
			}
			Converted::One(json!({"strip_prefix": {"prefixes": as_list(g(cfg, &["prefixes"]))}}))
		}
		"addprefix" => Converted::One(json!({"add_prefix": {"prefix": str_of(&["prefix"]).unwrap_or_default()}})),
		"replacepath" => Converted::One(json!({"replace_path": {"path": str_of(&["path"]).unwrap_or_default()}})),
		"replacepathregex" => Converted::One(json!({"replace_path_regex": {
			"regex": str_of(&["regex"]).unwrap_or_default(),
			"replacement": str_of(&["replacement"]).unwrap_or_default(),
		}})),
		"headers" => Converted::One(headers(cfg, sink, &label)),
		"ratelimit" => {
			let average = as_int(g(cfg, &["average"])).unwrap_or(0);
			if average <= 0 {
				sink.note(format!("middleware {label}: rateLimit.average 0 means no limit; left out"));
				return Converted::None;
			}
			let mut out = json!({"average": average, "period": duration(g(cfg, &["period"])).unwrap_or_else(|| "1s".into())});
			if let Some(b) = as_int(g(cfg, &["burst"])).filter(|b| *b > 0) {
				out["burst"] = json!(b);
			}
			if let Some(h) = g(cfg, &["sourceCriterion", "requestHeaderName"]).and_then(Value::as_str) {
				out["source"] = json!(format!("header:{h}"));
			} else if as_bool(g(cfg, &["sourceCriterion", "requestHost"]), false) {
				sink.note(format!("middleware {label}: rateLimit by request host is not converted; limited per client IP"));
			}
			if g(cfg, &["sourceCriterion", "ipStrategy"]).is_some() {
				sink.note(format!("middleware {label}: ipStrategy is not converted; rproxy uses global.trusted_proxies for the client IP"));
			}
			Converted::One(json!({"rate_limit": out}))
		}
		"inflightreq" => {
			if g(cfg, &["sourceCriterion"]).is_some() {
				sink.note(format!("middleware {label}: inFlightReq.sourceCriterion is not converted; counted per client IP"));
			}
			Converted::One(json!({"in_flight": {"amount": as_int(g(cfg, &["amount"])).unwrap_or(1)}}))
		}
		"ipallowlist" | "ipwhitelist" => {
			if g(cfg, &["ipStrategy"]).is_some() {
				sink.note(format!("middleware {label}: ipStrategy is not converted; rproxy uses global.trusted_proxies for the client IP"));
			}
			if g(cfg, &["rejectStatusCode"]).is_some() {
				sink.note(format!("middleware {label}: rejectStatusCode is not converted (rproxy answers 403)"));
			}
			Converted::One(json!({"ip_allow": {"source_range": as_list(g(cfg, &["sourceRange"]))}}))
		}
		"basicauth" => {
			let Some(secret) = str_of(&["secret"]) else {
				sink.note(format!("middleware {label}: basicAuth without a secret is not converted"));
				return Converted::None;
			};
			let Some(file) = sink.users_file(namespace, &secret) else {
				sink.note(format!(
					"middleware {label}: Secret {namespace}/{secret} has no users (htpasswd lines in the key users); left out"
				));
				return Converted::None;
			};
			let mut out = json!({"users_file": file});
			if let Some(r) = str_of(&["realm"]) {
				out["realm"] = json!(r);
			}
			// Traefik passes Authorization on unless removeHeader; rproxy removes it unless keep_authorization
			if !as_bool(g(cfg, &["removeHeader"]), false) {
				out["keep_authorization"] = json!(true);
			}
			if let Some(h) = str_of(&["headerField"]) {
				out["user_header"] = json!(h);
			}
			Converted::One(json!({"basic_auth": out}))
		}
		"forwardauth" => {
			let mut out = json!({"address": str_of(&["address"]).unwrap_or_default()});
			let rh = as_list(g(cfg, &["authResponseHeaders"]));
			if !rh.is_empty() {
				out["response_headers"] = json!(rh);
			}
			if as_bool(g(cfg, &["trustForwardHeader"]), false) {
				out["trust_forward_header"] = json!(true);
			}
			let qh = as_list(g(cfg, &["authRequestHeaders"]));
			if !qh.is_empty() {
				out["request_headers"] = json!(qh);
			}
			for opt in ["tls", "authResponseHeadersRegex", "addAuthCookiesToResponse"] {
				if g(cfg, &[opt]).is_some() {
					sink.note(format!("middleware {label}: forwardAuth.{opt} is not converted"));
				}
			}
			Converted::One(json!({"forward_auth": out}))
		}
		"compress" => {
			let mut out = Map::new();
			let enc = as_list(g(cfg, &["encodings"]));
			if !enc.is_empty() {
				out.insert("encodings".into(), json!(enc));
			}
			if let Some(m) = as_int(g(cfg, &["minResponseBodyBytes"])).filter(|m| *m > 0) {
				out.insert("min_size".into(), json!(m));
			}
			for opt in ["excludedContentTypes", "includedContentTypes", "defaultEncoding"] {
				if g(cfg, &[opt]).is_some() {
					sink.note(format!("middleware {label}: compress.{opt} is not converted"));
				}
			}
			Converted::One(json!({"compress": out}))
		}
		"retry" => {
			let mut out = json!({"attempts": as_int(g(cfg, &["attempts"])).unwrap_or(1)});
			if let Some(d) = duration(g(cfg, &["initialInterval"])) {
				out["initial_interval"] = json!(d);
			}
			Converted::One(json!({"retry": out}))
		}
		"circuitbreaker" => {
			let expr = str_of(&["expression"]).unwrap_or_default();
			let Some(ratio) = breaker_ratio(&expr) else {
				sink.note(format!(
					"middleware {label}: circuitBreaker expression {expr:?} cannot be converted (only NetworkErrorRatio / ResponseCodeRatio); left out"
				));
				return Converted::None;
			};
			let percent = ((ratio * 100.0).ceil() as i64).clamp(1, 100);
			let recovery = duration(g(cfg, &["fallbackDuration"])).unwrap_or_else(|| "10s".into());
			Converted::One(json!({"circuit_breaker": {"failure_percent": percent, "window": "10s", "recovery": recovery}}))
		}
		"errors" => {
			let Some(service) = g(cfg, &["service"]).and_then(|s| sink.errors_service(namespace, s)) else {
				sink.note(format!("middleware {label}: errors.service is not a Kubernetes Service; left out"));
				return Converted::None;
			};
			let status: Vec<String> = as_list(g(cfg, &["status"]));
			Converted::One(
				json!({"errors": {"status": status, "service": service, "path": str_of(&["query"]).unwrap_or_else(|| "/".into())}}),
			)
		}
		"buffering" => {
			let Some(limit) = as_int(g(cfg, &["maxRequestBodyBytes"])).filter(|l| *l > 0) else {
				sink.note(format!("middleware {label}: buffering without maxRequestBodyBytes is not converted"));
				return Converted::None;
			};
			Converted::One(json!({"buffering": {"max_request_body": limit}}))
		}
		"plugin" => {
			for (plugin, pcfg) in cfg.as_object().into_iter().flatten() {
				let p = plugin.to_ascii_lowercase();
				if p.contains("crowdsec") || p.contains("bouncer") {
					let appsec = as_bool(g(pcfg, &["crowdsecAppsecEnabled"]), false);
					sink.note(format!(
						"middleware {label}: the CrowdSec bouncer needs global.crowdsec in rproxy's settings file (not set by the controller)"
					));
					let mut mw = json!({"appsec": appsec});
					if appsec && as_bool(g(pcfg, &["crowdsecAppsecUnreachableBlock"]), true) {
						mw["on_error"] = json!("block");
					}
					return Converted::One(json!({"crowdsec": mw}));
				}
				sink.note(format!("middleware {label}: plugin {plugin} has no rproxy equivalent; left out"));
			}
			Converted::None
		}
		_ => {
			sink.note(format!("middleware {label}: type {kind} has no rproxy equivalent; left out"));
			Converted::None
		}
	}
}

/// `NetworkErrorRatio() > 0.30` → 0.30
fn breaker_ratio(expr: &str) -> Option<f64> {
	for f in ["NetworkErrorRatio()", "ResponseCodeRatio("] {
		if let Some(i) = expr.find(f) {
			let rest = &expr[i..];
			let after = rest.find('>')?;
			let num: String =
				rest[after + 1..].trim_start_matches('=').trim().chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
			return num.parse().ok();
		}
	}
	None
}

#[cfg(test)]
mod tests {
	use super::*;

	#[derive(Default)]
	struct S(Vec<String>);
	impl Sink for S {
		fn note(&mut self, t: String) {
			self.0.push(t);
		}
		fn users_file(&mut self, ns: &str, s: &str) -> Option<String> {
			Some(format!("/f/{ns}-{s}"))
		}
		fn errors_service(&mut self, ns: &str, s: &Value) -> Option<String> {
			Some(format!("{ns}/{}", s["name"].as_str()?))
		}
	}

	fn conv(spec: Value) -> (Option<Value>, Vec<String>) {
		let mut s = S::default();
		match convert("ns", "m", spec.as_object().unwrap(), &mut s) {
			Converted::One(v) => (Some(v), s.0),
			_ => (None, s.0),
		}
	}

	#[test]
	fn middlewares() {
		assert_eq!(
			conv(json!({"redirectScheme": {"scheme": "https", "permanent": true, "port": "443"}})).0,
			Some(json!({"redirect_scheme": {"scheme": "https", "permanent": true}}))
		);
		assert_eq!(
			conv(json!({"rateLimit": {"average": 100, "burst": 50, "period": "1m"}})).0,
			Some(json!({"rate_limit": {"average": 100, "burst": 50, "period": "1m"}}))
		);
		let (h, notes) = conv(
			json!({"headers": {"customRequestHeaders": {"X-A": "1", "X-B": ""}, "stsSeconds": 31536000, "frameDeny": true, "foo": 1}}),
		);
		assert_eq!(
			h.unwrap(),
			json!({"headers": {"request": {"set": {"X-A": "1"}, "remove": ["X-B"]}, "hsts": {"max_age": 31536000, "include_subdomains": false, "preload": false}, "frame_deny": true}})
		);
		assert!(notes[0].contains("headers.foo"));
		assert_eq!(
			conv(json!({"basicAuth": {"secret": "users"}})).0,
			Some(json!({"basic_auth": {"users_file": "/f/ns-users", "keep_authorization": true}}))
		);
		assert_eq!(
			conv(json!({"circuitBreaker": {"expression": "NetworkErrorRatio() > 0.30", "fallbackDuration": "30s"}})).0,
			Some(json!({"circuit_breaker": {"failure_percent": 30, "window": "10s", "recovery": "30s"}}))
		);
		assert_eq!(
			conv(json!({"errors": {"status": ["500-599"], "service": {"name": "err", "port": 80}, "query": "/{status}.html"}})).0.unwrap()
				["errors"]["service"],
			"ns/err"
		);
		assert!(conv(json!({"digestAuth": {}})).0.is_none());
		let mut s = S::default();
		assert!(
			matches!(convert("ns", "c", json!({"chain": {"middlewares": [{"name": "a"}, {"name": "b@kubernetescrd", "namespace": "x"}]}}).as_object().unwrap(), &mut s), Converted::Chain(v) if v == vec![(None, "a".to_string()), (Some("x".to_string()), "b".to_string())])
		);
		assert_eq!(duration(Some(&json!("1m30s"))).as_deref(), Some("90s"));
		assert_eq!(duration(Some(&json!(0.5))).as_deref(), Some("500ms"));
	}
}
