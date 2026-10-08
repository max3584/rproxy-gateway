//! `rproxy-gateway vip`: a sidecar of the fleet's rproxy pods (hostNetwork) that
//! puts VIPs on the node directly, without a Service or a load balancer
//! (docs/DESIGN-v0.4.x.md 7.). One Lease per VIP decides which pod has it
//! (`lease.rs`); the holder adds the address to the node's interface (rtnetlink)
//! and announces it (gratuitous ARP, unsolicited NA for IPv6), and lets it go
//! first when its rproxy stops being ready (`/readyz` draining on SIGTERM) or the
//! sidecar stops, so a planned move takes well under a second.
//!
//! The pod's rproxy and certsync do not use the Kubernetes API; only this
//! container gets a token (the chart's ServiceAccount `rproxy-gateway-vip`: the
//! VIPs' Leases, reading pods in its namespace and nodes).

pub mod lease;
pub mod packet;
#[cfg(target_os = "linux")]
mod sys;

use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::Deserialize;

/// The prefix of a VIP's Lease name; the rest is a hash of the address (`lease_name`).
pub const LEASE_PREFIX: &str = "rproxy-vip-";

/// A VIP as the chart's `fleet.vip.addresses` gives it: an address, or an address
/// with the interface it goes on and the nodes that may have it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
enum VipEntry {
	Address(String),
	Full(VipSpec),
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VipSpec {
	pub address: String,
	/// The interface (empty: the one whose subnet holds the VIP, else `--interface`).
	#[serde(default)]
	pub interface: String,
	/// Labels the node must have (empty: every node of the fleet).
	#[serde(default)]
	pub node_selector: BTreeMap<String, String>,
}

/// The Lease of a VIP: `rproxy-vip-` and the first 10 hex digits of the SHA-256 of
/// the address as written (the chart makes the Leases with the same name:
/// `sha256sum`). Addresses are written in their canonical form (`parse_vips`).
pub fn lease_name(address: &str) -> String {
	format!("{LEASE_PREFIX}{}", &crate::pem::sha256_hex(address.as_bytes())[..10])
}

/// An address in its canonical form (`2001:db8::10`, not `2001:0db8:0::10`), usable as a VIP.
pub fn parse_vip(s: &str) -> Result<IpAddr, String> {
	let ip: IpAddr = s.parse().map_err(|_| format!("VIP {s:?}: not an IP address"))?;
	if ip.to_string() != s {
		return Err(format!("VIP {s:?}: write it as {ip}"));
	}
	if !crate::render::usable_ip(ip) {
		return Err(format!("VIP {s}: unspecified, loopback, link-local or multicast"));
	}
	Ok(ip)
}

/// The VIPs from JSON (a list of addresses or of `{address, interface, nodeSelector}`)
/// or a comma-separated list of addresses.
pub fn parse_vips(s: &str) -> Result<Vec<VipSpec>, String> {
	let s = s.trim();
	let entries: Vec<VipEntry> = if s.starts_with('[') {
		serde_json::from_str(s).map_err(|e| format!("the VIPs: {e}"))?
	} else {
		s.split(',').map(str::trim).filter(|a| !a.is_empty()).map(|a| VipEntry::Address(a.to_string())).collect()
	};
	let mut out: Vec<VipSpec> = vec![];
	for e in entries {
		let v = match e {
			VipEntry::Address(a) => VipSpec { address: a, ..Default::default() },
			VipEntry::Full(v) => v,
		};
		parse_vip(&v.address)?;
		if out.iter().any(|o| o.address == v.address) {
			return Err(format!("VIP {} twice", v.address));
		}
		out.push(v);
	}
	Ok(out)
}

fn duration(s: &str) -> Result<Duration, String> {
	crate::render::params::duration(s).ok_or_else(|| format!("{s:?} is not a duration (3s, 500ms)"))
}

#[derive(clap::Args, Debug)]
pub struct Args {
	/// The VIPs: JSON (`["192.0.2.10", {"address": "2001:db8::10", "interface": "eth1", "nodeSelector": {"k": "v"}}]`)
	/// or `192.0.2.10,192.0.2.11`. Canonical addresses.
	#[arg(long, env = "RPROXY_VIPS")]
	pub vips: String,
	/// The interface of VIPs that name none (empty: the one whose subnet holds the VIP).
	#[arg(long, env = "RPROXY_VIP_INTERFACE", default_value = "")]
	pub interface: String,
	/// IP ranges VIPs must be in (the controller's `--address-cidr`); a VIP outside is never held.
	#[arg(long, env = "RPROXY_GATEWAY_ADDRESS_CIDR", value_delimiter = ',')]
	pub address_cidr: Vec<crate::render::Cidr>,
	/// The Leases' namespace (the controller's).
	#[arg(long, env = "POD_NAMESPACE")]
	pub namespace: String,
	/// This pod (its name in the Leases; its readiness gate).
	#[arg(long, env = "POD_NAME")]
	pub pod_name: String,
	/// This pod's node (its labels for `nodeSelector`; cordoned: VIPs move away).
	#[arg(long, env = "NODE_NAME")]
	pub node_name: String,
	/// The pod's IP (rproxy's `/readyz`; `/metrics` listens there).
	#[arg(long, env = "POD_IP")]
	pub pod_ip: IpAddr,
	/// rproxy's control API port (`/readyz`).
	#[arg(long, default_value_t = 9443)]
	pub rproxy_port: u16,
	/// The CA of rproxy's control API certificate.
	#[arg(long, default_value = "/etc/rproxy-gateway/api-tls/ca.crt")]
	pub ca_file: PathBuf,
	/// Where `/metrics` and `/healthz` are answered (on the pod's IP).
	#[arg(long, default_value_t = 9445)]
	pub metrics_port: u16,
	/// How long a Lease holds without a renewal (whole seconds).
	#[arg(long, env = "RPROXY_VIP_LEASE_DURATION", default_value = "3s", value_parser = duration)]
	pub lease_duration: Duration,
	/// How often the holder renews.
	#[arg(long, env = "RPROXY_VIP_RENEW_INTERVAL", default_value = "1s", value_parser = duration)]
	pub renew_interval: Duration,
	/// How soon a failed write is tried again (and how often rproxy's `/readyz` is read).
	#[arg(long, env = "RPROXY_VIP_RETRY_INTERVAL", default_value = "500ms", value_parser = duration)]
	pub retry_interval: Duration,
	/// Gratuitous ARPs (IPv6: unsolicited NAs) sent after taking a VIP.
	#[arg(long, env = "RPROXY_VIP_GARP_COUNT", default_value_t = 3)]
	pub garp_count: u32,
	#[arg(long, env = "RPROXY_VIP_GARP_INTERVAL", default_value = "200ms", value_parser = duration)]
	pub garp_interval: Duration,
	/// When the API server cannot be reached: `hold` keeps the VIP while rproxy is ready (another MAC
	/// announcing it makes the pod let go), `release` drops it when the Lease would have expired.
	#[arg(long, env = "RPROXY_VIP_ON_API_UNREACHABLE", value_enum, default_value_t = lease::OnApiUnreachable::Hold)]
	pub on_api_unreachable: lease::OnApiUnreachable,
}

impl Args {
	pub fn timing(&self) -> Result<lease::Timing, String> {
		let t = lease::Timing { duration: self.lease_duration, renew: self.renew_interval, retry: self.retry_interval };
		check_timing(&t)?;
		Ok(t)
	}
}

/// The Lease's duration is whole seconds; renewals come well before it ends.
pub fn check_timing(t: &lease::Timing) -> Result<(), String> {
	if t.duration.as_secs() == 0 || t.duration.subsec_nanos() != 0 {
		return Err("--lease-duration: whole seconds, at least 1s".into());
	}
	if t.renew.is_zero() || t.renew >= t.duration {
		return Err("--renew-interval: shorter than --lease-duration".into());
	}
	if t.retry.is_zero() || t.retry > t.renew {
		return Err("--retry-interval: not longer than --renew-interval".into());
	}
	Ok(())
}

/// The VIPs' state for `/metrics`.
#[derive(Default)]
struct Metrics {
	/// VIP → (held, reason → transitions).
	vips: BTreeMap<String, (bool, BTreeMap<&'static str, u64>)>,
}

impl Metrics {
	fn render(&self) -> String {
		let mut s = String::from("# HELP rproxy_vip_held Whether this pod holds the VIP.\n# TYPE rproxy_vip_held gauge\n");
		for (vip, (held, _)) in &self.vips {
			s += &format!("rproxy_vip_held{{vip=\"{vip}\"}} {}\n", u8::from(*held));
		}
		s += "# HELP rproxy_vip_transitions_total VIPs taken (acquire) and let go (by reason) by this pod.\n# TYPE rproxy_vip_transitions_total counter\n";
		for (vip, (_, t)) in &self.vips {
			for (reason, n) in t {
				s += &format!("rproxy_vip_transitions_total{{vip=\"{vip}\",reason=\"{reason}\"}} {n}\n");
			}
		}
		s
	}
}

/// What the VIPs' tasks share.
struct Shared {
	/// Other MACs heard announcing a VIP: when last.
	conflicts: Mutex<BTreeMap<IpAddr, Instant>>,
	/// VIPs this pod holds.
	held: AtomicUsize,
	metrics: Mutex<Metrics>,
	/// The last time the API server could not be reached.
	api_failed: Mutex<Option<Instant>>,
}

impl Shared {
	fn api_failed(&self) {
		*self.api_failed.lock().unwrap() = Some(Instant::now());
	}
	fn transition(&self, vip: &str, held: bool, reason: &'static str) {
		let mut m = self.metrics.lock().unwrap();
		let e = m.vips.entry(vip.to_string()).or_default();
		e.0 = held;
		*e.1.entry(reason).or_default() += 1;
	}
}

#[cfg(not(target_os = "linux"))]
pub async fn run(_: Args) -> anyhow::Result<()> {
	anyhow::bail!("the vip sidecar runs on Linux only")
}

#[cfg(target_os = "linux")]
pub use linux::run;

#[cfg(target_os = "linux")]
mod linux {
	use super::*;
	use anyhow::Context;
	use futures::StreamExt;
	use k8s_openapi::api::coordination::v1::Lease;
	use k8s_openapi::api::core::v1::{Node, Pod};
	use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;
	use kube::api::{Api, PostParams};
	use kube::runtime::watcher;
	use tokio::sync::watch;
	use tracing::{debug, error, info, warn};

	use super::lease::{Action, Inputs, Local, Reason, Timing, View};
	use super::packet::{Link, Mac};

	/// The latest object a watch saw, and how many times it listed again after an error.
	type Seen<K> = (Option<K>, u64);

	/// Watches one object by name (`list`/`watch` with a `metadata.name` field selector: the
	/// Role names the Leases).
	fn watch_one<K>(api: Api<K>, name: String, shared: Arc<Shared>) -> watch::Receiver<Seen<K>>
	where
		K: kube::Resource + Clone + std::fmt::Debug + serde::de::DeserializeOwned + Send + Sync + 'static,
	{
		let (tx, rx) = watch::channel((None, 0));
		tokio::spawn(async move {
			use kube::runtime::WatchStreamExt;
			let cfg = watcher::Config::default().fields(&format!("metadata.name={name}"));
			let mut s = watcher::watcher(api, cfg).backoff(Again).boxed();
			let (mut errored, mut epoch, mut listed) = (false, 0u64, false);
			while let Some(ev) = s.next().await {
				match ev {
					Ok(watcher::Event::Apply(k)) | Ok(watcher::Event::InitApply(k)) => {
						listed = true;
						tx.send_replace((Some(k), epoch));
					}
					Ok(watcher::Event::Delete(_)) => {
						tx.send_replace((None, epoch));
					}
					Ok(watcher::Event::Init) => listed = false,
					Ok(watcher::Event::InitDone) => {
						if errored {
							// seen again after the API server could not be reached: the times start again
							epoch += 1;
							errored = false;
						}
						let k = if listed { tx.borrow().0.clone() } else { None };
						tx.send_replace((k, epoch));
					}
					Err(e) => {
						debug!(name, error = %e, "watch failed");
						errored = true;
						shared.api_failed();
					}
				}
			}
		});
		rx
	}

	/// A watch that failed starts again after a second (not kube's backoff of up to 30 s: a VIP
	/// waits on its Lease's changes, and the API server may be back after an outage).
	struct Again;

	impl Iterator for Again {
		type Item = Duration;
		fn next(&mut self) -> Option<Duration> {
			Some(Duration::from_secs(1))
		}
	}

	impl kube::runtime::utils::Backoff for Again {
		fn reset(&mut self) {}
	}

	/// Whether rproxy answers `/readyz`, every `every`.
	fn watch_ready(rp: crate::rproxy::client::Client, addr: std::net::SocketAddr, every: Duration) -> watch::Receiver<bool> {
		let (tx, rx) = watch::channel(false);
		tokio::spawn(async move {
			loop {
				let ready = matches!(tokio::time::timeout(every.max(Duration::from_millis(300)), rp.ready(addr)).await, Ok(Ok(true)));
				if *tx.borrow() != ready {
					info!(ready, "rproxy /readyz");
				}
				tx.send_replace(ready);
				tokio::time::sleep(every).await;
			}
		});
		rx
	}

	/// Calls to the API server end quickly (a VIP waits on them).
	async fn call<T>(t: &Timing, f: impl std::future::Future<Output = Result<T, kube::Error>>) -> anyhow::Result<T> {
		match tokio::time::timeout(t.retry.max(Duration::from_secs(1)), f).await {
			Ok(r) => Ok(r?),
			Err(_) => anyhow::bail!("the API server did not answer in time"),
		}
	}

	pub async fn run(args: Args) -> anyhow::Result<()> {
		let timing = args.timing().map_err(anyhow::Error::msg)?;
		let vips = parse_vips(&args.vips).map_err(anyhow::Error::msg)?;
		anyhow::ensure!(!vips.is_empty(), "no VIPs (--vips)");
		let ca = std::fs::read(&args.ca_file).with_context(|| format!("{}", args.ca_file.display()))?;
		let rp = crate::rproxy::client::Client::new(Some(&ca), "")?;
		let client = kube::Client::try_default().await.context("connecting to Kubernetes")?;
		let shared = Arc::new(Shared {
			conflicts: Mutex::default(),
			held: AtomicUsize::new(0),
			metrics: Mutex::default(),
			api_failed: Mutex::default(),
		});
		for v in &vips {
			shared.metrics.lock().unwrap().vips.insert(v.address.clone(), (false, BTreeMap::new()));
		}
		tokio::spawn(serve_metrics(std::net::SocketAddr::new(args.pod_ip, args.metrics_port), shared.clone()));

		let pod = watch_one(Api::<Pod>::namespaced(client.clone(), &args.namespace), args.pod_name.clone(), shared.clone());
		let node = watch_one(Api::<Node>::all(client.clone()), args.node_name.clone(), shared.clone());
		let ready = watch_ready(rp, std::net::SocketAddr::new(args.pod_ip, args.rproxy_port), timing.retry);
		let leases: Api<Lease> = Api::namespaced(client.clone(), &args.namespace);
		listen(&vips, &args.interface, shared.clone());

		let (stop_tx, stop) = watch::channel(false);
		let mut tasks = vec![];
		for v in vips {
			let ip = parse_vip(&v.address).map_err(anyhow::Error::msg)?;
			let allowed = args.address_cidr.iter().any(|c| c.contains(ip));
			if !allowed {
				error!(vip = v.address, "the VIP is outside the allowed ranges (addressCIDRs): never held");
			}
			let name = lease_name(&v.address);
			let task = Task {
				lease_rx: watch_one(leases.clone(), name.clone(), shared.clone()),
				leases: leases.clone(),
				name,
				ip,
				allowed,
				interface: if v.interface.is_empty() { args.interface.clone() } else { v.interface.clone() },
				spec: v,
				me: args.pod_name.clone(),
				timing,
				on_unreachable: args.on_api_unreachable,
				garp: (args.garp_count, args.garp_interval),
				shared: shared.clone(),
				pod: pod.clone(),
				node: node.clone(),
				ready: ready.clone(),
				stop: stop.clone(),
			};
			tasks.push(tokio::spawn(task.run()));
		}
		info!(pod = args.pod_name, node = args.node_name, "vip started");
		crate::controller::shutdown_signal().await;
		info!("stopping: releasing the VIPs");
		let _ = stop_tx.send(true);
		let _ = tokio::time::timeout(Duration::from_secs(5), futures::future::join_all(tasks)).await;
		Ok(())
	}

	/// Hears ARP on the VIPs' interfaces and ND on all: another MAC announcing a VIP.
	fn listen(vips: &[VipSpec], default_iface: &str, shared: Arc<Shared>) {
		let ips: Vec<IpAddr> = vips.iter().filter_map(|v| parse_vip(&v.address).ok()).collect();
		let own: Arc<Mutex<Vec<Mac>>> = Arc::default();
		match sys::links() {
			Ok(links) => *own.lock().unwrap() = links.iter().filter_map(|l| l.mac).collect(),
			Err(e) => warn!(error = %e, "cannot read the interfaces"),
		}
		let note = {
			let (ips, own, shared) = (ips.clone(), own.clone(), shared.clone());
			move |mac: Mac, ip: IpAddr| {
				if ips.contains(&ip) && !own.lock().unwrap().contains(&mac) && mac != [0; 6] {
					shared.conflicts.lock().unwrap().insert(ip, Instant::now());
				}
			}
		};
		let mut ifaces: Vec<u32> = vec![];
		if let (Ok(links), Ok(addrs)) = (sys::links(), sys::addrs()) {
			for v in vips {
				let Ok(ip) = parse_vip(&v.address) else { continue };
				let name = if v.interface.is_empty() { default_iface } else { &v.interface };
				if let Some(l) = packet::pick_interface(&links, &addrs, ip, name) {
					if ip.is_ipv4() && !ifaces.contains(&l.index) {
						ifaces.push(l.index);
					}
				}
			}
		}
		for index in ifaces {
			let note = note.clone();
			std::thread::spawn(move || {
				if let Err(e) = sys::listen_arp(index, note) {
					warn!(interface = index, error = %e, "cannot hear ARP: conflicts are not detected");
				}
			});
		}
		if ips.iter().any(|i| i.is_ipv6()) {
			std::thread::spawn(move || {
				if let Err(e) = sys::listen_nd(note) {
					warn!(error = %e, "cannot hear neighbor discovery: conflicts are not detected");
				}
			});
		}
	}

	struct Task {
		leases: Api<Lease>,
		lease_rx: watch::Receiver<Seen<Lease>>,
		name: String,
		spec: VipSpec,
		ip: IpAddr,
		allowed: bool,
		interface: String,
		me: String,
		timing: Timing,
		on_unreachable: lease::OnApiUnreachable,
		garp: (u32, Duration),
		shared: Arc<Shared>,
		pod: watch::Receiver<Seen<Pod>>,
		node: watch::Receiver<Seen<Node>>,
		ready: watch::Receiver<bool>,
		stop: watch::Receiver<bool>,
	}

	/// What a VIP task keeps between decisions.
	struct State {
		local: Local,
		view: Option<View>,
		/// The `resourceVersion` and watch epoch `view.since` is for.
		seen: Option<(String, u64)>,
		/// The Lease as last read or written.
		latest: Option<Lease>,
		link: Option<Link>,
		link_checked: Option<Instant>,
		last_conflict: Option<Instant>,
		logged: Option<String>,
	}

	impl Task {
		fn vip(&self) -> &str {
			&self.spec.address
		}

		/// The interface the VIP goes on (looked up again every 10 s while missing).
		fn link(&self, st: &mut State) -> Option<Link> {
			if st.link.is_none() && st.link_checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(10)) {
				st.link_checked = Some(Instant::now());
				st.link = match (sys::links(), sys::addrs()) {
					(Ok(links), Ok(addrs)) => packet::pick_interface(&links, &addrs, self.ip, &self.interface).cloned(),
					(Err(e), _) | (_, Err(e)) => {
						warn!(vip = self.vip(), error = %e, "cannot read the interfaces");
						None
					}
				};
				if st.link.is_none() {
					self.once(st, format!("no interface for the VIP on this node (interface {:?})", self.interface));
				}
			}
			st.link.clone()
		}

		fn once(&self, st: &mut State, why: String) {
			if st.logged.as_deref() != Some(why.as_str()) {
				warn!(vip = self.vip(), "{why}");
				st.logged = Some(why);
			}
		}

		fn selected(&self, node: Option<&Node>) -> bool {
			if self.spec.node_selector.is_empty() {
				return true;
			}
			let labels = node.and_then(|n| n.metadata.labels.as_ref());
			self.spec.node_selector.iter().all(|(k, v)| labels.and_then(|l| l.get(k)) == Some(v))
		}

		/// Follows the watched Lease: `since` starts again when it changes or is seen again after an outage.
		fn observe(&self, st: &mut State) {
			let (lease, epoch) = self.lease_rx.borrow().clone();
			let Some(lease) = lease else {
				if st.view.is_some() {
					self.once(st, format!("the Lease {} is missing (the chart makes it)", self.name));
				}
				st.view = None;
				st.seen = None;
				return;
			};
			self.adopt(st, lease, epoch);
		}

		fn adopt(&self, st: &mut State, lease: Lease, epoch: u64) {
			let rv = lease.metadata.resource_version.clone().unwrap_or_default();
			let key = (rv, epoch);
			let spec = lease.spec.clone().unwrap_or_default();
			let holder = spec.holder_identity.unwrap_or_default();
			let duration = Duration::from_secs(spec.lease_duration_seconds.unwrap_or(1).max(1) as u64);
			if st.seen.as_ref() != Some(&key) || st.view.is_none() {
				st.view = Some(View { holder, duration, since: Instant::now() });
				st.seen = Some(key);
			}
			st.latest = Some(lease);
		}

		/// Writes the Lease with this pod as holder (take or renew).
		async fn write(&self, st: &mut State, take: bool) -> anyhow::Result<bool> {
			let Some(mut lease) = st.latest.clone() else { return Ok(false) };
			let now = jiff::Timestamp::now();
			let sp = lease.spec.get_or_insert_default();
			if take || sp.holder_identity.as_deref() != Some(self.me.as_str()) {
				if sp.holder_identity.as_deref() != Some(self.me.as_str()) {
					sp.lease_transitions = Some(sp.lease_transitions.unwrap_or(0).saturating_add(1));
				}
				sp.acquire_time = Some(MicroTime(now));
			}
			sp.holder_identity = Some(self.me.clone());
			sp.lease_duration_seconds = Some(self.timing.duration.as_secs() as i32);
			sp.renew_time = Some(MicroTime(now));
			match call(&self.timing, self.leases.replace(&self.name, &PostParams::default(), &lease)).await {
				Ok(l) => {
					let epoch = self.lease_rx.borrow().1;
					self.adopt(st, l, epoch);
					Ok(true)
				}
				Err(e) if is_conflict(&e) => {
					// written by someone else first: read it again
					if let Ok(Some(l)) = call(&self.timing, self.leases.get_opt(&self.name)).await {
						let epoch = self.lease_rx.borrow().1;
						self.adopt(st, l, epoch);
					}
					Ok(false)
				}
				Err(e) => {
					self.shared.api_failed();
					Err(e)
				}
			}
		}

		/// Clears the holder (only if it is this pod), so another pod takes the VIP at once.
		async fn clear(&self, st: &mut State) {
			let Some(mut lease) = st.latest.clone() else { return };
			let sp = lease.spec.get_or_insert_default();
			if sp.holder_identity.as_deref() != Some(self.me.as_str()) {
				return;
			}
			sp.holder_identity = None;
			sp.renew_time = Some(MicroTime(jiff::Timestamp::now()));
			match call(&self.timing, self.leases.replace(&self.name, &PostParams::default(), &lease)).await {
				Ok(l) => st.latest = Some(l),
				Err(e) => {
					if !is_conflict(&e) {
						self.shared.api_failed();
					}
					warn!(vip = self.vip(), error = format!("{e:#}"), "cannot clear the Lease (it expires)");
				}
			}
		}

		fn announce(&self, link: &Link) {
			let (Some(mac), (count, every)) = (link.mac, self.garp) else { return };
			let (index, ip, vip) = (link.index, self.ip, self.vip().to_string());
			tokio::spawn(async move {
				for i in 0..count {
					let r = match ip {
						IpAddr::V4(a) => sys::send_garp(index, mac, a),
						IpAddr::V6(a) => sys::send_na(index, mac, a),
					};
					if let Err(e) = r {
						warn!(vip, error = %e, "cannot announce the VIP");
						return;
					}
					if i + 1 < count {
						tokio::time::sleep(every).await;
					}
				}
			});
		}

		fn remove(&self, st: &mut State) {
			if let Some(l) = &st.link {
				if let Err(e) = sys::remove(l.index, self.ip) {
					error!(vip = self.vip(), interface = l.name, error = %e, "cannot remove the VIP");
				}
			}
		}

		fn let_go(&self, st: &mut State, reason: Reason) {
			self.remove(st);
			if st.local.holding {
				st.local.holding = false;
				self.shared.held.fetch_sub(1, Ordering::Relaxed);
			}
			st.local.fallback = false;
			st.local.renewed = None;
			self.shared.transition(self.vip(), false, reason.as_str());
			info!(vip = self.vip(), reason = reason.as_str(), interface = st.link.as_ref().map(|l| l.name.as_str()), "vip.release");
		}

		/// At start: an address left on the node by a pod that did not stop cleanly is removed,
		/// unless the Lease still names this pod (the sidecar restarted).
		async fn adopt_leftover(&self, st: &mut State) {
			let Some(link) = self.link(st) else { return };
			let present = sys::addrs().map(|a| a.iter().any(|a| a.ip == self.ip && a.index == link.index)).unwrap_or(false);
			if !present {
				return;
			}
			// the Lease as listed, for up to its duration
			let mut rx = self.lease_rx.clone();
			let _ = tokio::time::timeout(self.timing.duration, rx.wait_for(|(l, _)| l.is_some())).await;
			self.observe(st);
			if st.view.as_ref().is_some_and(|v| v.holder == self.me) {
				info!(vip = self.vip(), "the VIP is still this pod's (the sidecar restarted)");
				st.local.holding = true;
				self.shared.held.fetch_add(1, Ordering::Relaxed);
			} else {
				warn!(vip = self.vip(), interface = link.name, "removing the VIP left on this node");
				self.remove(st);
			}
		}

		async fn run(mut self) {
			let mut st = State {
				local: Local::default(),
				view: None,
				seen: None,
				latest: None,
				link: None,
				link_checked: None,
				last_conflict: None,
				logged: None,
			};
			self.adopt_leftover(&mut st).await;
			loop {
				self.observe(&mut st);
				let now = Instant::now();
				let conflict = {
					let c = self.shared.conflicts.lock().unwrap().get(&self.ip).copied();
					let new = c.is_some_and(|c| st.last_conflict.is_none_or(|l| c > l));
					if new {
						st.last_conflict = c;
					}
					new && st.local.holding
				};
				if conflict {
					warn!(vip = self.vip(), "another MAC announces the VIP");
				}
				let link = if self.allowed { self.link(&mut st) } else { None };
				let (pod, node) = (self.pod.borrow().0.clone(), self.node.borrow().0.clone());
				let applied = pod.as_ref().is_some_and(crate::controller::provision::gate_applied);
				let stopping = *self.stop.borrow();
				let inputs = Inputs {
					now,
					me: &self.me,
					lease: st.view.as_ref(),
					ready: *self.ready.borrow() && applied,
					selected: self.allowed && link.is_some() && self.selected(node.as_ref()),
					cordoned: node.as_ref().and_then(|n| n.spec.as_ref()).and_then(|s| s.unschedulable).unwrap_or(false),
					shutdown: stopping,
					conflict,
					held_elsewhere: self.shared.held.load(Ordering::Relaxed).saturating_sub(usize::from(st.local.holding)),
					api_failed: *self.shared.api_failed.lock().unwrap(),
				};
				let action = lease::decide(&self.timing, self.on_unreachable, &st.local, &inputs);
				match action {
					Action::Wait => {}
					Action::Take => {
						st.local.tried = Some(now);
						let cordoned = inputs.cordoned;
						let mut fresh = true;
						if st.view.as_ref().is_some_and(|v| !v.holder.is_empty() && v.holder != self.me) {
							// taking over an expired holder: read it once more (it may have renewed meanwhile)
							match call(&self.timing, self.leases.get_opt(&self.name)).await {
								Ok(Some(l)) => {
									let before = st.seen.clone();
									let epoch = self.lease_rx.borrow().1;
									self.adopt(&mut st, l, epoch);
									fresh = st.seen == before;
									// the API server failed since the Lease was last seen to change: it was not
									// seen unchanged for a whole duration (the holder may not have reached the
									// API server either); watch it again from now
									let failed = *self.shared.api_failed.lock().unwrap();
									if fresh && st.view.as_ref().is_some_and(|v| failed.is_some_and(|f| f > v.since)) {
										if let Some(v) = st.view.as_mut() {
											v.since = Instant::now();
										}
										fresh = false;
									}
								}
								Ok(None) => fresh = false,
								Err(e) => {
									self.shared.api_failed();
									debug!(vip = self.vip(), error = format!("{e:#}"), "cannot read the Lease");
									fresh = false;
								}
							}
						}
						if fresh {
							match self.write(&mut st, true).await {
								Ok(true) => {
									let link = link.expect("selected");
									match sys::add(link.index, self.ip) {
										Ok(()) => {
											st.local.holding = true;
											st.local.renewed = Some(Instant::now());
											st.local.fallback = cordoned;
											self.shared.held.fetch_add(1, Ordering::Relaxed);
											self.shared.transition(self.vip(), true, "acquire");
											info!(vip = self.vip(), interface = link.name, lease = self.name, "vip.acquire");
											self.announce(&link);
										}
										Err(e) => {
											error!(vip = self.vip(), interface = link.name, error = %e, "cannot add the VIP (NET_ADMIN?)");
											self.clear(&mut st).await;
											st.local.backoff = Some(Instant::now() + self.timing.duration);
										}
									}
								}
								Ok(false) => {}
								Err(e) => debug!(vip = self.vip(), error = format!("{e:#}"), "cannot take the Lease"),
							}
						}
					}
					Action::Renew => {
						st.local.tried = Some(now);
						match self.write(&mut st, false).await {
							Ok(true) => st.local.renewed = Some(Instant::now()),
							Ok(false) => {
								// someone else wrote it: the next decision sees who holds it
							}
							Err(e) => debug!(vip = self.vip(), error = format!("{e:#}"), "cannot renew the Lease"),
						}
					}
					Action::Announce => {
						if let Some(l) = &link {
							info!(vip = self.vip(), "announcing the VIP again");
							self.announce(l);
						}
					}
					Action::Release(reason) => {
						self.let_go(&mut st, reason);
						self.clear(&mut st).await;
						if reason == Reason::Conflict {
							st.local.backoff = Some(Instant::now() + self.timing.duration);
						}
					}
					Action::Drop(reason) => self.let_go(&mut st, reason),
				}
				if stopping && !st.local.holding {
					return;
				}
				let tick = Duration::from_millis(100);
				tokio::select! {
					_ = self.lease_rx.changed() => {}
					_ = self.ready.changed() => {}
					_ = self.pod.changed() => {}
					_ = self.stop.changed() => {}
					_ = tokio::time::sleep(tick) => {}
				}
			}
		}
	}

	fn is_conflict(e: &anyhow::Error) -> bool {
		matches!(e.downcast_ref::<kube::Error>(), Some(kube::Error::Api(s)) if s.code == 409)
	}

	/// `/metrics` and `/healthz` (plain HTTP on the pod's IP).
	async fn serve_metrics(addr: std::net::SocketAddr, shared: Arc<Shared>) {
		use http_body_util::Full;
		use hyper::body::Bytes;
		let listener = match tokio::net::TcpListener::bind(addr).await {
			Ok(l) => l,
			Err(e) => {
				error!(%addr, error = %e, "cannot listen for /metrics");
				return;
			}
		};
		loop {
			let Ok((tcp, _)) = listener.accept().await else { continue };
			let shared = shared.clone();
			tokio::spawn(async move {
				let svc = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
					let shared = shared.clone();
					async move {
						let (status, body) = match req.uri().path() {
							"/metrics" => (200, shared.metrics.lock().unwrap().render()),
							"/healthz" => (200, "ok".to_string()),
							_ => (404, "not found".to_string()),
						};
						Ok::<_, std::convert::Infallible>(
							hyper::Response::builder()
								.status(status)
								.header("content-type", "text/plain; version=0.0.4")
								.body(Full::new(Bytes::from(body)))
								.unwrap(),
						)
					}
				});
				let _ = hyper::server::conn::http1::Builder::new().serve_connection(hyper_util::rt::TokioIo::new(tcp), svc).await;
			});
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn lease_names() {
		// the chart: printf "rproxy-vip-%s" (sha256sum "192.0.2.10" | trunc 10)
		assert_eq!(lease_name("192.0.2.10"), "rproxy-vip-6d99cbd08f");
		assert_eq!(lease_name("2001:db8::10"), "rproxy-vip-3c7170530b");
	}

	#[test]
	fn vip_lists() {
		let v = parse_vips(r#"["192.0.2.10", {"address": "2001:db8::10", "interface": "eth1", "nodeSelector": {"zone": "a"}}]"#).unwrap();
		assert_eq!(v[0], VipSpec { address: "192.0.2.10".into(), ..Default::default() });
		assert_eq!(v[1].interface, "eth1");
		assert_eq!(v[1].node_selector.get("zone").map(String::as_str), Some("a"));
		assert_eq!(parse_vips("192.0.2.10, 192.0.2.11").unwrap().len(), 2);
		assert!(parse_vips("2001:0db8::10").unwrap_err().contains("write it as 2001:db8::10"));
		assert!(parse_vips("192.0.2.10,192.0.2.10").unwrap_err().contains("twice"));
		assert!(parse_vips("127.0.0.1").is_err());
		assert!(parse_vips(r#"[{"address": "192.0.2.10", "iface": "x"}]"#).is_err(), "unknown field");
		assert!(parse_vips("").unwrap().is_empty());
	}

	#[test]
	fn timings() {
		assert!(check_timing(&lease::Timing::default()).is_ok());
		let t = |d: u64, r: u64, y: u64| lease::Timing {
			duration: Duration::from_millis(d),
			renew: Duration::from_millis(r),
			retry: Duration::from_millis(y),
		};
		assert!(check_timing(&t(2500, 1000, 500)).is_err(), "whole seconds");
		assert!(check_timing(&t(3000, 3000, 500)).is_err());
		assert!(check_timing(&t(3000, 1000, 1500)).is_err());
		assert!(check_timing(&t(2000, 500, 250)).is_ok());
	}

	#[test]
	fn metrics_text() {
		let mut m = Metrics::default();
		m.vips.insert("192.0.2.10".into(), (true, [("acquire", 2), ("not_ready", 1)].into()));
		let s = m.render();
		assert!(s.contains("rproxy_vip_held{vip=\"192.0.2.10\"} 1\n"));
		assert!(s.contains("rproxy_vip_transitions_total{vip=\"192.0.2.10\",reason=\"not_ready\"} 1\n"));
	}
}
