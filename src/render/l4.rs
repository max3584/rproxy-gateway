//! TLSRoute, TCPRoute and UDPRoute → rproxy L4 rules.
//!
//! - TCPRoute / UDPRoute: the rule's `targets` (pod IPs from EndpointSlices, the
//!   backends' `weight` spread over their pods).
//! - TLSRoute: `tls.routes` entries by server name. An entry has one destination,
//!   so a backend is reached through its Service's ClusterIP (kube-proxy
//!   spreads the connections); with several backendRefs, the one with the
//!   largest weight.

use crate::k8s::gateway::L4Route;
use crate::render::backends::{self, Endpoint};
use crate::render::status::Cond;
use crate::render::world::{Key, World};
use crate::rproxy::model as rp;

/// What a route sends to.
#[derive(Debug, Default)]
pub struct Out {
	pub targets: Vec<rp::Target>,
	/// `ResolvedRefs` of the route (the first problem).
	pub resolved: Option<Cond>,
	/// The Services it sends to.
	pub services: Vec<Key>,
}

/// All backends of all rules of a TCPRoute / UDPRoute.
pub fn targets(world: &World, kind: &str, route: &L4Route) -> Out {
	let ns = route.metadata.namespace.clone().unwrap_or_default();
	let mut out = Out::default();
	let mut weighted: Vec<(u32, Vec<Endpoint>)> = vec![];
	for rule in &route.spec.rules {
		for b in &rule.backend_refs {
			match backends::resolve(world, kind, &ns, b) {
				Ok(eps) => {
					out.services.push((b.namespace.clone().unwrap_or_else(|| ns.clone()), b.name.clone()));
					weighted.push((b.weight.unwrap_or(1).max(0) as u32, eps));
				}
				Err(e) => {
					out.resolved.get_or_insert(e);
				}
			}
		}
	}
	if route.spec.rules.iter().all(|r| r.backend_refs.is_empty()) {
		out.resolved.get_or_insert(Cond::new("ResolvedRefs", false, "BackendNotFound", "the route has no backendRefs"));
	}
	out.targets = backends::spread(&weighted).into_iter().map(|(ep, weight)| rp::Target { addr: ep.addr, port: ep.port, weight }).collect();
	out
}

/// The destination of a TLSRoute: one address (see the module comment).
pub fn destination(world: &World, route: &L4Route) -> (Option<Endpoint>, Option<Cond>) {
	let ns = route.metadata.namespace.clone().unwrap_or_default();
	let mut best: Option<(i32, Endpoint)> = None;
	let mut resolved = None;
	for rule in &route.spec.rules {
		for b in &rule.backend_refs {
			let weight = b.weight.unwrap_or(1);
			if weight <= 0 {
				continue;
			}
			match backends::service_address(world, "TLSRoute", &ns, b) {
				Ok(Some(ep)) => {
					if best.as_ref().is_none_or(|(w, _)| weight > *w) {
						best = Some((weight, ep));
					}
				}
				Ok(None) => {}
				Err(e) => {
					resolved.get_or_insert(e);
				}
			}
		}
	}
	(best.map(|(_, ep)| ep), resolved)
}
