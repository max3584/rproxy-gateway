//! Migration readers (rproxy-api docs/DESIGN-v0.4.md 3.3, optional): Ingress
//! (`ingressClassName`, default `rproxy`) and Traefik's IngressRoute,
//! IngressRouteTCP, IngressRouteUDP, Middleware and TLSOption → rules added to
//! the rule set of one Gateway (`--migrate-to <namespace>/<name>`). The mapping
//! follows rproxy-api's `contrib/traefik2rproxy.py`
//! (docs/MIGRATING-FROM-TRAEFIK.md). Read only: nothing is written back; what
//! cannot be converted is left out with a note (the controller logs them,
//! `rproxy-gateway render` prints them).
//!
//! Traefik entry points are given as `name=port[/udp]` (default `web=80`,
//! `websecure=443`); an Ingress is served on `web`, and also on `websecure`
//! when it has `tls`.

use std::collections::BTreeMap;

use k8s_openapi::api::networking::v1::Ingress;
use serde_json::{Value, json};

use crate::k8s::gateway::BackendRef;
use crate::k8s::traefik::{IngressRoute, IngressRouteTcp, IngressRouteUdp, Middleware, RouteTls, ServiceRef, TlsOption};
use crate::render::backends::{self, Endpoint};
use crate::render::traefik_mw::{self, Converted, as_bool, as_list, g};
use crate::render::world::{Key, World};
use crate::render::{Options, cert_files};
use crate::rproxy::model as rp;

/// The resources read for migration (watched only when migration is on).
#[derive(Clone, Debug, Default)]
pub struct MigrationInput {
	pub ingresses: Vec<Ingress>,
	pub ingress_routes: Vec<IngressRoute>,
	pub ingress_routes_tcp: Vec<IngressRouteTcp>,
	pub ingress_routes_udp: Vec<IngressRouteUdp>,
	pub middlewares: BTreeMap<Key, Middleware>,
	pub tls_options: BTreeMap<Key, TlsOption>,
}

impl MigrationInput {
	pub fn is_empty(&self) -> bool {
		self.ingresses.is_empty()
			&& self.ingress_routes.is_empty()
			&& self.ingress_routes_tcp.is_empty()
			&& self.ingress_routes_udp.is_empty()
	}
}

/// Where migrated rules go.
#[derive(Clone, Debug)]
pub struct Settings {
	/// The Gateway whose rule set gets them.
	pub gateway: Key,
	/// Traefik entry points: name → (protocol, port).
	pub entry_points: BTreeMap<String, (rp::Protocol, u16)>,
	/// Ingresses of this class are read.
	pub ingress_class: String,
	/// References to other namespaces (services, middlewares, TLS options) without a
	/// ReferenceGrant (Traefik's `allowCrossNamespace`; off by default, as in Traefik).
	pub allow_cross_namespace: bool,
}

impl Settings {
	/// `name=port[/udp]` entries.
	pub fn parse_entry_points(list: &[String]) -> Result<BTreeMap<String, (rp::Protocol, u16)>, String> {
		let mut out = BTreeMap::new();
		for e in list {
			let (name, rest) = e.split_once('=').ok_or_else(|| format!("entry point {e:?}: name=port[/udp]"))?;
			let (port, proto) = match rest.split_once('/') {
				Some((p, "udp")) => (p, rp::Protocol::Udp),
				Some((p, "tcp")) | Some((p, "")) => (p, rp::Protocol::Tcp),
				None => (rest, rp::Protocol::Tcp),
				Some(_) => return Err(format!("entry point {e:?}: tcp or udp")),
			};
			let port: u16 = port.parse().map_err(|_| format!("entry point {e:?}: port"))?;
			out.insert(name.trim().to_string(), (proto, port));
		}
		Ok(out)
	}

	pub fn default_entry_points() -> BTreeMap<String, (rp::Protocol, u16)> {
		[("web".to_string(), (rp::Protocol::Tcp, 80)), ("websecure".to_string(), (rp::Protocol::Tcp, 443))].into()
	}
}

/// What migrated resources add to one (protocol, port).
#[derive(Clone, Debug, Default)]
pub struct PortAdd {
	pub routes: Vec<rp::HttpRoute>,
	/// Routes of TLS routers (served when the port terminates TLS).
	pub tls_routes: Vec<rp::HttpRoute>,
	pub services: BTreeMap<String, rp::Service>,
	pub middlewares: BTreeMap<String, Value>,
	pub default: Option<rp::HttpDefault>,
	pub certificates: Vec<rp::Certificate>,
	pub tls_options: Option<Value>,
	pub client_auth: Option<Value>,
	pub alpn: Option<Vec<String>>,
	pub tcp: Vec<TcpEntry>,
	pub udp: Vec<rp::Target>,
}

/// One IngressRouteTCP route on a port.
#[derive(Clone, Debug)]
pub struct TcpEntry {
	pub name: String,
	/// Server names (rproxy syntax); empty for `HostSNI(*)`.
	pub names: Vec<String>,
	pub targets: Vec<rp::Target>,
	/// One address for a `tls.routes` entry (the first Service's ClusterIP).
	pub dest: Option<Endpoint>,
	/// `None`: no TLS; `Some(true)`: passthrough; `Some(false)`: terminate.
	pub passthrough: Option<bool>,
	pub proxy: Option<i64>,
}

/// Everything migration adds.
#[derive(Clone, Debug, Default)]
pub struct Out {
	pub ports: BTreeMap<(rp::Protocol, u16), PortAdd>,
	pub files: BTreeMap<String, Vec<u8>>,
	pub notes: Vec<String>,
}

struct Ctx<'a> {
	world: &'a World,
	opts: &'a Options,
	out: Out,
	/// The port (rule) being filled, for middlewares that add services.
	port: (rp::Protocol, u16),
	entry_points: BTreeMap<String, (rp::Protocol, u16)>,
	allow_cross_namespace: bool,
}

/// Whether a ReferenceGrant in `to_ns` lets Traefik objects of `from_ns` refer to (`to_group`, `to_kind`, `name`).
fn traefik_granted(world: &World, from_ns: &str, to: (&str, &str, &str, &str)) -> bool {
	let (to_group, to_kind, to_ns, name) = to;
	world.grants.iter().filter(|g| g.metadata.namespace.as_deref() == Some(to_ns)).any(|g| {
		g.spec.from.iter().any(|f| {
			crate::k8s::traefik::GROUPS.contains(&f.group.as_str())
				&& matches!(f.kind.as_str(), "IngressRoute" | "IngressRouteTCP" | "IngressRouteUDP" | "Middleware")
				&& f.namespace == from_ns
		}) && g.spec.to.iter().any(|t| t.group == to_group && t.kind == to_kind && t.name.as_deref().is_none_or(|n| n == name))
	})
}

impl Ctx<'_> {
	/// Whether an object in `from_ns` may refer to one in `to_ns` (same namespace, a
	/// ReferenceGrant, or `allow_cross_namespace`); notes a refusal.
	fn cross(&mut self, from_ns: &str, to: (&str, &str, &str, &str)) -> bool {
		let (_, kind, to_ns, name) = to;
		if from_ns == to_ns || self.allow_cross_namespace || traefik_granted(self.world, from_ns, to) {
			return true;
		}
		self.note(format!(
			"{kind} {to_ns}/{name}: referred to from namespace {from_ns} without a ReferenceGrant (or --migration-allow-cross-namespace); left out"
		));
		false
	}

	fn note(&mut self, s: String) {
		if !self.out.notes.contains(&s) {
			self.out.notes.push(s);
		}
	}

	fn port(&mut self, key: (rp::Protocol, u16)) -> &mut PortAdd {
		self.out.ports.entry(key).or_default()
	}
}

impl traefik_mw::Sink for Ctx<'_> {
	fn note(&mut self, text: String) {
		Ctx::note(self, text)
	}

	fn users_file(&mut self, namespace: &str, secret: &str) -> Option<String> {
		let s = self.world.secrets.get(&(namespace.to_string(), secret.to_string()))?;
		let users = s.data.as_ref()?.get("users")?.0.clone();
		let name = format!("{}.htpasswd", crate::pem::short_hash(&users));
		self.out.files.insert(name.clone(), users);
		Some(format!("{}/{name}", self.opts.cert_dir))
	}

	fn errors_service(&mut self, namespace: &str, service: &Value) -> Option<String> {
		let r: ServiceRef = serde_json::from_value(service.clone()).ok()?;
		let (name, svc) = self.service(namespace, &r)?;
		let port = self.port;
		self.port(port).services.insert(name.clone(), svc);
		Some(name)
	}
}

impl Ctx<'_> {
	/// A Traefik service reference → an rproxy service (named `<ns>/<svc>:<port>`).
	fn service(&mut self, route_ns: &str, r: &ServiceRef) -> Option<(String, rp::Service)> {
		if r.kind.as_deref().is_some_and(|k| k != "Service") {
			self.note(format!("service {}: kind {} is not converted (Kubernetes Services only)", r.name, r.kind.as_deref().unwrap_or("")));
			return None;
		}
		let ns = r.namespace.clone().unwrap_or_else(|| route_ns.to_string());
		if !self.cross(route_ns, ("", "Service", &ns, &r.name)) {
			return None;
		}
		let (eps, port_label, https) = self.endpoints(&ns, r)?;
		let scheme = match r.scheme.as_deref() {
			Some(s) => s.to_string(),
			None if https => "https".into(),
			None => "http".into(),
		};
		let servers: Vec<rp::Server> =
			eps.iter().map(|e| rp::Server { url: format!("{scheme}://{}", e.authority()), ..Default::default() }).collect();
		if servers.is_empty() {
			self.note(format!("service {ns}/{}: no ready endpoints", r.name));
			return None;
		}
		let svc = rp::Service { servers, pass_host_header: r.pass_host_header.filter(|p| !p), ..Default::default() };
		Some((format!("{ns}/{}:{port_label}", r.name), svc))
	}

	/// Endpoints of a Service port given by number or name; (endpoints, port label, https?).
	fn endpoints(&mut self, ns: &str, r: &ServiceRef) -> Option<(Vec<Endpoint>, String, bool)> {
		let Some(svc) = self.world.services.get(&(ns.to_string(), r.name.clone())) else {
			self.note(format!("service {ns}/{} not found", r.name));
			return None;
		};
		let ports = svc.spec.as_ref().and_then(|s| s.ports.clone()).unwrap_or_default();
		let port = match &r.port {
			Some(Value::Number(n)) => n.as_i64().map(|n| n as i32),
			Some(Value::String(s)) => {
				s.parse::<i32>().ok().or_else(|| ports.iter().find(|p| p.name.as_deref() == Some(s.as_str())).map(|p| p.port))
			}
			_ => ports.first().map(|p| p.port),
		};
		let Some(port) = port else {
			self.note(format!("service {ns}/{}: port {:?} not found", r.name, r.port));
			return None;
		};
		let name = ports.iter().find(|p| p.port == port).and_then(|p| p.name.clone()).unwrap_or_default();
		let b = BackendRef { name: r.name.clone(), namespace: Some(ns.to_string()), port: Some(port), ..Default::default() };
		match backends::resolve(self.world, "IngressRoute", ns, &b) {
			Ok(eps) => Some((eps, port.to_string(), port == 443 || name.starts_with("https"))),
			Err(e) => {
				self.note(format!("service {ns}/{}: {}", r.name, e.message));
				None
			}
		}
	}

	/// Weighted targets of several Traefik service references (L4).
	fn targets(&mut self, route_ns: &str, refs: &[ServiceRef]) -> (Vec<rp::Target>, Option<Endpoint>) {
		let mut weighted = vec![];
		let mut dest = None;
		for r in refs {
			let ns = r.namespace.clone().unwrap_or_else(|| route_ns.to_string());
			if !self.cross(route_ns, ("", "Service", &ns, &r.name)) {
				continue;
			}
			if let Some((eps, port, _)) = self.endpoints(&ns, r) {
				if dest.is_none() {
					let b = BackendRef { name: r.name.clone(), namespace: Some(ns.clone()), port: port.parse().ok(), ..Default::default() };
					dest = backends::service_address(self.world, "IngressRouteTCP", &ns, &b).ok().flatten();
				}
				weighted.push((r.weight.unwrap_or(1).max(0) as u32, eps));
			}
		}
		let targets = backends::spread(&weighted).into_iter().map(|(e, w)| rp::Target { addr: e.addr, port: e.port, weight: w }).collect();
		(targets, dest)
	}

	/// A Middleware chain → names in `port`'s middlewares (converted once each).
	fn middleware_chain(&mut self, route_ns: &str, refs: &[(Option<String>, String)], depth: usize) -> Vec<String> {
		let mut out = vec![];
		for (ns, name) in refs {
			let ns = ns.clone().unwrap_or_else(|| route_ns.to_string());
			let name = name.split('@').next().unwrap_or(name).to_string();
			if !self.cross(route_ns, ("traefik.io", "Middleware", &ns, &name)) {
				continue;
			}
			let key = format!("traefik/{ns}/{name}");
			if self.out.ports.get(&self.port).is_some_and(|p| p.middlewares.contains_key(&key)) {
				out.push(key);
				continue;
			}
			let Some(mw) = self.world.migration.middlewares.get(&(ns.clone(), name.clone())).cloned() else {
				self.note(format!("middleware {ns}/{name} not found; left out"));
				continue;
			};
			match traefik_mw::convert(&ns, &name, &mw.spec, self) {
				Converted::One(v) => {
					let port = self.port;
					self.port(port).middlewares.insert(key.clone(), v);
					out.push(key);
				}
				Converted::Chain(inner) if depth < 5 => out.extend(self.middleware_chain(&ns, &inner, depth + 1)),
				_ => {}
			}
		}
		out
	}

	/// The TLS part of a router on a port: certificates, options.
	fn router_tls(&mut self, route_ns: &str, tls: &RouteTls, hosts: &[String], key: (rp::Protocol, u16)) {
		if let Some(secret) = &tls.secret_name {
			match self.secret_pair(route_ns, secret) {
				Some(c) => {
					let p = self.port(key);
					if !p.certificates.contains(&c) {
						p.certificates.push(c);
					}
				}
				None => self.note(format!("Secret {route_ns}/{secret}: not a TLS Secret (tls.crt, tls.key); left out")),
			}
		}
		if let Some(resolver) = &tls.cert_resolver {
			let mut domains: Vec<String> = tls.domains.iter().flat_map(|d| d.main.clone().into_iter().chain(d.sans.clone())).collect();
			if domains.is_empty() {
				domains = hosts.to_vec();
			}
			if domains.is_empty() {
				self.note(format!("certResolver {resolver}: no domain to name the certificate by; left out"));
			} else {
				let c = rp::Certificate { acme: Some(resolver.clone()), domains, ..Default::default() };
				let p = self.port(key);
				if !p.certificates.contains(&c) {
					p.certificates.push(c);
				}
				self.note(format!(
					"certResolver {resolver}: rproxy's settings file needs global.acme with this resolver (docs/ACME.md in rproxy-api)"
				));
			}
		}
		if let Some(o) = &tls.options {
			let ns = o.namespace.clone().unwrap_or_else(|| route_ns.to_string());
			let name = o.name.split('@').next().unwrap_or(&o.name).to_string();
			if !self.cross(route_ns, ("traefik.io", "TLSOption", &ns, &name)) {
				return;
			}
			match self.world.migration.tls_options.get(&(ns.clone(), name.clone())).cloned() {
				Some(opt) => self.tls_option(&ns, &name, &Value::Object(opt.spec), key),
				None if name == "default" => {}
				None => self.note(format!("TLSOption {ns}/{name} not found")),
			}
		}
	}

	fn secret_pair(&mut self, ns: &str, name: &str) -> Option<rp::Certificate> {
		let s = self.world.secrets.get(&(ns.to_string(), name.to_string()))?;
		let data = s.data.as_ref()?;
		let (crt, key) = (data.get("tls.crt")?, data.get("tls.key")?);
		crate::pem::check_pair(&crt.0, &key.0).ok()?;
		let (c, k) = cert_files(&crt.0, &key.0, &mut self.out.files, self.opts);
		Some(rp::Certificate { cert_file: Some(c), key_file: Some(k), ..Default::default() })
	}

	fn tls_option(&mut self, ns: &str, name: &str, spec: &Value, key: (rp::Protocol, u16)) {
		let label = format!("TLSOption {ns}/{name}");
		let mut options = serde_json::Map::new();
		match g(spec, &["minVersion"]).and_then(Value::as_str) {
			Some("VersionTLS12") => {
				options.insert("min_version".into(), json!("1.2"));
			}
			Some("VersionTLS13") => {
				options.insert("min_version".into(), json!("1.3"));
			}
			Some(v) => self.note(format!("{label}: minVersion {v}: rproxy supports TLS 1.2 and 1.3 only")),
			None => {}
		}
		let mut suites = vec![];
		for s in as_list(g(spec, &["cipherSuites"])) {
			match cipher(&s) {
				Some(r) => suites.push(r),
				None => self.note(format!("{label}: cipher suite {s} is not available in rproxy; left out")),
			}
		}
		if !suites.is_empty() {
			options.insert("cipher_suites".into(), json!(suites));
		}
		if g(spec, &["maxVersion"]).is_some() || g(spec, &["curvePreferences"]).is_some() || as_bool(g(spec, &["sniStrict"]), false) {
			self.note(format!("{label}: maxVersion / curvePreferences / sniStrict are not converted"));
		}
		let alpn = as_list(g(spec, &["alpnProtocols"]));
		let mut client_auth = None;
		if let Some(ca) = g(spec, &["clientAuth"]) {
			let mode = match g(ca, &["clientAuthType"]).and_then(Value::as_str).unwrap_or("NoClientCert") {
				"RequestClientCert" | "VerifyClientCertIfGiven" => "optional",
				"RequireAnyClientCert" | "RequireAndVerifyClientCert" => "required",
				_ => "none",
			};
			if mode != "none" {
				let mut auth = json!({"mode": mode});
				let names = as_list(g(ca, &["secretNames"]));
				let mut pem = vec![];
				for n in &names {
					let data = self.world.secrets.get(&(ns.to_string(), n.clone())).and_then(|s| s.data.clone()).unwrap_or_default();
					match data.get("tls.ca").or_else(|| data.get("ca.crt")) {
						Some(b) => pem.extend_from_slice(&b.0),
						None => self.note(format!("{label}: Secret {ns}/{n} has no tls.ca / ca.crt")),
					}
				}
				if pem.is_empty() {
					self.note(format!("{label}: clientAuth without a CA; left out"));
				} else {
					let file = format!("{}.ca", crate::pem::short_hash(&pem));
					self.out.files.insert(file.clone(), pem);
					auth["ca_file"] = json!(format!("{}/{file}", self.opts.cert_dir));
					client_auth = Some(auth);
				}
			}
		}
		let p = self.port(key);
		if !options.is_empty() {
			p.tls_options = Some(Value::Object(options));
		}
		if !alpn.is_empty() {
			p.alpn = Some(alpn);
		}
		if client_auth.is_some() {
			p.client_auth = client_auth;
		}
	}

	fn entry_points(&mut self, names: &[String], protocol: rp::Protocol, what: &str) -> Vec<(rp::Protocol, u16)> {
		let eps = self.entry_points.clone();
		let list: Vec<(rp::Protocol, u16)> = if names.is_empty() {
			eps.values().filter(|(p, _)| *p == protocol).copied().collect()
		} else {
			names.iter().filter_map(|n| eps.get(n).copied()).collect()
		};
		for n in names {
			if !eps.contains_key(n) {
				self.note(format!("{what}: entry point {n} is not configured (--traefik-entrypoint); left out there"));
			}
		}
		list.into_iter().filter(|(p, _)| *p == protocol).collect()
	}
}

/// Go (Traefik) cipher suite names → rustls names.
fn cipher(s: &str) -> Option<String> {
	Some(
		match s {
			"TLS_AES_128_GCM_SHA256" => "TLS13_AES_128_GCM_SHA256",
			"TLS_AES_256_GCM_SHA384" => "TLS13_AES_256_GCM_SHA384",
			"TLS_CHACHA20_POLY1305_SHA256" => "TLS13_CHACHA20_POLY1305_SHA256",
			"TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256"
			| "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384"
			| "TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256"
			| "TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384"
			| "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256"
			| "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256" => s,
			"TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305" => "TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256",
			"TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305" => "TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256",
			_ => return None,
		}
		.to_string(),
	)
}

const HTTP_MATCHERS: &[&str] =
	&["Host", "HostRegexp", "Path", "PathPrefix", "PathRegexp", "Method", "Header", "HeaderRegexp", "Query", "QueryRegexp", "ClientIP"];

/// A Traefik v2 / v3 router rule → rproxy's `match` (v3 syntax), or why not.
pub fn translate_rule(rule: &str) -> Result<String, String> {
	let mut out = String::new();
	let chars: Vec<char> = rule.trim().chars().collect();
	let mut i = 0;
	while i < chars.len() {
		let c = chars[i];
		if c.is_ascii_alphabetic() {
			let start = i;
			while i < chars.len() && chars[i].is_ascii_alphanumeric() {
				i += 1;
			}
			let mut name: String = chars[start..i].iter().collect();
			// the arguments
			let args_start = i;
			let mut args: Vec<String> = vec![];
			let mut j = i;
			while j < chars.len() && chars[j].is_whitespace() {
				j += 1;
			}
			if j >= chars.len() || chars[j] != '(' {
				return Err(format!("{name}: expected ("));
			}
			j += 1;
			loop {
				while j < chars.len() && (chars[j].is_whitespace() || chars[j] == ',') {
					j += 1;
				}
				if j >= chars.len() {
					return Err("unterminated matcher".into());
				}
				if chars[j] == ')' {
					j += 1;
					break;
				}
				let q = chars[j];
				if q != '`' && q != '"' {
					return Err(format!("{name}: arguments are quoted"));
				}
				j += 1;
				let s0 = j;
				while j < chars.len() && chars[j] != q {
					if q == '"' && chars[j] == '\\' {
						j += 1;
					}
					j += 1;
				}
				if j >= chars.len() {
					return Err("unterminated string".into());
				}
				args.push(chars[s0..j].iter().collect());
				j += 1;
			}
			let _ = args_start;
			i = j;
			match name.as_str() {
				"Headers" => name = "Header".into(),
				"HeadersRegexp" => name = "HeaderRegexp".into(),
				"HostHeader" => name = "Host".into(),
				_ => {}
			}
			if name == "Query" && args.len() == 1 && args[0].contains('=') {
				let (k, v) = args[0].split_once('=').unwrap();
				args = vec![k.to_string(), v.to_string()];
			}
			if !HTTP_MATCHERS.contains(&name.as_str()) {
				return Err(format!("matcher {name} has no rproxy equivalent"));
			}
			if args.iter().any(|a| has_placeholder(a)) {
				return Err(format!("{name} with v2 placeholders ({{name}} / {{name:regex}}) is not converted"));
			}
			let quoted: Vec<String> = args.iter().map(|a| if a.contains('`') { format!("\"{a}\"") } else { format!("`{a}`") }).collect();
			out.push_str(&format!("{name}({})", quoted.join(", ")));
			continue;
		}
		out.push(c);
		i += 1;
	}
	Ok(out)
}

/// A Traefik v2 placeholder: `{name}` or `{name:regex}`.
fn has_placeholder(s: &str) -> bool {
	let b = s.as_bytes();
	for i in 0..b.len() {
		if b[i] != b'{' {
			continue;
		}
		let mut j = i + 1;
		if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_') {
			while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
				j += 1;
			}
			if j < b.len() && (b[j] == b':' || b[j] == b'}') {
				return true;
			}
		}
	}
	false
}

/// `Host(...)` names of a rule (for certResolver domains).
fn host_names(rule: &str) -> Vec<String> {
	let mut out = vec![];
	let mut rest = rule;
	while let Some(i) = rest.find("Host(") {
		let after = &rest[i + 5..];
		let end = after.find(')').unwrap_or(after.len());
		for part in after[..end].split(',') {
			let n = part.trim().trim_matches('`').trim_matches('"');
			if !n.is_empty() && !n.contains('{') {
				out.push(n.to_ascii_lowercase());
			}
		}
		rest = &after[end..];
	}
	out
}

/// `HostSNI(a, b)` / `HostSNIRegexp(...)` → server names (rproxy syntax); empty for `*`.
fn sni_names(rule: &str, notes: &mut Vec<String>, what: &str) -> Vec<String> {
	let mut out = vec![];
	for (f, regexp) in [("HostSNIRegexp(", true), ("HostSNI(", false)] {
		let mut rest = rule;
		while let Some(i) = rest.find(f) {
			if !regexp && rest[..i].ends_with("HostSNIRegexp") {
				rest = &rest[i + f.len()..];
				continue;
			}
			let after = &rest[i + f.len()..];
			let end = after.find(')').unwrap_or(after.len());
			for part in after[..end].split(',') {
				let n = part.trim().trim_matches('`').trim_matches('"').to_string();
				if n.is_empty() || n == "*" {
					continue;
				}
				if regexp {
					match sni_regexp(&n) {
						Some(p) => out.push(p),
						None => notes.push(format!("{what}: HostSNIRegexp `{n}` is not a plain suffix; not converted")),
					}
				} else {
					out.push(n.to_ascii_lowercase());
				}
			}
			rest = &after[end..];
		}
	}
	out
}

/// `^.+\.example\.com$` → `**.example.com`, `^[^.]+\.example\.com$` → `*.example.com`.
fn sni_regexp(regex: &str) -> Option<String> {
	let body = regex.trim().trim_start_matches('^').trim_end_matches('$');
	for (prefixes, wildcard) in [
		(&[".+", ".*", "(.+)", "(.*)"][..], "**."),
		(&["[^.]+", "[^\\.]+", "[a-z0-9-]+", "[a-zA-Z0-9-]+", "[0-9a-z-]+", "[-a-z0-9]+"][..], "*."),
	] {
		for p in prefixes {
			if let Some(rest) = body.strip_prefix(&format!("{p}\\.")) {
				let suffix = rest.replace("\\.", ".");
				if !suffix.is_empty()
					&& suffix.split('.').all(|l| !l.is_empty() && l.chars().all(|c| c.is_ascii_alphanumeric() || c == '-'))
				{
					return Some(format!("{wildcard}{}", suffix.to_ascii_lowercase()));
				}
			}
		}
	}
	None
}

/// Renders the migrated resources.
pub fn render(world: &World, settings: &Settings, opts: &Options) -> Out {
	let mut ctx = Ctx {
		world,
		opts,
		out: Out::default(),
		port: (rp::Protocol::Tcp, 0),
		entry_points: settings.entry_points.clone(),
		allow_cross_namespace: settings.allow_cross_namespace,
	};
	let m = &world.migration;

	// IngressRoute
	let mut irs: Vec<&IngressRoute> = m.ingress_routes.iter().collect();
	irs.sort_by_key(|r| (r.metadata.namespace.clone(), r.metadata.name.clone()));
	for ir in irs {
		let ns = ir.metadata.namespace.clone().unwrap_or_default();
		let what = format!("IngressRoute {ns}/{}", ir.metadata.name.clone().unwrap_or_default());
		for key in ctx.entry_points(&ir.spec.entry_points, rp::Protocol::Tcp, &what) {
			ctx.port = key;
			for (i, r) in ir.spec.routes.iter().enumerate() {
				let rule = match translate_rule(&r.rule) {
					Ok(x) => x,
					Err(e) => {
						ctx.note(format!("{what} route {i}: {e}; left out"));
						continue;
					}
				};
				let mws =
					ctx.middleware_chain(&ns, &r.middlewares.iter().map(|m| (m.namespace.clone(), m.name.clone())).collect::<Vec<_>>(), 0);
				let answered = mws.iter().any(|n| {
					ctx.out.ports[&key]
						.middlewares
						.get(n)
						.is_some_and(|v| v.get("redirect_scheme").is_some() || v.get("redirect_regex").is_some())
				});
				let mut service = None;
				let mut servers = vec![];
				for s in &r.services {
					if let Some((_, svc)) = ctx.service(&ns, s) {
						let w = s.weight.unwrap_or(1).max(0) as u32;
						servers.extend(svc.servers.into_iter().map(|mut x| {
							if w != 1 {
								x.weight = Some(w * x.weight.unwrap_or(1));
							}
							x
						}));
					}
				}
				if !servers.is_empty() {
					let name = format!("traefik/{ns}/{}/r{i}", ir.metadata.name.clone().unwrap_or_default());
					ctx.port(key).services.insert(name.clone(), rp::Service { servers, ..Default::default() });
					service = Some(name);
				} else if !answered {
					ctx.note(format!("{what} route {i}: no usable service; left out"));
					continue;
				}
				let route = rp::HttpRoute {
					name: format!("traefik/{ns}/{}/r{i}", ir.metadata.name.clone().unwrap_or_default()),
					rule: rule.clone(),
					priority: r.priority.filter(|p| *p != 0),
					service,
					middlewares: mws,
					timeouts: None,
				};
				match &ir.spec.tls {
					Some(tls) => {
						ctx.router_tls(&ns, tls, &host_names(&rule), key);
						ctx.port(key).tls_routes.push(route);
					}
					None => ctx.port(key).routes.push(route),
				}
			}
		}
	}

	// Ingress
	let mut ings: Vec<&Ingress> =
		m.ingresses.iter().filter(|i| ingress_class(i).as_deref() == Some(settings.ingress_class.as_str())).collect();
	ings.sort_by_key(|i| {
		(i.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()), i.metadata.namespace.clone(), i.metadata.name.clone())
	});
	let web = settings.entry_points.get("web").copied().unwrap_or((rp::Protocol::Tcp, 80));
	let websecure = settings.entry_points.get("websecure").copied().unwrap_or((rp::Protocol::Tcp, 443));
	for ing in ings {
		let ns = ing.metadata.namespace.clone().unwrap_or_default();
		let iname = ing.metadata.name.clone().unwrap_or_default();
		let spec = ing.spec.clone().unwrap_or_default();
		let tls = spec.tls.clone().unwrap_or_default();
		let mut keys = vec![web];
		if !tls.is_empty() {
			keys.push(websecure);
		}
		for key in keys {
			ctx.port = key;
			let secure = key == websecure && !tls.is_empty();
			if secure {
				for t in &tls {
					if let Some(secret) = &t.secret_name {
						match ctx.secret_pair(&ns, secret) {
							Some(c) => {
								let p = ctx.port(key);
								if !p.certificates.contains(&c) {
									p.certificates.push(c);
								}
							}
							None => ctx.note(format!("Ingress {ns}/{iname}: Secret {secret} is not a TLS Secret; left out")),
						}
					}
				}
			}
			// a default backend applies to the whole port: only the Gateway's own namespace sets it
			if let Some(b) = spec.default_backend.as_ref().and_then(|b| b.service.clone()).filter(|_| ns == settings.gateway.0) {
				let r = ServiceRef { name: b.name.clone(), port: b.port.as_ref().and_then(port_value), ..Default::default() };
				if let Some((name, svc)) = ctx.service(&ns, &r) {
					let p = ctx.port(key);
					p.services.insert(name.clone(), svc);
					if p.default.is_none() {
						p.default = Some(rp::HttpDefault { status: 404, service: Some(name) });
					}
				}
			}
			for (ri, rule) in spec.rules.clone().unwrap_or_default().iter().enumerate() {
				for (pi, path) in rule.http.as_ref().map(|h| h.paths.clone()).unwrap_or_default().iter().enumerate() {
					let Some(b) = path.backend.service.clone() else {
						ctx.note(format!("Ingress {ns}/{iname}: resource backends are not converted"));
						continue;
					};
					let r = ServiceRef { name: b.name.clone(), port: b.port.as_ref().and_then(port_value), ..Default::default() };
					let Some((sname, svc)) = ctx.service(&ns, &r) else { continue };
					let mut parts = vec![];
					if let Some(h) = &rule.host {
						let h = if let Some(s) = h.strip_prefix("*.") { format!("*.{s}") } else { h.clone() };
						// Ingress wildcards are one label
						parts.push(format!("Host(`{}`)", h.to_ascii_lowercase()));
					}
					let p = path.path.clone().unwrap_or_else(|| "/".into());
					// embedded in a match expression: no quotes, backticks or control characters
					if p.chars().any(|c| c == '`' || c == '"' || c.is_control()) || !p.starts_with('/') {
						ctx.note(format!("Ingress {ns}/{iname}: path {p:?} is not a plain path; left out"));
						continue;
					}
					match path.path_type.as_str() {
						"Exact" => parts.push(format!("Path(`{p}`)")),
						"Prefix" => {
							let t = p.trim_end_matches('/');
							if !t.is_empty() {
								parts.push(format!("(Path(`{t}`) || PathPrefix(`{t}/`))"));
							}
						}
						_ => {
							if p != "/" {
								parts.push(format!("PathPrefix(`{p}`)"));
							}
						}
					}
					if parts.is_empty() {
						parts.push("PathPrefix(`/`)".into());
					}
					let port = ctx.port(key);
					port.services.insert(sname.clone(), svc);
					let route = rp::HttpRoute {
						name: format!("ingress/{ns}/{iname}/r{ri}/p{pi}"),
						rule: parts.join(" && "),
						priority: None,
						service: Some(sname),
						middlewares: vec![],
						timeouts: None,
					};
					if secure { port.tls_routes.push(route) } else { port.routes.push(route) }
				}
			}
		}
	}

	// IngressRouteTCP
	for ir in &m.ingress_routes_tcp {
		let ns = ir.metadata.namespace.clone().unwrap_or_default();
		let name = ir.metadata.name.clone().unwrap_or_default();
		let what = format!("IngressRouteTCP {ns}/{name}");
		for key in ctx.entry_points(&ir.spec.entry_points, rp::Protocol::Tcp, &what) {
			ctx.port = key;
			for (i, r) in ir.spec.routes.iter().enumerate() {
				let mut notes = vec![];
				let names = sni_names(&r.rule, &mut notes, &what);
				for n in notes {
					ctx.note(n);
				}
				if r.rule.contains("ClientIP") || r.rule.contains("&&") {
					ctx.note(format!("{what} route {i}: only HostSNI is converted"));
				}
				let (targets, dest) = ctx.targets(&ns, &r.services);
				if targets.is_empty() {
					ctx.note(format!("{what} route {i}: no usable service; left out"));
					continue;
				}
				let proxy = r.services.iter().find_map(|s| s.proxy_protocol.as_ref().and_then(|p| traefik_mw::as_int(g(p, &["version"]))));
				let passthrough = ir.spec.tls.as_ref().map(|t| t.passthrough);
				if let Some(tls) = ir.spec.tls.as_ref().filter(|t| !t.passthrough) {
					ctx.router_tls(&ns, tls, &names, key);
				}
				ctx.port(key).tcp.push(TcpEntry { name: format!("{what} route {i}"), names, targets, dest, passthrough, proxy });
			}
		}
	}

	// IngressRouteUDP
	for ir in &m.ingress_routes_udp {
		let ns = ir.metadata.namespace.clone().unwrap_or_default();
		let what = format!("IngressRouteUDP {ns}/{}", ir.metadata.name.clone().unwrap_or_default());
		for key in ctx.entry_points(&ir.spec.entry_points, rp::Protocol::Udp, &what) {
			ctx.port = key;
			for r in &ir.spec.routes {
				let (targets, _) = ctx.targets(&ns, &r.services);
				let p = ctx.port(key);
				if !p.udp.is_empty() {
					ctx.note(format!("{what}: entry point already has a UDP route; left out"));
					continue;
				}
				p.udp = targets;
			}
		}
	}
	ctx.out
}

fn port_value(p: &k8s_openapi::api::networking::v1::ServiceBackendPort) -> Option<Value> {
	p.number.map(|n| json!(n)).or_else(|| p.name.clone().map(Value::String))
}

/// The Ingresses read into the rule set (those of `settings.ingress_class`).
pub fn handled_ingresses<'a>(world: &'a World, settings: &Settings) -> Vec<&'a Ingress> {
	world.migration.ingresses.iter().filter(|i| ingress_class(i).as_deref() == Some(settings.ingress_class.as_str())).collect()
}

/// An Ingress's `status.loadBalancer.ingress` for the Gateway's addresses ((type, value)).
pub fn ingress_status(addresses: &[(String, String)]) -> Value {
	let entries: Vec<Value> =
		addresses.iter().map(|(kind, value)| if kind == "Hostname" { json!({"hostname": value}) } else { json!({"ip": value}) }).collect();
	json!({"loadBalancer": {"ingress": entries}})
}

fn ingress_class(i: &Ingress) -> Option<String> {
	i.spec
		.as_ref()
		.and_then(|s| s.ingress_class_name.clone())
		.or_else(|| i.metadata.annotations.as_ref().and_then(|a| a.get("kubernetes.io/ingress.class").cloned()))
}

/// Turns one port's additions into a rule, or merges them into `existing` (the
/// Gateway's rule on the same port). Returns notes for what cannot be combined.
pub fn apply_port(
	key: (rp::Protocol, u16),
	add: PortAdd,
	existing: Option<&mut rp::Rule>,
	base: rp::Rule,
) -> (Option<rp::Rule>, Vec<String>) {
	let mut notes = vec![];
	let where_ = format!("port {}/{}", key.0.as_str(), key.1);
	if key.0 == rp::Protocol::Udp {
		if add.udp.is_empty() {
			return (None, notes);
		}
		if existing.is_some() {
			notes.push(format!("{where_}: the Gateway already has a UDP rule here; the migrated UDP route is left out"));
			return (None, notes);
		}
		return (Some(rp::Rule { targets: add.udp, ..base }), notes);
	}
	let has_http = !add.routes.is_empty() || !add.tls_routes.is_empty() || add.default.is_some();
	let terminate = !add.tls_routes.is_empty() || add.tcp.iter().any(|t| t.passthrough == Some(false));
	if has_http {
		let routes = if !add.tls_routes.is_empty() {
			if !add.routes.is_empty() {
				notes.push(format!(
					"{where_}: routers without TLS on a TLS port are left out ({})",
					add.routes.iter().map(|r| r.name.as_str()).collect::<Vec<_>>().join(", ")
				));
			}
			add.tls_routes
		} else {
			add.routes
		};
		let passthrough: Vec<rp::TlsRoute> = add
			.tcp
			.iter()
			.filter(|t| t.passthrough == Some(true) && !t.names.is_empty() && t.dest.is_some())
			.map(|t| {
				let d = t.dest.clone().unwrap();
				rp::TlsRoute {
					server_names: t.names.clone(),
					remote_addr: d.addr,
					remote_port: d.port,
					passthrough: true,
					..Default::default()
				}
			})
			.collect();
		let rest: Vec<&str> =
			add.tcp.iter().filter(|t| !(t.passthrough == Some(true) && !t.names.is_empty())).map(|t| t.name.as_str()).collect();
		if !rest.is_empty() {
			notes.push(format!(
				"{where_}: TCP routers next to HTTP routers are combined only when they pass TLS through for named hosts; left out: {}",
				rest.join(", ")
			));
		}
		let https = terminate;
		match existing {
			Some(rule) => {
				let Some(http) = rule.http.as_mut() else {
					notes.push(format!("{where_}: the Gateway's rule here is not HTTP; migrated HTTP routes are left out"));
					return (None, notes);
				};
				if rule.tls.is_some() != https {
					notes.push(format!("{where_}: TLS differs between the Gateway's listener and the migrated routes; left out"));
					return (None, notes);
				}
				http.routes.extend(routes);
				http.services.extend(add.services);
				http.middlewares.extend(add.middlewares);
				if http.default.is_none() {
					http.default = add.default;
				}
				if let Some(tls) = rule.tls.as_mut() {
					for c in add.certificates {
						if !tls.certificates.contains(&c) {
							tls.certificates.push(c);
						}
					}
					tls.routes.extend(passthrough);
				}
				(None, notes)
			}
			None => {
				let mut rule = base;
				rule.http = Some(rp::Http { routes, default: add.default, services: add.services, middlewares: add.middlewares });
				if https {
					if add.certificates.is_empty() {
						notes.push(format!("{where_}: TLS routers without a certificate (secretName or certResolver); left out"));
						return (None, notes);
					}
					rule.tls = Some(rp::Tls {
						mode: "terminate",
						certificates: add.certificates,
						routes: passthrough,
						options: add.tls_options,
						client_auth: add.client_auth,
						alpn: add.alpn.unwrap_or_default(),
						..Default::default()
					});
				}
				(Some(rule), notes)
			}
		}
	} else if !add.tcp.is_empty() {
		if existing.is_some() {
			notes.push(format!("{where_}: the Gateway already has a rule here; migrated TCP routes are left out"));
			return (None, notes);
		}
		let catch_all: Vec<&TcpEntry> = add.tcp.iter().filter(|t| t.names.is_empty()).collect();
		let named: Vec<&TcpEntry> = add.tcp.iter().filter(|t| !t.names.is_empty()).collect();
		let default = catch_all.first().copied().unwrap_or(&add.tcp[0]);
		let mut rule = rp::Rule { targets: default.targets.clone(), ..base };
		if let Some(v @ (1 | 2)) = default.proxy {
			rule.extra.insert("source_ip".into(), json!(format!("proxy_v{v}")));
		}
		if named.is_empty() && default.passthrough.is_none() {
			if catch_all.len() > 1 {
				notes.push(format!("{where_}: several HostSNI(`*`) routers; the first is used"));
			}
			return (Some(rule), notes);
		}
		let routes: Vec<rp::TlsRoute> = named
			.iter()
			.filter_map(|t| {
				let d = t.dest.clone()?;
				Some(rp::TlsRoute {
					server_names: t.names.clone(),
					remote_addr: d.addr,
					remote_port: d.port,
					passthrough: terminate && t.passthrough == Some(true),
					..Default::default()
				})
			})
			.collect();
		let mut tls = rp::Tls { mode: if terminate { "terminate" } else { "sni" }, routes, ..Default::default() };
		if terminate {
			if add.certificates.is_empty() {
				notes.push(format!("{where_}: terminating TCP routers without a certificate; left out"));
				return (None, notes);
			}
			tls.certificates = add.certificates;
			tls.options = add.tls_options;
			tls.client_auth = add.client_auth;
		}
		if catch_all.is_empty() {
			tls.unmatched = Some("reject");
		}
		rule.tls = Some(tls);
		(Some(rule), notes)
	} else {
		(None, notes)
	}
}
