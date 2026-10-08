# rproxy-gateway

[![CI](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml)
[![e2e](https://github.com/max3584/rproxy-gateway/actions/workflows/e2e.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/e2e.yml)
[![cargo-deny](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Renovate](https://img.shields.io/badge/renovate-enabled-brightgreen?logo=renovatebot)](https://github.com/max3584/rproxy-gateway/issues?q=is%3Aissue+is%3Aopen+%22Dependency+Dashboard%22)

日本語: [README.md](README.md)

A Kubernetes controller for [rproxy](https://github.com/max3584/rproxy-api). It turns Gateway API resources (v1.6, standard channel) into rproxy rules and applies them through rproxy's control API (rule sets, `PUT /rulesets/{name}`) (max3584/rproxy-api#28; the design is docs/en/DESIGN-v0.4.md 3. in rproxy-api).

- rproxy knows nothing of the Kubernetes API. This controller and rproxy meet only at the control API (`docs/openapi.json` in rproxy-api). It needs rproxy v0.4.0 or later (`features.rulesets`).
- The first release ships together with rproxy v0.4.0 (milestone v0.4.0).

## What it does

| Resource | rproxy |
|---|---|
| `GatewayClass` (`controllerName: rproxy.max3584.net/gateway-controller`) | `Accepted`, `supportedFeatures` |
| `Gateway` listeners `HTTP`, `HTTPS`, `TLS` (Passthrough / Terminate), `TCP`, `UDP` | one rule per (protocol, address, port); listeners on the same port merge. `spec.addresses`, `infrastructure`, client certificate validation (`tls.frontend`), client certificates to backends (`tls.backend`) |
| `ListenerSet` | listeners added to a Gateway (`allowedListeners`) |
| `HTTPRoute`, `GRPCRoute` | `http.routes` (path, header, query and method matches in Gateway API's precedence), header modifiers (`add` too), redirects (301 to 308), URL and host rewrites, CORS, mirroring, retries, timeouts, filters per backendRef, weights, h2c backends, `RproxyMiddleware` (ExtensionRef) |
| `BackendTLSPolicy` | TLS to backends (CA, SNI, SAN) |
| `TLSRoute`, `TCPRoute`, `UDPRoute` | `tls.routes` (SNI, pod `targets` per name), `targets` |
| `ReferenceGrant` | Services and Secrets in other namespaces |
| Backends | the pod IPs from EndpointSlices (rproxy balances and health-checks them) |
| Status | `Accepted`, `Programmed`, `ResolvedRefs` of Gateways, listeners and routes (from the rproxy rules' `conditions`) |
| `RproxyMiddleware`, `RproxyPolicy`, `RproxyRule` (`rproxy.max3584.net/v1alpha1`) | settings Gateway API lacks (middlewares; L4 limits, bandwidth, GeoIP, passive health checks; rules verbatim) |
| Migration (`--migrate-to`) | reads Ingress and Traefik's IngressRoute, IngressRouteTCP, IngressRouteUDP, Middleware, TLSOption ([docs/en/MIGRATION.md](docs/en/MIGRATION.md)) |

Decisions and the mapping tables: [docs/en/DESIGN.md](docs/en/DESIGN.md). Gateway API conformance results: [docs/en/CONFORMANCE.md](docs/en/CONFORMANCE.md). Tenant separation, what is off by default, and permissions: [docs/en/SECURITY.md](docs/en/SECURITY.md). The design of what the v0.4 patches add for running on Kubernetes (rproxy settings per Gateway, Kustomize, the UI, VIPs): [docs/en/DESIGN-v0.4.x.md](docs/en/DESIGN-v0.4.x.md).

## Installing

```bash
# Gateway API's CRDs (standard channel; experimental-install.yaml for HTTPRoute retries)
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml
# the controller (CRDs, RBAC, GatewayClass rproxy)
helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway -n rproxy-gateway-system --create-namespace
```

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - {name: http, port: 80, protocol: HTTP}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
```

- By default (managed), the controller creates one rproxy Deployment and `LoadBalancer` Service per Gateway in the Gateway's namespace (`managed.serviceType`, `managed.replicas`), with the Gateway's `spec.infrastructure` labels and annotations and its `spec.addresses` (the Service's `externalIPs`).
- The controller runs 2 replicas by default; they elect a leader with a Lease and only it applies rule sets (docs/en/DESIGN.md, "High availability").
- With `fleet.enabled=true`, the chart's DaemonSet (`hostNetwork: true`) runs rproxy, which serves every Gateway.
- Chart values: [charts/rproxy-gateway/values.yaml](charts/rproxy-gateway/values.yaml).

## Availability (`managed.replicas` 2 or more)

rproxy pods become Ready only once the controller has applied their rule set (a readiness gate), and keep answering for a preStop (15 s by default) when they stop. Each Gateway gets a PodDisruptionBudget and its pods spread over nodes (docs/en/DESIGN.md, "rproxy availability").

The acceptance test (kind 1+3 nodes, `managed.replicas=2`, HTTP, HTTPS and TCP every 100 ms on new connections), longest time without an answer (s):

| Topology (`TOPOLOGY`) | pod deleted | pod on the announcing node deleted | drain | rollout restart | node lost |
|---|---|---|---|---|---|
| v0.4.0 (MetalLB L2, Local) | 1.2 | 10.2–14.9 | 2.0 (drain 32.9 s) | 10.8–13.5 | 5.6 |
| MetalLB L2, `Local` (default) | 0.2 | 0.3 | 1.2 (drain 16.5 s) | 0.2 | 7.8 |
| MetalLB L2, `Cluster` | 0.2 | 0.2 | 0.2 | 0.2 | 9.0 (some fail until ~59 s) |
| MetalLB BGP + ECMP (BFD), `Local` | 3.5 | — | 2.3 | 2.3 | 3.3 |
| MetalLB BGP + ECMP (BFD), `Cluster` | 0.2 | — | 0.2 | 0.1 | 13.3 (some fail until ~60 s) |
| NodePort + own L4 (HAProxy), `Local` | 0.2 | — | 2.2 | 2.2 | 6.4 |
| NodePort + own L4 (HAProxy), `Cluster` | 0.1 | — | 0.1 | 0.2 | 20.2 (some fail until ~63 s) |

- **The default (LoadBalancer, `externalTrafficPolicy: Local`) is recommended.** Clients' IPs reach rproxy, and replacing pods (deletion, drain, rollout) costs about a second at most. MetalLB L2 may drop a connection in flight when it moves the announcement (the drain's 1.2 s). When a node is lost, it takes as long as the load balancer needs to notice (MetalLB L2's memberlist: 5–8 s).
- **`Cluster`** hardly drops anything when pods are replaced (every node sends to ready pods), but clients' IPs are lost, and when a node is lost some connections keep failing until its pods leave the endpoints (when the node turns NotReady: 40–50 s).
- **BGP + ECMP**: with `Local`, MetalLB withdraws the route of a node with a terminating pod only once the pod is gone (3–4 s in FRR mode), so that much is lost (3–4 s). For close to 0 on planned replacements use `Cluster` (with the same tail when a node is lost). BFD takes a lost node out of the routes in about a second.
- **NodePort behind your own L4 load balancer**: with `Local`, the load balancer can take a node out only once rproxy has stopped (NodePort has no `healthCheckNodePort`), so the connections of that moment fail (about 2 s). If your load balancer can check the Service's `healthCheckNodePort`, use type `LoadBalancer` (a node whose pods are all terminating fails it, so it is taken out during the preStop).
- When a node is lost, its **backend** pods also stay in their EndpointSlices until the node turns NotReady (in every topology, some failures may follow the table's values for 40–60 s). Use RproxyPolicy's `outlierDetection` for backends, and let backends stop with a preStop too (5 s in the acceptance test; without it, each drain loses 1–3 s).

## Commands

| Command | What it does |
|---|---|
| `rproxy-gateway controller` | the controller (flags: `--help`; every flag can also be set as an `RPROXY_GATEWAY_*` environment variable) |
| `rproxy-gateway certsync` | in rproxy's pod, tells the controller which mounted certificate files are in place (no API access) |
| `rproxy-gateway crds` | prints the CRDs' YAML (the same as the chart's `crds/`) |
| `rproxy-gateway render -f <files>` | renders rule sets from manifests (no cluster, no rproxy; also a preview for migration) |

## Development

```bash
cargo build
cargo test                                   # unit tests (a fake rproxy included)
RPROXY_BIN=/path/to/rproxy-api cargo test --test rproxy   # against a real rproxy (v0.4, rule sets)
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

- e2e (`scripts/e2e.sh`) and conformance (`scripts/conformance.sh`) run on kind (Docker needed; the CI `e2e` workflow). rproxy is built from rproxy-api master (another ref when run by hand).
- Acceptance test (`scripts/acceptance.sh`, the manual `acceptance` workflow): installs the published chart and images on a 4-node kind cluster with MetalLB and cert-manager and measures failover, certificate renewal and state recovery under continuous traffic.

## License

MIT ([LICENSE](LICENSE))
