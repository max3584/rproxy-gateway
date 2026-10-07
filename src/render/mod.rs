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
pub mod status;
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
}

impl Default for Options {
	fn default() -> Self {
		Options { listen_addrs: vec!["0.0.0.0".into()], cert_dir: "/var/run/rproxy-gateway/certs".into(), labels: true }
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
}

impl GatewayPlan {
	/// The rules as the JSON sent to rproxy.
	pub fn rules_json(&self) -> Vec<Value> {
		self.rules.iter().map(|r| serde_json::to_value(r).expect("a rule serializes")).collect()
	}

	/// The (protocol, port) pairs rproxy listens on (for the Service).
	pub fn ports(&self) -> BTreeSet<(rp::Protocol, u16)> {
		self.rules.iter().map(|r| (r.protocol, r.listen_port)).collect()
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
}

impl Family {
	fn route_kinds(self) -> &'static [&'static str] {
		&["HTTPRoute"]
	}
}

struct ListenerState<'a> {
	l: &'a Listener,
	port: u16,
	family: Option<Family>,
	conds: Vec<Cond>,
	supported_kinds: Vec<RouteGroupKind>,
	/// Certificate files (cert, key) for HTTPS.
	certs: Vec<(String, String)>,
	accepted: bool,
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
		attached: 0,
	};
	let family = match l.protocol.as_str() {
		"HTTP" => Some(Family::Http),
		"HTTPS" => Some(Family::Https),
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
	if family == Family::Https {
		let tls = l.tls.clone().unwrap_or_default();
		if tls.mode.as_deref().unwrap_or("Terminate") != "Terminate" {
			st.accepted = false;
			st.conds.push(Cond::new("Accepted", false, "UnsupportedValue", "HTTPS listeners terminate TLS (tls.mode Terminate)"));
		}
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
			resolved = Cond::new("ResolvedRefs", false, "InvalidCertificateRef", "an HTTPS listener needs certificateRefs");
		}
		if st.certs.is_empty() {
			// nothing to serve with: the listener is not programmed
			st.accepted = st.accepted && false;
			if st.conds.iter().all(|c| c.kind != "Accepted") {
				st.conds.push(Cond::new("Accepted", false, "InvalidCertificateRef", "no usable certificate"));
			}
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
		if listeners[i].family.is_none() {
			continue;
		}
		for j in 0..i {
			if listeners[j].family.is_none() || listeners[j].port != listeners[i].port {
				continue;
			}
			let (a, b) = (listeners[j].family, listeners[i].family);
			let reason = if a != b {
				Some("ProtocolConflict")
			} else if listeners[j].l.hostname.as_deref().map(str::to_ascii_lowercase)
				== listeners[i].l.hostname.as_deref().map(str::to_ascii_lowercase)
			{
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
		.filter(|(j, o)| *j != i && o.accepted && o.port == listeners[i].port && o.family == listeners[i].family)
		.filter_map(|(_, o)| o.l.hostname.clone())
		.filter(|h| match me {
			None => true,
			Some(m) => m.starts_with("*.") && !m.eq_ignore_ascii_case(h) && hostname::covers(m, h),
		})
		.collect()
}

/// One route attached to a listener.
struct Attachment {
	route: usize,
	listener: usize,
	hosts: Option<Vec<String>>,
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

	// HTTPRoutes
	let mut attachments: Vec<Attachment> = vec![];
	for (ri, route) in world.http_routes.iter().enumerate() {
		let rns = route.metadata.namespace.clone().unwrap_or_default();
		for p in route.spec.parent_refs.iter().filter(|p| refers_to(p, &rns, &gw_ns, &gw_name)) {
			let accepted = attach(world, &gw_ns, &mut listeners, p, RouteKind::Http, &rns, &route.spec.hostnames, |li, hosts| {
				attachments.push(Attachment { route: ri, listener: li, hosts })
			});
			plan.parents.push(ParentStatus {
				kind: RouteKind::Http,
				namespace: rns.clone(),
				name: route.metadata.name.clone().unwrap_or_default(),
				generation: route.metadata.generation.unwrap_or(0),
				parent_ref: p.clone(),
				conds: vec![accepted],
				rule_keys: BTreeSet::new(),
			});
		}
	}

	// one rule per port
	let mut ports: BTreeMap<u16, Vec<usize>> = BTreeMap::new();
	for (i, st) in listeners.iter().enumerate() {
		if st.accepted {
			ports.entry(st.port).or_default().push(i);
		}
	}
	let mut route_results: BTreeMap<usize, (Option<Cond>, Option<String>)> = BTreeMap::new();
	for (port, members) in &ports {
		let family = listeners[members[0]].family.expect("accepted listeners have a family");
		let key = rp::rule_key(rp::Protocol::Tcp, &opts.listen_addrs[0], *port);
		let ctx = http::Ctx { world, scheme: if family == Family::Https { "https" } else { "http" }, port: *port };
		let mut entries = vec![];
		let mut services = BTreeMap::new();
		let mut middlewares = BTreeMap::new();
		for a in attachments.iter().filter(|a| members.contains(&a.listener)) {
			let route: &HttpRoute = &world.http_routes[a.route];
			let out = http::build(&ctx, route, a.hosts.as_deref(), &exclusions(&listeners, a.listener));
			let entry = route_results.entry(a.route).or_insert((None, None));
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
			let rname = route.metadata.name.clone().unwrap_or_default();
			let rns = route.metadata.namespace.clone().unwrap_or_default();
			for p in plan.parents.iter_mut().filter(|p| p.kind == RouteKind::Http && p.namespace == rns && p.name == rname) {
				if listeners_for(&listeners, &p.parent_ref).contains(&a.listener) {
					p.rule_keys.insert(key.clone());
				}
			}
		}
		let routes = http::assign_priorities(&mut entries);
		let mut rule = new_rule(rp::Protocol::Tcp, *port, opts);
		rule.http = Some(rp::Http { routes, default: None, services, middlewares });
		if family == Family::Https {
			let mut certificates: Vec<rp::Certificate> = vec![];
			// listeners with a host name first, so a client without SNI gets the catch-all's certificate last
			for i in members {
				for (crt, key) in &listeners[*i].certs {
					let c = rp::Certificate { cert_file: Some(crt.clone()), key_file: Some(key.clone()), ..Default::default() };
					if !certificates.contains(&c) {
						certificates.push(c);
					}
				}
			}
			rule.tls = Some(rp::Tls { mode: "terminate", certificates, ..Default::default() });
		}
		for i in members {
			listeners[*i].attached = attachments.iter().filter(|a| a.listener == *i).map(|a| a.route).collect::<BTreeSet<_>>().len() as i32;
		}
		plan.rules.push(rule);
	}

	// route conditions: ResolvedRefs, unsupported values
	for p in plan.parents.iter_mut() {
		let ri = world.http_routes.iter().position(|r| {
			r.metadata.namespace.as_deref() == Some(p.namespace.as_str()) && r.metadata.name.as_deref() == Some(p.name.as_str())
		});
		let (resolved, unsupported) = ri.and_then(|ri| route_results.get(&ri).cloned()).unwrap_or_else(|| {
			// not attached anywhere: still check its references
			let resolved = ri.and_then(|ri| {
				let ctx = http::Ctx { world, scheme: "http", port: 80 };
				http::build(&ctx, &world.http_routes[ri], None, &[]).resolved
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

	plan.listeners = listeners
		.iter()
		.map(|st| ListenerPlan {
			name: st.l.name.clone(),
			supported_kinds: st.supported_kinds.clone(),
			attached: st.attached,
			conds: st.conds.clone(),
			rule_key: st.accepted.then(|| rp::rule_key(rp::Protocol::Tcp, &opts.listen_addrs[0], st.port)),
		})
		.collect();
	let any = listeners.iter().any(|l| l.accepted);
	plan.conds.push(if any || listeners.is_empty() {
		Cond::ok("Accepted", "Accepted")
	} else {
		Cond::new("Accepted", false, "ListenersNotValid", "no listener is valid")
	});
	if opts.labels {
		for r in &mut plan.rules {
			r.labels.insert("gateway.networking.k8s.io/gateway-name".into(), gw_name.clone());
			r.labels.insert("gateway.networking.k8s.io/gateway-namespace".into(), gw_ns.clone());
			r.labels.insert("app.kubernetes.io/managed-by".into(), "rproxy-gateway".into());
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
