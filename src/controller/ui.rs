//! What the UI reads (docs/DESIGN-v0.4.x.md 4., docs/SECURITY.md): with `--ui-namespace`, the
//! controller writes one Secret there, `rproxy-ui-discovery`, listing the rproxy pods of the
//! Gateways the UI may see (the parameters' `ui.visible` not false), the CA certificate of their
//! control API, and for each Gateway a token rproxy accepts for reading only (`rules:read`,
//! `metrics:read`; derived from the master token apart from the controller's token).
//!
//! - `nodes.yaml`: `nodes` (one per pod: `k8s:<ns>/<gateway>/<pod>`, its `https://<pod IP>:9443`,
//!   the name in its control API certificate, the CA and token files) and `groups` (one per
//!   Gateway, `k8s:<ns>/<gateway>`). Fleet: the group `k8s:fleet`.
//! - `ca.crt`: the CA's certificate (never its key).
//! - `token-<id>` (`token-fleet`): the UI's token of that Gateway (the fleet).
//!
//! Only pods that take the UI's token are listed (`provision::ui_pods`, `accepting`): Ready (and their
//! rule set applied), not being deleted, made from the pod template with the current token file (rproxy
//! v0.4.1 reads it at start), and seen to take the token (rproxy v0.4.2 reads a changed token file
//! again, a little after the kubelet updates the mounted Secret; a pod answering 401 again and again
//! would lock the UI out). The last
//! interval of a stopping pod's usage is not collected (design 10. Q16). The Secret is written only
//! when its content changes, and deleted when nothing is visible.

use std::collections::BTreeMap;

use base64::Engine;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use kube::Api;
use kube::api::{DeleteParams, Patch, PatchParams};
use tracing::info;

use crate::controller::bootstrap;
use crate::controller::provision::{API_PORT, Endpoint, MANAGER};
use crate::render::GatewayPlan;
use crate::rproxy::client::{API_SERVER_NAME, Client, server_name_for};

/// The Secret in the UI's namespace.
pub const DISCOVERY_SECRET: &str = "rproxy-ui-discovery";

/// The UI's group of one Gateway (or of the fleet).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Group {
	/// `k8s:<ns>/<gateway>` or `k8s:fleet`; its nodes are `<name>/<pod>`.
	pub name: String,
	/// The name in the pods' control API certificate.
	pub server_name: String,
	/// The key of its token in the Secret.
	pub token_key: String,
	pub token: String,
	/// (pod name, pod IP), by pod name.
	pub pods: Vec<(String, String)>,
}

/// Whether the UI may see a Gateway: its parameters are valid and do not set `ui.visible: false`.
pub fn visible(plan: &GatewayPlan) -> bool {
	plan.parameters_error.is_none() && plan.parameters.as_ref().and_then(|p| p.ui.as_ref()).and_then(|u| u.visible) != Some(false)
}

fn pod_list(pods: &[Endpoint]) -> Vec<(String, String)> {
	let mut out: Vec<(String, String)> = pods.iter().map(|e| (e.pod.clone(), e.ip.clone())).collect();
	out.sort();
	out.dedup();
	out
}

/// A managed Gateway's group (`None` without a pod to read).
pub fn managed_group(ns: &str, name: &str, id: &str, master: &str, pods: &[Endpoint]) -> Option<Group> {
	let pods = pod_list(pods);
	(!pods.is_empty()).then(|| Group {
		name: format!("k8s:{ns}/{name}"),
		server_name: server_name_for(id),
		token_key: format!("token-{id}"),
		token: bootstrap::derive_ui_token(master, id),
		pods,
	})
}

/// The fleet's group (`None` without a pod to read).
pub fn fleet_group(master: &str, pods: &[Endpoint]) -> Option<Group> {
	let pods = pod_list(pods);
	(!pods.is_empty()).then(|| Group {
		name: "k8s:fleet".into(),
		server_name: API_SERVER_NAME.into(),
		token_key: "token-fleet".into(),
		token: bootstrap::derive_ui_token(master, "fleet"),
		pods,
	})
}

/// Pods asked whether they take the UI's token: (pod uid, the token's hash) → taken, or when last asked.
static ASKED: std::sync::Mutex<BTreeMap<(String, String), Result<(), std::time::Instant>>> = std::sync::Mutex::new(BTreeMap::new());

/// How soon a pod that did not take the UI's token yet is asked again: rproxy locks a source out after
/// 20 failed tokens in a minute (`RPROXY_API_LOCKOUT_FAILURES`), and reads a changed token file within
/// 10 s (`RPROXY_TOKENS_CHECK_SECS`) of the kubelet updating it.
pub const ASK_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// Whether to ask a pod now (`None`: never asked).
pub fn ask_now(asked: Option<&Result<(), std::time::Instant>>, now: std::time::Instant) -> bool {
	match asked {
		None => true,
		Some(Ok(())) => false,
		Some(Err(t)) => now.saturating_duration_since(*t) >= ASK_EVERY,
	}
}

/// The pods of `pods` that take the UI's `token` (`GET /rules` answers 200), asking each at most
/// every `ASK_EVERY` until it does. Pods gone are forgotten (`forget`).
pub async fn accepting(rp: &Client, pods: &[Endpoint], token: &str, server_name: &str) -> Vec<Endpoint> {
	let ui = rp.with_token(token, server_name);
	let hash = crate::pem::short_hash(token.as_bytes());
	let mut out = vec![];
	for ep in pods {
		let key = (ep.uid.clone(), hash.clone());
		let now = std::time::Instant::now();
		let asked = ASKED.lock().unwrap().get(&key).cloned();
		if asked == Some(Ok(())) {
			out.push(ep.clone());
			continue;
		}
		if !ask_now(asked.as_ref(), now) {
			continue;
		}
		let Ok(addr) = format!("{}:{}", if ep.ip.contains(':') { format!("[{}]", ep.ip) } else { ep.ip.clone() }, ep.api_port).parse()
		else {
			continue;
		};
		let took = matches!(ui.can_read(addr).await, Ok(true));
		ASKED.lock().unwrap().insert(key, if took { Ok(()) } else { Err(now) });
		if took {
			out.push(ep.clone());
		} else {
			tracing::debug!(pod = ep.pod, "the pod does not take the UI's token yet");
		}
	}
	out
}

/// Forgets pods that are gone (their uids not in `live`).
pub fn forget(live: &std::collections::BTreeSet<String>) {
	ASKED.lock().unwrap().retain(|(uid, _), _| live.contains(uid));
}

/// A YAML scalar (a JSON string is one).
fn q(s: &str) -> String {
	serde_json::to_string(s).unwrap_or_default()
}

fn url(ip: &str) -> String {
	if ip.contains(':') { format!("https://[{ip}]:{API_PORT}") } else { format!("https://{ip}:{API_PORT}") }
}

/// `nodes.yaml` (groups by name, their pods by name).
pub fn nodes_yaml(groups: &[Group]) -> String {
	let mut groups: Vec<&Group> = groups.iter().collect();
	groups.sort_by(|a, b| a.name.cmp(&b.name));
	let mut out =
		String::from("# written by rproxy-gateway: rproxy pods the UI may read (token scopes rules:read, metrics:read). Do not edit.\n");
	out.push_str("nodes:");
	if groups.is_empty() {
		out.push_str(" []");
	}
	out.push('\n');
	for g in &groups {
		for (pod, ip) in &g.pods {
			out.push_str(&format!("  - name: {}\n", q(&format!("{}/{pod}", g.name))));
			out.push_str(&format!("    url: {}\n", q(&url(ip))));
			out.push_str(&format!("    tls_server_name: {}\n", q(&g.server_name)));
			out.push_str("    tls_ca: ca.crt\n");
			out.push_str(&format!("    token_file: {}\n", q(&g.token_key)));
			out.push_str("    readonly: true\n");
		}
	}
	out.push_str("groups:");
	if groups.is_empty() {
		out.push_str(" []");
	}
	out.push('\n');
	for g in &groups {
		out.push_str(&format!("  - name: {}\n", q(&g.name)));
		let nodes: Vec<String> = g.pods.iter().map(|(pod, _)| q(&format!("{}/{pod}", g.name))).collect();
		out.push_str(&format!("    nodes: [{}]\n", nodes.join(", ")));
		out.push_str("    readonly: true\n");
	}
	out
}

/// The Secret's data.
pub fn discovery_data(groups: &[Group], ca_pem: &[u8]) -> BTreeMap<String, Vec<u8>> {
	let mut data: BTreeMap<String, Vec<u8>> =
		[("nodes.yaml".to_string(), nodes_yaml(groups).into_bytes()), ("ca.crt".to_string(), ca_pem.to_vec())].into();
	for g in groups {
		data.insert(g.token_key.clone(), g.token.clone().into_bytes());
	}
	data
}

fn labels() -> BTreeMap<String, String> {
	[
		("app.kubernetes.io/managed-by".to_string(), MANAGER.to_string()),
		("app.kubernetes.io/component".to_string(), "ui-discovery".to_string()),
	]
	.into()
}

/// The discovery Secret.
pub fn discovery_secret(ns: &str, data: &BTreeMap<String, Vec<u8>>) -> Secret {
	Secret {
		metadata: ObjectMeta {
			name: Some(DISCOVERY_SECRET.into()),
			namespace: Some(ns.into()),
			labels: Some(labels()),
			..Default::default()
		},
		type_: Some("Opaque".into()),
		data: Some(data.iter().map(|(k, v)| (k.clone(), ByteString(v.clone()))).collect()),
		..Default::default()
	}
}

/// Writes the discovery Secret in `ns` when it changes; deletes it (ours only) when no group is left.
pub async fn apply(client: &kube::Client, ns: &str, groups: &[Group], ca_pem: &[u8]) -> anyhow::Result<()> {
	let api: Api<Secret> = Api::namespaced(client.clone(), ns);
	let current = api.get_opt(DISCOVERY_SECRET).await?;
	let ours =
		|s: &Secret| s.metadata.labels.as_ref().and_then(|l| l.get("app.kubernetes.io/managed-by")).map(String::as_str) == Some(MANAGER);
	if groups.is_empty() {
		if current.as_ref().is_some_and(ours) {
			info!(namespace = ns, secret = DISCOVERY_SECRET, "no Gateway is shown to the UI: deleting its discovery Secret");
			api.delete(DISCOVERY_SECRET, &DeleteParams::default()).await?;
		}
		return Ok(());
	}
	let data = discovery_data(groups, ca_pem);
	let have: Option<BTreeMap<String, Vec<u8>>> =
		current.as_ref().map(|s| s.data.clone().unwrap_or_default().into_iter().map(|(k, v)| (k, v.0)).collect());
	if current.as_ref().is_some_and(ours) && have.as_ref() == Some(&data) {
		return Ok(());
	}
	info!(namespace = ns, secret = DISCOVERY_SECRET, gateways = groups.len(), "writing the UI's discovery Secret");
	// a Secret holds exactly these keys (an apply would keep keys it no longer lists only if
	// another manager owned them; ours are all ours)
	api.patch(DISCOVERY_SECRET, &PatchParams::apply(MANAGER).force(), &Patch::Apply(&discovery_secret(ns, &data))).await?;
	Ok(())
}

/// Fleet: the UI's token entry in the fleet's token file (`rproxy-gateway-token`), added or removed.
pub async fn apply_fleet_token(client: &kube::Client, ns: &str, master: &str, shown: bool) -> anyhow::Result<()> {
	let api: Api<Secret> = Api::namespaced(client.clone(), ns);
	let Some(s) = api.get_opt(bootstrap::TOKEN_SECRET).await? else { return Ok(()) };
	let mut want = bootstrap::token_file(master, bootstrap::FLEET_RULESETS);
	if shown {
		want.push_str(&bootstrap::ui_token_entry(&bootstrap::derive_ui_token(master, "fleet")));
	}
	let have = s.data.as_ref().and_then(|d| d.get("tokens.yaml")).map(|b| b.0.clone());
	if have.as_deref() == Some(want.as_bytes()) {
		return Ok(());
	}
	info!(secret = bootstrap::TOKEN_SECRET, ui = shown, "writing the fleet's token file (the UI's read-only token)");
	let patch = serde_json::json!({"data": {"tokens.yaml": base64::engine::general_purpose::STANDARD.encode(want.as_bytes())}});
	api.patch(bootstrap::TOKEN_SECRET, &PatchParams::default(), &Patch::Merge(&patch)).await?;
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn asks_again_later() {
		let t0 = std::time::Instant::now();
		assert!(ask_now(None, t0));
		assert!(!ask_now(Some(&Ok(())), t0 + ASK_EVERY * 10), "taken: not asked again");
		assert!(!ask_now(Some(&Err(t0)), t0 + ASK_EVERY / 2));
		assert!(ask_now(Some(&Err(t0)), t0 + ASK_EVERY));
	}

	fn ep(pod: &str, ip: &str) -> Endpoint {
		Endpoint {
			namespace: "team-a".into(),
			pod: pod.into(),
			uid: format!("uid-{pod}"),
			ip: ip.into(),
			host_ip: None,
			api_port: API_PORT,
			certsync_port: 9444,
			certs: None,
			target: Some("team-a-web-abc123".into()),
			rproxy_image: None,
		}
	}

	#[test]
	fn the_secret_lists_pods_read_only_and_holds_no_key() {
		let g = managed_group("team-a", "web", "team-a-web-abc123", "master", &[ep("rproxy-b", "fd00::5"), ep("rproxy-a", "10.0.0.5")])
			.unwrap();
		let fleet = fleet_group("master", &[ep("fleet-x", "192.0.2.1")]).unwrap();
		let data = discovery_data(&[g.clone(), fleet.clone()], b"-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n");
		assert_eq!(data.keys().cloned().collect::<Vec<_>>(), ["ca.crt", "nodes.yaml", "token-fleet", "token-team-a-web-abc123"]);
		assert!(data.values().all(|v| !String::from_utf8_lossy(v).contains("PRIVATE KEY")), "no key");
		assert_eq!(data["token-team-a-web-abc123"], bootstrap::derive_ui_token("master", "team-a-web-abc123").into_bytes());
		assert_ne!(
			data["token-team-a-web-abc123"],
			bootstrap::derive_token("master", "team-a-web-abc123").into_bytes(),
			"not the controller's"
		);
		let doc: serde_json::Value = serde_saphyr::from_str(&String::from_utf8(data["nodes.yaml"].clone()).unwrap()).unwrap();
		let nodes = doc["nodes"].as_array().unwrap();
		assert_eq!(nodes.len(), 3);
		// groups by name (k8s:fleet first), pods by name
		assert_eq!(nodes[0]["name"], "k8s:fleet/fleet-x");
		assert_eq!(nodes[0]["tls_server_name"], API_SERVER_NAME);
		assert_eq!(nodes[0]["token_file"], "token-fleet");
		assert_eq!(nodes[1]["name"], "k8s:team-a/web/rproxy-a");
		assert_eq!(nodes[1]["url"], "https://10.0.0.5:9443");
		assert_eq!(nodes[1]["tls_server_name"], "team-a-web-abc123.rproxy-api.rproxy-gateway.internal");
		assert_eq!(nodes[1]["tls_ca"], "ca.crt");
		assert_eq!(nodes[1]["token_file"], "token-team-a-web-abc123");
		assert_eq!(nodes[1]["readonly"], true);
		assert_eq!(nodes[2]["url"], "https://[fd00::5]:9443");
		let groups = doc["groups"].as_array().unwrap();
		assert_eq!(groups[1]["name"], "k8s:team-a/web");
		assert_eq!(groups[1]["nodes"], serde_json::json!(["k8s:team-a/web/rproxy-a", "k8s:team-a/web/rproxy-b"]));
		assert_eq!(groups[1]["readonly"], true);
		// the same pods in another order: the same content (no write)
		let again = managed_group("team-a", "web", "team-a-web-abc123", "master", &[ep("rproxy-a", "10.0.0.5"), ep("rproxy-b", "fd00::5")]);
		assert_eq!(discovery_data(&[fleet, again.unwrap()], b"-----BEGIN CERTIFICATE-----\nCA\n-----END CERTIFICATE-----\n"), data);
		// no pod: no group
		assert!(managed_group("team-a", "web", "x", "master", &[]).is_none());
		let empty: serde_json::Value = serde_saphyr::from_str(&nodes_yaml(&[])).unwrap();
		assert_eq!(empty, serde_json::json!({"nodes": [], "groups": []}));
	}

	#[test]
	fn hidden_gateways_are_not_shown() {
		let mut plan = GatewayPlan::default();
		assert!(visible(&plan), "shown by default");
		plan.parameters = Some(serde_json::from_value(serde_json::json!({"ui": {"visible": false}})).unwrap());
		assert!(!visible(&plan));
		plan.parameters = Some(serde_json::from_value(serde_json::json!({"ui": {"visible": true}})).unwrap());
		assert!(visible(&plan));
		plan.parameters_error = Some("bad".into());
		assert!(!visible(&plan), "invalid parameters: not shown");
	}
}
