//! PEM checks and content hashes for certificate files.

use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// Whether `crt` holds at least one certificate and `key` a private key (both PEM).
pub fn check_pair(crt: &[u8], key: &[u8]) -> Result<(), String> {
	let certs: Vec<_> = CertificateDer::pem_slice_iter(crt).collect::<Result<_, _>>().map_err(|e| format!("tls.crt: {e}"))?;
	if certs.is_empty() {
		return Err("tls.crt holds no certificate".into());
	}
	PrivateKeyDer::from_pem_slice(key).map_err(|e| format!("tls.key: {e}"))?;
	Ok(())
}

/// Whether `pem` holds at least one certificate (a CA bundle).
pub fn check_certs(pem: &[u8]) -> Result<usize, String> {
	let certs: Vec<_> = CertificateDer::pem_slice_iter(pem).collect::<Result<_, _>>().map_err(|e| e.to_string())?;
	if certs.is_empty() {
		return Err("no PEM certificate".into());
	}
	Ok(certs.len())
}

/// The first 16 hex digits of the SHA-256 of `data`.
pub fn short_hash(data: &[u8]) -> String {
	hex(&ring::digest::digest(&ring::digest::SHA256, data).as_ref()[..8])
}

/// The SHA-256 of `data` in hex.
pub fn sha256_hex(data: &[u8]) -> String {
	hex(ring::digest::digest(&ring::digest::SHA256, data).as_ref())
}

pub fn hex(bytes: &[u8]) -> String {
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
pub mod tests {
	use super::*;

	/// A self-signed certificate and its key, for tests.
	pub fn pair(name: &str) -> (String, String) {
		let key = rcgen::KeyPair::generate().unwrap();
		let cert = rcgen::CertificateParams::new(vec![name.to_string()]).unwrap().self_signed(&key).unwrap();
		(cert.pem(), key.serialize_pem())
	}

	#[test]
	fn pairs_and_hashes() {
		let (crt, key) = pair("example.com");
		check_pair(crt.as_bytes(), key.as_bytes()).unwrap();
		assert!(check_pair(b"garbage", key.as_bytes()).is_err());
		assert!(check_pair(crt.as_bytes(), b"garbage").is_err());
		assert_eq!(short_hash(b"").len(), 16);
		assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
	}
}
