//! fleet with VIPs (docs/DESIGN-v0.4.x.md 7.): the fleet's addresses are the
//! VIPs the administrator listed (`--fleet-vip`, the chart's `fleet.vip.addresses`),
//! held by the `vip` sidecars of the fleet's pods. The controller checks the VIPs
//! (inside `--address-cidr`, not another Service's or a node's address), writes
//! them as the Gateways' addresses (a Gateway picks some with `spec.addresses`)
//! and reads their Leases: a VIP nobody has held for a while makes the Gateways
//! using it `Programmed: False` (`AddressNotUsable`).

use std::collections::BTreeMap;
use std::time::Duration;

use k8s_openapi::api::coordination::v1::Lease;
use kube::Api;

use crate::render::Cidr;
use crate::render::world::World;

/// How long a VIP may be without a holder before its Gateways are not `Programmed`
/// (a planned move takes well under a second, a lost node the Lease's duration).
pub const UNHELD_GRACE: Duration = Duration::from_secs(10);

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Holder {
	/// A pod holds it (or held it a moment ago).
	Held(String),
	/// Nobody has held it for longer than `UNHELD_GRACE`: why.
	Unheld(String),
	/// The Lease could not be read.
	Unknown,
}

/// Who holds a VIP, from its Lease (`None`: the Lease does not exist) at `now`.
pub fn holder(name: &str, lease: Option<&Lease>, now: jiff::Timestamp) -> Holder {
	let Some(lease) = lease else {
		return Holder::Unheld(format!("its Lease {name} does not exist (the chart makes it)"));
	};
	let spec = lease.spec.clone().unwrap_or_default();
	let who = spec.holder_identity.clone().unwrap_or_default();
	let duration = jiff::SignedDuration::from_secs(i64::from(spec.lease_duration_seconds.unwrap_or(1).max(1)));
	let grace = jiff::SignedDuration::try_from(UNHELD_GRACE).unwrap_or_default();
	let last = spec.renew_time.or(spec.acquire_time).map(|t| t.0);
	let current = last.and_then(|t| t.checked_add(duration).ok()).is_some_and(|until| until > now);
	if !who.is_empty() && current {
		return Holder::Held(who);
	}
	// free (released, or expired) for how long: from the last write
	match last {
		Some(t) if t.checked_add(grace).is_ok_and(|until| until > now) => Holder::Held(who),
		_ if who.is_empty() => Holder::Unheld("no rproxy pod holds it (none ready, or none on a node it may be on)".into()),
		_ => Holder::Unheld(format!("its holder {who} stopped renewing the Lease")),
	}
}

/// Reads the VIPs' Leases.
pub async fn holders(client: &kube::Client, ns: &str, vips: &[String]) -> BTreeMap<String, Holder> {
	let api: Api<Lease> = Api::namespaced(client.clone(), ns);
	let now = jiff::Timestamp::now();
	let mut out = BTreeMap::new();
	for v in vips {
		let name = crate::vip::lease_name(v);
		let h = match api.get_opt(&name).await {
			Ok(l) => holder(&name, l.as_ref(), now),
			Err(e) => {
				tracing::debug!(vip = v, error = %e, "cannot read the VIP's Lease");
				Holder::Unknown
			}
		};
		out.insert(v.clone(), h);
	}
	out
}

/// Why a VIP cannot be used, if it cannot: outside `cidrs` (none: VIPs are off), a
/// Service's cluster, external or load balancer address, or a node's (a fleet pod's host IP).
pub fn unusable(world: &World, vip: &str, cidrs: &[Cidr], host_ips: &[String]) -> Option<String> {
	let ip: std::net::IpAddr = match crate::vip::parse_vip(vip) {
		Ok(ip) => ip,
		Err(e) => return Some(e),
	};
	if !cidrs.iter().any(|c| c.contains(ip)) {
		return Some(if cidrs.is_empty() {
			format!("VIP {vip}: static addresses are off (the controller's --address-cidr)")
		} else {
			format!("VIP {vip}: outside the ranges the controller allows (--address-cidr)")
		});
	}
	if host_ips.iter().any(|h| h.parse::<std::net::IpAddr>().ok() == Some(ip)) {
		return Some(format!("VIP {vip}: a node has that address"));
	}
	if let Some((ns, name)) = crate::render::service_with(world, ip, |_, _| false) {
		return Some(format!("VIP {vip}: Service {ns}/{name} has that address"));
	}
	None
}

/// A Gateway's addresses: the VIPs it asks for (`spec.addresses`), else every usable
/// VIP; `Err`: why they cannot be used (`AddressNotUsable`).
pub fn gateway_vips(
	requested: &[String],
	vips: &[String],
	unusable: &BTreeMap<String, String>,
	holders: &BTreeMap<String, Holder>,
) -> Result<Vec<String>, String> {
	let usable: Vec<String> = vips.iter().filter(|v| !unusable.contains_key(*v)).cloned().collect();
	let unheld = |v: &String| match holders.get(v) {
		Some(Holder::Unheld(why)) => Some(format!("VIP {v}: {why}")),
		_ => None,
	};
	if requested.is_empty() {
		if usable.is_empty() {
			let why: Vec<&str> = unusable.values().map(String::as_str).collect();
			return Err(format!("no usable VIP ({})", why.join("; ")));
		}
		// some VIP held is enough: the Gateway is reached through it
		if let Some(why) = usable.iter().map(unheld).collect::<Option<Vec<String>>>() {
			return Err(why.join("; "));
		}
		return Ok(usable);
	}
	for a in requested {
		if let Some(why) = unusable.get(a) {
			return Err(why.clone());
		}
		if !vips.contains(a) {
			return Err(format!("{a} is not a VIP of the rproxy fleet ({})", usable.join(", ")));
		}
		if let Some(why) = unheld(a) {
			return Err(why);
		}
	}
	Ok(requested.to_vec())
}

#[cfg(test)]
mod tests {
	use super::*;
	use k8s_openapi::api::coordination::v1::LeaseSpec;
	use k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime;

	fn lease(holder: Option<&str>, renewed: Option<jiff::Timestamp>) -> Lease {
		Lease {
			metadata: Default::default(),
			spec: Some(LeaseSpec {
				holder_identity: holder.map(str::to_string),
				lease_duration_seconds: Some(3),
				renew_time: renewed.map(MicroTime),
				..Default::default()
			}),
		}
	}

	#[test]
	fn holders_from_leases() {
		let now = jiff::Timestamp::from_second(1_000_000).unwrap();
		let ago = |s: i64| Some(now.checked_sub(jiff::SignedDuration::from_secs(s)).unwrap());
		assert_eq!(holder("l", Some(&lease(Some("a"), ago(1))), now), Holder::Held("a".into()));
		// expired a moment ago, or released a moment ago: a move under way
		assert_eq!(holder("l", Some(&lease(Some("a"), ago(5))), now), Holder::Held("a".into()));
		assert_eq!(holder("l", Some(&lease(None, ago(5))), now), Holder::Held(String::new()));
		assert!(matches!(holder("l", Some(&lease(None, ago(11))), now), Holder::Unheld(w) if w.contains("no rproxy pod")));
		assert!(matches!(holder("l", Some(&lease(Some("a"), ago(30))), now), Holder::Unheld(w) if w.contains("a stopped")));
		assert!(matches!(holder("l", Some(&lease(None, None)), now), Holder::Unheld(_)), "never held");
		assert!(matches!(holder("rproxy-vip-x", None, now), Holder::Unheld(w) if w.contains("rproxy-vip-x does not exist")));
	}

	#[test]
	fn usable_vips() {
		let world = World::default();
		let cidrs: Vec<Cidr> = vec!["192.0.2.0/24".parse().unwrap()];
		assert_eq!(unusable(&world, "192.0.2.10", &cidrs, &[]), None);
		assert!(unusable(&world, "192.0.2.10", &[], &[]).unwrap().contains("static addresses are off"));
		assert!(unusable(&world, "198.51.100.1", &cidrs, &[]).unwrap().contains("outside"));
		assert!(unusable(&world, "192.0.2.10", &cidrs, &["192.0.2.10".into()]).unwrap().contains("a node"));
		let mut world = World::default();
		let svc: k8s_openapi::api::core::v1::Service = serde_json::from_value(serde_json::json!({
			"metadata": {"name": "dns", "namespace": "kube-system"},
			"spec": {"clusterIP": "10.96.0.10", "externalIPs": ["192.0.2.10"]}
		}))
		.unwrap();
		world.services.insert(("kube-system".into(), "dns".into()), svc);
		assert!(unusable(&world, "192.0.2.10", &cidrs, &[]).unwrap().contains("kube-system/dns"));
	}

	#[test]
	fn gateway_addresses() {
		let vips: Vec<String> = vec!["192.0.2.10".into(), "192.0.2.11".into(), "192.0.2.12".into()];
		let unusable: BTreeMap<String, String> = [("192.0.2.12".to_string(), "VIP 192.0.2.12: outside".to_string())].into();
		let mut holders: BTreeMap<String, Holder> =
			[("192.0.2.10".to_string(), Holder::Held("a".into())), ("192.0.2.11".to_string(), Holder::Unheld("nobody".into()))].into();
		assert_eq!(gateway_vips(&[], &vips, &unusable, &holders).unwrap(), vec!["192.0.2.10", "192.0.2.11"], "every usable VIP");
		assert_eq!(gateway_vips(&["192.0.2.10".into()], &vips, &unusable, &holders).unwrap(), vec!["192.0.2.10"]);
		assert!(gateway_vips(&["192.0.2.11".into()], &vips, &unusable, &holders).unwrap_err().contains("VIP 192.0.2.11: nobody"));
		assert!(gateway_vips(&["192.0.2.12".into()], &vips, &unusable, &holders).unwrap_err().contains("outside"));
		assert!(gateway_vips(&["192.0.2.99".into()], &vips, &unusable, &holders).unwrap_err().contains("not a VIP"));
		holders.insert("192.0.2.10".into(), Holder::Unheld("nobody".into()));
		assert!(gateway_vips(&[], &vips, &unusable, &holders).is_err(), "no VIP held");
		holders.insert("192.0.2.10".into(), Holder::Unknown);
		assert!(gateway_vips(&[], &vips, &unusable, &holders).is_ok(), "a Lease not read is not an error");
		let all: BTreeMap<String, String> = vips.iter().map(|v| (v.clone(), format!("VIP {v}: off"))).collect();
		assert!(gateway_vips(&[], &vips, &all, &holders).unwrap_err().starts_with("no usable VIP"));
	}
}
