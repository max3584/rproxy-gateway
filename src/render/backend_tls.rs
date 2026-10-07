//! BackendTLSPolicy → TLS to the backends (rproxy's per-service `tls`,
//! `features.services` `tls`), and the Gateway's client certificate to them
//! (`spec.tls.backend.clientCertificateRef`).
//!
//! - A policy targets Services of its namespace (`sectionName`: one port, by
//!   name). Two policies with the same target conflict: the oldest wins (GEP-713),
//!   the other gets `Accepted: False` (`Conflicted`). One for a port wins over
//!   one for the whole Service.
//! - `caCertificateRefs` are ConfigMaps (`ca.crt`) of the policy's namespace,
//!   written as one file; `wellKnownCACertificates: System` uses rproxy's
//!   default roots. A policy without a usable CA gets `Accepted: False`
//!   (`NoValidCACertificate`) and requests for its backends are answered with 500.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::k8s::gateway::GROUP;
use crate::render::status::Cond;
use crate::render::world::{Key, World};
use crate::render::{Options, ca_file};

#[derive(Clone, Debug, Default, Deserialize)]
pub struct BackendTlsPolicy {
	#[serde(default)]
	pub metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta,
	pub spec: BackendTlsPolicySpec,
	#[serde(default)]
	pub status: Option<Value>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackendTlsPolicySpec {
	#[serde(default)]
	pub target_refs: Vec<TargetRef>,
	#[serde(default)]
	pub validation: Validation,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetRef {
	#[serde(default)]
	pub group: String,
	pub kind: String,
	pub name: String,
	#[serde(default)]
	pub section_name: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Validation {
	#[serde(default)]
	pub ca_certificate_refs: Vec<crate::k8s::gateway::LocalObjectReference>,
	#[serde(default)]
	pub well_known_ca_certificates: Option<String>,
	#[serde(default)]
	pub hostname: String,
	#[serde(default)]
	pub subject_alt_names: Vec<SubjectAltName>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct SubjectAltName {
	#[serde(rename = "type")]
	pub kind: String,
	#[serde(default)]
	pub hostname: Option<String>,
	#[serde(default)]
	pub uri: Option<String>,
}

/// What a policy gives a backend: rproxy's service `tls`, or (no usable CA) nothing that may be sent to.
#[derive(Clone, Debug, PartialEq)]
pub enum Tls {
	Use(Value),
	Invalid,
}

/// A policy's state, before it is known which Gateways use it.
#[derive(Clone, Debug)]
pub struct PolicyState {
	pub key: Key,
	pub generation: i64,
	/// `Accepted` and `ResolvedRefs`.
	pub conds: Vec<Cond>,
}

/// The policies that apply, by (namespace, Service, port name or `None` for the whole Service).
#[derive(Clone, Debug, Default)]
pub struct Index {
	by_target: BTreeMap<(String, String, Option<String>), (Key, Tls)>,
	/// Every policy naming a Service (conflicted ones too: they get status where the Service is used).
	by_service: BTreeMap<(String, String), std::collections::BTreeSet<Key>>,
	pub states: BTreeMap<Key, PolicyState>,
}

impl Index {
	/// The policy for a Service's port: one for the port, else one for the whole Service.
	pub fn get(&self, ns: &str, service: &str, port_name: Option<&str>) -> Option<&(Key, Tls)> {
		port_name
			.and_then(|p| self.by_target.get(&(ns.to_string(), service.to_string(), Some(p.to_string()))))
			.or_else(|| self.by_target.get(&(ns.to_string(), service.to_string(), None)))
	}

	/// Every policy naming a Service.
	pub fn naming(&self, ns: &str, service: &str) -> impl Iterator<Item = &Key> {
		self.by_service.get(&(ns.to_string(), service.to_string())).into_iter().flatten()
	}
}

/// Reads every BackendTLSPolicy: conflicts, CA files. `client`: the Gateway's
/// client certificate files (cert, key) for the backends.
pub fn index(world: &World, client: Option<&(String, String)>, files: &mut BTreeMap<String, Vec<u8>>, opts: &Options) -> Index {
	let mut out = Index::default();
	let mut policies: Vec<&BackendTlsPolicy> = world.backend_tls_policies.iter().collect();
	// GEP-713: the oldest wins, then namespace/name
	policies.sort_by_key(|p| {
		(p.metadata.creation_timestamp.as_ref().map(|t| t.0.to_string()), p.metadata.namespace.clone(), p.metadata.name.clone())
	});
	for p in policies {
		let ns = p.metadata.namespace.clone().unwrap_or_default();
		let key = (ns.clone(), p.metadata.name.clone().unwrap_or_default());
		let (tls, resolved) = settings(world, &ns, &p.spec.validation, client, files, opts);
		let mut conds = vec![];
		let mut conflicted = false;
		for t in p.spec.target_refs.iter().filter(|t| t.group.is_empty() && t.kind == "Service") {
			out.by_service.entry((ns.clone(), t.name.clone())).or_default().insert(key.clone());
			let target = (ns.clone(), t.name.clone(), t.section_name.clone());
			if out.by_target.contains_key(&target) {
				conflicted = true;
				continue;
			}
			out.by_target.insert(target, (key.clone(), tls.clone()));
		}
		conds.push(match (&tls, conflicted) {
			(_, true) => Cond::new("Accepted", false, "Conflicted", "an older BackendTLSPolicy has the same target"),
			(Tls::Invalid, _) => Cond::new("Accepted", false, "NoValidCACertificate", resolved.message.clone()),
			(Tls::Use(_), false) => Cond::ok("Accepted", "Accepted"),
		});
		conds.push(resolved);
		out.states.insert(key.clone(), PolicyState { key, generation: p.metadata.generation.unwrap_or(0), conds });
	}
	out
}

/// rproxy's service `tls` of a policy's validation, and its `ResolvedRefs`.
fn settings(
	world: &World,
	ns: &str,
	v: &Validation,
	client: Option<&(String, String)>,
	files: &mut BTreeMap<String, Vec<u8>>,
	opts: &Options,
) -> (Tls, Cond) {
	let mut bundle: Vec<u8> = vec![];
	let mut resolved = Cond::ok("ResolvedRefs", "ResolvedRefs");
	for r in &v.ca_certificate_refs {
		let problem = if !r.group.is_empty() || r.kind != "ConfigMap" {
			Some(Cond::new(
				"ResolvedRefs",
				false,
				"InvalidKind",
				format!("caCertificateRef {}/{} {}: only ConfigMaps are supported", r.group, r.kind, r.name),
			))
		} else {
			match world.config_maps.get(&(ns.to_string(), r.name.clone())).and_then(|c| c.data.as_ref()).and_then(|d| d.get("ca.crt")) {
				Some(pem) if crate::pem::check_certs(pem.as_bytes()).is_ok() => {
					bundle.extend_from_slice(pem.trim_end().as_bytes());
					bundle.push(b'\n');
					None
				}
				Some(_) => Some(Cond::new(
					"ResolvedRefs",
					false,
					"InvalidCACertificateRef",
					format!("ConfigMap {ns}/{}: ca.crt is not PEM", r.name),
				)),
				None => Some(Cond::new(
					"ResolvedRefs",
					false,
					"InvalidCACertificateRef",
					format!("ConfigMap {ns}/{} not found or without ca.crt", r.name),
				)),
			}
		};
		if let Some(p) = problem {
			if resolved.status {
				resolved = p;
			}
		}
	}
	let system = v.well_known_ca_certificates.as_deref() == Some("System");
	if bundle.is_empty() && !system {
		if resolved.status {
			resolved = Cond::new("ResolvedRefs", false, "InvalidCACertificateRef", "no CA certificate");
		}
		return (Tls::Invalid, resolved);
	}
	let mut tls = json!({ "server_name": v.hostname });
	if !bundle.is_empty() {
		tls["ca_file"] = json!(ca_file(&bundle, files, opts));
	}
	let sans: Vec<&str> =
		v.subject_alt_names.iter().filter_map(|s| if s.kind == "URI" { s.uri.as_deref() } else { s.hostname.as_deref() }).collect();
	if !sans.is_empty() {
		tls["subject_alt_names"] = json!(sans);
	}
	if let Some((crt, key)) = client {
		tls["cert_file"] = json!(crt);
		tls["key_file"] = json!(key);
	}
	(Tls::Use(tls), resolved)
}

/// The Gateway's client certificate for backends (`spec.tls.backend.clientCertificateRef`):
/// its files, or the Gateway's `ResolvedRefs: False`.
pub fn client_certificate(
	world: &World,
	gw: &crate::k8s::gateway::Gateway,
	files: &mut BTreeMap<String, Vec<u8>>,
	opts: &Options,
) -> Option<Result<(String, String), Cond>> {
	let r = gw.spec.tls.as_ref()?.backend.as_ref()?.client_certificate_ref.as_ref()?;
	let gw_ns = gw.metadata.namespace.as_deref().unwrap_or_default();
	let bad = |reason: &str, m: String| Cond::new("ResolvedRefs", false, reason, m);
	let group = r.group.as_deref().unwrap_or("");
	let kind = r.kind.as_deref().unwrap_or("Secret");
	if !group.is_empty() || kind != "Secret" {
		return Some(Err(bad(
			"InvalidClientCertificateRef",
			format!("clientCertificateRef {group}/{kind} {}: only Secrets are supported", r.name),
		)));
	}
	let ns = r.namespace.as_deref().unwrap_or(gw_ns);
	if !world.granted((GROUP, "Gateway", gw_ns), ("", "Secret", ns, &r.name)) || (ns != gw_ns && !opts.cross_namespace_secrets) {
		return Some(Err(bad(
			"RefNotPermitted",
			format!("Secret {ns}/{}: no ReferenceGrant allows the reference (or --cross-namespace-secrets is off)", r.name),
		)));
	}
	let Some(secret) = world.secrets.get(&(ns.to_string(), r.name.clone())) else {
		return Some(Err(bad("InvalidClientCertificateRef", format!("Secret {ns}/{} not found", r.name))));
	};
	let data = secret.data.clone().unwrap_or_default();
	let (Some(crt), Some(key)) = (data.get("tls.crt"), data.get("tls.key")) else {
		return Some(Err(bad("InvalidClientCertificateRef", format!("Secret {ns}/{} has no tls.crt and tls.key", r.name))));
	};
	if let Err(e) = crate::pem::check_pair(&crt.0, &key.0) {
		return Some(Err(bad("InvalidClientCertificateRef", format!("Secret {ns}/{}: {e}", r.name))));
	}
	Some(Ok(crate::render::cert_files(&crt.0, &key.0, files, opts)))
}
