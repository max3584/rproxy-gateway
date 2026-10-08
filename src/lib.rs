//! rproxy-gateway: a Kubernetes Gateway API controller for rproxy (see README.md).

// nested `if let` reads better than let chains for the status logic
#![allow(clippy::collapsible_if)]

pub mod certsync;
pub mod controller;
pub mod k8s;
pub mod manifests;
pub mod pem;
pub mod render;
pub mod rproxy;
pub mod vip;
