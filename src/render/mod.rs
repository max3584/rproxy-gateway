//! Rendering: one Gateway (and what attaches to it) → one rproxy rule set, plus
//! the status to write back. Pure: everything comes from a `World` snapshot.
//!
//! - One (protocol, address, port) is one rproxy rule; listeners on the same
//!   port merge into it (different host names).
//! - The set is named `k8s/<Gateway namespace>/<Gateway name>`.
//! - Certificates (`certificateRefs` Secrets) become files on the rproxy host
//!   (written by `certsync`), named by a hash of their content; key material is
//!   never sent over the control API.

pub mod backends;
pub mod hostname;
pub mod http;
pub mod l4;
pub mod migrate;
pub mod policy;
pub mod status;
pub mod traefik_mw;
pub mod world;

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::k8s::gateway::{GROUP, Gateway, HttpRoute, Listener, ParentReference, RouteGroupKind};
use crate::rproxy::model as rp;
use status::Cond;
use world::World;

/// How rules are written.
#[derive(Clone, Debug)]
pub struct Options {
	/// The listen addresses: the first is `listen_addr`, the rest `extra_listen_addrs`.
	pub listen_addrs: Vec<String>,
	/// Where certsync writes certificate files on the rproxy host.
	pub cert_dir: String,
	/// Whether rproxy takes `labels` (`features.labels`).
	pub labels: bool,
	/// Ingress / Traefik resources go into this Gateway's set.
	pub migration: Option<migrate::Settings>,
}

impl Default for Options {
	fn default() -> Self {
		Options { listen_addrs: vec!["0.0.0.0".into()], cert_dir: "/var/run/rproxy-gateway/certs".into(), labels: true, migration: None }
	}
}

/// The kinds of route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteKind {
	Http,
	Tls,
	Tcp,
	Udp,
}

impl RouteKind {
	pub fn kind(self) -> &'static str {
		match self {
			RouteKind::Http => "HTTPRoute",
			RouteKind::Tls => "TLSRoute",
			RouteKind::Tcp => "TCPRoute",
			RouteKind::Udp => "UDPRoute",
		}
	}
}

/// A route's status for one parent reference (`status.parents[]`).
#[derive(Clone, Debug)]
pub struct ParentStatus {
	pub kind: RouteKind,
	pub namespace: String,
	pub name: String,
	pub generation: i64,
	pub parent_ref: ParentReference,
	pub conds: Vec<Cond>,
	/// The rules carrying this route (their rproxy conditions are added).
	pub rule_keys: BTreeSet<String>,
}

/// A listener's status (`status.listeners[]`), before rproxy's answer.
#[derive(Clone, Debug)]
pub struct ListenerPlan {
	pub name: String,
	pub supported_kinds: Vec<RouteGroupKind>,
	pub attached: i32,
	/// `Accepted`, `ResolvedRefs`, `Conflicted` (`Programmed` comes from rproxy).
	pub conds: Vec<Cond>,
	/// The rule serving the listener (none when it is not accepted).
	pub rule_key: Option<String>,
	/// Accepted and with what it needs (certificates) to be programmed.
	pub servable: bool,
}

/// Everything about one Gateway.
#[derive(Clone, Debug, Default)]
pub struct GatewayPlan {
	pub namespace: String,
	pub name: String,
	pub generation: i64,
	/// `k8s/<namespace>/<name>`
	pub ruleset: String,
	pub rules: Vec<rp::Rule>,
	/// Certificate files: name in the certificate directory → content.
	pub files: BTreeMap<String, Vec<u8>>,
	pub listeners: Vec<ListenerPlan>,
	pub parents: Vec<ParentStatus>,
	/// The Gateway's `Accepted` (and why not).
	pub conds: Vec<Cond>,
	/// RproxyRules added to the set (verbatim).
	pub raw: Vec<Value>,
	pub raw_status: Vec<policy::RawStatus>,
	/// RproxyPolicy status for this Gateway.
	pub policies: Vec<policy::PolicyStatus>,
	/// What migration could not convert.
	pub notes: Vec<String>,
	/// The (protocol, port) of listeners that can be served (Service ports).
	pub listener_ports: BTreeSet<(rp::Protocol, u16)>,
}

impl GatewayPlan {
	/// The rules as the JSON sent to rproxy.
	pub fn rules_json(&self) -> Vec<Value> {
		self.rules.iter().map(|r| serde_json::to_value(r).expect("a rule serializes")).chain(self.raw.iter().cloned()).collect()
	}

	/// The (protocol, port) pairs rproxy listens on (for the Service), ranges of RproxyRules included.
	pub fn ports(&self) -> BTreeSet<(rp::Protocol, u16)> {
		let mut out: BTreeSet<_> = self.rules.iter().map(|r| (r.protocol, r.listen_port)).collect();
		out.extend(self.listener_ports.iter().copied());
		for r in &self.raw {
			let protocol =
				if r["protocol"].as_str().is_some_and(|p| p.eq_ignore_ascii_case("udp")) { rp::Protocol::Udp } else { rp::Protocol::Tcp };
			let Some(start) = r["listen_port"].as_u64().and_then(|p| u16::try_from(p).ok()) else { continue };
			let end = r["listen_port_end"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(start).max(start);
			// a Service holds a limited number of ports
			for port in (start..=end).take(100) {
				out.insert((protocol, port));
			}
		}
		out
	}
}

/// A Gateway API duration (`1h`, `30s`, `500ms`, `1m30s`) in milliseconds.
pub fn duration_ms(s: &str) -> Option<u64> {
	let mut total = 0u64;
	let mut rest = s;
	if rest.is_empty() {
		return None;
	}
	while !rest.is_empty() {
		let digits = rest.find(|c: char| !c.is_ascii_digit())?;
		if digits == 0 {
			return None;
		}
		let n: u64 = rest[..digits].parse().ok()?;
		rest = &rest[digits..];
		let (unit, len) = if rest.starts_with("ms") {
			(1, 2)
		} else if rest.starts_with('h') {
			(3_600_000, 1)
		} else if rest.starts_with('m') {
			(60_000, 1)
		} else if rest.starts_with('s') {
			(1000, 1)
		} else {
			return None;
		};
		total = total.checked_add(n.checked_mul(unit)?)?;
		rest = &rest[len..];
	}
	Some(total)
}

/// What a listener carries, once validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Family {
	Http,
	Https,
	TlsPassthrough,
	TlsTerminate,
	Tcp,
	Udp,
}

impl Family {
	fn route_kinds(self) -> &'static [&'static str] {
		match self {
			Family::Http | Family::Https => &["HTTPRoute"],
			Family::TlsPassthrough | Family::TlsTerminate => &["TLSRoute"],
			Family::Tcp => &["TCPRoute"],
			Family::Udp => &["UDPRoute"],
		}
	}

	fn protocol(self) -> rp::Protocol {
		if self == Family::Udp { rp::Protocol::Udp } else { rp::Protocol::Tcp }
	}

	/// Whether listeners of the two families can share a port (one rproxy rule).
	fn shares_with(self, other: Family) -> bool {
		use Family::*;
		self.protocol() != other.protocol()
			|| self == other
			|| matches!(
				(self, other),
				(Https, TlsPassthrough) | (TlsPassthrough, Https) | (TlsTerminate, TlsPassthrough) | (TlsPassthrough, TlsTerminate)
			)
	}

	/// Whether listeners of this family are told apart by host name.
	fn by_hostname(self) -> bool {
		!matches!(self, Family::Tcp | Family::Udp)
	}
}

struct ListenerState<'a> {
	l: &'a Listener,
	port: u16,
	family: Option<Family>,
	conds: Vec<Cond>,
	supported_kinds: Vec<RouteGroupKind>,
	/// Certificate files (cert, key) for HTTPS and terminating TLS listeners.
	certs: Vec<(String, String)>,
	accepted: bool,
	/// Accepted but with nothing to serve with (no usable certificate): routes
	/// attach, but no rule is made.
	unservable: bool,
	attached: i32,
}

/// The ruleset name of a Gateway.
pub fn ruleset_name(namespace: &str, name: &str) -> String {
	format!("k8s/{namespace}/{name}")
}

/// Whether `parent` refers to the Gateway `gw` (from a route in `route_ns`).
pub fn refers_to(parent: &ParentReference, route_ns: &str, gw_ns: &str, gw_name: &str) -> bool {
	parent.group.as_deref().unwrap_or(GROUP) == GROUP
		&& parent.kind.as_deref().unwrap_or("Gateway") == "Gateway"
		&& parent.namespace.as_deref().unwrap_or(route_ns) == gw_ns
		&& parent.name == gw_name
}

/// A certificate Secret → files, or why not.
fn certificate(
	world: &World,
	gw_ns: &str,
	r: &crate::k8s::gateway::SecretObjectReference,
	files: &mut BTreeMap<String, Vec<u8>>,
	opts: &Options,
) -> Result<(String, String), Cond> {
	let bad = |reason: &str, message: String| Cond::new("ResolvedRefs", false, reason, message);
	let group = r.group.as_deref().unwrap_or("");
	let kind = r.kind.as_deref().unwrap_or("Secret");
	if !group.is_empty() || kind != "Secret" {
		return Err(bad("InvalidCertificateRef", format!("certificateRef {group}/{kind} {}: only Secrets are supported", r.name)));
	}
	let ns = r.namespace.as_deref().unwrap_or(gw_ns);
	if !world.granted((GROUP, "Gateway", gw_ns), ("", "Secret", ns, &r.name)) {
		return Err(bad("RefNotPermitted", format!("Secret {ns}/{}: no ReferenceGrant allows the reference", r.name)));
	}
	let Some(secret) = world.secrets.get(&(ns.to_string(), r.name.clone())) else {
		return Err(bad("InvalidCertificateRef", format!("Secret {ns}/{} not found", r.name)));
	};
	let data = secret.data.clone().unwrap_or_default();
	let (Some(crt), Some(key)) = (data.get("tls.crt"), data.get("tls.key")) else {
		return Err(bad("InvalidCertificateRef", format!("Secret {ns}/{} has no tls.crt and tls.key", r.name)));
	};
	crate::pem::check_pair(&crt.0, &key.0).map_err(|e| bad("InvalidCertificateRef", format!("Secret {ns}/{}: {e}", r.name)))?;
	Ok(cert_files(&crt.0, &key.0, files, opts))
}

/// Adds a certificate and key to `files` (named by their hash); returns their paths.
pub fn cert_files(crt: &[u8], key: &[u8], files: &mut BTreeMap<String, Vec<u8>>, opts: &Options) -> (String, String) {
	let mut both = crt.to_vec();
	both.extend_from_slice(key);
	let id = crate::pem::short_hash(&both);
	files.insert(format!("{id}.crt"), crt.to_vec());
	files.insert(format!("{id}.key"), key.to_vec());
	(format!("{}/{id}.crt", opts.cert_dir), format!("{}/{id}.key", opts.cert_dir))
}

fn validate_listener<'a>(
	world: &World,
	gw: &Gateway,
	l: &'a Listener,
	files: &mut BTreeMap<String, Vec<u8>>,
	opts: &Options,
) -> ListenerState<'a> {
	let gw_ns = gw.metadata.namespace.as_deref().unwrap_or_default();
	let mut st = ListenerState {
		l,
		port: u16::try_from(l.port).unwrap_or(0),
		family: None,
		conds: vec![],
		supported_kinds: vec![],
		certs: vec![],
		accepted: true,
		unservable: false,
		attached: 0,
	};
	let tls_mode = l.tls.as_ref().and_then(|t| t.mode.clone()).unwrap_or_else(|| "Terminate".into());
	let family = match l.protocol.as_str() {
		"HTTP" => Some(Family::Http),
		"HTTPS" => Some(Family::Https),
		"TLS" if tls_mode == "Passthrough" => Some(Family::TlsPassthrough),
		"TLS" => Some(Family::TlsTerminate),
		"TCP" => Some(Family::Tcp),
		"UDP" => Some(Family::Udp),
		_ => None,
	};
	let Some(family) = family else {
		st.accepted = false;
		st.conds.push(Cond::new("Accepted", false, "UnsupportedProtocol", format!("protocol {} is not supported", l.protocol)));
		st.conds.push(Cond::ok("ResolvedRefs", "ResolvedRefs"));
		return st;
	};
	st.family = Some(family);
	// route kinds
	let defaults = family.route_kinds();
	let mut invalid_kinds = vec![];
	match l.allowed_routes.as_ref().and_then(|a| a.kinds.as_ref()) {
		Some(kinds) => {
			for k in kinds {
				let group = k.group.as_deref().unwrap_or(GROUP);
				if group == GROUP && defaults.contains(&k.kind.as_str()) {
					let rk = RouteGroupKind { group: Some(GROUP.into()), kind: k.kind.clone() };
					if !st.supported_kinds.contains(&rk) {
						st.supported_kinds.push(rk);
					}
				} else {
					invalid_kinds.push(format!("{group}/{}", k.kind));
				}
			}
		}
		None => st.supported_kinds = defaults.iter().map(|k| RouteGroupKind { group: Some(GROUP.into()), kind: (*k).into() }).collect(),
	}
	let mut resolved = Cond::ok("ResolvedRefs", "ResolvedRefs");
	if !invalid_kinds.is_empty() {
		resolved =
			Cond::new("ResolvedRefs", false, "InvalidRouteKinds", format!("route kinds not supported: {}", invalid_kinds.join(", ")));
	}
	if family == Family::Https && tls_mode != "Terminate" {
		st.accepted = false;
		st.conds.push(Cond::new("Accepted", false, "UnsupportedValue", "HTTPS listeners terminate TLS (tls.mode Terminate)"));
	}
	if matches!(family, Family::Https | Family::TlsTerminate) {
		let tls = l.tls.clone().unwrap_or_default();
		for r in &tls.certificate_refs {
			match certificate(world, gw_ns, r, files, opts) {
				Ok(pair) => {
					if !st.certs.contains(&pair) {
						st.certs.push(pair);
					}
				}
				Err(e) => {
					if resolved.status {
						resolved = e;
					}
				}
			}
		}
		if tls.certificate_refs.is_empty() && resolved.status {
			resolved = Cond::new("ResolvedRefs", false, "InvalidCertificateRef", "the listener terminates TLS and needs certificateRefs");
		}
		if st.certs.is_empty() {
			// nothing to serve with: routes still attach, the listener is not programmed
			st.unservable = true;
		}
	}
	if st.conds.iter().all(|c| c.kind != "Accepted") {
		st.conds.push(Cond::ok("Accepted", "Accepted"));
	}
	st.conds.push(resolved);
	st
}

/// Marks conflicting listeners (same port, protocols that cannot share it, or the same host name).
fn conflicts(listeners: &mut [ListenerState]) {
	for i in 0..listeners.len() {
		let Some(b) = listeners[i].family else { continue };
		for j in 0..i {
			let Some(a) = listeners[j].family else { continue };
			if listeners[j].port != listeners[i].port || a.protocol() != b.protocol() {
				continue;
			}
			let same_host = listeners[j].l.hostname.as_deref().map(str::to_ascii_lowercase)
				== listeners[i].l.hostname.as_deref().map(str::to_ascii_lowercase);
			let reason = if !a.shares_with(b) {
				Some("ProtocolConflict")
			} else if (same_host || !b.by_hostname()) && a == b {
				Some("HostnameConflict")
			} else {
				None
			};
			if let Some(reason) = reason {
				let st = &mut listeners[i];
				st.accepted = false;
				status::set(&mut st.conds, Cond::new("Conflicted", true, reason, "another listener on this port takes it"));
				status::set(&mut st.conds, Cond::new("Accepted", false, reason, "conflicts with another listener"));
				break;
			}
		}
		if listeners[i].conds.iter().all(|c| c.kind != "Conflicted") {
			listeners[i].conds.push(Cond::new("Conflicted", false, "NoConflicts", ""));
		}
	}
}

/// Whether a route of `kind` in `route_ns` may attach to `st`: `Err(reason)` if not.
fn allowed(world: &World, gw_ns: &str, st: &ListenerState, kind: RouteKind, route_ns: &str) -> Result<(), &'static str> {
	if !st.supported_kinds.iter().any(|k| k.kind == kind.kind()) {
		return Err("NotAllowedByListeners");
	}
	let ns = st.l.allowed_routes.as_ref().and_then(|a| a.namespaces.as_ref());
	let ok = match ns.and_then(|n| n.from.as_deref()).unwrap_or("Same") {
		"All" => true,
		"Selector" => ns.and_then(|n| n.selector.as_ref()).is_some_and(|s| world.namespace_matches(route_ns, s)),
		_ => route_ns == gw_ns,
	};
	if ok { Ok(()) } else { Err("NotAllowedByListeners") }
}

/// Host names more specific listeners on the same port take from listener `i`.
fn exclusions(listeners: &[ListenerState], i: usize) -> Vec<String> {
	let me = listeners[i].l.hostname.as_deref();
	listeners
		.iter()
		.enumerate()
		.filter(|(j, o)| *j != i && o.accepted && !o.unservable && o.port == listeners[i].port && o.family == listeners[i].family)
		.filter_map(|(_, o)| o.l.hostname.clone())
		.filter(|h| match me {
			None => true,
			Some(m) => m.starts_with("*.") && !m.eq_ignore_ascii_case(h) && hostname::covers(m, h),
		})
		.collect()
}

/// One route attached to a listener.
struct Attachment {
	kind: RouteKind,
	route: usize,
	listener: usize,
	hosts: Option<Vec<String>>,
}

/// The parts of a route attachment needs.
struct RouteInfo<'a> {
	kind: RouteKind,
	index: usize,
	meta: &'a k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
	parent_refs: &'a [ParentReference],
	hostnames: &'a [String],
}

fn routes(world: &World) -> Vec<RouteInfo<'_>> {
	let mut out = vec![];
	for (i, r) in world.http_routes.iter().enumerate() {
		out.push(RouteInfo {
			kind: RouteKind::Http,
			index: i,
			meta: &r.metadata,
			parent_refs: &r.spec.parent_refs,
			hostnames: &r.spec.hostnames,
		});
	}
	for (kind, list) in [(RouteKind::Tls, &world.tls_routes), (RouteKind::Tcp, &world.tcp_routes), (RouteKind::Udp, &world.udp_routes)] {
		for (i, r) in list.iter().enumerate() {
			out.push(RouteInfo { kind, index: i, meta: &r.metadata, parent_refs: &r.spec.parent_refs, hostnames: &r.spec.hostnames });
		}
	}
	out
}

fn l4_route(world: &World, kind: RouteKind, index: usize) -> &crate::k8s::gateway::L4Route {
	match kind {
		RouteKind::Tls => &world.tls_routes[index],
		RouteKind::Tcp => &world.tcp_routes[index],
		_ => &world.udp_routes[index],
	}
}

/// Renders one Gateway.
pub fn render_gateway(world: &World, gw: &Gateway, opts: &Options) -> GatewayPlan {
	let gw_ns = gw.metadata.namespace.clone().unwrap_or_default();
	let gw_name = gw.metadata.name.clone().unwrap_or_default();
	let mut plan = GatewayPlan {
		namespace: gw_ns.clone(),
		name: gw_name.clone(),
		generation: gw.metadata.generation.unwrap_or(0),
		ruleset: ruleset_name(&gw_ns, &gw_name),
		..Default::default()
	};
	let mut files = BTreeMap::new();
	let mut listeners: Vec<ListenerState> = gw.spec.listeners.iter().map(|l| validate_listener(world, gw, l, &mut files, opts)).collect();
	conflicts(&mut listeners);

	// attach routes
	let mut attachments: Vec<Attachment> = vec![];
	for r in routes(world) {
		let rns = r.meta.namespace.clone().unwrap_or_default();
		for p in r.parent_refs.iter().filter(|p| refers_to(p, &rns, &gw_ns, &gw_name)) {
			let accepted = attach(world, &gw_ns, &mut listeners, p, r.kind, &rns, r.hostnames, |li, hosts| {
				attachments.push(Attachment { kind: r.kind, route: r.index, listener: li, hosts })
			});
			plan.parents.push(ParentStatus {
				kind: r.kind,
				namespace: rns.clone(),
				name: r.meta.name.clone().unwrap_or_default(),
				generation: r.meta.generation.unwrap_or(0),
				parent_ref: p.clone(),
				conds: vec![accepted],
				rule_keys: BTreeSet::new(),
			});
		}
	}
	for (i, st) in listeners.iter_mut().enumerate() {
		st.attached = attachments.iter().filter(|a| a.listener == i).map(|a| (a.kind, a.route)).collect::<BTreeSet<_>>().len() as i32;
	}

	// one rule per (protocol, port)
	let mut groups: BTreeMap<(rp::Protocol, u16), Vec<usize>> = BTreeMap::new();
	for (i, st) in listeners.iter().enumerate() {
		if let (true, false, Some(f)) = (st.accepted, st.unservable, st.family) {
			groups.entry((f.protocol(), st.port)).or_default().push(i);
		}
	}
	// per route: (ResolvedRefs, unsupported)
	let mut results: BTreeMap<(RouteKind, usize), (Option<Cond>, Option<String>)> = BTreeMap::new();
	let mut listener_keys: BTreeMap<usize, String> = BTreeMap::new();
	let mut l4_services: BTreeMap<String, Vec<world::Key>> = BTreeMap::new();
	let mut http_services: BTreeMap<(String, String), Vec<world::Key>> = BTreeMap::new();
	for ((protocol, port), members) in &groups {
		let key = rp::rule_key(*protocol, &opts.listen_addrs[0], *port);
		let has = |f: Family| members.iter().any(|i| listeners[*i].family == Some(f));
		let mine = |a: &Attachment| members.contains(&a.listener);
		let mut rule = new_rule(*protocol, *port, opts);
		let mut carried: Vec<(RouteKind, usize, usize)> = vec![];
		let mut certificates: Vec<rp::Certificate> = vec![];
		for i in members {
			for (crt, k) in &listeners[*i].certs {
				let c = rp::Certificate { cert_file: Some(crt.clone()), key_file: Some(k.clone()), ..Default::default() };
				if !certificates.contains(&c) {
					certificates.push(c);
				}
			}
		}
		// TLS routes by server name (passthrough, or terminated on a TLS listener)
		let mut tls_routes: Vec<rp::TlsRoute> = vec![];
		for a in attachments.iter().filter(|a| a.kind == RouteKind::Tls && mine(a)) {
			let route = l4_route(world, RouteKind::Tls, a.route);
			let (dest, resolved) = l4::destination(world, route);
			let entry = results.entry((RouteKind::Tls, a.route)).or_insert((None, None));
			if entry.0.is_none() {
				entry.0 = resolved;
			}
			// no usable backend: connections for its names are accepted and closed (Gateway API
			// expects a reset, not a refused connection), through a port nothing listens on
			let dest = dest.unwrap_or(backends::Endpoint { addr: "127.0.0.1".into(), port: 1 });
			let names: Vec<String> = a.hosts.clone().unwrap_or_default().iter().map(|h| hostname::to_rproxy(h)).collect();
			if names.is_empty() {
				continue;
			}
			let passthrough =
				listeners[a.listener].family == Some(Family::TlsPassthrough) && (has(Family::Https) || has(Family::TlsTerminate));
			tls_routes.push(rp::TlsRoute { server_names: names, remote_addr: dest.addr.clone(), remote_port: dest.port, passthrough });
			carried.push((RouteKind::Tls, a.route, a.listener));
		}
		if has(Family::Http) || has(Family::Https) {
			let https = has(Family::Https);
			let ctx = http::Ctx { world, scheme: if https { "https" } else { "http" }, port: *port };
			let mut entries = vec![];
			let mut services = BTreeMap::new();
			let mut middlewares = BTreeMap::new();
			for a in attachments.iter().filter(|a| a.kind == RouteKind::Http && mine(a)) {
				let route: &HttpRoute = &world.http_routes[a.route];
				let mut out = http::build(&ctx, route, a.hosts.as_deref(), &exclusions(&listeners, a.listener));
				let several = attachments.iter().filter(|b| b.kind == RouteKind::Http && b.route == a.route && mine(b)).count() > 1;
				if several {
					// one rproxy route per listener: names stay unique
					for e in &mut out.entries {
						e.route.name = format!("{}@{}", e.route.name, listeners[a.listener].l.name);
					}
				}
				let entry = results.entry((RouteKind::Http, a.route)).or_insert((None, None));
				if entry.0.is_none() {
					entry.0 = out.resolved.clone();
				}
				if entry.1.is_none() {
					entry.1 = out.unsupported.clone();
				}
				if out.unsupported.is_some() {
					continue;
				}
				entries.extend(out.entries);
				services.extend(out.services);
				middlewares.extend(out.middlewares);
				for (svc, b) in out.backends {
					http_services.insert((key.clone(), svc), b);
				}
				carried.push((RouteKind::Http, a.route, a.listener));
			}
			let routes = http::assign_priorities(&mut entries);
			rule.http = Some(rp::Http { routes, default: None, services, middlewares });
			if https {
				let passthrough: Vec<rp::TlsRoute> = tls_routes.into_iter().filter(|r| r.passthrough).collect();
				rule.tls = Some(rp::Tls { mode: "terminate", certificates, routes: passthrough, ..Default::default() });
			}
		} else if has(Family::TlsPassthrough) || has(Family::TlsTerminate) {
			if tls_routes.is_empty() {
				// nothing to send to: no rule (the port stays closed)
				continue;
			}
			let first = &tls_routes[0];
			rule.targets = vec![rp::Target { addr: first.remote_addr.clone(), port: first.remote_port, weight: None }];
			let terminate = has(Family::TlsTerminate);
			rule.tls = Some(rp::Tls {
				mode: if terminate { "terminate" } else { "sni" },
				routes: tls_routes,
				certificates: if terminate { certificates } else { vec![] },
				unmatched: Some("reject"),
				..Default::default()
			});
		} else {
			// TCP or UDP: the routes' backends together
			let kind = if has(Family::Udp) { RouteKind::Udp } else { RouteKind::Tcp };
			let mut targets: Vec<rp::Target> = vec![];
			// several routes on one TCP / UDP listener: the oldest gets the traffic (all are accepted)
			let oldest = attachments
				.iter()
				.filter(|a| a.kind == kind && mine(a))
				.min_by_key(|a| {
					let m = &l4_route(world, kind, a.route).metadata;
					(m.creation_timestamp.as_ref().map(|t| t.0.to_string()), m.namespace.clone(), m.name.clone())
				})
				.map(|a| a.route);
			for a in attachments.iter().filter(|a| a.kind == kind && mine(a)) {
				if Some(a.route) != oldest {
					continue;
				}
				let out = l4::targets(world, kind.kind(), l4_route(world, kind, a.route));
				let entry = results.entry((kind, a.route)).or_insert((None, None));
				if entry.0.is_none() {
					entry.0 = out.resolved.clone();
				}
				l4_services.entry(key.clone()).or_default().extend(out.services);
				for t in out.targets {
					if !targets.iter().any(|x| x.addr == t.addr && x.port == t.port) {
						targets.push(t);
					}
				}
				carried.push((kind, a.route, a.listener));
			}
			if targets.is_empty() {
				continue;
			}
			rule.targets = targets;
		}
		for (kind, ri, li) in carried {
			let route_meta = match kind {
				RouteKind::Http => &world.http_routes[ri].metadata,
				k => &l4_route(world, k, ri).metadata,
			};
			for p in plan.parents.iter_mut().filter(|p| {
				p.kind == kind && Some(&p.namespace) == route_meta.namespace.as_ref() && Some(&p.name) == route_meta.name.as_ref()
			}) {
				if listeners_for(&listeners, &p.parent_ref).contains(&li) {
					p.rule_keys.insert(key.clone());
				}
			}
		}
		for i in members {
			listener_keys.insert(*i, key.clone());
		}
		plan.rules.push(rule);
	}

	// route conditions: ResolvedRefs, unsupported values
	for p in plan.parents.iter_mut() {
		let index = match p.kind {
			RouteKind::Http => world.http_routes.iter().position(|r| {
				r.metadata.namespace.as_deref() == Some(p.namespace.as_str()) && r.metadata.name.as_deref() == Some(p.name.as_str())
			}),
			k => {
				let list = match k {
					RouteKind::Tls => &world.tls_routes,
					RouteKind::Tcp => &world.tcp_routes,
					_ => &world.udp_routes,
				};
				list.iter().position(|r| {
					r.metadata.namespace.as_deref() == Some(p.namespace.as_str()) && r.metadata.name.as_deref() == Some(p.name.as_str())
				})
			}
		};
		let (resolved, unsupported) = index.and_then(|i| results.get(&(p.kind, i)).cloned()).unwrap_or_else(|| {
			// not carried by any rule: still check its references
			let resolved = index.and_then(|i| match p.kind {
				RouteKind::Http => http::build(&http::Ctx { world, scheme: "http", port: 80 }, &world.http_routes[i], None, &[]).resolved,
				RouteKind::Tls => l4::destination(world, &world.tls_routes[i]).1,
				k => l4::targets(world, k.kind(), l4_route(world, k, i)).resolved,
			});
			(resolved, None)
		});
		if let Some(u) = unsupported {
			if status::get(&p.conds, "Accepted").is_some_and(|c| c.status) {
				status::set(&mut p.conds, Cond::new("Accepted", false, "UnsupportedValue", u));
			}
		}
		p.conds.push(resolved.unwrap_or_else(|| Cond::ok("ResolvedRefs", "ResolvedRefs")));
	}

	// Ingress and Traefik resources
	if let Some(m) = opts.migration.as_ref().filter(|m| m.gateway == (gw_ns.clone(), gw_name.clone())) {
		let out = migrate::render(world, m, opts);
		for (key, add) in out.ports {
			let rkey = rp::rule_key(key.0, &opts.listen_addrs[0], key.1);
			let existing = plan.rules.iter_mut().find(|r| r.key() == rkey);
			let (rule, notes) = migrate::apply_port(key, add, existing, new_rule(key.0, key.1, opts));
			plan.notes.extend(notes);
			if let Some(r) = rule {
				plan.rules.push(r);
			}
		}
		files.extend(out.files);
		plan.notes.extend(out.notes);
	}

	// rproxy's own resources
	let by_name: BTreeMap<String, String> = listener_keys.iter().map(|(i, k)| (listeners[*i].l.name.clone(), k.clone())).collect();
	plan.policies = policy::apply(
		world,
		gw,
		&mut plan.rules,
		&policy::Targets { listeners: &by_name, l4_services: &l4_services, http_services: &http_services },
	);
	let taken: Vec<String> = plan.rules.iter().map(|r| r.key()).collect();
	let (raw, raw_status) = policy::raw_rules(world, gw, &taken);
	plan.raw = raw;
	plan.raw_status = raw_status;

	plan.listeners = listeners
		.iter()
		.enumerate()
		.map(|(i, st)| ListenerPlan {
			name: st.l.name.clone(),
			supported_kinds: st.supported_kinds.clone(),
			attached: st.attached,
			conds: st.conds.clone(),
			rule_key: listener_keys.get(&i).cloned(),
			servable: st.accepted && !st.unservable,
		})
		.collect();
	// Service ports: every listener that can be served, with or without a rule yet
	plan.listener_ports =
		listeners.iter().filter(|l| l.accepted && !l.unservable).filter_map(|l| l.family.map(|f| (f.protocol(), l.port))).collect();
	let any = listeners.iter().any(|l| l.accepted);
	let all = listeners.iter().all(|l| l.accepted);
	plan.conds.push(if listeners.is_empty() || all {
		Cond::ok("Accepted", "Accepted")
	} else if any {
		Cond::new("Accepted", true, "ListenersNotValid", "some listeners are not valid")
	} else {
		Cond::new("Accepted", false, "ListenersNotValid", "no listener is valid")
	});
	if opts.labels {
		let labels: BTreeMap<String, String> = [
			("gateway.networking.k8s.io/gateway-name".to_string(), gw_name.clone()),
			("gateway.networking.k8s.io/gateway-namespace".to_string(), gw_ns.clone()),
			("app.kubernetes.io/managed-by".to_string(), "rproxy-gateway".to_string()),
		]
		.into();
		for r in &mut plan.rules {
			r.labels.extend(labels.clone());
		}
		for r in &mut plan.raw {
			if let Some(o) = r.as_object_mut() {
				let l = o.entry("labels").or_insert_with(|| Value::Object(Default::default()));
				if let Some(l) = l.as_object_mut() {
					for (k, v) in &labels {
						l.entry(k.clone()).or_insert_with(|| Value::String(v.clone()));
					}
				}
			}
		}
	}
	plan.files = files;
	plan
}

fn new_rule(protocol: rp::Protocol, port: u16, opts: &Options) -> rp::Rule {
	rp::Rule {
		protocol,
		listen_addr: opts.listen_addrs[0].clone(),
		listen_port: port,
		extra_listen_addrs: opts.listen_addrs[1..].to_vec(),
		..Default::default()
	}
}

/// The listeners a parent reference names (by section name and port).
fn listeners_for(listeners: &[ListenerState], p: &ParentReference) -> Vec<usize> {
	listeners
		.iter()
		.enumerate()
		.filter(|(_, st)| p.section_name.as_deref().is_none_or(|s| s == st.l.name) && p.port.is_none_or(|port| i32::from(st.port) == port))
		.map(|(i, _)| i)
		.collect()
}

/// Attaches a route through one parent reference; returns its `Accepted`.
#[allow(clippy::too_many_arguments)]
fn attach(
	world: &World,
	gw_ns: &str,
	listeners: &mut [ListenerState],
	p: &ParentReference,
	kind: RouteKind,
	route_ns: &str,
	hostnames: &[String],
	mut add: impl FnMut(usize, Option<Vec<String>>),
) -> Cond {
	let candidates = listeners_for(listeners, p);
	if candidates.is_empty() {
		return Cond::new("Accepted", false, "NoMatchingParent", "no listener matches the parentRef's sectionName and port");
	}
	let mut reason = "NotAllowedByListeners";
	let mut attached = false;
	for i in candidates {
		let st = &listeners[i];
		if !st.accepted {
			continue;
		}
		if let Err(r) = allowed(world, gw_ns, st, kind, route_ns) {
			reason = r;
			continue;
		}
		let listener_host = if matches!(kind, RouteKind::Tcp | RouteKind::Udp) { None } else { st.l.hostname.as_deref() };
		match hostname::effective(listener_host, hostnames) {
			Ok(hosts) => {
				attached = true;
				add(i, hosts);
			}
			Err(hostname::NoMatch) => {
				if reason == "NotAllowedByListeners" {
					reason = "NoMatchingListenerHostname";
				}
			}
		}
	}
	if attached {
		Cond::ok("Accepted", "Accepted")
	} else {
		let message = match reason {
			"NoMatchingListenerHostname" => "no listener host name intersects the route's host names",
			_ => "no listener allows this route (kind or namespace)",
		};
		Cond::new("Accepted", false, reason, message)
	}
}

#[cfg(test)]
mod tests;
