//! Gateway address routability (GEP-5093, Gateway API v1.7 experimental): `routability` on
//! `spec.addresses` and `status.addresses`, and the `AddressesAssigned` condition.
//!
//! - Empty (or unset): the default, as before: the Gateway's Service (its load balancer
//!   addresses, else its ClusterIP) or a static address (`externalIPs`).
//! - `Cluster`: a ClusterIP. Each such entry gets a ClusterIP Service of its own selecting the
//!   Gateway's rproxy pods (managed mode), with the value as its `clusterIP` when one is given;
//!   a value outside the ServiceCIDR, or taken, is refused by the API server: the entry is not
//!   assigned. The fleet has no ClusterIP of a Gateway's own: not assigned there.
//! - `testing.x-k8s.io/sentinel` (reserved for conformance, no guarantee): as `Cluster`.
//! - Anything else (another prefixed value): not supported, not assigned.
//!
//! Every status address carries `routability`: `Cluster` for a ClusterIP, else empty. Each spec
//! entry gets one distinct address; `AddressesAssigned` says whether all, some or none did.

use std::collections::BTreeMap;

use crate::render::status::Cond;

pub const CLUSTER: &str = "Cluster";
/// Reserved for conformance (GEP-5093): supported, with no routability guarantee.
pub const SENTINEL: &str = "testing.x-k8s.io/sentinel";
pub const CONDITION: &str = "AddressesAssigned";

/// One `IPAddress` entry of `spec.addresses`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Request {
	/// Empty: any address.
	pub value: String,
	/// Empty: the default.
	pub routability: String,
	/// Why it may not be served (a ClusterIP value the class's policy does not allow).
	pub refused: Option<String>,
}

impl Request {
	/// Served by a ClusterIP Service of its own (managed mode).
	pub fn dedicated(&self) -> bool {
		(self.routability == CLUSTER || self.routability == SENTINEL) && self.refused.is_none()
	}

	/// The suffix of the name of its own Service (`index`: its place in `spec.addresses`): by the
	/// value when there is one (8 hex digits of its hash: a changed value is another Service, as a
	/// Service's ClusterIP cannot change), else by the place (`n<index>`: reordering `spec.addresses`
	/// makes those Services again, with other ClusterIPs).
	pub fn service_suffix(&self, index: usize) -> String {
		if self.value.is_empty() { format!("n{index}") } else { crate::pem::short_hash(self.value.as_bytes())[..8].to_string() }
	}

	fn describe(&self, index: usize) -> String {
		let mut what = vec![];
		if !self.routability.is_empty() {
			what.push(self.routability.clone());
		}
		if !self.value.is_empty() {
			what.push(self.value.clone());
		}
		if what.is_empty() { format!("spec.addresses[{index}]") } else { format!("spec.addresses[{index}] ({})", what.join(" ")) }
	}
}

/// An address of `status.addresses`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatusAddress {
	pub kind: String,
	pub value: String,
	pub routability: String,
}

impl StatusAddress {
	pub fn new(kind: &str, value: &str, routability: &str) -> StatusAddress {
		StatusAddress { kind: kind.into(), value: value.into(), routability: routability.into() }
	}
}

/// What serves the requests.
#[derive(Clone, Debug)]
pub struct Sources {
	/// The Gateway's own addresses, in order of preference (the Service's load balancer
	/// addresses, else its ClusterIPs; the fleet's addresses). Empty: none yet.
	pub default: Vec<StatusAddress>,
	/// The static addresses (empty routability, with a value) are in place (`Err`: why not).
	pub statics: Result<(), String>,
	/// The ClusterIP of each dedicated request's Service, by its place in `spec.addresses`
	/// (`Err`: why there is none).
	pub dedicated: BTreeMap<usize, Result<String, String>>,
	/// Why dedicated requests cannot be served at all (the fleet; the sentinel, which carries no
	/// guarantee, then takes a default address).
	pub no_dedicated: Option<String>,
}

impl Default for Sources {
	fn default() -> Sources {
		Sources { default: vec![], statics: Err("waiting for the rproxy Service".into()), dedicated: BTreeMap::new(), no_dedicated: None }
	}
}

/// `status.addresses` and the `AddressesAssigned` condition for `requests` (`spec.addresses`).
pub fn assign(requests: &[Request], src: &Sources) -> (Vec<StatusAddress>, Cond) {
	if requests.is_empty() {
		let cond = if src.default.is_empty() {
			Cond::new(CONDITION, false, "NotAssigned", "waiting for an address (the rproxy Service)")
		} else {
			Cond::ok(CONDITION, "AddressesAssigned")
		};
		return (src.default.clone(), cond);
	}
	let mut out: Vec<StatusAddress> = vec![];
	let mut missing: Vec<String> = vec![];
	for (i, r) in requests.iter().enumerate() {
		let taken = |out: &[StatusAddress], v: &str| out.iter().any(|a| a.value == v);
		let any_default = |out: &[StatusAddress]| match src
			.default
			.iter()
			.find(|a| !taken(out, &a.value) && !requests.iter().any(|q| q.value == a.value))
		{
			Some(a) => Ok(a.clone()),
			None if src.default.is_empty() => Err("waiting for an address (the rproxy Service)".to_string()),
			None => Err("no other address of the Gateway is left for it".to_string()),
		};
		let got: Result<StatusAddress, String> = if let Some(why) = &r.refused {
			Err(why.clone())
		} else if r.routability == SENTINEL && r.value.is_empty() && src.no_dedicated.is_some() {
			// no guarantee: any address of the Gateway (the fleet's), reported as asked
			any_default(&out).map(|a| StatusAddress { routability: SENTINEL.into(), ..a })
		} else if r.dedicated() {
			match (&src.no_dedicated, src.dedicated.get(&i)) {
				(Some(why), _) => Err(why.clone()),
				(None, Some(Ok(ip))) if !r.value.is_empty() && ip != &r.value => {
					Err(format!("its Service has the ClusterIP {ip}, not {}", r.value))
				}
				(None, Some(Ok(ip))) => Ok(StatusAddress::new("IPAddress", ip, &r.routability)),
				(None, Some(Err(why))) => Err(why.clone()),
				(None, None) => Err("waiting for its Service".into()),
			}
		} else if !r.routability.is_empty() {
			Err(format!("routability {} is not supported (empty, {CLUSTER} or {SENTINEL})", r.routability))
		} else if !r.value.is_empty() {
			// an external IP: not in the ServiceCIDR
			src.statics.clone().map(|_| StatusAddress::new("IPAddress", &r.value, ""))
		} else {
			any_default(&out)
		};
		match got {
			Ok(a) if taken(&out, &a.value) => missing.push(format!("{}: {} is another entry's", r.describe(i), a.value)),
			Ok(a) => out.push(a),
			Err(why) => missing.push(format!("{}: {why}", r.describe(i))),
		}
	}
	let cond = if missing.is_empty() {
		Cond::ok(CONDITION, "AddressesAssigned")
	} else if out.is_empty() {
		Cond::new(CONDITION, false, "NotAssigned", missing.join("; "))
	} else {
		Cond::new(CONDITION, false, "PartiallyAssigned", missing.join("; "))
	};
	(out, cond)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn req(value: &str, routability: &str) -> Request {
		Request { value: value.into(), routability: routability.into(), refused: None }
	}

	fn lb_and_cluster() -> Sources {
		Sources { default: vec![StatusAddress::new("IPAddress", "203.0.113.5", "")], statics: Ok(()), ..Default::default() }
	}

	#[test]
	fn no_request_reports_the_default() {
		let src = Sources { default: vec![StatusAddress::new("IPAddress", "10.96.0.10", CLUSTER)], ..Default::default() };
		let (a, c) = assign(&[], &src);
		assert_eq!(a, src.default);
		assert_eq!((c.status, c.reason.as_str()), (true, "AddressesAssigned"));
		let (a, c) = assign(&[], &Sources::default());
		assert!(a.is_empty());
		assert_eq!((c.status, c.reason.as_str()), (false, "NotAssigned"));
	}

	/// One empty and one `Cluster` request: one distinct address each.
	#[test]
	fn empty_and_cluster() {
		let mut src = lb_and_cluster();
		src.dedicated.insert(1, Ok("10.96.1.1".into()));
		let (a, c) = assign(&[req("", ""), req("", CLUSTER)], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "203.0.113.5", ""), StatusAddress::new("IPAddress", "10.96.1.1", CLUSTER)]);
		assert!(c.status, "{c:?}");
		// a ClusterIP Service as the Gateway's: the empty request is reported as Cluster
		let src = Sources {
			default: vec![StatusAddress::new("IPAddress", "10.96.0.10", CLUSTER)],
			dedicated: [(0, Ok("10.96.1.1".to_string()))].into(),
			..Default::default()
		};
		let (a, c) = assign(&[req("", CLUSTER), req("", "")], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "10.96.1.1", CLUSTER), StatusAddress::new("IPAddress", "10.96.0.10", CLUSTER)]);
		assert!(c.status);
	}

	#[test]
	fn static_cluster_value() {
		let mut src = lb_and_cluster();
		src.dedicated.insert(0, Ok("10.96.7.7".into()));
		let (a, c) = assign(&[req("10.96.7.7", CLUSTER)], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "10.96.7.7", CLUSTER)]);
		assert!(c.status);
		// outside the ServiceCIDR: the API server refused the Service
		src.dedicated.insert(0, Err("the API server refused its Service: provided IP is not in the valid range".into()));
		let (a, c) = assign(&[req("192.0.2.77", CLUSTER)], &src);
		assert!(a.is_empty());
		assert_eq!((c.status, c.reason.as_str()), (false, "NotAssigned"));
		assert!(c.message.contains("spec.addresses[0] (Cluster 192.0.2.77)"), "{}", c.message);
		// with a satisfiable empty request: partially assigned
		let (a, c) = assign(&[req("192.0.2.77", CLUSTER), req("", "")], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "203.0.113.5", "")]);
		assert_eq!((c.status, c.reason.as_str()), (false, "PartiallyAssigned"));
		assert!(c.message.contains("spec.addresses[0]") && !c.message.contains("spec.addresses[1]"), "{}", c.message);
	}

	#[test]
	fn sentinel_and_unknown() {
		let mut src = lb_and_cluster();
		src.dedicated.insert(0, Ok("10.96.2.2".into()));
		let (a, c) = assign(&[req("", SENTINEL), req("", "example.com/CorpWan"), req("", "testing.x-k8s.io/other")], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "10.96.2.2", SENTINEL)]);
		assert_eq!((c.status, c.reason.as_str()), (false, "PartiallyAssigned"));
		assert!(c.message.contains("example.com/CorpWan is not supported") && c.message.contains("testing.x-k8s.io/other"));
	}

	#[test]
	fn statics_and_the_fleet() {
		let src = Sources {
			default: vec![StatusAddress::new("IPAddress", "198.51.100.1", "")],
			statics: Ok(()),
			no_dedicated: Some("the rproxy fleet has no ClusterIP of a Gateway's own".into()),
			..Default::default()
		};
		let (a, c) = assign(&[req("192.0.2.10", ""), req("", CLUSTER)], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "192.0.2.10", "")]);
		assert_eq!(c.reason, "PartiallyAssigned");
		// two empty requests and one address: the second is not assigned
		let (a, c) = assign(&[req("", ""), req("", "")], &src);
		assert_eq!(a.len(), 1);
		assert_eq!(c.reason, "PartiallyAssigned");
		// a static value is not handed to an empty request as well
		let src = Sources { default: vec![StatusAddress::new("IPAddress", "192.0.2.10", "")], statics: Ok(()), ..Default::default() };
		let (a, c) = assign(&[req("", ""), req("192.0.2.10", "")], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "192.0.2.10", "")]);
		assert_eq!(c.reason, "PartiallyAssigned");
	}

	/// The fleet: the sentinel takes a default address, reported as the sentinel; a refused value is not assigned.
	#[test]
	fn fleet_sentinel_and_refused() {
		let src = Sources {
			default: vec![StatusAddress::new("IPAddress", "198.51.100.1", "")],
			statics: Ok(()),
			no_dedicated: Some("the rproxy fleet has no ClusterIP of a Gateway's own".into()),
			..Default::default()
		};
		let (a, c) = assign(&[req("", SENTINEL)], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "198.51.100.1", SENTINEL)]);
		assert!(c.status);
		let refused = Request { refused: Some("policy.clusterIPs".into()), ..req("10.96.0.200", CLUSTER) };
		assert!(!refused.dedicated());
		let (a, c) = assign(&[refused, req("", "")], &src);
		assert_eq!(a.len(), 1);
		assert!(c.message.contains("policy.clusterIPs"), "{}", c.message);
		// an unusable static address: the others are still assigned
		let src = Sources {
			statics: Err("0.0.0.0 cannot be used".into()),
			dedicated: [(1, Ok("10.96.3.3".to_string()))].into(),
			..Default::default()
		};
		let (a, c) = assign(&[req("0.0.0.0", ""), req("", CLUSTER)], &src);
		assert_eq!(a, vec![StatusAddress::new("IPAddress", "10.96.3.3", CLUSTER)]);
		assert_eq!(c.reason, "PartiallyAssigned");
	}

	#[test]
	fn service_names() {
		assert_eq!(req("", CLUSTER).service_suffix(3), "n3");
		let a = req("10.96.7.7", CLUSTER).service_suffix(0);
		assert!(a.len() == 8 && a.chars().all(|c| c.is_ascii_hexdigit()), "{a}");
		assert_eq!(a, req("10.96.7.7", SENTINEL).service_suffix(5), "by the value, not the place");
	}
}
