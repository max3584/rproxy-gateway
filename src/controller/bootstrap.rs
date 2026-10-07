//! The Secrets the controller and rproxy share, created on first start:
//!
//! - `rproxy-gateway-ca`: a CA (only the controller reads the key).
//! - `rproxy-gateway-api-tls`: rproxy's control API certificate, issued by the CA
//!   for `rproxy-api.rproxy-gateway.internal` (pods are reached by IP).
//! - `rproxy-gateway-token`: the controller's token (`token`) and the token file
//!   rproxy reads (`tokens.yaml`, only its SHA-256).

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
	let cert = ca_params()?.self_signed(&key)?;
	Ok((cert.pem(), key.serialize_pem()))
}

/// The CA's parameters: the same each time, so the issuer of a certificate can be
/// built again from the stored key (its subject is what certificates name as issuer).
fn ca_params() -> anyhow::Result<rcgen::CertificateParams> {
	let mut params = rcgen::CertificateParams::new(Vec::<String>::new())?;
	params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
	params.distinguished_name = rcgen::DistinguishedName::new();
	params.distinguished_name.push(rcgen::DnType::CommonName, "rproxy-gateway CA");
	params.key_usages = vec![rcgen::KeyUsagePurpose::KeyCertSign, rcgen::KeyUsagePurpose::CrlSign];
	Ok(params)
}

/// rproxy's control API certificate issued by the CA: (certificate PEM, key PEM).
pub fn issue_api_cert(ca_key_pem: &str) -> anyhow::Result<(String, String)> {
	let ca_key = rcgen::KeyPair::from_pem(ca_key_pem)?;
	let issuer = rcgen::Issuer::new(ca_params()?, ca_key);
	let key = rcgen::KeyPair::generate()?;
	let mut params = rcgen::CertificateParams::new(vec![API_SERVER_NAME.to_string()])?;
	params.distinguished_name.push(rcgen::DnType::CommonName, API_SERVER_NAME);
	params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
	let cert = params.signed_by(&key, &issuer)?;
	Ok((cert.pem(), key.serialize_pem()))
}

/// A random token (hex) and the token file entry for it.
pub fn new_token() -> anyhow::Result<(String, String)> {
	use ring::rand::SecureRandom;
	let mut bytes = [0u8; 32];
	ring::rand::SystemRandom::new().fill(&mut bytes).map_err(|_| anyhow::anyhow!("no randomness"))?;
	let token = crate::pem::hex(&bytes);
	Ok((token.clone(), token_file(&token)))
}

/// rproxy's YAML token file for the controller's token.
pub fn token_file(token: &str) -> String {
	format!(
		"# written by rproxy-gateway: the controller's token (rule sets; ACME certificates for Traefik certResolver)\ntokens:\n  - name: rproxy-gateway\n    sha256: {}\n    scopes: [rules:read, rules:write, acme:write]\n",
		crate::pem::sha256_hex(token.as_bytes())
	)
}

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
	if current.as_ref().and_then(|s| field(s, "ca.crt")).as_deref() != Some(ca_pem.as_slice()) {
		let (crt, key) = issue_api_cert(std::str::from_utf8(&ca_key)?)?;
		let s = secret(API_TLS_SECRET, ns, &[("tls.crt", crt.into_bytes()), ("tls.key", key.into_bytes()), ("ca.crt", ca_pem.clone())]);
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
			let (token, file) = new_token()?;
			info!(secret = TOKEN_SECRET, "creating the controller's token");
			create_or_get(&api, &pp, secret(TOKEN_SECRET, ns, &[("token", token.into_bytes()), ("tokens.yaml", file.into_bytes())])).await?
		}
	};
	let token = String::from_utf8(field(&tok, "token").context("the token Secret has no token")?)?;
	Ok(Bootstrap { ca_pem, token: token.trim().to_string() })
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
		let (crt, key) = issue_api_cert(&ca_key).unwrap();
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
	fn token_files() {
		let (token, file) = new_token().unwrap();
		assert_eq!(token.len(), 64);
		assert!(file.contains(&crate::pem::sha256_hex(token.as_bytes())));
		assert!(file.contains("rules:write"));
	}
}
