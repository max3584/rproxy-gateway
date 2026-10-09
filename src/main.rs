//! rproxy-gateway: a Kubernetes Gateway API controller for rproxy.
//!
//! The controller turns Gateway API resources (and, for migration, Ingress and
//! Traefik CRDs) into rproxy rule sets and applies them through rproxy's control
//! API (`PUT /rulesets/{name}`). See README.md and docs/DESIGN.md.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use rproxy_gateway::controller::provision::{Fleet, Managed, Mode, ProbeTiming};
use rproxy_gateway::{certsync, controller, k8s, render};

#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {
	/// Log format.
	#[arg(long, env = "RPROXY_GATEWAY_LOG_FORMAT", default_value = "text", value_parser = ["text", "json"], global = true)]
	log_format: String,
	#[command(subcommand)]
	command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
	/// Run the controller.
	Controller(Box<ControllerArgs>),
	/// Write certificate Secrets into rproxy's certificate directory (runs next to rproxy).
	Certsync(certsync::Args),
	/// Print rproxy-gateway's CRDs (RproxyMiddleware, RproxyPolicy, RproxyRule, RproxyGatewayParameters) as YAML.
	Crds,
	/// Render manifests (YAML files) into rule sets without a cluster: what the controller would PUT.
	Render(RenderArgs),
}

#[derive(clap::Args, Debug)]
struct ControllerArgs {
	/// GatewayClass `spec.controllerName` handled by this controller.
	#[arg(long, env = "RPROXY_GATEWAY_CONTROLLER_NAME", default_value = "rproxy.max3584.net/gateway-controller")]
	controller_name: String,
	/// The controller's namespace (its Secrets; managed rproxy pods).
	#[arg(long, env = "POD_NAMESPACE", default_value = "rproxy-gateway-system")]
	namespace: String,
	/// `managed`: one rproxy Deployment and Service per Gateway; `fleet`: rproxy pods deployed beforehand serve all Gateways.
	#[arg(long, env = "RPROXY_GATEWAY_MODE", default_value = "managed", value_parser = ["managed", "fleet"])]
	mode: String,
	/// managed: the rproxy image.
	#[arg(long, env = "RPROXY_GATEWAY_RPROXY_IMAGE", default_value = rproxy_gateway::controller::provision::RPROXY_IMAGE)]
	rproxy_image: String,
	/// managed: this controller's image (runs certsync next to rproxy).
	#[arg(long, env = "RPROXY_GATEWAY_IMAGE", default_value = concat!("ghcr.io/max3584/rproxy-gateway:", env!("CARGO_PKG_VERSION")))]
	controller_image: String,
	/// managed: rproxy pods per Gateway.
	#[arg(long, env = "RPROXY_GATEWAY_REPLICAS", default_value_t = 1)]
	replicas: i32,
	/// managed: the type of each Gateway's Service.
	#[arg(long, env = "RPROXY_GATEWAY_SERVICE_TYPE", default_value = "LoadBalancer")]
	service_type: String,
	/// managed: make a NetworkPolicy per Gateway so only the controller reaches rproxy's control API and certsync.
	#[arg(long, env = "RPROXY_GATEWAY_NETWORK_POLICY", default_value_t = true, action = clap::ArgAction::Set)]
	network_policy: bool,
	/// managed: `externalTrafficPolicy` of each Gateway's Service: `Local` keeps clients' addresses
	/// (only nodes with a ready rproxy pod take traffic), `Cluster` lets every node forward (smoother
	/// failover, clients' addresses lost). Unset: `Local` for LoadBalancer, `Cluster` for NodePort.
	#[arg(long, env = "RPROXY_GATEWAY_EXTERNAL_TRAFFIC_POLICY", value_parser = ["Local", "Cluster"])]
	external_traffic_policy: Option<String>,
	/// managed: LoadBalancer Services get node ports (`allocateLoadBalancerNodePorts`).
	#[arg(long, env = "RPROXY_GATEWAY_ALLOCATE_LOAD_BALANCER_NODE_PORTS", default_value_t = true, action = clap::ArgAction::Set)]
	allocate_load_balancer_node_ports: bool,
	/// managed: seconds a stopping rproxy pod keeps serving (preStop) before SIGTERM while it leaves the
	/// Service's endpoints and load balancers; 0: none. Unset: none for rproxy with a graceful shutdown
	/// (the shutdown delay does it), 15 for older rproxy images (terminationGracePeriodSeconds 15 more).
	/// Set: this preStop for every pod, before the shutdown delay and drain.
	#[arg(long, env = "RPROXY_GATEWAY_PRE_STOP_SECS")]
	pre_stop_secs: Option<u32>,
	/// managed, rproxy with a graceful shutdown (`features.graceful_shutdown`, v0.4.1): after SIGTERM,
	/// rproxy keeps accepting this long while `/readyz` says `draining` and load balancers move away
	/// (RPROXY_SHUTDOWN_DELAY; it replaces the preStop). The default of the parameters'
	/// `rproxy.shutdown.delay`. `15s`, `250ms`, `1m`; at most 10m.
	#[arg(long, env = "RPROXY_GATEWAY_SHUTDOWN_DELAY", default_value = "15s", value_parser = shutdown_duration)]
	shutdown_delay: Duration,
	/// managed: then rproxy closes its listeners and lets connections end for up to this long
	/// (RPROXY_SHUTDOWN_DRAIN). The default of the parameters' `rproxy.shutdown.drain`.
	/// terminationGracePeriodSeconds is the delay, the drain and 5 more.
	#[arg(long, env = "RPROXY_GATEWAY_SHUTDOWN_DRAIN", default_value = "25s", value_parser = shutdown_duration)]
	shutdown_drain: Duration,
	/// managed, rproxy with a graceful shutdown: the path of its readiness probe: `/readyz` (also not
	/// ready while restoring at start and while shutting down: the endpoint stops serving during the
	/// delay) or `/healthz` (the process answers). Older images: `/healthz`.
	#[arg(long, env = "RPROXY_GATEWAY_READINESS_PATH", default_value = "/readyz", value_parser = ["/healthz", "/readyz"])]
	readiness_path: String,
	/// managed: rproxy's readiness probe, `periodSeconds=2,timeoutSeconds=1,failureThreshold=2,successThreshold=1,initialDelaySeconds=0`
	/// (these are the defaults; any of them).
	#[arg(long, env = "RPROXY_GATEWAY_READINESS_PROBE", default_value = "", value_parser = |s: &str| ProbeTiming::parse(s, ProbeTiming::READINESS))]
	readiness_probe: ProbeTiming,
	/// managed: rproxy's liveness probe (same form; defaults `periodSeconds=5,timeoutSeconds=1,failureThreshold=3`).
	#[arg(long, env = "RPROXY_GATEWAY_LIVENESS_PROBE", default_value = "", value_parser = |s: &str| ProbeTiming::parse(s, ProbeTiming::LIVENESS))]
	liveness_probe: ProbeTiming,
	/// managed: imagePullPolicy of rproxy pods.
	#[arg(long, env = "RPROXY_GATEWAY_IMAGE_PULL_POLICY", default_value = "IfNotPresent")]
	image_pull_policy: String,
	/// fleet: label selector of the rproxy pods (in the controller's namespace).
	#[arg(long, env = "RPROXY_GATEWAY_FLEET_SELECTOR", default_value = "app.kubernetes.io/name=rproxy,app.kubernetes.io/component=fleet")]
	fleet_selector: String,
	/// fleet: addresses written to Gateway status (default: the pods' host IPs).
	#[arg(long, env = "RPROXY_GATEWAY_FLEET_ADDRESS", value_delimiter = ',')]
	fleet_address: Vec<String>,
	/// fleet: where Gateways' rules listen. `wildcard` (--listen-addr: every Gateway's port on every
	/// address) or `addresses` (a Gateway with spec.addresses, inside --address-cidr, on those only, so
	/// Gateways on different addresses can use the same port; rproxy with features.listen_freebind, v0.4.3).
	#[arg(long, env = "RPROXY_GATEWAY_FLEET_LISTEN", default_value = "wildcard", value_parser = ["wildcard", "addresses"])]
	fleet_listen: String,
	/// rproxy's passive health checks (`outlier_detection` of `http` services, rproxy's L7 keys as
	/// `key=value,...`) every HTTPRoute / GRPCRoute backend gets unless an RproxyPolicy sets them. "": none.
	#[arg(long, env = "RPROXY_GATEWAY_BACKEND_OUTLIER_HTTP", default_value = "consecutive_gateway_failures=3,consecutive_5xx=0,ejection_time=10s,max_ejection_time=1m,max_ejected_percent=50", value_parser = key_values)]
	backend_outlier_http: serde_json::Value,
	/// The same for L4 rules (TCP, TLS, UDP routes; rproxy's L4 shape).
	#[arg(long, env = "RPROXY_GATEWAY_BACKEND_OUTLIER_L4", default_value = "consecutive_failures=1,ejection_time=10s,max_ejection_time=1m", value_parser = key_values)]
	backend_outlier_l4: serde_json::Value,
	/// How long rproxy may take to connect to an `http` backend (`timeouts.connect`). "": rproxy's 5 s.
	#[arg(long, env = "RPROXY_GATEWAY_BACKEND_CONNECT_TIMEOUT_HTTP", default_value = "1s", value_parser = duration_opt)]
	backend_connect_timeout_http: String,
	/// How long rproxy may take to connect to an L4 tcp backend before trying the next (`connect_timeout`,
	/// rproxy v0.4.3). "": rproxy's (5 s with other backends, else the OS's).
	#[arg(long, env = "RPROXY_GATEWAY_BACKEND_CONNECT_TIMEOUT_L4", default_value = "1s", value_parser = duration_opt)]
	backend_connect_timeout_l4: String,
	/// Addresses rproxy rules listen on (the first is listen_addr, the rest extra_listen_addrs).
	#[arg(long, env = "RPROXY_GATEWAY_LISTEN_ADDR", value_delimiter = ',', default_value = "0.0.0.0")]
	listen_addr: Vec<String>,
	/// Seconds between full passes (also how soon an rproxy restart is noticed without a pod event).
	#[arg(long, env = "RPROXY_GATEWAY_RESYNC_SECS", default_value_t = 30)]
	resync_secs: u64,
	/// Where `/healthz` is answered.
	#[arg(long, env = "RPROXY_GATEWAY_HEALTH_ADDR", default_value = "0.0.0.0:8081")]
	health_addr: SocketAddr,
	/// IP ranges Gateways' `spec.addresses` may take (managed mode: the Service's
	/// `externalIPs`). Empty: static addresses are off. Never include the Service or pod ranges.
	#[arg(long, env = "RPROXY_GATEWAY_ADDRESS_CIDR", value_delimiter = ',')]
	address_cidr: Vec<rproxy_gateway::render::Cidr>,
	/// Let ExternalName Services be backends (they can name anything: the API server, other namespaces).
	#[arg(long, env = "RPROXY_GATEWAY_ALLOW_EXTERNAL_NAME_SERVICES")]
	allow_external_name_services: bool,
	/// Annotation prefixes of `spec.infrastructure.annotations` let onto the Service even if
	/// they steer addresses or load balancers (`metallb.universe.tf/`, `service.beta.kubernetes.io/`, ...).
	#[arg(long, env = "RPROXY_GATEWAY_SERVICE_ANNOTATION_PREFIX", value_delimiter = ',')]
	service_annotation_prefix: Vec<String>,
	/// Watch only these namespaces (and the controller's own); empty: all. With a list, the chart
	/// gives the controller Roles in those namespaces instead of a ClusterRole for namespaced objects.
	#[arg(long, env = "RPROXY_GATEWAY_WATCH_NAMESPACES", value_delimiter = ',')]
	watch_namespaces: Vec<String>,
	/// Let certificateRefs (and the backend client certificate) name Secrets of other namespaces when
	/// a ReferenceGrant allows it. Managed mode copies those keys into the Gateway's namespace.
	#[arg(long, env = "RPROXY_GATEWAY_CROSS_NAMESPACE_SECRETS", default_value_t = true, action = clap::ArgAction::Set)]
	cross_namespace_secrets: bool,
	/// fleet: read RproxyRules (off by default: fleet pods are shared by every Gateway).
	#[arg(long, env = "RPROXY_GATEWAY_FLEET_RPROXY_RULES")]
	fleet_rproxy_rules: bool,
	/// The UI's namespace (TCP-UDP-rproxy-ui's chart): the controller writes the Secret
	/// `rproxy-ui-discovery` there with the rproxy pods of the Gateways shown to the UI (their
	/// parameters' `ui.visible`), the control API's CA certificate and read-only tokens
	/// (`rules:read`, `metrics:read`). Empty: off (docs/SECURITY.md).
	#[arg(long, env = "RPROXY_GATEWAY_UI_NAMESPACE", default_value = "")]
	ui_namespace: String,
	/// The labels of the UI's pods (`k=v,k2=v2`): the Gateways' NetworkPolicy lets them reach the control API.
	#[arg(long, env = "RPROXY_GATEWAY_UI_POD_SELECTOR", default_value = "app.kubernetes.io/name=rproxy-ui,app.kubernetes.io/component=ui")]
	ui_pod_selector: String,
	/// Leader election: run several replicas, one of which acts (a Lease in the controller's namespace).
	#[arg(long, env = "RPROXY_GATEWAY_LEADER_ELECT", default_value_t = true, action = clap::ArgAction::Set)]
	leader_elect: bool,
	/// The Lease's name.
	#[arg(long, env = "RPROXY_GATEWAY_LEADER_LEASE", default_value = "rproxy-gateway")]
	leader_lease: String,
	/// This replica's name in the Lease (default: the pod name, `POD_NAME`, else the host name).
	#[arg(long, env = "POD_NAME")]
	leader_identity: Option<String>,
	#[command(flatten)]
	migration: MigrationArgs,
}

/// Migration from Ingress and Traefik (read only).
#[derive(clap::Args, Debug)]
struct MigrationArgs {
	/// Read Ingress (of `--ingress-class`) and Traefik IngressRoute* / Middleware / TLSOption into this Gateway's rule set (`namespace/name`).
	#[arg(long, env = "RPROXY_GATEWAY_MIGRATE_TO")]
	migrate_to: Option<String>,
	/// The Ingress class read.
	#[arg(long, env = "RPROXY_GATEWAY_INGRESS_CLASS", default_value = "rproxy")]
	ingress_class: String,
	/// Let Ingress and Traefik objects refer to services, middlewares and TLS options in other
	/// namespaces without a ReferenceGrant (Traefik's `allowCrossNamespace`; off by default).
	#[arg(long, env = "RPROXY_GATEWAY_MIGRATION_ALLOW_CROSS_NAMESPACE")]
	migration_allow_cross_namespace: bool,
	/// Traefik entry points: `name=port[/udp]` (default `web=80`, `websecure=443`).
	#[arg(long = "traefik-entrypoint", env = "RPROXY_GATEWAY_TRAEFIK_ENTRYPOINTS", value_delimiter = ',')]
	traefik_entrypoints: Vec<String>,
}

impl MigrationArgs {
	fn settings(&self) -> anyhow::Result<Option<render::migrate::Settings>> {
		let Some(to) = &self.migrate_to else { return Ok(None) };
		let (ns, name) = to.split_once('/').ok_or_else(|| anyhow::anyhow!("--migrate-to: namespace/name"))?;
		let entry_points = if self.traefik_entrypoints.is_empty() {
			render::migrate::Settings::default_entry_points()
		} else {
			render::migrate::Settings::parse_entry_points(&self.traefik_entrypoints).map_err(anyhow::Error::msg)?
		};
		Ok(Some(render::migrate::Settings {
			gateway: (ns.to_string(), name.to_string()),
			entry_points,
			ingress_class: self.ingress_class.clone(),
			allow_cross_namespace: self.migration_allow_cross_namespace,
		}))
	}
}

#[derive(clap::Args, Debug)]
struct RenderArgs {
	/// Manifest files (YAML streams or JSON); `-` reads stdin.
	#[arg(short = 'f', long = "file", required = true)]
	files: Vec<PathBuf>,
	/// GatewayClass controller name to render for.
	#[arg(long, default_value = "rproxy.max3584.net/gateway-controller")]
	controller_name: String,
	#[command(flatten)]
	migration: MigrationArgs,
}

/// `--shutdown-delay`, `--shutdown-drain`: like the parameters' `rproxy.shutdown` (0s to 10m).
/// `--backend-outlier-*`: `key=value,...` (a number or a word each: `consecutive_failures=3,ejection_time=10s`)
/// as a JSON object; "" for none.
fn key_values(s: &str) -> Result<serde_json::Value, String> {
	let mut out = serde_json::Map::new();
	for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
		let (k, v) = part.split_once('=').ok_or_else(|| format!("{part:?}: key=value"))?;
		let (k, v) = (k.trim(), v.trim());
		if k.is_empty() || !k.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
			return Err(format!("{k:?}: rproxy's outlier_detection keys are lowercase words"));
		}
		let value = v.parse::<u64>().map(serde_json::Value::from).unwrap_or_else(|_| serde_json::Value::String(v.to_string()));
		out.insert(k.to_string(), value);
	}
	Ok(serde_json::Value::Object(out))
}

/// `--backend-connect-timeout-*`: a duration (`500ms`, `2s`), or "" for rproxy's own.
fn duration_opt(s: &str) -> Result<String, String> {
	let s = s.trim();
	match rproxy_gateway::render::duration_ms(s) {
		_ if s.is_empty() => Ok(String::new()),
		Some(ms) if (100..=600_000).contains(&ms) => Ok(s.to_string()),
		_ => Err(format!("{s:?}: a duration from 100ms to 10m (rproxy's range), or empty")),
	}
}

/// The `--backend-*` flags as what every backend gets.
fn backend_defaults(a: &ControllerArgs) -> rproxy_gateway::render::BackendDefaults {
	let object = |v: &serde_json::Value| Some(v.clone()).filter(|v| v.as_object().is_some_and(|o| !o.is_empty()));
	let duration = |s: &str| (!s.is_empty()).then(|| s.to_string());
	rproxy_gateway::render::BackendDefaults {
		http_outlier: object(&a.backend_outlier_http),
		l4_outlier: object(&a.backend_outlier_l4),
		http_connect_timeout: duration(&a.backend_connect_timeout_http),
		l4_connect_timeout: duration(&a.backend_connect_timeout_l4),
	}
}

fn shutdown_duration(s: &str) -> Result<Duration, String> {
	let d = render::params::duration(s).ok_or_else(|| format!("{s:?} is not a duration (5s, 250ms, 1m)"))?;
	if d > Duration::from_secs(600) {
		return Err("at most 10m".into());
	}
	Ok(d)
}

fn init_logging(format: &str) {
	let filter = tracing_subscriber::EnvFilter::try_from_env("RPROXY_GATEWAY_LOG").unwrap_or_else(|_| "info".into());
	let b = tracing_subscriber::fmt().with_env_filter(filter).with_writer(std::io::stderr);
	if format == "json" {
		b.json().init();
	} else {
		b.init();
	}
}

fn render_files(args: &RenderArgs) -> anyhow::Result<()> {
	let mut world = render::world::World::default();
	for f in &args.files {
		let text = if f.as_os_str() == "-" { std::io::read_to_string(std::io::stdin())? } else { std::fs::read_to_string(f)? };
		world.read_manifests(&text).map_err(|e| anyhow::anyhow!("{}: {e}", f.display()))?;
	}
	let classes: Vec<String> =
		world.classes.iter().filter(|c| c.spec.controller_name == args.controller_name).filter_map(|c| c.metadata.name.clone()).collect();
	let mut out = serde_json::Map::new();
	for gw in world.gateways.iter().filter(|g| classes.is_empty() || classes.contains(&g.spec.gateway_class_name)) {
		let opts = render::Options { migration: args.migration.settings()?, ..Default::default() };
		let plan = render::render_gateway(&world, gw, &opts);
		let listeners: Vec<serde_json::Value> = plan
			.listeners
			.iter()
			.map(|l| serde_json::json!({"name": l.name, "attachedRoutes": l.attached, "conditions": render::status::to_k8s(&l.conds, plan.generation, None, "-")}))
			.collect();
		let routes: Vec<serde_json::Value> = plan
			.parents
			.iter()
			.map(|p| serde_json::json!({"kind": p.kind.kind(), "route": format!("{}/{}", p.namespace, p.name), "parentRef": p.parent_ref, "conditions": render::status::to_k8s(&p.conds, p.generation, None, "-")}))
			.collect();
		out.insert(
			plan.ruleset.clone(),
			serde_json::json!({
				"generation": plan.generation,
				"rules": plan.rules_json(),
				"files": plan.files.keys().collect::<Vec<_>>(),
				"notes": plan.notes,
				"listeners": listeners,
				"routes": routes,
			}),
		);
	}
	println!("{}", serde_json::to_string_pretty(&out)?);
	Ok(())
}

fn main() -> anyhow::Result<()> {
	let cli = Cli::parse();
	let _ = rustls::crypto::ring::default_provider().install_default();
	match cli.command {
		Command::Crds => {
			print!("{}", k8s::crd::crds_yaml());
			Ok(())
		}
		Command::Render(args) => render_files(&args),
		Command::Certsync(args) => {
			init_logging(&cli.log_format);
			tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(certsync::run(args))
		}
		Command::Controller(a) => {
			init_logging(&cli.log_format);
			let backends = backend_defaults(&a);
			let mode = match a.mode.as_str() {
				"fleet" => Mode::Fleet(Fleet {
					selector: a.fleet_selector,
					addresses: a.fleet_address,
					listen_on_addresses: a.fleet_listen == "addresses",
				}),
				_ => Mode::Managed(Managed {
					rproxy_image: a.rproxy_image,
					controller_image: a.controller_image,
					replicas: a.replicas,
					service_type: a.service_type,
					pull_policy: a.image_pull_policy,
					network_policy: a.network_policy.then(|| a.namespace.clone()),
					external_traffic_policy: a.external_traffic_policy,
					allocate_node_ports: a.allocate_load_balancer_node_ports,
					pre_stop_secs: a.pre_stop_secs,
					// set from the cluster's version at start
					native_sleep: false,
					shutdown_delay: a.shutdown_delay,
					shutdown_drain: a.shutdown_drain,
					readiness: a.readiness_probe,
					readiness_path: a.readiness_path,
					liveness: a.liveness_probe,
				}),
			};
			let cfg = controller::Config {
				controller_name: a.controller_name,
				namespace: a.namespace,
				mode,
				listen_addrs: a.listen_addr,
				resync: Duration::from_secs(a.resync_secs.max(5)),
				health: a.health_addr,
				migration: a.migration.settings()?,
				address_cidrs: a.address_cidr.clone(),
				allow_external_name: a.allow_external_name_services,
				service_annotations: a.service_annotation_prefix.clone(),
				fleet_rproxy_rules: a.fleet_rproxy_rules,
				cross_namespace_secrets: a.cross_namespace_secrets,
				watch_namespaces: a.watch_namespaces.clone(),
				backends,
				ui: (!a.ui_namespace.trim().is_empty()).then(|| rproxy_gateway::controller::provision::UiAccess {
					namespace: a.ui_namespace.trim().to_string(),
					pod_selector: rproxy_gateway::controller::provision::parse_selector(&a.ui_pod_selector),
				}),
				leader: a.leader_elect.then(|| {
					let identity = a
						.leader_identity
						.clone()
						.filter(|s| !s.is_empty())
						.or_else(|| std::fs::read_to_string("/etc/hostname").ok().map(|h| h.trim().to_string()))
						.filter(|s| !s.is_empty())
						.unwrap_or_else(|| format!("rproxy-gateway-{}", std::process::id()));
					controller::leader::Settings::new(&a.leader_lease, &identity)
				}),
			};
			tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(controller::run(cfg))
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn backend_flags() {
		assert_eq!(
			key_values("consecutive_failures=3, ejection_time=10s").unwrap(),
			serde_json::json!({"consecutive_failures": 3, "ejection_time": "10s"})
		);
		assert_eq!(key_values("").unwrap(), serde_json::json!({}));
		assert!(key_values("consecutive_failures").is_err());
		assert!(key_values("Bad-Key=1").is_err());
		assert_eq!(duration_opt("1s").unwrap(), "1s");
		assert_eq!(duration_opt("").unwrap(), "");
		assert!(duration_opt("10ms").is_err() && duration_opt("soon").is_err());
		let a = Cli::try_parse_from(["rproxy-gateway", "controller"]).unwrap();
		let Command::Controller(a) = a.command else { panic!() };
		assert_eq!(backend_defaults(&a), rproxy_gateway::render::BackendDefaults::standard(), "the flags' defaults are the standard ones");
	}
}
