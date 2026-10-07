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
	/// What the Gateway's rproxy pods take.
	pub features: Features,
}

impl Default for Options {
	fn default() -> Self {
		Options {
			listen_addrs: vec!["0.0.0.0".into()],
			cert_dir: "/var/run/rproxy-gateway/certs".into(),
			labels: true,
			migration: None,
			features: Features::default(),
		}
	}
}

/// The newer rproxy settings Gateway API needs (rproxy-api docs/API.md, "Gateway
/// API 向けの L7・TLS"), from `GET /capabilities` `features`. A setting is used
/// only when every pod of the Gateway takes it; a route that needs a missing one
/// is not accepted (`UnsupportedValue`, naming what is missing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Features {
	pub headers_add: bool,
	pub redirect_status: bool,
	pub route_timeouts: bool,
	pub server_middlewares: bool,
	pub retry_status: bool,
	pub server_status: bool,
	pub cors: bool,
	pub mirror: bool,
	pub replace_host: bool,
	pub service_protocol: bool,
	pub service_tls: bool,
	pub tls_route_targets: bool,
}

impl Default for Features {
	/// Everything (pods not asked yet; rproxy v0.4.0 has all of them).
	fn default() -> Self {
		Features {
			headers_add: true,
			redirect_status: true,
			route_timeouts: true,
			server_middlewares: true,
			retry_status: true,
			server_status: true,
			cors: true,
			mirror: true,
			replace_host: true,
			service_protocol: true,
			service_tls: true,
			tls_route_targets: true,
		}
	}
}

impl Features {
	pub fn of(c: &rp::Capabilities) -> Features {
		let opt = |n: &str| c.lists("http_options", n);
		Features {
			headers_add: opt("headers_add"),
			redirect_status: opt("redirect_status"),
			route_timeouts: opt("route_timeouts"),
			server_middlewares: opt("server_middlewares"),
			retry_status: opt("retry_status"),
			server_status: opt("server_status"),
			cors: c.lists("middlewares", "cors"),
			mirror: c.lists("middlewares", "mirror"),
			replace_host: c.lists("middlewares", "replace_host"),
			service_protocol: c.lists("services", "protocol"),
			service_tls: c.lists("services", "tls"),
			tls_route_targets: c.feature("tls_route_targets"),
		}
	}

	/// What both take.
	pub fn and(&self, o: &Features) -> Features {
		Features {
			headers_add: self.headers_add && o.headers_add,
			redirect_status: self.redirect_status && o.redirect_status,
			route_timeouts: self.route_timeouts && o.route_timeouts,
			server_middlewares: self.server_middlewares && o.server_middlewares,
			retry_status: self.retry_status && o.retry_status,
			server_status: self.server_status && o.server_status,
			cors: self.cors && o.cors,
			mirror: self.mirror && o.mirror,
			replace_host: self.replace_host && o.replace_host,
			service_protocol: self.service_protocol && o.service_protocol,
			service_tls: self.service_tls && o.service_tls,
			tls_route_targets: self.tls_route_targets && o.tls_route_targets,
		}
	}
}

/// `Err` naming the rproxy feature a setting needs when the pods lack it.
pub fn needs(have: bool, what: &str, feature: &str) -> Result<(), String> {
	if have { Ok(()) } else { Err(format!("{what} needs a newer rproxy (GET /capabilities: {feature})")) }
}

/// The kinds of route.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum RouteKind {
	Http,
	Grpc,
	Tls,
	Tcp,
	Udp,
}

impl RouteKind {
	pub fn kind(self) -> &'static str {
		match self {
			RouteKind::Http => "HTTPRoute",
			RouteKind::Grpc => "GRPCRoute",
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

/// A ListenerSet's status (for the Gateway it names).
#[derive(Clone, Debug)]
pub struct ListenerSetPlan {
	pub namespace: String,
	pub name: String,
	pub generation: i64,
	/// `Accepted` (`Programmed` comes from rproxy's answer).
	pub accepted: Cond,
	pub listeners: Vec<ListenerPlan>,
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
	/// `spec.addresses`: the IP addresses asked for (empty: whatever the Service gets).
	pub addresses: Vec<String>,
	/// Why an address of `spec.addresses` cannot be used (`Programmed: False`, `AddressNotUsable`).
	pub address_error: Option<String>,
	/// The ListenerSets naming this Gateway (attached or not).
	pub listener_sets: Vec<ListenerSetPlan>,
	/// `status.attachedListenerSets`.
	pub attached_listener_sets: i32,
}

impl GatewayPlan {
	/// Whether the Gateway is accepted (else nothing is deployed or applied for it).
	pub fn accepted(&self) -> bool {
		status::get(&self.conds, "Accepted").is_none_or(|c| c.status)
	}
}

/// Whether an IP address can be a Gateway's address: a unicast address that
/// is not unspecified, loopback or link-local (Kubernetes refuses those as a
/// Service's `externalIPs` too).
pub fn usable_ip(ip: std::net::IpAddr) -> bool {
	match ip {
		std::net::IpAddr::V4(v4) => {
			!(v4.is_unspecified() || v4.is_loopback() || v4.is_link_local() || v4.is_multicast() || v4.is_broadcast())
		}
		std::net::IpAddr::V6(v6) => !(v6.is_unspecified() || v6.is_loopback() || v6.is_multicast() || v6.is_unicast_link_local()),
	}
}

/// `spec.addresses` → (the IPs asked for, why one cannot be used); `Err`: an
/// address type that is not supported (`Accepted: False`, `UnsupportedAddress`).
pub fn gateway_addresses(gw: &Gateway) -> Result<(Vec<String>, Option<String>), String> {
	let mut ips = vec![];
	let mut unusable = None;
	for a in &gw.spec.addresses {
		let kind = a.kind.as_deref().unwrap_or("IPAddress");
		if kind != "IPAddress" {
			return Err(format!("address type {kind} is not supported (IPAddress only)"));
		}
		if a.value.is_empty() {
			// an address of this type, any value: the Service's
			continue;
		}
		match a.value.parse::<std::net::IpAddr>() {
			Ok(ip) if usable_ip(ip) => ips.push(ip.to_string()),
			Ok(_) => {
				unusable.get_or_insert(format!("{} cannot be used (unspecified, loopback, link-local or multicast)", a.value));
			}
			Err(_) => return Err(format!("{:?} is not an IP address", a.value)),
		}
	}
	Ok((ips, unusable))
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
			Family::Http | Family::Https => &["HTTPRoute", "GRPCRoute"],
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

/// Whose listener: the Gateway's own, or a ListenerSet's (index into the Gateway's attached sets).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Owner {
	Gateway,
	Set(usize),
}

struct ListenerState<'a> {
	l: &'a Listener,
	owner: Owner,
	/// The owner's namespace (references and `allowedRoutes` `Same` are relative to it).
	ns: String,
	/// A name unique across the Gateway and its ListenerSets (rproxy route names).
	label: String,
	port: u16,
	family: Option<Family>,
	conds: Vec<Cond>,
	supported_kinds: Vec<RouteGroupKind>,
	/// Certificate files (cert, key) for HTTPS and terminating TLS listeners.
	certs: Vec<(String, String)>,
	/// The CA file client certificates are validated against (frontend validation).
	client_ca: Option<String>,
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

/// Whether a ListenerSet names the Gateway `gw_ns/gw_name` as its parent.
fn set_refers_to(ls: &crate::k8s::gateway::ListenerSet, gw_ns: &str, gw_name: &str) -> bool {
	let p = &ls.spec.parent_ref;
	let ls_ns = ls.metadata.namespace.as_deref().unwrap_or_default();
	p.group.as_deref().unwrap_or(GROUP) == GROUP
		&& p.kind.as_deref().unwrap_or("Gateway") == "Gateway"
		&& p.namespace.as_deref().unwrap_or(ls_ns) == gw_ns
		&& p.name == gw_name
}

/// Whether the Gateway's `allowedListeners` lets a ListenerSet in `ns` attach (none by default).
fn set_allowed(world: &World, gw: &Gateway, ns: &str) -> bool {
	let n = gw.spec.allowed_listeners.as_ref().and_then(|a| a.namespaces.as_ref());
	match n.and_then(|n| n.from.as_deref()).unwrap_or("None") {
		"All" => true,
		"Same" => gw.metadata.namespace.as_deref() == Some(ns),
		"Selector" => n.and_then(|n| n.selector.as_ref()).is_some_and(|s| world.namespace_matches(ns, s)),
		_ => false,
	}
}

/// What a route's parent reference names: the Gateway, one of its attached ListenerSets, or neither.
fn parent_owner(
	p: &ParentReference,
	route_ns: &str,
	gw_ns: &str,
	gw_name: &str,
	sets: &[&crate::k8s::gateway::ListenerSet],
) -> Option<Owner> {
	if refers_to(p, route_ns, gw_ns, gw_name) {
		return Some(Owner::Gateway);
	}
	if p.group.as_deref().unwrap_or(GROUP) != GROUP || p.kind.as_deref() != Some("ListenerSet") {
		return None;
	}
	let ns = p.namespace.as_deref().unwrap_or(route_ns);
	sets.iter()
		.position(|ls| ls.metadata.namespace.as_deref() == Some(ns) && ls.metadata.name.as_deref() == Some(p.name.as_str()))
		.map(Owner::Set)
}

/// A certificate Secret → files, or why not.
fn certificate(
	world: &World,
	from: (&str, &str),
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
	let (from_kind, from_ns) = from;
	let ns = r.namespace.as_deref().unwrap_or(from_ns);
	if !world.granted((GROUP, from_kind, from_ns), ("", "Secret", ns, &r.name)) {
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

/// Where a listener comes from: its owner, the owner's kind and namespace (for
/// ReferenceGrants), and its unique label.
struct Origin {
	owner: Owner,
	kind: &'static str,
	ns: String,
	label: String,
}

fn validate_listener<'a>(
	world: &World,
	gw: &Gateway,
	l: &'a Listener,
	origin: Origin,
	files: &mut BTreeMap<String, Vec<u8>>,
	opts: &Options,
) -> ListenerState<'a> {
	let gw_ns = gw.metadata.namespace.as_deref().unwrap_or_default();
	let from = (origin.kind, origin.ns.clone());
	let mut st = ListenerState {
		l,
		owner: origin.owner,
		ns: origin.ns,
		label: origin.label,
		port: u16::try_from(l.port).unwrap_or(0),
		family: None,
		conds: vec![],
		supported_kinds: vec![],
		certs: vec![],
		client_ca: None,
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
			match certificate(world, (from.0, &from.1), r, files, opts) {
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
		// client certificates (spec.tls.frontend: the port's configuration, else the default)
		if let Some(v) = frontend_validation(gw, st.port) {
			if v.mode.as_deref() != Some("AllowInsecureFallback") {
				match client_ca(world, gw_ns, v, files, opts) {
					(Some(ca), problem) => {
						st.client_ca = Some(ca);
						if let Some(p) = problem {
							if resolved.status {
								resolved = p;
							}
						}
					}
					(None, problem) => {
						let p = problem.unwrap_or_else(|| Cond::new("ResolvedRefs", false, "InvalidCACertificateRef", "no CA certificate"));
						st.accepted = false;
						st.conds.push(Cond::new(
							"Accepted",
							false,
							"NoValidCACertificate",
							format!("no usable CA certificate: {}", p.message),
						));
						if resolved.status {
							resolved = p;
						}
					}
				}
			}
		}
	}
	if st.conds.iter().all(|c| c.kind != "Accepted") {
		st.conds.push(Cond::ok("Accepted", "Accepted"));
	}
	st.conds.push(resolved);
	st
}

/// The frontend validation of a port: `spec.tls.frontend.perPort`, else its `default`.
fn frontend_validation(gw: &Gateway, port: u16) -> Option<&crate::k8s::gateway::FrontendTlsValidation> {
	let f = gw.spec.tls.as_ref()?.frontend.as_ref()?;
	match f.per_port.iter().find(|p| p.port == i32::from(port)) {
		Some(p) => p.tls.validation.as_ref(),
		None => f.default.validation.as_ref(),
	}
}

/// The CA bundle of `caCertificateRefs` as a file (the usable ones), and the
/// first reference that is not usable (`ResolvedRefs: False`).
fn client_ca(
	world: &World,
	gw_ns: &str,
	v: &crate::k8s::gateway::FrontendTlsValidation,
	files: &mut BTreeMap<String, Vec<u8>>,
	opts: &Options,
) -> (Option<String>, Option<Cond>) {
	let mut bundle: Vec<u8> = vec![];
	let mut problem: Option<Cond> = None;
	let bad = |reason: &str, message: String| Cond::new("ResolvedRefs", false, reason, message);
	for r in &v.ca_certificate_refs {
		let group = r.group.as_deref().unwrap_or("");
		let kind = r.kind.as_deref().unwrap_or("ConfigMap");
		let ns = r.namespace.as_deref().unwrap_or(gw_ns);
		let p = if !group.is_empty() || kind != "ConfigMap" {
			Some(bad("InvalidCACertificateKind", format!("caCertificateRef {group}/{kind} {}: only ConfigMaps are supported", r.name)))
		} else if !world.granted((GROUP, "Gateway", gw_ns), ("", "ConfigMap", ns, &r.name)) {
			Some(bad("RefNotPermitted", format!("ConfigMap {ns}/{}: no ReferenceGrant allows the reference", r.name)))
		} else {
			match world.config_maps.get(&(ns.to_string(), r.name.clone())).and_then(|c| c.data.as_ref()).and_then(|d| d.get("ca.crt")) {
				None => Some(bad("InvalidCACertificateRef", format!("ConfigMap {ns}/{} not found or without ca.crt", r.name))),
				Some(pem) => match crate::pem::check_certs(pem.as_bytes()) {
					Ok(_) => {
						bundle.extend_from_slice(pem.trim_end().as_bytes());
						bundle.push(b'\n');
						None
					}
					Err(e) => Some(bad("InvalidCACertificateRef", format!("ConfigMap {ns}/{} ca.crt: {e}", r.name))),
				},
			}
		};
		if problem.is_none() {
			problem = p;
		}
	}
	if bundle.is_empty() {
		return (None, problem);
	}
	(Some(ca_file(&bundle, files, opts)), problem)
}

/// Adds a CA bundle to `files` (named by its hash); returns its path.
pub fn ca_file(pem: &[u8], files: &mut BTreeMap<String, Vec<u8>>, opts: &Options) -> String {
	let name = format!("{}.ca.crt", crate::pem::short_hash(pem));
	files.insert(name.clone(), pem.to_vec());
	format!("{}/{name}", opts.cert_dir)
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
fn allowed(world: &World, st: &ListenerState, kind: RouteKind, route_ns: &str) -> Result<(), &'static str> {
	if !st.supported_kinds.iter().any(|k| k.kind == kind.kind()) {
		return Err("NotAllowedByListeners");
	}
	let ns = st.l.allowed_routes.as_ref().and_then(|a| a.namespaces.as_ref());
	let ok = match ns.and_then(|n| n.from.as_deref()).unwrap_or("Same") {
		"All" => true,
		"Selector" => ns.and_then(|n| n.selector.as_ref()).is_some_and(|s| world.namespace_matches(route_ns, s)),
		_ => route_ns == st.ns,
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
	for (kind, list) in [(RouteKind::Http, &world.http_routes), (RouteKind::Grpc, &world.grpc_routes)] {
		for (i, r) in list.iter().enumerate() {
			out.push(RouteInfo { kind, index: i, meta: &r.metadata, parent_refs: &r.spec.parent_refs, hostnames: &r.spec.hostnames });
		}
	}
	for (kind, list) in [(RouteKind::Tls, &world.tls_routes), (RouteKind::Tcp, &world.tcp_routes), (RouteKind::Udp, &world.udp_routes)] {
		for (i, r) in list.iter().enumerate() {
			out.push(RouteInfo { kind, index: i, meta: &r.metadata, parent_refs: &r.spec.parent_refs, hostnames: &r.spec.hostnames });
		}
	}
	out
}

/// An HTTPRoute, or a GRPCRoute as the HTTPRoute it amounts to.
fn http_route(world: &World, kind: RouteKind, index: usize) -> &HttpRoute {
	if kind == RouteKind::Grpc { &world.grpc_routes[index] } else { &world.http_routes[index] }
}

/// What `http::build` needs for a route of `kind`.
fn http_ctx<'a>(world: &'a World, opts: &'a Options, scheme: &'static str, port: u16, kind: RouteKind) -> http::Ctx<'a> {
	http::Ctx { world, scheme, port, features: &opts.features, kind: kind.kind(), grpc: kind == RouteKind::Grpc }
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
	// ListenerSets naming this Gateway, in precedence order (oldest, then namespace/name)
	let mut named: Vec<&crate::k8s::gateway::ListenerSet> =
		world.listener_sets.iter().filter(|ls| set_refers_to(ls, &gw_ns, &gw_name)).collect();
	named.sort_by_key(|ls| {
		(ls.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()), ls.metadata.namespace.clone(), ls.metadata.name.clone())
	});
	let (sets, refused): (Vec<_>, Vec<_>) =
		named.into_iter().partition(|ls| set_allowed(world, gw, ls.metadata.namespace.as_deref().unwrap_or_default()));
	for ls in refused {
		plan.listener_sets.push(ListenerSetPlan {
			namespace: ls.metadata.namespace.clone().unwrap_or_default(),
			name: ls.metadata.name.clone().unwrap_or_default(),
			generation: ls.metadata.generation.unwrap_or(0),
			accepted: Cond::new("Accepted", false, "NotAllowed", "the Gateway's allowedListeners does not allow this ListenerSet"),
			listeners: vec![],
		});
	}
	// the Gateway's listeners first, then the ListenerSets' (earlier ones win conflicts)
	let mut listeners: Vec<ListenerState> = gw
		.spec
		.listeners
		.iter()
		.map(|l| {
			let origin = Origin { owner: Owner::Gateway, kind: "Gateway", ns: gw_ns.clone(), label: l.name.clone() };
			validate_listener(world, gw, l, origin, &mut files, opts)
		})
		.collect();
	for (si, ls) in sets.iter().enumerate() {
		let ls_ns = ls.metadata.namespace.clone().unwrap_or_default();
		let ls_name = ls.metadata.name.clone().unwrap_or_default();
		for l in &ls.spec.listeners {
			let origin = Origin { owner: Owner::Set(si), kind: "ListenerSet", ns: ls_ns.clone(), label: format!("{ls_name}/{}", l.name) };
			listeners.push(validate_listener(world, gw, l, origin, &mut files, opts));
		}
	}
	conflicts(&mut listeners);

	// attach routes
	let mut attachments: Vec<Attachment> = vec![];
	// the owner each entry of plan.parents names
	let mut owners: Vec<Owner> = vec![];
	for r in routes(world) {
		let rns = r.meta.namespace.clone().unwrap_or_default();
		for p in r.parent_refs {
			let Some(owner) = parent_owner(p, &rns, &gw_ns, &gw_name, &sets) else { continue };
			let accepted = attach(world, &mut listeners, owner, p, r.kind, &rns, r.hostnames, |li, hosts| {
				attachments.push(Attachment { kind: r.kind, route: r.index, listener: li, hosts })
			});
			owners.push(owner);
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
		// client certificates: the port's CA (validation is per port)
		let client_auth =
			members.iter().find_map(|i| listeners[*i].client_ca.clone()).map(|ca| serde_json::json!({"mode": "required", "ca_file": ca}));
		// TLS routes by server name (passthrough, or terminated on a TLS listener)
		let mut tls_routes: Vec<rp::TlsRoute> = vec![];
		for a in attachments.iter().filter(|a| a.kind == RouteKind::Tls && mine(a)) {
			let route = l4_route(world, RouteKind::Tls, a.route);
			// every pod of every backend (weights spread over them) where rproxy takes several
			// destinations per name (`tls_route_targets`); else the Service's ClusterIP
			let (targets, resolved) = if opts.features.tls_route_targets {
				let out = l4::targets(world, "TLSRoute", route);
				(out.targets, out.resolved)
			} else {
				let (dest, resolved) = l4::destination(world, route);
				(dest.map(|d| vec![rp::Target { addr: d.addr, port: d.port, weight: None }]).unwrap_or_default(), resolved)
			};
			let entry = results.entry((RouteKind::Tls, a.route)).or_insert((None, None));
			if entry.0.is_none() {
				entry.0 = resolved;
			}
			let names: Vec<String> = a.hosts.clone().unwrap_or_default().iter().map(|h| hostname::to_rproxy(h)).collect();
			if names.is_empty() {
				continue;
			}
			let passthrough =
				listeners[a.listener].family == Some(Family::TlsPassthrough) && (has(Family::Https) || has(Family::TlsTerminate));
			let mut tr = rp::TlsRoute { server_names: names, passthrough, ..Default::default() };
			match targets.as_slice() {
				// no usable backend: connections for its names are accepted and closed (Gateway API
				// expects a reset, not a refused connection), through a port nothing listens on
				[] => (tr.remote_addr, tr.remote_port) = ("127.0.0.1".into(), 1),
				[one] if one.weight.is_none() && !opts.features.tls_route_targets => {
					(tr.remote_addr, tr.remote_port) = (one.addr.clone(), one.port);
				}
				_ => tr.targets = targets,
			}
			tls_routes.push(tr);
			carried.push((RouteKind::Tls, a.route, a.listener));
		}
		if has(Family::Http) || has(Family::Https) {
			let https = has(Family::Https);
			let mut entries = vec![];
			let mut services = BTreeMap::new();
			let mut middlewares = BTreeMap::new();
			for a in attachments.iter().filter(|a| matches!(a.kind, RouteKind::Http | RouteKind::Grpc) && mine(a)) {
				let ctx = http_ctx(world, opts, if https { "https" } else { "http" }, *port, a.kind);
				let route: &HttpRoute = http_route(world, a.kind, a.route);
				let mut out = http::build(&ctx, route, a.hosts.as_deref(), &exclusions(&listeners, a.listener));
				if a.kind == RouteKind::Grpc {
					// names apart from an HTTPRoute of the same namespace and name
					out.prefix_names("grpc:");
				}
				let several = attachments.iter().filter(|b| b.kind == a.kind && b.route == a.route && mine(b)).count() > 1;
				if several {
					// one rproxy route per listener: names stay unique
					for e in &mut out.entries {
						e.route.name = format!("{}@{}", e.route.name, listeners[a.listener].label);
					}
				}
				let entry = results.entry((a.kind, a.route)).or_insert((None, None));
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
				carried.push((a.kind, a.route, a.listener));
			}
			let routes = http::assign_priorities(&mut entries);
			rule.http = Some(rp::Http { routes, default: None, services, middlewares });
			if https {
				let passthrough: Vec<rp::TlsRoute> = tls_routes.into_iter().filter(|r| r.passthrough).collect();
				rule.tls = Some(rp::Tls { mode: "terminate", certificates, routes: passthrough, client_auth, ..Default::default() });
			}
		} else if has(Family::TlsPassthrough) || has(Family::TlsTerminate) {
			if tls_routes.is_empty() {
				// nothing to send to: no rule (the port stays closed)
				continue;
			}
			let first = &tls_routes[0];
			rule.targets = if first.targets.is_empty() {
				vec![rp::Target { addr: first.remote_addr.clone(), port: first.remote_port, weight: None }]
			} else {
				first.targets.clone()
			};
			let terminate = has(Family::TlsTerminate);
			rule.tls = Some(rp::Tls {
				mode: if terminate { "terminate" } else { "sni" },
				routes: tls_routes,
				certificates: if terminate { certificates } else { vec![] },
				client_auth: if terminate { client_auth } else { None },
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
				RouteKind::Http | RouteKind::Grpc => &http_route(world, kind, ri).metadata,
				k => &l4_route(world, k, ri).metadata,
			};
			for (p, owner) in plan.parents.iter_mut().zip(&owners).filter(|(p, _)| {
				p.kind == kind && Some(&p.namespace) == route_meta.namespace.as_ref() && Some(&p.name) == route_meta.name.as_ref()
			}) {
				if listeners_for(&listeners, *owner, &p.parent_ref).contains(&li) {
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
			RouteKind::Http | RouteKind::Grpc => {
				let list = if p.kind == RouteKind::Grpc { &world.grpc_routes } else { &world.http_routes };
				list.iter().position(|r| {
					r.metadata.namespace.as_deref() == Some(p.namespace.as_str()) && r.metadata.name.as_deref() == Some(p.name.as_str())
				})
			}
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
				RouteKind::Http | RouteKind::Grpc => {
					let ctx = http_ctx(world, opts, "http", 80, p.kind);
					http::build(&ctx, http_route(world, p.kind, i), None, &[]).resolved
				}
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
	let by_name: BTreeMap<String, String> = listener_keys
		.iter()
		.filter(|(i, _)| listeners[**i].owner == Owner::Gateway)
		.map(|(i, k)| (listeners[*i].l.name.clone(), k.clone()))
		.collect();
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

	let listener_plan = |(i, st): (usize, &ListenerState)| ListenerPlan {
		name: st.l.name.clone(),
		supported_kinds: st.supported_kinds.clone(),
		attached: st.attached,
		conds: st.conds.clone(),
		rule_key: listener_keys.get(&i).cloned(),
		servable: st.accepted && !st.unservable,
	};
	plan.listeners = listeners.iter().enumerate().filter(|(_, st)| st.owner == Owner::Gateway).map(listener_plan).collect();
	for (si, ls) in sets.iter().enumerate() {
		let mine: Vec<ListenerPlan> =
			listeners.iter().enumerate().filter(|(_, st)| st.owner == Owner::Set(si)).map(listener_plan).collect();
		// accepted with at least one valid listener
		let accepted = if mine.iter().any(|l| status::get(&l.conds, "Accepted").is_some_and(|c| c.status)) {
			Cond::ok("Accepted", "Accepted")
		} else {
			Cond::new("Accepted", false, "ListenersNotValid", "no listener of the ListenerSet is valid")
		};
		plan.listener_sets.push(ListenerSetPlan {
			namespace: ls.metadata.namespace.clone().unwrap_or_default(),
			name: ls.metadata.name.clone().unwrap_or_default(),
			generation: ls.metadata.generation.unwrap_or(0),
			accepted,
			listeners: mine,
		});
	}
	// Service ports: every listener that can be served, with or without a rule yet
	plan.listener_ports =
		listeners.iter().filter(|l| l.accepted && !l.unservable).filter_map(|l| l.family.map(|f| (f.protocol(), l.port))).collect();
	let own: Vec<&ListenerState> = listeners.iter().filter(|l| l.owner == Owner::Gateway).collect();
	let any = own.iter().any(|l| l.accepted);
	let all = own.iter().all(|l| l.accepted);
	let addresses = gateway_addresses(gw);
	let params = gw.spec.infrastructure.as_ref().and_then(|i| i.parameters_ref.as_ref());
	plan.conds.push(if let Some(p) = params {
		// no parameters kind is supported
		Cond::new("Accepted", false, "InvalidParameters", format!("parametersRef {}/{} {}: not supported", p.group, p.kind, p.name))
	} else if let Err(e) = &addresses {
		Cond::new("Accepted", false, "UnsupportedAddress", e.clone())
	} else if own.is_empty() || all {
		Cond::ok("Accepted", "Accepted")
	} else if any {
		Cond::new("Accepted", true, "ListenersNotValid", "some listeners are not valid")
	} else {
		Cond::new("Accepted", false, "ListenersNotValid", "no listener is valid")
	});
	if let Ok((ips, unusable)) = addresses {
		plan.addresses = ips;
		plan.address_error = unusable;
	}
	if !plan.accepted() {
		for ls in plan.listener_sets.iter_mut().filter(|ls| ls.accepted.status) {
			ls.accepted = Cond::new("Accepted", false, "ParentNotAccepted", "the Gateway is not accepted");
		}
	}
	plan.attached_listener_sets = plan.listener_sets.iter().filter(|ls| ls.accepted.status).count() as i32;
	// client certificates not enforced somewhere (rproxy does not ask for them then)
	let frontend = gw.spec.tls.as_ref().and_then(|t| t.frontend.as_ref());
	let insecure = frontend.is_some_and(|f| {
		f.default
			.validation
			.iter()
			.chain(f.per_port.iter().filter_map(|p| p.tls.validation.as_ref()))
			.any(|v| v.mode.as_deref() == Some("AllowInsecureFallback"))
	});
	if insecure {
		plan.conds.push(Cond::new(
			"InsecureFrontendValidationMode",
			true,
			"ConfigurationChanged",
			"AllowInsecureFallback: connections without a valid client certificate are accepted",
		));
	}
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
fn listeners_for(listeners: &[ListenerState], owner: Owner, p: &ParentReference) -> Vec<usize> {
	listeners
		.iter()
		.enumerate()
		.filter(|(_, st)| st.owner == owner)
		.filter(|(_, st)| p.section_name.as_deref().is_none_or(|s| s == st.l.name) && p.port.is_none_or(|port| i32::from(st.port) == port))
		.map(|(i, _)| i)
		.collect()
}

/// Attaches a route through one parent reference; returns its `Accepted`.
#[allow(clippy::too_many_arguments)]
fn attach(
	world: &World,
	listeners: &mut [ListenerState],
	owner: Owner,
	p: &ParentReference,
	kind: RouteKind,
	route_ns: &str,
	hostnames: &[String],
	mut add: impl FnMut(usize, Option<Vec<String>>),
) -> Cond {
	let candidates = listeners_for(listeners, owner, p);
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
		if let Err(r) = allowed(world, st, kind, route_ns) {
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
