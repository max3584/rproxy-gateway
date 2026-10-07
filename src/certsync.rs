//! `rproxy-gateway certsync`: runs next to rproxy and writes the certificate files
//! the controller names in rules (rproxy-api docs/DESIGN-v0.4.md 3.3: key
//! material is never sent over the control API).
//!
//! Watches the Secrets matching a label selector in one namespace and writes all
//! their keys as files into a directory shared with rproxy (written to a
//! temporary name, then renamed). Files no Secret holds any more are removed
//! after a while, so rules still naming them keep working until the controller
//! has replaced them. `GET /files` lists the files present, which the controller
//! checks before it PUTs rules that name them.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Context;
use futures::StreamExt;
use http_body_util::Full;
use hyper::body::Bytes;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use k8s_openapi::api::core::v1::Secret;
use kube::Api;
use kube::runtime::reflector::{self, Store};
use kube::runtime::{WatchStreamExt, watcher};
use tracing::{info, warn};

/// How long a file no Secret holds stays on disk.
pub const LINGER: Duration = Duration::from_secs(300);

#[derive(clap::Args, Debug)]
pub struct Args {
	/// The namespace of the Secrets.
	#[arg(long, env = "POD_NAMESPACE")]
	pub namespace: String,
	/// Label selector of the Secrets (e.g. `rproxy.max3584.net/certs-for=fleet`).
	#[arg(long)]
	pub selector: String,
	/// The directory shared with rproxy.
	#[arg(long, default_value = crate::controller::provision::CERT_DIR)]
	pub dir: PathBuf,
	/// Where `GET /files` and `GET /healthz` are answered.
	#[arg(long, default_value = "0.0.0.0:9444")]
	pub listen: SocketAddr,
}

/// File names must be plain names (the controller writes `<hash>.crt` / `.key`).
fn safe_name(n: &str) -> bool {
	!n.is_empty() && n.len() <= 200 && !n.starts_with('.') && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Writes `files` into `dir`; removes files absent since longer than `LINGER`
/// (`absent` remembers since when). Returns the names present afterwards.
pub fn sync_dir(
	dir: &Path,
	files: &BTreeMap<String, Vec<u8>>,
	absent: &mut BTreeMap<String, Instant>,
	now: Instant,
) -> std::io::Result<BTreeSet<String>> {
	std::fs::create_dir_all(dir)?;
	for (name, content) in files {
		if !safe_name(name) {
			warn!(file = name, "not a plain file name; skipped");
			continue;
		}
		let path = dir.join(name);
		if std::fs::read(&path).ok().as_deref() == Some(content.as_slice()) {
			continue;
		}
		let tmp = dir.join(format!(".{name}.tmp"));
		std::fs::write(&tmp, content)?;
		// keys are read by rproxy (the same user in the pod), nobody else
		#[cfg(unix)]
		{
			use std::os::unix::fs::PermissionsExt;
			std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o640))?;
		}
		std::fs::rename(&tmp, &path)?;
		info!(file = name, "written");
	}
	let mut present = BTreeSet::new();
	for entry in std::fs::read_dir(dir)? {
		let name = entry?.file_name().to_string_lossy().to_string();
		if name.starts_with('.') {
			continue;
		}
		if files.contains_key(&name) {
			absent.remove(&name);
			present.insert(name);
			continue;
		}
		let since = *absent.entry(name.clone()).or_insert(now);
		if now.duration_since(since) >= LINGER {
			std::fs::remove_file(dir.join(&name))?;
			absent.remove(&name);
			info!(file = name, "removed (no Secret holds it)");
		} else {
			present.insert(name);
		}
	}
	Ok(present)
}

fn wanted(store: &Store<Secret>) -> BTreeMap<String, Vec<u8>> {
	let mut out = BTreeMap::new();
	for s in store.state() {
		for (k, v) in s.data.clone().unwrap_or_default() {
			out.insert(k, v.0);
		}
	}
	out
}

pub async fn run(args: Args) -> anyhow::Result<()> {
	let client = kube::Client::try_default().await?;
	let api: Api<Secret> = Api::namespaced(client, &args.namespace);
	let (store, writer) = reflector::store();
	let changed = Arc::new(tokio::sync::Notify::new());
	let notify = changed.clone();
	let stream = reflector::reflector(writer, watcher(api, watcher::Config::default().labels(&args.selector)).default_backoff());
	tokio::spawn(async move {
		let mut stream = std::pin::pin!(stream);
		while let Some(ev) = stream.next().await {
			match ev {
				Ok(_) => notify.notify_one(),
				Err(e) => warn!(error = %e, "watch error"),
			}
		}
	});
	let present: Arc<Mutex<Option<BTreeSet<String>>>> = Arc::new(Mutex::new(None));
	let listener = tokio::net::TcpListener::bind(args.listen).await.with_context(|| format!("listen on {}", args.listen))?;
	tokio::spawn(serve(listener, present.clone()));
	store.wait_until_ready().await?;
	info!(dir = %args.dir.display(), selector = %args.selector, "certsync started");
	let mut absent = BTreeMap::new();
	loop {
		match sync_dir(&args.dir, &wanted(&store), &mut absent, Instant::now()) {
			Ok(names) => *present.lock().unwrap() = Some(names),
			Err(e) => warn!(error = %e, dir = %args.dir.display(), "cannot write the certificate files"),
		}
		// changes, and now and then for files to remove
		let _ = tokio::time::timeout(Duration::from_secs(30), changed.notified()).await;
	}
}

/// Answers `GET /files` (the names in `present`; 503 until it is set) and `GET /healthz`.
pub async fn serve(listener: tokio::net::TcpListener, present: Arc<Mutex<Option<BTreeSet<String>>>>) {
	loop {
		let Ok((tcp, _)) = listener.accept().await else { continue };
		let present = present.clone();
		tokio::spawn(async move {
			let svc = hyper::service::service_fn(move |req: Request<hyper::body::Incoming>| {
				let present = present.clone();
				async move {
					let names = present.lock().unwrap().clone();
					let (status, body) = match (req.uri().path(), names) {
						("/files", Some(n)) => (200, serde_json::to_vec(&n).unwrap_or_default()),
						("/healthz", Some(_)) => (200, b"ok".to_vec()),
						(_, None) => (503, b"starting".to_vec()),
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
	fn files_are_written_and_removed_later() {
		let dir = std::env::temp_dir().join(format!("certsync-test-{}", std::process::id()));
		let _ = std::fs::remove_dir_all(&dir);
		let mut absent = BTreeMap::new();
		let t0 = Instant::now();
		let mut files: BTreeMap<String, Vec<u8>> = [("a.crt".to_string(), b"A".to_vec()), ("../evil".to_string(), b"x".to_vec())].into();
		let present = sync_dir(&dir, &files, &mut absent, t0).unwrap();
		assert_eq!(present, ["a.crt".to_string()].into());
		assert_eq!(std::fs::read(dir.join("a.crt")).unwrap(), b"A");
		files.clear();
		files.insert("b.crt".into(), b"B".to_vec());
		let present = sync_dir(&dir, &files, &mut absent, t0).unwrap();
		assert!(present.contains("a.crt"), "kept for a while");
		let present = sync_dir(&dir, &files, &mut absent, t0 + LINGER).unwrap();
		assert_eq!(present, ["b.crt".to_string()].into());
		assert!(!dir.join("a.crt").exists());
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
