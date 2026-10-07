//! `rproxy-gateway certsync`: runs next to rproxy and tells the controller which
//! certificate files are in place (rproxy-api docs/DESIGN-v0.4.md 3.3: key
//! material is never sent over the control API).
//!
//! The files come from the Gateway's certificate Secret (`rproxy-<id>-certs`, or
//! `rproxy-fleet-certs`), which the controller writes and the kubelet mounts as
//! a volume into the pod: neither rproxy nor certsync use the Kubernetes API
//! (the pod has no ServiceAccount token), so they cannot read other Secrets.
//! certsync answers `GET /files` with the names in that directory, which the
//! controller checks before it PUTs rules that name them (the kubelet updates
//! a mounted Secret a little after it changes).

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
	/// Where `GET /files` and `GET /healthz` are answered.
	#[arg(long, default_value = "0.0.0.0:9444")]
	pub listen: SocketAddr,
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
	let listener = tokio::net::TcpListener::bind(args.listen).await.with_context(|| format!("listen on {}", args.listen))?;
	info!(dir = %args.dir.display(), listen = %args.listen, "certsync started");
	serve(listener, args.dir).await;
	Ok(())
}

/// Answers `GET /files` (the names in `dir`) and `GET /healthz`.
pub async fn serve(listener: tokio::net::TcpListener, dir: PathBuf) {
	loop {
		let Ok((tcp, _)) = listener.accept().await else { continue };
		let dir = dir.clone();
		tokio::spawn(async move {
			let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
				let dir = dir.clone();
				async move {
					let (status, body) = match req.uri().path() {
						"/files" => match list(&dir) {
							Ok(n) => (200, serde_json::to_vec(&n).unwrap_or_default()),
							Err(e) => (503, format!("{}: {e}", dir.display()).into_bytes()),
						},
						"/healthz" => (200, b"ok".to_vec()),
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

/// The files certsync on `ip` has written (the controller's side of `GET /files`).
pub async fn files(ip: &str, port: u16) -> anyhow::Result<BTreeSet<String>> {
	let addr: SocketAddr = format!("{}:{port}", if ip.contains(':') { format!("[{ip}]") } else { ip.to_string() }).parse()?;
	let fut = async {
		let tcp = tokio::net::TcpStream::connect(addr).await?;
		let (mut sender, conn) = hyper::client::conn::http1::handshake(TokioIo::new(tcp)).await?;
		tokio::spawn(conn);
		let req = Request::get("/files").header("host", "certsync").body(Full::new(Bytes::new()))?;
		let resp = sender.send_request(req).await?;
		let status = resp.status();
		let body = http_body_util::BodyExt::collect(resp.into_body()).await?.to_bytes();
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
}
