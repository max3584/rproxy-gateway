//! Backend references → the pod addresses rproxy sends to (EndpointSlices, not
//! the Service's ClusterIP, so rproxy's balancing and health checks see each pod).

use crate::k8s::gateway::{BackendRef, GROUP};
use crate::render::status::Cond;
use crate::render::world::World;

/// One address and port of a backend.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Endpoint {
	/// An IP (or, for an ExternalName Service, a host name rproxy resolves).
	pub addr: String,
	pub port: u16,
}

impl Endpoint {
	/// `10.0.0.1:80`, `[fd00::1]:80`
	pub fn authority(&self) -> String {
		if self.addr.contains(':') { format!("[{}]:{}", self.addr, self.port) } else { format!("{}:{}", self.addr, self.port) }
	}
}

/// Why a reference does not resolve: the reason and message of `ResolvedRefs: False`.
pub type RefError = Cond;

fn refused(reason: &str, message: String) -> RefError {
	Cond::new("ResolvedRefs", false, reason, message)
}

/// Resolves `b`, referenced from a route of `route_kind` in `route_ns`.
/// The endpoints may be empty (a Service without ready pods).
pub fn resolve(world: &World, route_kind: &str, route_ns: &str, b: &BackendRef) -> Result<Vec<Endpoint>, RefError> {
	let group = b.group.as_deref().unwrap_or("");
	let kind = b.kind.as_deref().unwrap_or("Service");
	if !(group.is_empty() && kind == "Service") {
		return Err(refused("InvalidKind", format!("backend {group}/{kind} {}: only Services are supported", b.name)));
	}
	let ns = b.namespace.as_deref().unwrap_or(route_ns);
	if !world.granted((GROUP, route_kind, route_ns), ("", "Service", ns, &b.name)) {
		return Err(refused("RefNotPermitted", format!("Service {ns}/{}: no ReferenceGrant allows the reference", b.name)));
	}
	let Some(svc) = world.services.get(&(ns.to_string(), b.name.clone())) else {
		return Err(refused("BackendNotFound", format!("Service {ns}/{} not found", b.name)));
	};
	let Some(port) = b.port else {
		return Err(refused("BackendNotFound", format!("Service {ns}/{}: backendRef has no port", b.name)));
	};
	let spec = svc.spec.clone().unwrap_or_default();
	if spec.type_.as_deref() == Some("ExternalName") {
		let host = spec.external_name.unwrap_or_default();
		let Ok(port) = u16::try_from(port) else {
			return Err(refused("BackendNotFound", format!("Service {ns}/{}: port {port}", b.name)));
		};
		return Ok(vec![Endpoint { addr: host, port }]);
	}
	let Some(svc_port) = spec.ports.iter().flatten().find(|p| p.port == port) else {
		return Err(refused("BackendNotFound", format!("Service {ns}/{} has no port {port}", b.name)));
	};
	let mut out = vec![];
	for slice in world.slices.get(&(ns.to_string(), b.name.clone())).into_iter().flatten() {
		if !matches!(slice.address_type.as_str(), "IPv4" | "IPv6") {
			continue;
		}
		// the slice's port of the same name (unnamed matches unnamed)
		let Some(number) = slice
			.ports
			.iter()
			.flatten()
			.find(|p| p.name.as_deref().unwrap_or("") == svc_port.name.as_deref().unwrap_or(""))
			.and_then(|p| p.port)
		else {
			continue;
		};
		let Ok(number) = u16::try_from(number) else { continue };
		for ep in slice.endpoints.iter().flatten() {
			let ready = ep.conditions.as_ref().and_then(|c| c.ready).unwrap_or(true);
			if !ready {
				continue;
			}
			for addr in &ep.addresses {
				out.push(Endpoint { addr: addr.clone(), port: number });
			}
		}
	}
	out.sort();
	out.dedup();
	Ok(out)
}

/// The `appProtocol` of the Service port a backendRef names (`kubernetes.io/h2c`, `kubernetes.io/ws`, ...).
pub fn app_protocol(world: &World, route_ns: &str, b: &BackendRef) -> Option<String> {
	let ns = b.namespace.as_deref().unwrap_or(route_ns);
	let svc = world.services.get(&(ns.to_string(), b.name.clone()))?;
	svc.spec.as_ref()?.ports.iter().flatten().find(|p| Some(p.port) == b.port)?.app_protocol.clone()
}

/// One address for a backend that rproxy can only send to as a whole (a
/// `tls.routes` entry has one destination): the Service's ClusterIP, or the
/// first ready endpoint of a headless Service.
pub fn service_address(world: &World, route_kind: &str, route_ns: &str, b: &BackendRef) -> Result<Option<Endpoint>, RefError> {
	let eps = resolve(world, route_kind, route_ns, b)?;
	let ns = b.namespace.as_deref().unwrap_or(route_ns);
	let spec = world.services.get(&(ns.to_string(), b.name.clone())).and_then(|s| s.spec.clone()).unwrap_or_default();
	let port = b.port.and_then(|p| u16::try_from(p).ok()).unwrap_or(0);
	match spec.cluster_ip.as_deref() {
		Some(ip) if ip != "None" && !ip.is_empty() && spec.type_.as_deref() != Some("ExternalName") => {
			if eps.is_empty() {
				// no ready pod: nothing would answer
				return Ok(None);
			}
			Ok(Some(Endpoint { addr: ip.to_string(), port }))
		}
		_ => Ok(eps.into_iter().next()),
	}
}

/// Weights of each endpoint so that each backend gets its `weight` share
/// (spread evenly over its endpoints): `None` when all are equal.
pub fn spread(backends: &[(u32, Vec<Endpoint>)]) -> Vec<(Endpoint, Option<u32>)> {
	spread_indexed(backends).into_iter().map(|(_, e, w)| (e, w)).collect()
}

/// `spread`, with the index of the backend each endpoint belongs to.
pub fn spread_indexed(backends: &[(u32, Vec<Endpoint>)]) -> Vec<(usize, Endpoint, Option<u32>)> {
	let live: Vec<(usize, &(u32, Vec<Endpoint>))> = backends.iter().enumerate().filter(|(_, (w, e))| *w > 0 && !e.is_empty()).collect();
	// the least common multiple of the endpoint counts, so each share divides evenly
	let mut lcm: u64 = 1;
	for (_, (_, e)) in &live {
		let n = e.len() as u64;
		lcm = lcm / gcd(lcm, n) * n;
		if lcm > 10_000 {
			lcm = 0;
			break;
		}
	}
	let mut out = vec![];
	for (i, (w, e)) in &live {
		let n = e.len() as u64;
		let each = if lcm == 0 { (u64::from(*w) * 100).div_ceil(n) } else { u64::from(*w) * lcm / n };
		for ep in e {
			out.push((*i, ep.clone(), Some(each.clamp(1, 1_000_000) as u32)));
		}
	}
	// the smallest weights with the same ratios: rproxy's round robin then spreads in short cycles
	let g = out.iter().filter_map(|(_, _, w)| w.map(u64::from)).fold(0, gcd);
	if g > 1 {
		for (_, _, w) in &mut out {
			*w = w.map(|w| (u64::from(w) / g) as u32);
		}
	}
	let first = out.first().map(|(_, _, w)| *w);
	if out.iter().all(|(_, _, w)| Some(*w) == first) {
		for (_, _, w) in &mut out {
			*w = None;
		}
	}
	out
}

fn gcd(a: u64, b: u64) -> u64 {
	if b == 0 { a } else { gcd(b, a % b) }
}

#[cfg(test)]
mod tests {
	use super::*;

	fn eps(n: usize, base: &str) -> Vec<Endpoint> {
		(0..n).map(|i| Endpoint { addr: format!("{base}.{i}"), port: 80 }).collect()
	}

	#[test]
	fn weights_spread_over_endpoints() {
		let out = spread(&[(1, eps(2, "10.0.0")), (1, eps(3, "10.0.1"))]);
		let w: Vec<u32> = out.iter().map(|(_, w)| w.unwrap()).collect();
		assert_eq!(w, vec![3, 3, 2, 2, 2]);
		let out = spread(&[(70, eps(1, "a")), (30, eps(1, "b")), (0, eps(1, "c"))]);
		assert_eq!(out.iter().map(|(_, w)| w.unwrap()).collect::<Vec<_>>(), vec![7, 3]);
		let out = spread(&[(1, eps(2, "a"))]);
		assert!(out.iter().all(|(_, w)| w.is_none()));
		assert!(spread(&[(1, vec![])]).is_empty());
		assert_eq!(Endpoint { addr: "fd00::1".into(), port: 80 }.authority(), "[fd00::1]:80");
	}
}
