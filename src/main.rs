//! rproxy-gateway: a Kubernetes Gateway API controller for rproxy.
//!
//! The controller turns Gateway API resources (and, for migration, Ingress and
//! Traefik CRDs) into rproxy rule sets and applies them through rproxy's control
//! API (`PUT /rulesets/{name}`). See README.md and docs/DESIGN.md.

use clap::Parser;

#[derive(Parser, Debug)]
#[command(version, about)]
struct Cli {}

fn main() {
	let _cli = Cli::parse();
	println!("rproxy-gateway {}", env!("CARGO_PKG_VERSION"));
}
