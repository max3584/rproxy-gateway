//! A client of rproxy's control API: HTTPS (the controller's own CA, a fixed
//! server name since rproxy pods are reached by IP) and a bearer token.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, anyhow, bail};
use http_body_util::{BodyExt, Full};
use hyper::body::Bytes;
use hyper::{Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use rustls_pki_types::ServerName;
use serde::de::DeserializeOwned;
use serde_json::Value;

use super::model::{Capabilities, Ruleset, RulesetApplied, RulesetRequest, RulesetSummary};

/// The name in rproxy's control API certificate (`tls::issue_api_cert`).
pub const API_SERVER_NAME: &str = "rproxy-api.rproxy-gateway.internal";

const TIMEOUT: Duration = Duration::from_secs(10);

/// How to reach rproxy pods.
#[derive(Clone)]
pub struct Client {
	tls: Option<tokio_rustls::TlsConnector>,
	token: Arc<str>,
}

/// An answer that is not a success.
#[derive(Debug)]
pub struct ApiError {
	pub status: StatusCode,
	/// rproxy's error `code` (`stale_generation`, `precondition_failed`, ...).
	pub code: String,
	pub message: String,
}

impl std::fmt::Display for ApiError {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		write!(f, "{} {}: {}", self.status.as_u16(), self.code, self.message)
	}
}

impl std::error::Error for ApiError {}

impl Client {
	/// `ca_pem`: the CA that issued rproxy's control API certificate (`None`: plain HTTP, for tests).
	pub fn new(ca_pem: Option<&[u8]>, token: &str) -> anyhow::Result<Client> {
		let tls = match ca_pem {
			None => None,
			Some(pem) => {
				use rustls_pki_types::pem::PemObject;
				let mut roots = rustls::RootCertStore::empty();
				for c in rustls_pki_types::CertificateDer::pem_slice_iter(pem) {
					roots.add(c.context("the CA certificate")?).context("the CA certificate")?;
				}
				let provider = Arc::new(rustls::crypto::ring::default_provider());
				let config = rustls::ClientConfig::builder_with_provider(provider)
					.with_safe_default_protocol_versions()?
					.with_root_certificates(roots)
					.with_no_client_auth();
				Some(tokio_rustls::TlsConnector::from(Arc::new(config)))
			}
		};
		Ok(Client { tls, token: token.into() })
	}

	async fn send(
		&self,
		addr: SocketAddr,
		method: Method,
		path: &str,
		headers: &[(&str, &str)],
		body: Option<Vec<u8>>,
	) -> anyhow::Result<(StatusCode, hyper::HeaderMap, Bytes)> {
		let fut = async {
			let tcp = tokio::net::TcpStream::connect(addr).await.with_context(|| format!("connect {addr}"))?;
			let _ = tcp.set_nodelay(true);
			let mut req = Request::builder()
				.method(method)
				.uri(path)
				.header("host", API_SERVER_NAME)
				.header("authorization", format!("Bearer {}", self.token))
				.header("user-agent", concat!("rproxy-gateway/", env!("CARGO_PKG_VERSION")));
			for (k, v) in headers {
				req = req.header(*k, *v);
			}
			if body.is_some() {
				req = req.header("content-type", "application/json");
			}
			let req = req.body(Full::new(Bytes::from(body.unwrap_or_default())))?;
			let resp = match &self.tls {
				Some(tls) => {
					let stream =
						tls.connect(ServerName::try_from(API_SERVER_NAME)?, tcp).await.with_context(|| format!("TLS to {addr}"))?;
					let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
					tokio::spawn(conn);
					sender.send_request(req).await?
				}
				None => {
					let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
					tokio::spawn(conn);
					sender.send_request(req).await?
				}
			};
			let status = resp.status();
			let headers = resp.headers().clone();
			let body = resp.into_body().collect().await?.to_bytes();
			anyhow::Ok((status, headers, body))
		};
		tokio::time::timeout(TIMEOUT, fut).await.map_err(|_| anyhow!("{addr}: timed out"))?
	}

	async fn json<T: DeserializeOwned>(
		&self,
		addr: SocketAddr,
		method: Method,
		path: &str,
		headers: &[(&str, &str)],
		body: Option<Vec<u8>>,
	) -> anyhow::Result<(T, hyper::HeaderMap)> {
		let (status, h, bytes) = self.send(addr, method, path, headers, body).await?;
		if !status.is_success() {
			return Err(api_error(status, &bytes).into());
		}
		let v = serde_json::from_slice(&bytes).with_context(|| format!("{addr} {path}: not the expected JSON"))?;
		Ok((v, h))
	}

	/// `GET /readyz`: whether the pod restored its rules and takes requests.
	pub async fn ready(&self, addr: SocketAddr) -> anyhow::Result<bool> {
		let (status, _, _) = self.send(addr, Method::GET, "/readyz", &[], None).await?;
		Ok(status == StatusCode::OK)
	}

	pub async fn capabilities(&self, addr: SocketAddr) -> anyhow::Result<Capabilities> {
		Ok(self.json(addr, Method::GET, "/capabilities", &[], None).await?.0)
	}

	/// `GET /rulesets/{name}`; `None` when the set does not exist (after a restart).
	pub async fn get_ruleset(&self, addr: SocketAddr, name: &str) -> anyhow::Result<Option<Ruleset>> {
		match self.json::<Ruleset>(addr, Method::GET, &format!("/rulesets/{name}"), &[], None).await {
			Ok((set, _)) => Ok(Some(set)),
			Err(e) => match e.downcast_ref::<ApiError>() {
				Some(a) if a.status == StatusCode::NOT_FOUND => Ok(None),
				_ => Err(e),
			},
		}
	}

	pub async fn list_rulesets(&self, addr: SocketAddr) -> anyhow::Result<Vec<RulesetSummary>> {
		Ok(self.json(addr, Method::GET, "/rulesets", &[], None).await?.0)
	}

	/// `PUT /rulesets/{name}` with `If-Match` when the current etag is known.
	pub async fn put_ruleset(
		&self,
		addr: SocketAddr,
		name: &str,
		generation: i64,
		rules: &[Value],
		if_match: Option<&str>,
	) -> anyhow::Result<RulesetApplied> {
		let body = serde_json::to_vec(&RulesetRequest { generation, rules })?;
		let headers: Vec<(&str, &str)> = if_match.map(|e| vec![("if-match", e)]).unwrap_or_default();
		Ok(self.json(addr, Method::PUT, &format!("/rulesets/{name}"), &headers, Some(body)).await?.0)
	}

	pub async fn delete_ruleset(&self, addr: SocketAddr, name: &str) -> anyhow::Result<()> {
		let (status, _, bytes) = self.send(addr, Method::DELETE, &format!("/rulesets/{name}"), &[], None).await?;
		if status.is_success() || status == StatusCode::NOT_FOUND { Ok(()) } else { bail!(api_error(status, &bytes)) }
	}
}

fn api_error(status: StatusCode, body: &[u8]) -> ApiError {
	let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
	ApiError {
		status,
		code: v["code"].as_str().unwrap_or_default().to_string(),
		message: v["message"]
			.as_str()
			.or(v["error"].as_str())
			.map(str::to_string)
			.unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(500).collect()),
	}
}
