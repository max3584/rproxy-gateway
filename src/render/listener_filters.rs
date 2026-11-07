//! A listener's `filters.requests` (GEP-5224, Gateway API v1.7 `GatewayListenerFilters`) → rproxy's
//! `http.pre_routing` (rproxy v0.4.22): middlewares every request of the rule goes through, in the
//! listener's order, before a route is chosen.
//!
//! - `ExternalAuth` is a `forward_auth`, as on an HTTPRoute (`http::external_auth`); its ReferenceGrant
//!   is from the Gateway (or the ListenerSet).
//! - `ExtensionRef` names an RproxyMiddleware of the listener's namespace. Its kind must be one rproxy
//!   runs before route matching (`features.pre_routing_kinds`: not `retry`, `mirror`, `cache`,
//!   `replace_host`), it may not change `Host` (GEP-5224: listener selection does not change), and it
//!   may not name a service (a route's own rules only; a listener has none).
//! - Each filter is one middleware, named `listener:f<i>` (no route's name has `:` but `grpc:`), so the
//!   order is the list's (never `InvalidListenerFilterOrder`), and rproxy's rule that a `pre_routing`
//!   name is left out of route chains never applies: the same RproxyMiddleware on the listener and on a
//!   route runs twice, as two independent instances (GEP-5224, "both instances execute").
//! - A filter whose reference does not resolve answers 500 in its place (`ResolvedRefs: False`,
//!   `InvalidListenerFilterRef` or `RefNotPermitted`): nothing passes without it.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::{Value, json};

use crate::k8s::crd::GROUP as RPROXY_GROUP;
use crate::k8s::gateway::{ExternalAuth, Listener, LocalObjectReference};
use crate::render::http::{self, Ctx, Output};
use crate::render::status::Cond;
use crate::rproxy::model as rp;

/// What every rproxy pod must answer for listener filters (`GET /capabilities`).
pub const FEATURE: &str = "http_options.pre_routing";

/// The kinds rproxy-api v0.4.22 runs before route matching (docs/API.md; `features.pre_routing_kinds`
/// is what a pod says): for a Gateway with no pod yet whose image is known to have `pre_routing`.
pub const DOCUMENTED_KINDS: &[&str] = &[
	"redirect_scheme",
	"redirect_regex",
	"ip_allow",
	"headers",
	"strip_prefix",
	"add_prefix",
	"replace_path",
	"replace_path_regex",
	"respond",
	"rate_limit",
	"in_flight",
	"crowdsec",
	"compress",
	"buffering",
	"circuit_breaker",
	"errors",
	"basic_auth",
	"forward_auth",
	"oidc",
	"geoip",
	"cors",
];

/// The kinds that decide who the user is: with one before routing, every route's `cache` needs
/// `share_after_auth: true` (rproxy-api docs/API.md, #340).
pub const AUTH_KINDS: &[&str] = &["basic_auth", "forward_auth", "oidc"];

/// The listener's `filters.requests`, if it has any.
pub fn requests(l: &Listener) -> Option<&Vec<Value>> {
	l.filters.as_ref().and_then(|f| f.get("requests")).and_then(Value::as_array).filter(|r| !r.is_empty())
}

/// `filters.requests[i] (<type>)`: how a filter is named in conditions.
pub fn label(i: usize, f: &Value) -> String {
	format!("filters.requests[{i}] ({})", f.get("type").and_then(Value::as_str).unwrap_or("?"))
}

/// The middleware name of filter `i`.
pub fn name(i: usize) -> String {
	format!("listener:f{i}")
}

/// What a listener's filters add to its rule.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Chain {
	/// `http.pre_routing`, in the listener's order.
	pub names: Vec<String>,
	pub middlewares: BTreeMap<String, Value>,
	pub services: BTreeMap<String, rp::Service>,
	/// The BackendTLSPolicies of the ExternalAuth backends.
	pub tls_policies: BTreeSet<crate::render::world::Key>,
}

impl Chain {
	/// Whether a filter decides who the user is (`AUTH_KINDS`).
	pub fn authenticates(&self) -> bool {
		self.middlewares.values().any(|m| m.as_object().is_some_and(|o| o.keys().any(|k| AUTH_KINDS.contains(&k.as_str()))))
	}
}

/// A listener's filters as rproxy middlewares.
#[derive(Debug, Default)]
pub struct Built {
	pub chain: Chain,
	/// `ResolvedRefs: False` for the first filter whose reference does not resolve.
	pub resolved: Option<Cond>,
}

/// The listener's filters (`reqs`) in namespace `ns`; `kinds`: what rproxy runs before route matching.
/// `Err`: a filter that cannot be used (`Accepted: False`, `UnsupportedValue`; the message names it).
pub fn build(ctx: &Ctx, ns: &str, reqs: &[Value], kinds: &[String]) -> Result<Built, String> {
	let mut out = Built::default();
	for (i, f) in reqs.iter().enumerate() {
		let at = label(i, f);
		let n = name(i);
		let mut part = Output::default();
		let mw = filter(ctx, ns, f, &n, kinds, &mut part).map_err(|e| format!("{at}: {e}"))?;
		let mw = match mw {
			Some(mw) => mw,
			None => {
				// not resolved: this filter answers 500, nothing reaches routing without it
				let c = part.resolved.take().unwrap_or_else(|| Cond::new("ResolvedRefs", false, "InvalidListenerFilterRef", ""));
				let reason = if c.reason == "RefNotPermitted" { "RefNotPermitted" } else { "InvalidListenerFilterRef" };
				out.resolved.get_or_insert(Cond::new("ResolvedRefs", false, reason, format!("{at}: {}", c.message)));
				json!({"respond": {"status": 500, "body": "500 Internal Server Error\n"}})
			}
		};
		crate::render::validate::middleware(&mw).map_err(|e| format!("{at}: {e}"))?;
		out.chain.names.push(n.clone());
		out.chain.middlewares.insert(n, mw);
		out.chain.services.extend(part.services);
		out.chain.tls_policies.extend(part.tls_policies);
	}
	Ok(out)
}

/// One filter's middleware; `None`: its reference does not resolve (`out.resolved` says why).
fn filter(ctx: &Ctx, ns: &str, f: &Value, name: &str, kinds: &[String], out: &mut Output) -> Result<Option<Value>, String> {
	let allowed = |kind: &str| -> Result<(), String> {
		if kinds.iter().any(|k| k == kind) {
			Ok(())
		} else {
			Err(format!(
				"the middleware kind {kind} cannot run before route matching (rproxy's pre_routing_kinds: {})",
				if kinds.is_empty() { "none".to_string() } else { kinds.join(", ") }
			))
		}
	};
	match f.get("type").and_then(Value::as_str).unwrap_or_default() {
		"ExternalAuth" => {
			let ea: ExternalAuth = f
				.get("externalAuth")
				.cloned()
				.ok_or("externalAuth is missing")
				.and_then(|v| serde_json::from_value(v).map_err(|_| "externalAuth cannot be read"))?;
			allowed("forward_auth")?;
			let mw = http::external_auth(ctx, ns, &ea, name, out)?;
			if let Some(mw) = &mw {
				refuse_host_change(mw)?;
			}
			Ok(mw)
		}
		"ExtensionRef" => {
			let r: LocalObjectReference = f
				.get("extensionRef")
				.cloned()
				.ok_or("extensionRef is missing")
				.and_then(|v| serde_json::from_value(v).map_err(|_| "extensionRef cannot be read"))?;
			if r.group != RPROXY_GROUP || r.kind != "RproxyMiddleware" {
				return Err(format!("ExtensionRef {}/{} is not supported (RproxyMiddleware only)", r.group, r.kind));
			}
			let Some(mw) = ctx.world.middlewares.get(&(ns.to_string(), r.name.clone())) else {
				out.resolved = Some(Cond::new(
					"ResolvedRefs",
					false,
					"InvalidListenerFilterRef",
					format!("RproxyMiddleware {ns}/{} not found", r.name),
				));
				return Ok(None);
			};
			let v = Value::Object(mw.spec.0.0.clone());
			let what = |e: String| format!("RproxyMiddleware {}: {e}", r.name);
			let kind = v.as_object().and_then(|o| o.keys().next().cloned()).unwrap_or_default();
			crate::render::validate::middleware(&v).map_err(what)?;
			allowed(&kind).map_err(what)?;
			if let Some((dir, names)) = ctx.files {
				if let Some(path) = crate::render::foreign_file(&v, dir, names) {
					return Err(what(format!("{path}: only this Gateway's certificate files may be named")));
				}
			}
			crate::render::validate::supported(&v, ctx.features).map_err(what)?;
			crate::render::tenant::check(&v, ctx.middleware_policy).map_err(what)?;
			if let Some(at) = crate::render::tenant::SERVICE_FIELDS.iter().find(|at| v.pointer(at).is_some_and(|s| !s.is_null())) {
				return Err(what(format!(
					"{} names a service: only a route's own rules can be named, and a listener has none (forward_auth: address, or the ExternalAuth filter)",
					at.trim_start_matches('/').replace('/', ".")
				)));
			}
			refuse_host_change(&v).map_err(what)?;
			Ok(Some(v))
		}
		other => Err(format!("filter type {other:?} is not supported (ExternalAuth or ExtensionRef)")),
	}
}

/// GEP-5224: a listener filter may not change `Host` / `:authority` (rproxy puts the client's back,
/// but a filter written to change it is refused rather than quietly not doing so).
fn refuse_host_change(v: &Value) -> Result<(), String> {
	let host = |n: &str| n.eq_ignore_ascii_case("host") || n.eq_ignore_ascii_case(":authority");
	let req = &v["headers"]["request"];
	let named = req["set"].as_object().into_iter().chain(req["add"].as_object()).flat_map(|m| m.keys()).any(|k| host(k))
		|| req["remove"].as_array().into_iter().flatten().filter_map(Value::as_str).any(host)
		|| v["forward_auth"]["response_headers"].as_array().into_iter().flatten().filter_map(Value::as_str).any(host);
	if named { Err("a listener filter cannot change Host or :authority (GEP-5224)".into()) } else { Ok(()) }
}

/// Whether a route's middleware is a `cache` without `share_after_auth: true`: refused by rproxy when
/// a listener filter before it decides who the user is (`Chain::authenticates`).
pub fn unshared_cache(mw: &Value) -> bool {
	mw.get("cache").is_some_and(|c| c.get("share_after_auth") != Some(&Value::Bool(true)))
}
