//! rproxy-gateway: a Kubernetes Gateway API controller for rproxy.
//!
//! The controller turns Gateway API resources (and, for migration, Ingress and
//! Traefik CRDs) into rproxy rule sets and applies them through rproxy's control
//! API (`PUT /rulesets/{name}`). See README.md and docs/DESIGN.md.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::{Parser, Subcommand};
use rproxy_gateway::controller::provision::{Fleet, Managed, Mode};
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
	/// Print rproxy-gateway's CRDs (RproxyMiddleware, RproxyPolicy, RproxyRule) as YAML.
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
	#[arg(long, env = "RPROXY_GATEWAY_RPROXY_IMAGE", default_value = "ghcr.io/max3584/rproxy-gateway/rproxy:0.4.0")]
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
	/// managed: the ServiceAccount of rproxy pods (certsync reads Secrets in this namespace).
	#[arg(long, env = "RPROXY_GATEWAY_PROXY_SERVICE_ACCOUNT", default_value = "rproxy-gateway-proxy")]
	proxy_service_account: String,
	/// managed: imagePullPolicy of rproxy pods.
	#[arg(long, env = "RPROXY_GATEWAY_IMAGE_PULL_POLICY", default_value = "IfNotPresent")]
	image_pull_policy: String,
	/// fleet: label selector of the rproxy pods (in the controller's namespace).
	#[arg(long, env = "RPROXY_GATEWAY_FLEET_SELECTOR", default_value = "app.kubernetes.io/name=rproxy,app.kubernetes.io/component=fleet")]
	fleet_selector: String,
	/// fleet: addresses written to Gateway status (default: the pods' host IPs).
	#[arg(long, env = "RPROXY_GATEWAY_FLEET_ADDRESS", value_delimiter = ',')]
	fleet_address: Vec<String>,
	/// Addresses rproxy rules listen on (the first is listen_addr, the rest extra_listen_addrs).
	#[arg(long, env = "RPROXY_GATEWAY_LISTEN_ADDR", value_delimiter = ',', default_value = "0.0.0.0")]
	listen_addr: Vec<String>,
	/// Seconds between full passes (also how soon an rproxy restart is noticed without a pod event).
	#[arg(long, env = "RPROXY_GATEWAY_RESYNC_SECS", default_value_t = 30)]
	resync_secs: u64,
	/// Where `/healthz` is answered.
	#[arg(long, env = "RPROXY_GATEWAY_HEALTH_ADDR", default_value = "0.0.0.0:8081")]
	health_addr: SocketAddr,
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
			let mode = match a.mode.as_str() {
				"fleet" => Mode::Fleet(Fleet { selector: a.fleet_selector, addresses: a.fleet_address }),
				_ => Mode::Managed(Managed {
					rproxy_image: a.rproxy_image,
					controller_image: a.controller_image,
					replicas: a.replicas,
					service_type: a.service_type,
					service_account: a.proxy_service_account,
					pull_policy: a.image_pull_policy,
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
