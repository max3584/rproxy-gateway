//! `rproxy-gateway certsync`: runs next to rproxy and tells the controller which
//! certificate files are in place (rproxy-api docs/DESIGN-v0.4.md 3.3: key
//! material is never sent over the control API).
//!
//! The files come from the Gateway's certificate Secret (`rproxy-<id>-certs`, or
//! `rproxy-fleet-certs`), which the controller writes and the kubelet mounts as
//! a volume into the pod: neither rproxy nor certsync use the Kubernetes API
//! (the pod has no ServiceAccount token), so they cannot read other Secrets.
//! certsync answers `POST /files` (a JSON list of names) with those of them that
//! are in that directory, which the controller checks before it PUTs rules that
//! name them (the kubelet updates a mounted Secret a little after it changes).
//! It never lists the directory: file names are hashes of their content, so
//! knowing whether a file is there needs its name, which only the controller has.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tracing::info;

/// How long the controller keeps a file no rule needs any more in the Secret
/// (rules still naming it keep working until they are replaced).
pub const LINGER: Duration = Duration::from_secs(300);

#[derive(clap::Args, Debug)]
pub struct Args {
	/// The directory of the certificate files (the mounted Secret).
	#[arg(long, default_value = crate::controller::provision::CERT_DIR)]
	pub dir: PathBuf,
	/// Where `POST /files` and `GET /healthz` are answered.
	#[arg(long, default_value = "0.0.0.0:9444")]
	pub listen: SocketAddr,
	/// The pod's IP (`POD_IP`): listen there only, with the port of `--listen`.
	#[arg(long, env = "POD_IP")]
	pub pod_ip: Option<std::net::IpAddr>,
}

/// The file names in `dir`: plain names only (a mounted Secret also holds
/// `..data` and timestamped directories, which start with a dot).
pub fn list(dir: &Path) -> std::io::Result<BTreeSet<String>> {
	let mut out = BTreeSet::new();
	let entries = match std::fs::read_dir(dir) {
		Ok(e) => e,
		// no Secret mounted yet (optional volume): no files
		Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(out),
		Err(e) => return Err(e),
	};
	for entry in entries {
		let name = entry?.file_name().to_string_lossy().to_string();
		if !name.starts_with('.') {
			out.insert(name);
		}
	}
	Ok(out)
}

pub async fn run(args: Args) -> anyhow::Result<()> {
	let listen = match args.pod_ip {
		Some(ip) => SocketAddr::new(ip, args.listen.port()),
		None => args.listen,
	};
	let listener = tokio::net::TcpListener::bind(listen).await.with_context(|| format!("listen on {listen}"))?;
	info!(dir = %args.dir.display(), %listen, "certsync started");
	serve(listener, args.dir).await;
	Ok(())
}

/// The most a request or answer body may hold.
pub const BODY_LIMIT: usize = 1 << 20;

/// Answers `POST /files` (which of the names asked for are in `dir`) and `GET /healthz`.
pub async fn serve(listener: tokio::net::TcpListener, dir: PathBuf) {
	loop {
		let Ok((tcp, _)) = listener.accept().await else { continue };
		let dir = dir.clone();
		tokio::spawn(async move {
			let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
				let dir = dir.clone();
				async move {
					let (status, body) = match (req.method().as_str(), req.uri().path()) {
						("POST", "/files") => {
							let body = http_body_util::BodyExt::collect(http_body_util::Limited::new(req.into_body(), BODY_LIMIT)).await;
							match body.ok().and_then(|b| serde_json::from_slice::<Vec<String>>(&b.to_bytes()).ok()) {
								Some(asked) => match list(&dir) {
									Ok(present) => {
										let found: BTreeSet<&String> = asked.iter().filter(|n| present.contains(*n)).collect();
										(200, serde_json::to_vec(&found).unwrap_or_default())
									}
									Err(e) => (503, format!("{}: {e}", dir.display()).into_bytes()),
								},
								None => (400, b"a JSON list of file names".to_vec()),
							}
						}
						("GET", "/healthz") => (200, b"ok".to_vec()),
						_ => (404, b"not found".to_vec()),
					};
					Ok::<_, std::convert::Infallible>(
						Response::builder()
							.status(status)
							.header("content-type", "application/json")
							.body(Full::new(Bytes::from(body)))
							.unwrap(),
					)
				}
			});
			let _ = hyper::server::conn::http1::Builder::new().serve_connection(TokioIo::new(tcp), svc).await;
		});
	}
}

/// Which of `names` certsync on `ip` has (the controller's side of `POST /files`).
pub async fn present(ip: &str, port: u16, names: &[&String]) -> anyhow::Result<BTreeSet<String>> {
	let addr: SocketAddr = format!("{}:{port}", if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() }).parse()?;
	let fut = async {
		let tcp = tokio::net::TcpStream::connect(addr).await?;
		let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
		tokio::spawn(conn);
		let req = Request::post("/files")
			.header("host", "certsync")
			.header("content-type", "application/json")
			.body(Full::new(Bytes::from(serde_json::to_vec(names)?)))?;
		let resp = sender.send_request(req).await?;
		let status = resp.status();
		let body = http_body_util::BodyExt::collect(http_body_util::Limited::new(resp.into_body(), BODY_LIMIT))
			.await
			.map_err(|e| anyhow::anyhow!("certsync {addr}: {e}"))?
			.to_bytes();
		anyhow::ensure!(status.is_success(), "certsync {addr}: {status}");
		Ok(serde_json::from_slice(&body)?)
	};
	tokio::time::timeout(Duration::from_secs(5), fut).await.map_err(|_| anyhow::anyhow!("certsync {addr}: timed out"))?
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lists_plain_names() {
		let dir = std::env::temp_dir().join(format!("certsync-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		assert!(list(&dir).unwrap().is_empty(), "no directory: no files");
		// the layout of a mounted Secret
		std::fs::create_dir_all(dir.join("..2026_10_07_00_00_00.1")).unwrap();
		std::fs::write(dir.join("..2026_10_07_00_00_00.1/a.crt"), b"A").unwrap();
		#[cfg(unix)]
		{
			std::os::unix::fs::symlink("..2026_10_07_00_00_00.1", dir.join("..data")).unwrap();
			std::os::unix::fs::symlink("..data/a.crt", dir.join("a.crt")).unwrap();
			assert_eq!(list(&dir).unwrap(), ["a.crt".to_string()].into());
			assert_eq!(std::fs::read(dir.join("a.crt")).unwrap(), b"A");
		}
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[tokio::test]
	async fn answers_only_for_names_asked() {
		let dir = std::env::temp_dir().join(format!("certsync-serve-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		std::fs::create_dir_all(&dir).unwrap();
		std::fs::write(dir.join("abc.key"), b"k").unwrap();
		std::fs::write(dir.join("other.key"), b"k").unwrap();
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let port = listener.local_addr().unwrap().port();
		tokio::spawn(serve(listener, dir.clone()));
		let (a, b) = ("abc.key".to_string(), "missing.key".to_string());
		let got = present("127.0.0.1", port, &[&a, &b]).await.unwrap();
		assert_eq!(got, [a.clone()].into(), "other.key is never told");
		// no listing
		let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
		let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await.unwrap();
		tokio::spawn(conn);
		let resp = sender.send_request(Request::get("/files").header("host", "x").body(Full::new(Bytes::new())).unwrap()).await.unwrap();
		assert_eq!(resp.status(), 404);
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
