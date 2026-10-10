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
| `RproxyMiddleware`, `RproxyPolicy`, `RproxyRule` (`rproxy.max3584.net/v1beta1`; v0.4.4's `v1alpha1` works too, same shape, deprecated) | settings Gateway API lacks (middlewares; L4 limits, bandwidth, GeoIP, passive health checks; rules verbatim) |
| `RproxyGatewayParameters` (GatewayClass and Gateway `parametersRef`) | managed rproxy per Gateway (replicas, PDB, resources, pod and Service settings, rproxy's performance settings) |
| Migration (`--migrate-to`) | reads Ingress and Traefik's IngressRoute, IngressRouteTCP, IngressRouteUDP, Middleware, TLSOption ([docs/en/MIGRATION.md](docs/en/MIGRATION.md)) |

Decisions and the mapping tables: [docs/en/DESIGN.md](docs/en/DESIGN.md). Gateway API conformance results: [docs/en/CONFORMANCE.md](docs/en/CONFORMANCE.md). Tenant separation, what is off by default, and permissions: [docs/en/SECURITY.md](docs/en/SECURITY.md). The design of what the v0.4 patches add for running on Kubernetes (rproxy settings per Gateway, Kustomize, the UI): [docs/en/DESIGN-v0.4.x.md](docs/en/DESIGN-v0.4.x.md). Providing Gateways' addresses on the platform (MetalLB, kube-vip, Cilium, cloud load balancers, NodePort + your own L4, keepalived for fleet): [docs/en/PLATFORM.md](docs/en/PLATFORM.md).

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
- A Gateway's own rproxy shape (replicas, resources, PDB, Service settings, ...): write an `RproxyGatewayParameters` in the Gateway's namespace and name it in `spec.infrastructure.parametersRef`. Every Gateway's defaults and what Gateways may set (`policy`): the chart's `managed.parameters` (the GatewayClass's reference). When upgrading from v0.4.1 with `helm upgrade`, apply the CRDs first (`helm upgrade` does not install new CRDs): `kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v<version>/rproxy.max3584.net.yaml`. From v0.4.4, applying the CRDs makes `v1beta1` available (without it everything keeps working on `v1alpha1`; [docs/en/DESIGN-v0.4.x.md](docs/en/DESIGN-v0.4.x.md) 11.4).

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyGatewayParameters
metadata: {name: web, namespace: default}
spec:
  replicas: 3
  pod: {resources: {rproxy: {requests: {cpu: 500m, memory: 128Mi}}}}
---
# in the Gateway's spec:
#   infrastructure: {parametersRef: {group: rproxy.max3584.net, kind: RproxyGatewayParameters, name: web}}
```
- To show Gateways' rproxy, read only, in the management UI ([TCP-UDP-rproxy-ui](https://github.com/max3584/TCP-UDP-rproxy-ui)'s chart, installed apart), set the chart's `ui.namespace` to the UI's namespace. The controller writes the Secret `rproxy-ui-discovery` there (rproxy pods, the CA certificate, read-only tokens). rproxy v0.4.2 reads a changed token file again, so setting `ui.namespace` and showing or hiding a Gateway do not roll the pods (pods show in the UI once they take its token, within a minute or two; older rproxy images roll the pods once). Hide a Gateway with `ui: {visible: false}` in its parameters ([docs/en/SECURITY.md](docs/en/SECURITY.md), "Showing Gateways to the UI").
- The controller runs 2 replicas by default; they elect a leader with a Lease and only it applies rule sets (docs/en/DESIGN.md, "High availability").
- With `fleet.enabled=true`, the chart's DaemonSet (`hostNetwork: true`) runs rproxy, which serves every Gateway. With `fleet.listen: addresses` (default `wildcard`; rproxy v0.4.3's `listen_freebind`) a Gateway with `spec.addresses` listens on its own addresses only (inside `managed.addressCIDRs`): Gateways on different addresses can use the same port (443 and so on), even before a node has the address (the platform puts it there: MetalLB, kube-vip, keepalived, a cloud LB...; [docs/en/DESIGN-v0.4.x.md](docs/en/DESIGN-v0.4.x.md) 12.). The same port on the same address (or a wildcard) belongs to the older Gateway. When it stops it keeps accepting for `fleet.shutdown.delay` (5 s by default) and waits for open connections up to `fleet.shutdown.drain` (25 s by default). Point the health check of the load balancer or VIP in front at `https://<node>:9443/readyz` (503 once it starts stopping), and make the delay longer than it takes to take the node out. rproxy-gateway does not hold addresses (VIPs) itself (v0.4.4's `fleet.vip` was removed in v0.4.5): keepalived, your own L4, or a Service selecting the fleet's pods are in [docs/en/PLATFORM.md](docs/en/PLATFORM.md), "fleet".
- Chart values: [charts/rproxy-gateway/values.yaml](charts/rproxy-gateway/values.yaml). The controller's settings are passed in a ConfigMap `rproxy-gateway-config` (`RPROXY_GATEWAY_*` environment variables); `controller.extraArgs` stay arguments and win over it.

### Supported versions

Kubernetes 1.29 or later (the chart's `kubeVersion`) with Gateway API CRDs v1.0 or later (v1.0 and v1.1 with the limits below). The pairings below pass the e2e in the compatibility run (every Monday, or by hand with `gh workflow run e2e.yml -f compat=true`). The pairings are worked out from what is released at run time, so a new Kubernetes (the newest patch and the next minor's beta / rc) or Gateway API is in the next run; v1.5 and v1.6 (the latest two) also pass conformance core.

| Gateway API CRDs | Kubernetes needed (for the CRDs' validation) | Pairings checked |
|---|---|---|
| v1.0 | — | k8s 1.37. Gateway `infrastructure.parametersRef` came with v1.1, so per-Gateway RproxyGatewayParameters are not available (the GatewayClass parametersRef is) |
| v1.1 | — | k8s 1.37. GRPCRoute does not work ([#61](https://github.com/max3584/rproxy-gateway/issues/61)) |
| v1.2, v1.3, v1.4 | 1.29 or later | k8s 1.29, 1.30 (v1.4); 1.37 (v1.2-v1.4) |
| v1.5 | 1.31 or later (`isIP`) | k8s 1.31, 1.37 |
| v1.6 | 1.32 or later (`dns1123Label`) | k8s 1.32-1.37 |
| v1.7.0-rc.1 | — | k8s 1.37 (the new conformance tests: [#62](https://github.com/max3584/rproxy-gateway/issues/62)) |

Kubernetes older than the chart allows is checked too, with `kubeVersion` relaxed for those runs only (recorded): 1.26 to 1.28 pass the whole e2e. 1.23 to 1.25 refuse the PodDisruptionBudget's `unhealthyPodEvictionPolicy` (from 1.26), so Gateways with 2 or more replicas get no PodDisruptionBudget (the e2e stops there and the rest is not checked; conformance passes). On 1.22 the Gateway API CRDs do not install.

With older CRDs, TCPRoute and UDPRoute (`v1alpha2` before v1.6), TLSRoute (`v1alpha3` in v1.4) and ReferenceGrant (`v1beta1` in v1.4) are read and written at the version the API server serves. `experimental-install.yaml` is recommended (TCPRoute, UDPRoute and HTTPRoute retries).

### Installing without Helm (kubectl, Kustomize)

Releases carry manifests rendered from the chart: `install.yaml` (managed), `install-fleet.yaml` (fleet) and `crds.yaml` (rproxy's CRDs only, for GitOps that applies CRDs first). None includes Gateway API's CRDs.

```bash
kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v<version>/install.yaml
```

With Kustomize, use [config/default](config/default) (fleet: [config/fleet](config/fleet)) as the base. The controller's settings are the `RPROXY_GATEWAY_*` of `rproxy-gateway controller --help`, changed with a `configMapGenerator` and `behavior: merge` (the ConfigMap's name gets a hash, so a change rolls the controller). Examples: [config/samples](config/samples) (image digests, managed replicas, one controller replica, the class's default `RproxyGatewayParameters` (the chart's `managed.parameters`)).

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - github.com/max3584/rproxy-gateway//config/default?ref=v<version>
configMapGenerator:
  - name: rproxy-gateway-config
    namespace: rproxy-gateway-system
    behavior: merge
    literals: [RPROXY_GATEWAY_REPLICAS=2]
```

- `config/` is the chart's defaults rendered by `scripts/render-config.sh` (not edited by hand; CI finds drift from the chart).
- Values that change the shape of RBAC (`controller.watchNamespaces`: a Role per namespace) or make the chart add objects (`migration.createIngressClass`) need Helm.

## Availability (`managed.replicas` 2 or more)

rproxy pods become Ready only once the controller has applied their rule set (a readiness gate), and when they stop they keep accepting for 15 s after SIGTERM (`managed.shutdown.delay`) while `/readyz` takes them out of the Service, then close their listeners and let open connections end for up to 25 s (`managed.shutdown.drain`) (rproxy v0.4.1; older rproxy images: a 15 s preStop). `managed.preStopSeconds` is unset by default from 0.4.2; if you set it for 0.4.1, that preStop stays on every pod and the delay and drain follow it (remove the value if you do not need it). Each Gateway gets a PodDisruptionBudget and its pods spread over nodes (docs/en/DESIGN.md, "rproxy availability").

Platform setup per shape (MetalLB L2, BGP + BFD, kube-vip, Cilium, cloud, NodePort + HAProxy, ClusterIP): [docs/en/PLATFORM.md](docs/en/PLATFORM.md).

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
- **BGP + ECMP**: with `Local`, MetalLB withdraws the route of a node with a terminating pod only once the pod is gone (3–4 s in FRR mode), so that much is lost (3–4 s). For close to 0 on planned replacements use `Cluster` (with the same tail when a node is lost). BFD takes a lost node out of the routes in about a second. BGP convergence is network-side tuning (BFD, timers), so the acceptance test's `bgp` topology is measured and recorded only (gaps do not fail it).
- **NodePort behind your own L4 load balancer**: with `Local`, the load balancer can take a node out only once rproxy has stopped (NodePort has no `healthCheckNodePort`), so the connections of that moment fail (about 2 s). If your load balancer can check the Service's `healthCheckNodePort`, use type `LoadBalancer` (a node whose pods are all terminating fails it, so it is taken out while rproxy still accepts).
- When a node is lost, its **backend** pods also stay in their EndpointSlices until the node turns NotReady (in every topology, some failures may follow the table's values for 40–60 s). From v0.4.5 the controller gives every backend rproxy's passive health checks (`outlier_detection`) and short connect timeouts, so such a pod is left out after a few failures, and HTTPRoute backends get a 30 s limit to their response headers (the chart's `backends`; RproxyPolicy's `outlierDetection`, `connectTimeout` and `responseTimeout` win per Gateway, listener or Service; [docs/en/DESIGN-v0.4.x.md](docs/en/DESIGN-v0.4.x.md) 13.), and let backends stop with a preStop too (5 s in the acceptance test; without it, each drain loses 1–3 s).

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
