//! A snapshot of everything rendering reads, and lookups over it.

use std::collections::BTreeMap;

use k8s_openapi::api::core::v1::{ConfigMap, Secret, Service};
use k8s_openapi::api::discovery::v1::EndpointSlice;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::{LabelSelector, ObjectMeta};

use crate::k8s::crd::{RproxyMiddleware, RproxyPolicy, RproxyRule};
use crate::k8s::gateway::{Gateway, GatewayClass, HttpRoute, L4Route, ReferenceGrant};
use crate::render::migrate::MigrationInput;

/// (namespace, name)
pub type Key = (String, String);

pub fn key(meta: &ObjectMeta) -> Key {
	(meta.namespace.clone().unwrap_or_default(), meta.name.clone().unwrap_or_default())
}

#[derive(Clone, Debug, Default)]
pub struct World {
	pub classes: Vec<GatewayClass>,
	pub gateways: Vec<Gateway>,
	pub http_routes: Vec<HttpRoute>,
	pub tls_routes: Vec<L4Route>,
	pub tcp_routes: Vec<L4Route>,
	pub udp_routes: Vec<L4Route>,
	pub grants: Vec<ReferenceGrant>,
	pub services: BTreeMap<Key, Service>,
	/// Keyed by (namespace, Service name).
	pub slices: BTreeMap<Key, Vec<EndpointSlice>>,
	pub secrets: BTreeMap<Key, Secret>,
	/// ConfigMaps with a `ca.crt` (CA certificates).
	pub config_maps: BTreeMap<Key, ConfigMap>,
	/// Namespace labels.
	pub namespaces: BTreeMap<String, BTreeMap<String, String>>,
	pub middlewares: BTreeMap<Key, RproxyMiddleware>,
	pub policies: Vec<RproxyPolicy>,
	pub raw_rules: Vec<RproxyRule>,
	/// Ingress and Traefik resources (only when migration is on).
	pub migration: MigrationInput,
}

impl World {
	pub fn add_slice(&mut self, slice: EndpointSlice) {
		let ns = slice.metadata.namespace.clone().unwrap_or_default();
		let Some(svc) = slice.metadata.labels.as_ref().and_then(|l| l.get("kubernetes.io/service-name")).cloned() else {
			return;
		};
		self.slices.entry((ns, svc)).or_default().push(slice);
	}

	/// Whether a ReferenceGrant in `to_ns` lets objects of (`from_group`, `from_kind`) in
	/// `from_ns` refer to (`to_group`, `to_kind`, `to_name`). Same namespace needs no grant.
	pub fn granted(&self, from: (&str, &str, &str), to: (&str, &str, &str, &str)) -> bool {
		let (from_group, from_kind, from_ns) = from;
		let (to_group, to_kind, to_ns, to_name) = to;
		if from_ns == to_ns {
			return true;
		}
		self.grants.iter().filter(|g| g.metadata.namespace.as_deref() == Some(to_ns)).any(|g| {
			g.spec.from.iter().any(|f| f.group == from_group && f.kind == from_kind && f.namespace == from_ns)
				&& g.spec.to.iter().any(|t| t.group == to_group && t.kind == to_kind && t.name.as_deref().is_none_or(|n| n == to_name))
		})
	}

	pub fn namespace_matches(&self, ns: &str, selector: &LabelSelector) -> bool {
		let empty = BTreeMap::new();
		let labels = self.namespaces.get(ns).unwrap_or(&empty);
		selector_matches(selector, labels)
	}
}

/// A Kubernetes label selector against labels.
pub fn selector_matches(selector: &LabelSelector, labels: &BTreeMap<String, String>) -> bool {
	if let Some(ml) = &selector.match_labels {
		if ml.iter().any(|(k, v)| labels.get(k) != Some(v)) {
			return false;
		}
	}
	for e in selector.match_expressions.iter().flatten() {
		let values = e.values.clone().unwrap_or_default();
		let have = labels.get(&e.key);
		let ok = match e.operator.as_str() {
			"In" => have.is_some_and(|v| values.contains(v)),
			"NotIn" => have.is_none_or(|v| !values.contains(v)),
			"Exists" => have.is_some(),
			"DoesNotExist" => have.is_none(),
			_ => false,
		};
		if !ok {
			return false;
		}
	}
	true
}

#[cfg(test)]
mod tests {
	use super::*;
	use k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelectorRequirement;

	#[test]
	fn selectors() {
		let labels: BTreeMap<String, String> = [("env".to_string(), "prod".to_string())].into();
		let mut s = LabelSelector { match_labels: Some(labels.clone()), ..Default::default() };
		assert!(selector_matches(&s, &labels));
		assert!(!selector_matches(&s, &BTreeMap::new()));
		s.match_expressions = Some(vec![LabelSelectorRequirement { key: "team".into(), operator: "DoesNotExist".into(), values: None }]);
		assert!(selector_matches(&s, &labels));
		assert!(selector_matches(&LabelSelector::default(), &BTreeMap::new()));
	}
}
