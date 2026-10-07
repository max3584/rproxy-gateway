//! The Secrets the controller and rproxy share, created on first start:
//!
//! - `rproxy-gateway-ca`: a CA (only the controller reads the key).
//! - `rproxy-gateway-api-tls`: rproxy's control API certificate, issued by the CA
//!   for `rproxy-api.rproxy-gateway.internal` (pods are reached by IP).
//! - `rproxy-gateway-token`: the controller's master token (`token`) and the
//!   token file the fleet's rproxy reads (`tokens.yaml`, only its SHA-256).
//!
//! Each managed Gateway's rproxy gets its own credentials in its own namespace
//! (`provision::api_secret`): a control API certificate for its own name
//! (`<id>.rproxy-api.rproxy-gateway.internal`) and a token derived from the
//! master token (HMAC-SHA256 with the Gateway's id). Reading one Gateway's
//! namespace does not give a way into another Gateway's rproxy.

use std::collections::BTreeMap;

use anyhow::Context;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::PostParams;
use tracing::info;

use crate::rproxy::client::API_SERVER_NAME;

pub const CA_SECRET: &str = "rproxy-gateway-ca";
pub const API_TLS_SECRET: &str = "rproxy-gateway-api-tls";
pub const TOKEN_SECRET: &str = "rproxy-gateway-token";

/// What the controller needs to talk to rproxy.
pub struct Bootstrap {
	pub ca_pem: Vec<u8>,
	/// The CA's key (issues each managed Gateway's control API certificate).
	pub ca_key_pem: String,
	/// The master token (the fleet's token; Gateways' tokens derive from it).
	pub token: String,
}

fn labels() -> BTreeMap<String, String> {
	[("app.kubernetes.io/managed-by".to_string(), "rproxy-gateway".to_string())].into()
}

fn secret(name: &str, ns: &str, data: &[(&str, Vec<u8>)]) -> Secret {
	Secret {
		metadata: ObjectMeta { name: Some(name.into()), namespace: Some(ns.into()), labels: Some(labels()), ..Default::default() },
		data: Some(data.iter().map(|(k, v)| (k.to_string(), ByteString(v.clone()))).collect()),
		..Default::default()
	}
}

fn field(s: &Secret, key: &str) -> Option<Vec<u8>> {
	s.data.as_ref()?.get(key).map(|b| b.0.clone())
}

/// A new CA: (certificate PEM, key PEM).
pub fn new_ca() -> anyhow::Result<(String, String)> {
	let key = rcgen::KeyPair::generate()?;
	let mut params = ca_params()?;
	set_validity(&mut params, CA_DAYS)?;
	let cert = params.self_signed(&key)?;
	Ok((cert.pem(), key.serialize_pem()))
}

/// How long the CA is valid (it is replaced by deleting its Secret: docs/SECURITY.md).
pub const CA_DAYS: i64 = 3650;
/// How long a control API certificate is valid; it is issued again `RENEW_DAYS` before it ends.
pub const LEAF_DAYS: i64 = 365;
pub const RENEW_DAYS: i64 = 30;
/// On a control API Secret: when its certificate ends (RFC 3339).
pub const ANNOTATION_NOT_AFTER: &str = "rproxy.max3584.net/not-after";

/// Valid from yesterday for `days` days.
fn set_validity(params: &mut rcgen::CertificateParams, days: i64) -> anyhow::Result<()> {
	let ymd = |d: jiff::civil::Date| rcgen::date_time_ymd(i32::from(d.year()), d.month() as u8, d.day() as u8);
	let today = jiff::Timestamp::now().to_zoned(jiff::tz::TimeZone::UTC).date();
	params.not_before = ymd(today.yesterday()?);
	params.not_after = ymd(today.checked_add(jiff::Span::new().days(days))?);
	Ok(())
}

/// When a certificate issued now for `days` days ends.
pub fn not_after(days: i64) -> jiff::Timestamp {
	jiff::Timestamp::now().checked_add(jiff::SignedDuration::from_hours(24 * days)).unwrap_or(jiff::Timestamp::MAX)
}

/// Whether a certificate ending at `not_after` (RFC 3339) is to be issued again.
pub fn renew(not_after: Option<&str>) -> bool {
	let Some(t) = not_after.and_then(|t| t.parse::<jiff::Timestamp>().ok()) else { return true };
	t.duration_since(jiff::Timestamp::now()) < jiff::SignedDuration::from_hours(24 * RENEW_DAYS)
}

/// The CA's parameters: the same each time, so the issuer of a certificate can be
/// built again from the stored key (its subject is what certificates name as issuer).
fn ca_params() -> anyhow::Result<rcgen::CertificateParams> {
	let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
	// it issues control API certificates only: no intermediate CAs, names under rproxy-gateway.internal
	params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Constrained(0));
	params.name_constraints = Some(rcgen::NameConstraints {
		permitted_subtrees: vec![rcgen::GeneralSubtree::DnsName("rproxy-gateway.internal".into())],
		excluded_subtrees: vec![],
	});
	params.distinguished_name = rcgen::DistinguishedName::new();
	params.distinguished_name.push(rcgen::DnType::CommonName, "rproxy-gateway CA");
	params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
	Ok(params)
}

/// rproxy's control API certificate for `name` issued by the CA: (certificate PEM, key PEM).
pub fn issue_api_cert(ca_key_pem: &str, name: &str) -> anyhow::Result<(String, String)> {
	let ca_key = rcgen::KeyPair::from_pem(ca_key_pem)?;
	let issuer = rcgen::Issuer::new(ca_params()?, ca_key);
	let key = rcgen::KeyPair::generate()?;
	let mut params = rcgen::CertificateParams::new(vec![name.to_string()])?;
	params.distinguished_name.push(rcgen::DnType::CommonName, name);
	params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
	set_validity(&mut params, LEAF_DAYS)?;
	let cert = params.signed_by(&key, &issuer)?;
	Ok((cert.pem(), key.serialize_pem()))
}

/// A random token (hex) and the token file entry for it.
pub fn new_token() -> anyhow::Result<(String, String)> {
	use ring::rand::SecureRandom;
	let mut bytes = [0u8; 32];
	ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("no randomness"))?;
	let token = crate::pem::hex(&bytes);
	Ok((token.clone(), token_file(&token, FLEET_RULESETS)))
}

/// The token of one target (a managed Gateway's id): HMAC-SHA256 of the master token.
pub fn derive_token(master: &str, target: &str) -> String {
	let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, master.as_bytes());
	crate::pem::hex(ring::hmac::sign(&key, format!("rproxy-gateway/{target}").as_bytes()).as_ref())
}

/// rproxy's YAML token file for the controller's token.
/// The token is named `rproxy-gateway` whatever its value: rproxy makes the token's name
/// the owner of the rule sets it creates, so a rotated token (same name) still owns them.
/// `allow_rulesets`: the rule set name prefixes it may change (`k8s/`; a managed Gateway's
/// own set).
pub fn token_file(token: &str, allow_rulesets: &str) -> String {
	format!(
		"# written by rproxy-gateway: the controller's token (rule sets; ACME certificates for Traefik certResolver)\ntokens:\n  - name: rproxy-gateway\n    sha256: {}\n    scopes: [rules:read, rules:write, acme:write]\n    allow_rulesets: [\"{}\"]\n",
		crate::pem::sha256_hex(token.as_bytes()),
		allow_rulesets
	)
}

/// The rule sets the fleet's token may change: every Gateway's.
pub const FLEET_RULESETS: &str = "k8s/";

/// Creates the Secrets that are missing; reads them all.
pub async fn ensure(client: &kube::Client, ns: &str) -> anyhow::Result<Bootstrap> {
	let api: Api<Secret> = Api::namespaced(client.clone(), ns);
	let pp = PostParams::default();
	let ca = match api.get_opt(CA_SECRET).await? {
		Some(s) => s,
		None => {
			let (crt, key) = new_ca()?;
			info!(secret = CA_SECRET, "creating the CA for rproxy's control API");
			create_or_get(&api, &pp, secret(CA_SECRET, ns, &[("tls.crt", crt.into_bytes()), ("tls.key", key.into_bytes())])).await?
		}
	};
	let ca_pem = field(&ca, "tls.crt").context("the CA Secret has no tls.crt")?;
	let ca_key = field(&ca, "tls.key").context("the CA Secret has no tls.key")?;
	let current = api.get_opt(API_TLS_SECRET).await?;
	let ends = current.as_ref().and_then(|s| s.metadata.annotations.as_ref()).and_then(|a| a.get(ANNOTATION_NOT_AFTER)).cloned();
	if current.as_ref().and_then(|s| field(s, "ca.crt")).as_deref() != Some(ca_pem.as_slice()) || renew(ends.as_deref()) {
		let (crt, key) = issue_api_cert(std::str::from_utf8(&ca_key)?, API_SERVER_NAME)?;
		let mut s = secret(API_TLS_SECRET, ns, &[("tls.crt", crt.into_bytes()), ("tls.key", key.into_bytes()), ("ca.crt", ca_pem.clone())]);
		s.metadata.annotations = Some([(ANNOTATION_NOT_AFTER.to_string(), not_after(LEAF_DAYS).to_string())].into());
		info!(secret = API_TLS_SECRET, "issuing rproxy's control API certificate");
		match current {
			Some(_) => {
				api.replace(API_TLS_SECRET, &pp, &s).await?;
			}
			None => {
				create_or_get(&api, &pp, s).await?;
			}
		}
	}
	let tok = match api.get_opt(TOKEN_SECRET).await? {
		Some(s) => s,
		None => {
			let (token, _) = new_token()?;
			let file = token_file(&token, FLEET_RULESETS);
			info!(secret = TOKEN_SECRET, "creating the controller's token");
			create_or_get(&api, &pp, secret(TOKEN_SECRET, ns, &[("token", token.into_bytes()), ("tokens.yaml", file.into_bytes())])).await?
		}
	};
	let token = String::from_utf8(field(&tok, "token").context("the token Secret has no token")?)?;
	// the token file follows the controller's version (a rotated token is a new Secret)
	let file = token_file(token.trim(), FLEET_RULESETS);
	if field(&tok, "tokens.yaml").as_deref() != Some(file.as_bytes()) {
		let mut s = tok.clone();
		s.data.get_or_insert_default().insert("tokens.yaml".into(), ByteString(file.into_bytes()));
		info!(secret = TOKEN_SECRET, "writing the fleet's token file again");
		api.replace(TOKEN_SECRET, &pp, &s).await?;
	}
	Ok(Bootstrap { ca_pem, ca_key_pem: String::from_utf8(ca_key)?, token: token.trim().to_string() })
}

/// Creates `s`; if another replica created it first, reads that one.
async fn create_or_get(api: &Api<Secret>, pp: &PostParams, s: Secret) -> anyhow::Result<Secret> {
	let name = s.metadata.name.clone().unwrap_or_default();
	match api.create(pp, &s).await {
		Ok(s) => Ok(s),
		Err(kube::Error::Api(e)) if e.code == 409 => Ok(api.get(&name).await?),
		Err(e) => Err(e.into()),
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn the_api_certificate_chains_to_the_ca() {
		let (ca, ca_key) = new_ca().unwrap();
		let (crt, key) = issue_api_cert(&ca_key, API_SERVER_NAME).unwrap();
		crate::pem::check_pair(crt.as_bytes(), key.as_bytes()).unwrap();
		// a client trusting the CA accepts the certificate for the fixed name
		use rustls::client::danger::ServerCertVerifier;
		use rustls_pki_types::pem::PemObject;
		let mut roots = rustls::RootCertStore::empty();
		roots.add(rustls_pki_types::CertificateDer::from_pem_slice(ca.as_bytes()).unwrap()).unwrap();
		let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
		let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(std::sync::Arc::new(roots), provider).build().unwrap();
		let leaf = rustls_pki_types::CertificateDer::from_pem_slice(crt.as_bytes()).unwrap();
		let name = rustls_pki_types::ServerName::try_from(API_SERVER_NAME).unwrap();
		verifier.verify_server_cert(&leaf, &[], &name, &[], rustls_pki_types::UnixTime::now()).unwrap();
	}

	#[test]
	fn the_ca_only_issues_names_under_its_domain() {
		use rustls::client::danger::ServerCertVerifier;
		use rustls_pki_types::pem::PemObject;
		let (ca, ca_key) = new_ca().unwrap();
		let mut roots = rustls::RootCertStore::empty();
		roots.add(rustls_pki_types::CertificateDer::from_pem_slice(ca.as_bytes()).unwrap()).unwrap();
		let provider = std::sync::Arc::new(rustls::crypto::ring::default_provider());
		let verifier = rustls::client::WebPkiServerVerifier::builder_with_provider(std::sync::Arc::new(roots), provider).build().unwrap();
		let (crt, _) = issue_api_cert(&ca_key, "example.com").unwrap();
		let leaf = rustls_pki_types::CertificateDer::from_pem_slice(crt.as_bytes()).unwrap();
		let name = rustls_pki_types::ServerName::try_from("example.com").unwrap();
		assert!(verifier.verify_server_cert(&leaf, &[], &name, &[], rustls_pki_types::UnixTime::now()).is_err(), "name constraints");
	}

	#[test]
	fn derived_tokens() {
		let a = derive_token("master", "default-web-abcdef");
		assert_eq!(a.len(), 64);
		assert_eq!(a, derive_token("master", "default-web-abcdef"));
		assert_ne!(a, derive_token("master", "default-api-abcdef"));
		assert_ne!(a, derive_token("other", "default-web-abcdef"));
	}

	#[test]
	fn token_files() {
		let (token, file) = new_token().unwrap();
		assert_eq!(token.len(), 64);
		assert!(file.contains(&crate::pem::sha256_hex(token.as_bytes())));
		assert!(file.contains("rules:write"));
		assert!(file.contains("allow_rulesets: [\"k8s/\"]"), "{file}");
		assert!(token_file(&token, "k8s/default/web").contains("allow_rulesets: [\"k8s/default/web\"]"));
	}
}
