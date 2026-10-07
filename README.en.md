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

Decisions and the mapping tables: [docs/en/DESIGN.md](docs/en/DESIGN.md). Gateway API conformance results: [docs/en/CONFORMANCE.md](docs/en/CONFORMANCE.md).

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

## License

MIT ([LICENSE](LICENSE))
