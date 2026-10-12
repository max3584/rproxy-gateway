//! Traefik `Middleware` CRDs → rproxy middlewares. The mapping is rproxy-api's
//! `rproxy-traefik` crate (`convert_middleware`, the same one `rproxy-traefik-convert`
//! uses for the file provider; docs/MIGRATING-FROM-TRAEFIK.md); this module answers
//! what depends on Kubernetes: basicAuth users from a Secret, the `errors` service as a
//! Kubernetes Service, and CrowdSec's global settings (not set by the controller). What
//! has no rproxy equivalent is left out with a note.

use rproxy_traefik::value::g;
use rproxy_traefik::{self as traefik, MiddlewareEnv, NoteSink};
use serde_json::{Map, Value};

pub use rproxy_traefik::ChainRef;

/// What converting needs from outside: notes, and files (htpasswd) to write next to rproxy.
pub trait Sink {
	fn note(&mut self, text: String);
	/// Users of a basicAuth Secret written as a file; returns its path.
	fn users_file(&mut self, namespace: &str, secret: &str) -> Option<String>;
	/// A Service used by `errors`; returns the rproxy service name.
	fn errors_service(&mut self, namespace: &str, service: &Value) -> Option<String>;
}

/// One converted middleware.
#[derive(Debug, PartialEq)]
pub enum Converted {
	/// An rproxy middleware.
	One(Value),
	/// A chain: the Middlewares it names (provider stripped; the namespace if given).
	Chain(Vec<ChainRef>),
	/// Nothing (left out, with a note).
	None,
}

/// The crate's [`MiddlewareEnv`] for a Middleware in `namespace`.
struct Env<'a> {
	namespace: &'a str,
	sink: &'a mut dyn Sink,
}

impl NoteSink for Env<'_> {
	fn note(&mut self, where_: &str, text: &str) {
		self.sink.note(if where_.is_empty() { text.to_string() } else { format!("{where_}: {text}") });
	}
}

impl MiddlewareEnv for Env<'_> {
	/// The users come from the Secret `secret` (key `users`, htpasswd lines) in the
	/// Middleware's namespace, written as a file next to rproxy.
	fn basic_auth_users_file(&mut self, _name: &str, cfg: &traefik::Value, where_: &str) -> Option<String> {
		let Some(secret) = g(cfg, &["secret"]).and_then(traefik::Value::as_str).filter(|s| !s.is_empty()) else {
			self.note(where_, "basicAuth without a secret is not converted");
			return None;
		};
		let file = self.sink.users_file(self.namespace, secret);
		if file.is_none() {
			let ns = self.namespace;
			self.note(where_, &format!("Secret {ns}/{secret} has no users (htpasswd lines in the key users); left out"));
		}
		file
	}

	/// The service is a Kubernetes Service reference (`name`, `port`, `namespace`).
	fn errors_service(&mut self, service: Option<&traefik::Value>, where_: &str) -> Option<String> {
		let found = service.and_then(|s| self.sink.errors_service(self.namespace, &Value::from(s)));
		if found.is_none() {
			self.note(where_, "errors.service is not a Kubernetes Service; left out");
		}
		found
	}

	/// The controller does not write rproxy's settings file: say what it needs.
	fn crowdsec_global(&mut self, global: traefik::Map, trusted_ips: Vec<traefik::Value>, where_: &str) {
		let global = Value::from(&traefik::Value::Map(global));
		self.note(
			where_,
			&format!("the CrowdSec bouncer needs global.crowdsec in rproxy's settings file (not set by the controller): {global}"),
		);
		if !trusted_ips.is_empty() {
			let ips = Value::from(&traefik::Value::List(trusted_ips));
			self.note(where_, &format!("forwardedHeadersTrustedIPs: set global.trusted_proxies in rproxy's settings file: {ips}"));
		}
	}
}

/// Converts the Traefik Middleware `spec` (`{type: settings}`) of `namespace/name`.
pub fn convert(namespace: &str, name: &str, spec: &Map<String, Value>, sink: &mut dyn Sink) -> Converted {
	let spec = traefik::Value::from(&Value::Object(spec.clone()));
	let mut env = Env { namespace, sink };
	match traefik::convert_middleware(name, &spec, &format!("middleware {namespace}/{name}"), &mut env) {
		traefik::Converted::One(v) => Converted::One(Value::from(&v)),
		traefik::Converted::Chain(refs) => Converted::Chain(refs),
		traefik::Converted::None => Converted::None,
	}
}

#[cfg(test)]
mod tests {
	use serde_json::json;

	use super::*;

	#[derive(Default)]
	struct S(Vec<String>);
	impl Sink for S {
		fn note(&mut self, t: String) {
			self.0.push(t);
		}
		fn users_file(&mut self, ns: &str, s: &str) -> Option<String> {
			(s != "missing").then(|| format!("/f/{ns}-{s}"))
		}
		fn errors_service(&mut self, ns: &str, s: &Value) -> Option<String> {
			Some(format!("{ns}/{}", s["name"].as_str()?))
		}
	}

	fn conv(spec: Value) -> (Converted, Vec<String>) {
		let mut s = S::default();
		let out = convert("ns", "m", spec.as_object().unwrap(), &mut s);
		(out, s.0)
	}

	fn one(spec: Value) -> Option<Value> {
		match conv(spec).0 {
			Converted::One(v) => Some(v),
			_ => None,
		}
	}

	// the mapping itself is tested in rproxy-traefik; these test the Kubernetes side
	#[test]
	fn middlewares() {
		assert_eq!(
			one(json!({"redirectScheme": {"scheme": "https", "permanent": true, "port": "443"}})),
			Some(json!({"redirect_scheme": {"scheme": "https", "permanent": true}}))
		);
		let (h, notes) = conv(json!({"headers": {"customRequestHeaders": {"X-A": "1", "X-B": ""}, "frameDeny": true, "foo": 1}}));
		assert_eq!(h, Converted::One(json!({"headers": {"request": {"set": {"X-A": "1"}, "remove": ["X-B"]}, "frame_deny": true}})));
		assert_eq!(notes, ["middleware ns/m: headers.foo is not converted"]);
		// basicAuth: the Secret in the Middleware's namespace
		assert_eq!(
			one(json!({"basicAuth": {"secret": "users"}})),
			Some(json!({"basic_auth": {"users_file": "/f/ns-users", "keep_authorization": true}}))
		);
		let (none, notes) = conv(json!({"basicAuth": {"secret": "missing"}}));
		assert_eq!(none, Converted::None);
		assert!(notes[0].contains("Secret ns/missing has no users"), "{notes:?}");
		assert!(notes_of(json!({"basicAuth": {"users": ["a:b"]}}))[0].contains("without a secret"));
		// errors: a Kubernetes Service
		assert_eq!(
			one(json!({"errors": {"status": ["500-599"], "service": {"name": "err", "port": 80}, "query": "/{status}.html"}})).unwrap()["errors"]
				["service"],
			"ns/err"
		);
		assert!(notes_of(json!({"errors": {"status": ["500"]}}))[0].contains("not a Kubernetes Service"));
		// CrowdSec: the global settings are a note
		let (cs, notes) =
			conv(json!({"plugin": {"bouncer": {"crowdsecLapiHost": "cs:8080", "forwardedHeadersTrustedIPs": ["10.0.0.0/8"]}}}));
		assert_eq!(cs, Converted::One(json!({"crowdsec": {"appsec": false}})));
		assert!(notes.iter().any(|n| n.contains("global.crowdsec") && n.contains("http://cs:8080")), "{notes:?}");
		assert!(notes.iter().any(|n| n.contains("global.trusted_proxies") && n.contains("10.0.0.0/8")), "{notes:?}");
		assert_eq!(conv(json!({"digestAuth": {}})).0, Converted::None);
		assert_eq!(
			conv(json!({"chain": {"middlewares": [{"name": "a"}, {"name": "b@kubernetescrd", "namespace": "x"}]}})).0,
			Converted::Chain(vec![
				ChainRef { name: "a".into(), namespace: None },
				ChainRef { name: "b".into(), namespace: Some("x".into()) }
			])
		);
	}

	fn notes_of(spec: Value) -> Vec<String> {
		conv(spec).1
	}
}
