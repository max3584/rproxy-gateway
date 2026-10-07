//! Reading Kubernetes manifests (YAML or JSON) into a `World`, for
//! `rproxy-gateway render` and tests. The controller builds its `World` from
//! watches (`controller::cache`) through the same `World::insert`.

use serde_json::Value;

use crate::render::world::World;

impl World {
	/// Adds one object by its `apiVersion` and `kind`; unknown kinds are ignored.
	/// `Err` when a known kind does not have its shape.
	pub fn insert(&mut self, obj: Value) -> Result<(), String> {
		let api = obj["apiVersion"].as_str().unwrap_or_default().to_string();
		let kind = obj["kind"].as_str().unwrap_or_default().to_string();
		let group = api.rsplit_once('/').map(|(g, _)| g).unwrap_or("");
		let name =
			format!("{kind} {}/{}", obj["metadata"]["namespace"].as_str().unwrap_or(""), obj["metadata"]["name"].as_str().unwrap_or(""));
		let err = |e: serde_json::Error| format!("{name}: {e}");
		match (group, kind.as_str()) {
			(crate::k8s::gateway::GROUP, "GatewayClass") => self.classes.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "Gateway") => self.gateways.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "ListenerSet") => self.listener_sets.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "HTTPRoute") => self.http_routes.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "GRPCRoute") => {
				let g: crate::k8s::gateway::GrpcRoute = serde_json::from_value(obj).map_err(err)?;
				self.grpc_routes.push(g.to_http());
			}
			(crate::k8s::gateway::GROUP, "TLSRoute") => self.tls_routes.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "TCPRoute") => self.tcp_routes.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "UDPRoute") => self.udp_routes.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::gateway::GROUP, "ReferenceGrant") => self.grants.push(serde_json::from_value(obj).map_err(err)?),
			("", "Service") => {
				let s: k8s_openapi::api::core::v1::Service = serde_json::from_value(obj).map_err(err)?;
				self.services.insert(crate::render::world::key(&s.metadata), s);
			}
			("discovery.k8s.io", "EndpointSlice") => self.add_slice(serde_json::from_value(obj).map_err(err)?),
			("", "Secret") => {
				let mut obj = obj;
				// stringData, as kubectl apply would turn it into data
				if let Some(Value::Object(sd)) = obj.get("stringData").cloned() {
					use base64::Engine;
					let data = obj["data"].as_object().cloned().unwrap_or_default();
					let mut data = data;
					for (k, v) in sd {
						let b = base64::engine::general_purpose::STANDARD.encode(v.as_str().unwrap_or_default());
						data.insert(k, Value::String(b));
					}
					obj["data"] = Value::Object(data);
				}
				let s: k8s_openapi::api::core::v1::Secret = serde_json::from_value(obj).map_err(err)?;
				self.secrets.insert(crate::render::world::key(&s.metadata), s);
			}
			("", "ConfigMap") => {
				let c: k8s_openapi::api::core::v1::ConfigMap = serde_json::from_value(obj).map_err(err)?;
				self.config_maps.insert(crate::render::world::key(&c.metadata), c);
			}
			("", "Namespace") => {
				let n: k8s_openapi::api::core::v1::Namespace = serde_json::from_value(obj).map_err(err)?;
				self.namespaces.insert(n.metadata.name.clone().unwrap_or_default(), n.metadata.labels.unwrap_or_default());
			}
			(crate::k8s::crd::GROUP, "RproxyMiddleware") => {
				let m: crate::k8s::crd::RproxyMiddleware = serde_json::from_value(obj).map_err(err)?;
				self.middlewares.insert(crate::render::world::key(&m.metadata), m);
			}
			(crate::k8s::crd::GROUP, "RproxyPolicy") => self.policies.push(serde_json::from_value(obj).map_err(err)?),
			(crate::k8s::crd::GROUP, "RproxyRule") => self.raw_rules.push(serde_json::from_value(obj).map_err(err)?),
			("networking.k8s.io", "Ingress") => self.migration.ingresses.push(serde_json::from_value(obj).map_err(err)?),
			(g, "IngressRoute") if crate::k8s::traefik::GROUPS.contains(&g) => {
				self.migration.ingress_routes.push(serde_json::from_value(obj).map_err(err)?)
			}
			(g, "IngressRouteTCP") if crate::k8s::traefik::GROUPS.contains(&g) => {
				self.migration.ingress_routes_tcp.push(serde_json::from_value(obj).map_err(err)?)
			}
			(g, "IngressRouteUDP") if crate::k8s::traefik::GROUPS.contains(&g) => {
				self.migration.ingress_routes_udp.push(serde_json::from_value(obj).map_err(err)?)
			}
			(g, "Middleware") if crate::k8s::traefik::GROUPS.contains(&g) => {
				let m: crate::k8s::traefik::Middleware = serde_json::from_value(obj).map_err(err)?;
				self.migration.middlewares.insert(crate::render::world::key(&m.metadata), m);
			}
			(g, "TLSOption") if crate::k8s::traefik::GROUPS.contains(&g) => {
				let m: crate::k8s::traefik::TlsOption = serde_json::from_value(obj).map_err(err)?;
				self.migration.tls_options.insert(crate::render::world::key(&m.metadata), m);
			}
			_ => {}
		}
		Ok(())
	}

	/// Reads a YAML stream (or one JSON document, or a `List`).
	pub fn read_manifests(&mut self, text: &str) -> Result<(), String> {
		let docs: Vec<Value> = serde_saphyr::from_multiple(text).map_err(|e| e.to_string())?;
		for doc in docs {
			if doc.is_null() {
				continue;
			}
			if doc["kind"] == "List" {
				for item in doc["items"].as_array().cloned().unwrap_or_default() {
					self.insert(item)?;
				}
			} else {
				self.insert(doc)?;
			}
		}
		Ok(())
	}
}
