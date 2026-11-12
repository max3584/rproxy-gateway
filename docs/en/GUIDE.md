日本語: [../GUIDE.md](../GUIDE.md)

# rproxy-gateway configuration guide

This guide shows how to create Gateways in Kubernetes with rproxy-gateway, send outside traffic through them to your apps, and keep them running. It is written for a PaaS operator who knows the Kubernetes basics (Deployments, Services, namespaces, Helm) but has not used rproxy before. Each task comes with YAML you can paste as it is and the commands that tell you whether it worked.

This guide keeps **standard Gateway API**, which means the same on every implementation, apart from **rproxy-only settings**, which work only with rproxy-gateway. A map of what is standard and what is rproxy-specific is in [Standard Gateway API and rproxy-only settings](#standard-gateway-api-and-rproxy-only-settings).

Related documents:

- The design and the reasons behind it are in [DESIGN.md](DESIGN.md) and [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md).
- How to share one cluster between several teams (tenants) is in [SECURITY.md](SECURITY.md).
- How to give Gateways addresses that the outside can reach (MetalLB and others) is in [PLATFORM.md](PLATFORM.md).
- How to move from Ingress or Traefik is in [MIGRATION.md](MIGRATION.md).

The names in the examples (`app.example.com`, the `default` namespace, the Service `app`) and the addresses (`192.0.2.0/24`, `203.0.113.0/24`) are reserved for documentation. Replace them with your own.

The examples were checked with this version (v0.4.22, with the bundled rproxy v0.4.22). Every YAML was checked against the CRD schemas, and its conversion into rproxy rules was checked with `rproxy-gateway render` ([15.3](#153-checking-without-a-cluster)).

## Contents

[Standard Gateway API and rproxy-only settings](#standard-gateway-api-and-rproxy-only-settings) (a map of this guide)

**Part 1: Getting started**

0. [Gateway API in brief](#0-gateway-api-in-brief)
1. [Install](#1-install)

**Part 2: Writing standard Gateway API** (the same on other implementations)

2. [A first Gateway and HTTPRoute](#2-a-first-gateway-and-httproute)
3. [HTTPS](#3-https)
4. [HTTP routing](#4-http-routing)
5. [L4 forwarding](#5-l4-forwarding)
6. [Other namespaces](#6-other-namespaces)
7. [Other Gateway API features](#7-other-gateway-api-features)

**Part 3: rproxy-only settings** (this implementation's extensions)

8. [rproxy's CRDs](#8-rproxys-crds)
9. [managed and fleet](#9-managed-and-fleet)
10. [Changing things while they run](#10-changing-things-while-they-run)
11. [How it talks to rproxy](#11-how-it-talks-to-rproxy)
12. [Security basics](#12-security-basics)

**Part 4: Operations and troubleshooting**

13. [Operations](#13-operations)
14. [Recipes](#14-recipes)
15. [Troubleshooting](#15-troubleshooting)

The chapter numbers changed from the versions of this guide up to v0.4.22. The former 7.5 to 7.8 (listener filters, BackendTLSPolicy, ClusterTrustBundle, address routability) are now 7.1 to 7.4, the former 7.1 to 7.4 (rproxy's CRDs) are now 8.1 to 8.4, and the former chapters 8 to 14 are now 9 to 15.

## Standard Gateway API and rproxy-only settings

Two kinds of settings appear in this guide: **standard Gateway API**, which means the same on every Gateway API implementation, and **rproxy-only settings**, which work only with rproxy-gateway. Only the former carries over as it is when you move to another implementation, so this guide keeps the two clearly apart.

The text marks them like this:

- **No mark**: standard Gateway API (standard-channel features). The same YAML means the same on other implementations. Which features an implementation supports still differs, so check the support table (conformance results) of the implementation you move to.
- **Gateway API experimental**: part of standard Gateway API, but still in the experimental channel (not GA). It needs the experimental CRDs, and its shape may change in later versions.
- **rproxy-specific**: rproxy-gateway's extensions and behaviour particular to rproxy. These do not carry over to other implementations. rproxy's CRDs, chart values, managed and fleet, how rproxy applies changes (`in_place`, `recreate`), the readiness gate and the per-Gateway NetworkPolicy all belong here.

**What standard Gateway API does (the same on other implementations)**

| Task | Section |
|---|---|
| Create a Gateway and routes, and check their status | [2.2](#22-gateway-and-httproute), [2.4](#24-check) |
| HTTPS (certificate Secrets, redirecting HTTP to HTTPS) | [3.1](#31-a-tls-secret-in-certificaterefs), [3.3](#33-redirect-http-to-https) |
| Routing by hostname, path and header; weights; filters; timeouts | [4.1](#41-hostnames) to [4.5](#45-retries-and-timeouts) |
| gRPC routing | [4.6](#46-grpcroute) |
| Forwarding TCP, UDP and TLS as they are | [5.](#5-l4-forwarding) |
| Using a Gateway across namespaces (allowedRoutes, ReferenceGrant, ListenerSet) | [6.](#6-other-namespaces) |
| TLS to backends, client certificate validation | [7.1](#71-backendtlspolicy), [7.2](#72-clustertrustbundle-and-client-certificate-validation) |
| HTTPRoute `retry` and `ExternalAuth`, listener filters, address routability (**Gateway API experimental**) | [4.5](#45-retries-and-timeouts), [7.3](#73-listener-filters), [7.4](#74-address-routability) |

**rproxy-only settings (this implementation's extensions)**

| What | Why it exists (what the standard cannot express) | Section |
|---|---|---|
| Chart values | How rproxy-gateway is installed, and defaults for every Gateway | [1.2](#12-helm-install), [1.3](#13-main-chart-values) |
| What managed mode creates | The per-Gateway rproxy Deployment, Service and so on | [2.3](#23-what-gets-created) |
| `RproxyPolicy` | Limiting client addresses, capping connections and bandwidth, L4 timing, passive health checks. Standard Gateway API has no fields for these | [8.1](#81-rproxypolicy) |
| `RproxyMiddleware` | Processing the standard filters lack: rate limits, allowing by address, security headers, `forward_auth` and more. Used from the standard extension point `ExtensionRef` | [8.2](#82-rproxymiddleware) |
| `RproxyRule` | rproxy rules Gateway API cannot express, such as forwarding outside the cluster | [8.3](#83-rproxyrule) |
| `RproxyGatewayParameters` | The shape of the rproxy pods and Service: what the standard `parametersRef` points at | [8.4](#84-rproxygatewayparameters) |
| managed and fleet | Where the rproxy pods run | [9.](#9-managed-and-fleet) |
| How changes are applied (`in_place`, `recreate`), the readiness gate, how pods stop | What happens to existing connections on changes and pod replacement | [10.](#10-changing-things-while-they-run) |
| Control API, tokens, NetworkPolicy | How the controller and rproxy connect, and how that is protected | [11.](#11-how-it-talks-to-rproxy), [12.](#12-security-basics) |

Part 4 (operations, recipes, troubleshooting) covers both. In the recipes, the rproxy-specific parts are marked.

## Part 1: Getting started

Part 1 gives the big picture of Gateway API (0.) and shows how to install rproxy-gateway (1.). Gateway API's CRDs in 1.1 are standard, but the Helm chart and its values in 1.2 and 1.3 belong to rproxy-gateway.

## 0. Gateway API in brief

This chapter gives the big picture of Kubernetes Gateway API, which is what rproxy-gateway reads. If you have used Gateway API before, the last paragraph is all you need.

Gateway API is the standard Kubernetes API for saying "send traffic that comes from outside the cluster to this Service". It is the successor to Ingress, and its resources are split by the role of the person who writes them.

| Resource | Written by | What it says |
|---|---|---|
| `GatewayClass` | The cluster administrator (for rproxy-gateway, the chart creates one named `rproxy`) | Which controller handles Gateways of this class |
| `Gateway` | The Gateway's owner (the infrastructure team) | The list of listeners. Each listener has a port, a protocol, a hostname and certificates |
| `HTTPRoute`, `GRPCRoute`, `TLSRoute`, `TCPRoute`, `UDPRoute` | The app's owner | Which requests or connections go to which Service |
| `ReferenceGrant` | The owner of the namespace being referred to | Permission for other namespaces to use a Service or Secret in this namespace |

Traffic flows like this:

```
client ──▶ Gateway listener (:80 / :443) ──▶ route (hostname, path) ──▶ the Service's pods
            └─ an rproxy pod (per Gateway) listens
```

rproxy-gateway is the controller that reads these resources, turns them into rules for rproxy (the proxy that actually relays the traffic) and sends them to each Gateway's rproxy. rproxy itself knows nothing about Kubernetes; it simply follows the rules it gets from rproxy-gateway (details in [11.](#11-how-it-talks-to-rproxy)).

## 1. Install

This chapter covers installing rproxy-gateway in a cluster and the chart values worth deciding at install time. You install Gateway API's CRDs first, then the rproxy-gateway chart.

### 1.1 Gateway API's CRDs

The CRDs that define Gateway API's resources (Gateway, HTTPRoute and so on) are not part of the rproxy-gateway chart. Install them once per cluster, before the chart.

Gateway API's CRDs come in two channels:

| Channel | What it contains (v1.6.3) |
|---|---|
| standard | GatewayClass, Gateway, HTTPRoute, GRPCRoute, TLSRoute, TCPRoute, UDPRoute, ListenerSet, ReferenceGrant, BackendTLSPolicy |
| experimental | Everything in standard, plus fields that are still experimental |

The standard channel is usually enough. You need the experimental CRDs only for these features:

| Feature | CRDs needed |
|---|---|
| HTTPRoute `retry` ([4.5](#45-retries-and-timeouts)) | experimental in v1.6; part of standard from v1.7 |
| HTTPRoute `ExternalAuth` filter | experimental |
| Listener filters (`filters.requests`, [7.3](#73-listener-filters)) | v1.7 experimental |
| `spec.addresses[].routability` ([7.4](#74-address-routability)) | v1.7 experimental |

Install one of the two:

```bash
# standard (usually enough)
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml
# experimental (for the features above; for v1.7's features, v1.7.0's experimental-install.yaml)
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/experimental-install.yaml
```

Gateway API v1.6 needs Kubernetes 1.32 or later. Which Kubernetes and Gateway API versions go together is listed under "Supported versions" in the [README](../../README.en.md).

To check that they are installed, list the CRDs; you should see the `gateway.networking.k8s.io` ones:

```bash
kubectl get crd | grep gateway.networking.k8s.io
```

### 1.2 helm install

> **rproxy-specific**: from here on, this is how to install rproxy-gateway itself.

Next, install rproxy-gateway itself with Helm:

```bash
helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway \
  -n rproxy-gateway-system --create-namespace -f values.yaml
```

You can leave out `-f values.yaml`; the defaults work (see [1.3](#13-main-chart-values) for what you might change). The chart installs rproxy-gateway's own CRDs (`rproxy.max3584.net`), RBAC, the controller (2 replicas) and the GatewayClass `rproxy`.

Once it is installed, check the controller pods and the GatewayClass:

```bash
kubectl -n rproxy-gateway-system get pods
# NAME                              READY   STATUS    RESTARTS   AGE
# rproxy-gateway-6c9d8f7b5d-4xk2p   1/1     Running   0          1m
# rproxy-gateway-6c9d8f7b5d-9qm7r   1/1     Running   0          1m
kubectl get gatewayclass rproxy
# NAME     CONTROLLER                              ACCEPTED   AGE
# rproxy   rproxy.max3584.net/gateway-controller   True       1m
```

If the GatewayClass shows `ACCEPTED` `True`, the controller has taken it on. To see which Gateway API features the controller says it supports, run `kubectl get gatewayclass rproxy -o jsonpath='{.status.supportedFeatures[*].name}'`.

To install without Helm (`install.yaml` or Kustomize), see "Installing without Helm (kubectl, Kustomize)" in the [README](../../README.en.md).

### 1.3 Main chart values

> **rproxy-specific**: chart values belong to rproxy-gateway. Standard Gateway API has no fields for how an implementation is installed or for its defaults. What you want to change per Gateway can be changed with RproxyGatewayParameters ([8.4](#84-rproxygatewayparameters)) through the standard entry point `spec.infrastructure.parametersRef`.

This section explains the chart values you are most likely to decide at install time, grouped by what they control. Every value works at its default, so put only the ones you want to change in `values.yaml`. All values, including those not listed here, are in [values.yaml](../../charts/rproxy-gateway/values.yaml) with comments.

Two terms first:

- **managed** (the default mode): every time you create a Gateway, the controller creates an rproxy Deployment and Service just for that Gateway, in the Gateway's namespace. The `managed.*` values shape these per-Gateway rproxies.
- **fleet**: one set of rproxy pods installed by the chart (a DaemonSet) serves every Gateway. You switch to it with `fleet.enabled: true`. [9.](#9-managed-and-fleet) explains the difference and how to choose.

**The controller**

| Value | Default | What it decides |
|---|---|---|
| `controller.replicas` | `2` | How many controller pods run |
| `controller.watchNamespaces` | `[]` (every namespace) | Which namespaces the controller watches |
| `controller.crossNamespaceSecrets` | `true` | Whether certificate Secrets from other namespaces may be used |

- `controller.replicas`: however many controllers run, only one of them, the leader chosen through a Lease (a Kubernetes locking mechanism), sends changes to rproxy. The others wait as standbys and take over if the leader stops. With two, a standby can take over right away while a controller pod is being replaced. The default is normally fine ([13.3](#133-scaling-and-ha)).
- `controller.watchNamespaces`: when empty, the controller watches Gateways and routes in every namespace. When you list namespaces, it watches only those (plus the release namespace), and its permissions shrink from a cluster-wide ClusterRole to Roles in those namespaces. Use it when you know which namespaces will use rproxy-gateway and want the controller to hold fewer permissions ([12.](#12-security-basics)).
- `controller.crossNamespaceSecrets`: when `true`, a Gateway's `certificateRefs` may name a Secret in another namespace that a ReferenceGrant ([6.2](#62-referencegrant-using-another-namespaces-services-and-secrets)) allows. In managed mode, that private key is copied into a Secret in the Gateway's namespace. Set it to `false` if you do not want private keys copied across namespaces ([SECURITY.md](SECURITY.md)).

**Per-Gateway rproxy (managed)**

| Value | Default | What it decides |
|---|---|---|
| `managed.replicas` | `1` | How many rproxy pods each Gateway gets |
| `managed.serviceType` | `LoadBalancer` | The type of each Gateway's Service |
| `managed.externalTrafficPolicy` | `""` (chosen by Service type) | Which nodes' rproxy pods the Service hands client traffic to |
| `managed.allocateLoadBalancerNodePorts` | `true` | Whether `LoadBalancer` Services get node ports |
| `managed.addressCIDRs` | `[]` (none allowed) | Which addresses Gateways may put in `spec.addresses` |
| `managed.networkPolicy` | `true` | Whether each Gateway gets a NetworkPolicy |
| `managed.shutdown` | `{delay: 15s, drain: 25s}` | How an rproxy pod stops |
| `managed.parameters` | `{}` | Defaults shared by every Gateway, and what tenants may change |

- `managed.replicas`: one pod works, but traffic is interrupted while that pod is being replaced (an upgrade or a node drain). Use 2 or more in production. With 2 or more, the controller creates a PDB (PodDisruptionBudget: it tells Kubernetes how many pods may be stopped at once during a drain and similar events) with `maxUnavailable: 1`, and spreads the pods over different nodes where it can. To change it for one Gateway, use `replicas` in [8.4](#84-rproxygatewayparameters).
- `managed.serviceType`: choose it by how the Gateway gets its address.
  - `LoadBalancer` (default): the cluster's load balancer implementation (MetalLB, a cloud load balancer) gives the Service an address, and that becomes the Gateway's address. In a cluster without one, no address is assigned and the Gateway stays `Programmed: False` ([2.4](#24-check)).
  - `NodePort`: traffic is received on a port of every node. You put your own load balancer in front ([PLATFORM.md](PLATFORM.md)).
  - `ClusterIP`: reachable only from inside the cluster. The Gateway's address is the Service's ClusterIP.
- `managed.externalTrafficPolicy`: this is the Kubernetes Service's `externalTrafficPolicy`. It decides which node's rproxy pod receives traffic that comes from outside.
  - `Local`: traffic goes only to an rproxy pod on the node it arrived at. rproxy sees the client's real address, so RproxyPolicy's `allowFrom` ([8.1](#81-rproxypolicy)) and the logs can tell clients apart. Load balancers check the Service's `healthCheckNodePort` and send traffic only to nodes with a ready rproxy pod.
  - `Cluster`: traffic that arrives at any node can be forwarded to an rproxy pod on another node. Failover is smoother when a node goes down or the node announcing the address changes, but rproxy sees a node's address instead of the client's, so the client address is lost.
  - When empty (the default), it is `Local` for `LoadBalancer` and Kubernetes' default `Cluster` for `NodePort`. `ClusterIP` Services get none. Keep `Local` if you allow or limit clients by address; choose `Cluster` if smoother failover matters more. To change it for one Gateway, use `service.externalTrafficPolicy` in [8.4](#84-rproxygatewayparameters). The reasoning is in "rproxy availability (managed)" in [DESIGN.md](DESIGN.md).
- `managed.allocateLoadBalancerNodePorts`: Kubernetes normally gives `LoadBalancer` Services node ports (NodePorts) too. Load balancers like MetalLB deliver the address straight to a node and do not use node ports, so setting this to `false` avoids opening ports nobody uses. It has no effect on other Service types.
- `managed.addressCIDRs`: when a Gateway puts a fixed address in `spec.addresses`, managed mode makes it the Service's `externalIPs`. Only addresses inside the ranges listed here may be used; when the list is empty (the default), fixed addresses cannot be used at all. This keeps tenants from listening on arbitrary addresses. Never include the Service or pod address ranges: a Gateway could then capture traffic meant for another Service ([7.4](#74-address-routability), [PLATFORM.md](PLATFORM.md)).
- `managed.networkPolicy`: when `true`, each Gateway gets a NetworkPolicy so that only the controller can reach rproxy's control ports. The listener ports stay open to everyone. It has no effect if your CNI does not support NetworkPolicy ([12.](#12-security-basics)).
- `managed.shutdown`: how an rproxy pod stops (during an upgrade or a node drain). After SIGTERM, rproxy keeps accepting new connections for `delay` while reporting itself not ready, so that the Service and load balancers move traffic to other pods in the meantime. It then closes its listeners and waits up to `drain` for existing connections to finish. Connections still open after `drain` are cut. Lengthen `drain` to wait for long WebSocket or database connections, and `delay` if your load balancer takes longer to move traffic away. To change it for one Gateway, use `rproxy.shutdown` in [8.4](#84-rproxygatewayparameters) ([10.3](#103-pod-replacement-and-the-readiness-gate)).
- `managed.parameters`: defaults shared by every managed Gateway (pod count, resources and so on) and, under `policy`, which fields tenants may change per Gateway. The chart turns this into an `RproxyGatewayParameters` ([8.4](#84-rproxygatewayparameters)).

**fleet**

| Value | Default | What it decides |
|---|---|---|
| `fleet.enabled` | `false` | Whether to run in fleet mode instead of managed |

With `true`, no per-Gateway rproxies are created; the chart's DaemonSet serves every Gateway. The other fleet values (`fleet.addresses` and so on) and what to watch out for when switching are in [9.](#9-managed-and-fleet).

**Backends**

These three are the defaults for how rproxy connects to and watches the pods behind your routes.

| Value | Default | What it decides |
|---|---|---|
| `backends.connectTimeout` | `{http: 1s, l4: 1s}` | How long to wait when connecting to a backend pod |
| `backends.responseTimeout` | `30s` | How long to wait for an HTTPRoute backend's response headers |
| `backends.outlierDetection` | (see below) | Temporarily leaving out pods that keep failing (passive health checks) |

- `backends.connectTimeout`: `http` applies to HTTP routes and `l4` to TCP, TLS and UDP routes. If an HTTP connection fails, rproxy answers 502; for L4 it tries the next pod. Lengthen it when backends outside the cluster or far away on the network cannot accept a connection within a second. An empty string means rproxy's own default (5 seconds; for L4 with a single pod, the OS default of about 2 minutes).
- `backends.responseTimeout`: how long rproxy waits, after sending a request, for the response headers. Past it, rproxy answers 504. It does not apply to rules with their own `timeouts` ([4.5](#45-retries-and-timeouts)) or to GRPCRoutes. Response bodies (large downloads, SSE) and established WebSockets are not cut. If some app needs more than 30 seconds before it answers, the usual approach is to raise the limit for that route only, with `timeouts` or RproxyPolicy's `responseTimeout` ([8.1](#81-rproxypolicy)). An empty string means rproxy's own default (60 seconds).
- `backends.outlierDetection`: when a backend pod fails, Kubernetes takes a while to remove it from the EndpointSlice (40 to 60 seconds when the whole node goes down). In the meantime rproxy counts failures itself and leaves broken pods out for a while. By default, HTTP leaves out a pod after 3 failures in a row of the kinds 502, 503, 504, no connection or response timeout (5xx answers from the app itself are not counted), and L4 leaves out a pod after one failed connection. A pod is left out for 10 seconds at first, doubling each time up to a minute. For HTTP, at most half of the pods are left out. `{}` means rproxy's own behaviour (L4 leaves a pod out for 10 seconds after one failure; HTTP leaves nothing out). To change it for particular Gateways or Services only, use RproxyPolicy's `outlierDetection` ([8.1](#81-rproxypolicy)).

**Other**

| Value | Default | What it decides |
|---|---|---|
| `ui.namespace` | `""` (off) | Whether the management UI can see Gateways' rproxies |
| `migration.migrateTo` | `""` (off) | Which Gateway takes in Ingress and Traefik configuration |

- `ui.namespace`: when you give the namespace of the management UI (installed with its own chart), the controller writes there a Secret (`rproxy-ui-discovery`) the UI uses to find Gateways' rproxies, along with read-only tokens. It also opens the Gateways' NetworkPolicy so the UI's pods can reach the control API. Leave it empty if you do not use the UI.
- `migration.migrateTo`: when bringing Ingress or Traefik configuration into rproxy, the `namespace/name` of the Gateway that receives it. The steps are in [MIGRATION.md](MIGRATION.md).

**Example**: a `values.yaml` for a cluster where every Gateway gets two rproxy pods and MetalLB hands out addresses:

```yaml
# values.yaml
managed:
  replicas: 2
  allocateLoadBalancerNodePorts: false
```

`replicas: 2` keeps traffic flowing while pods are being replaced. `allocateLoadBalancerNodePorts: false` avoids assigning node ports that MetalLB does not use.

## Part 2: Writing standard Gateway API

Part 2 (chapters 2 to 7) is about writing standard Gateway API. The YAML here means the same on other Gateway API implementations. Where the text touches on how rproxy runs it (default timings, what happens on changes and so on), it is marked **rproxy-specific**. Experimental-channel features are marked **Gateway API experimental**.

## 2. A first Gateway and HTTPRoute

This chapter builds the smallest setup (one HTTP listener and one route attached to it) and checks that traffic from outside reaches the app. Later chapters build on what you create here.

### 2.1 The backend app

First, the app to send traffic to. It is the echo server used in Gateway API's own tests: it answers with the request it received as JSON, including which pod received it.

```yaml
apiVersion: apps/v1
kind: Deployment
metadata: {name: app, namespace: default}
spec:
  replicas: 2
  selector: {matchLabels: {app: app}}
  template:
    metadata: {labels: {app: app}}
    spec:
      containers:
        - name: echo
          image: registry.k8s.io/gateway-api/echo-basic:v1.5.1
          env:
            - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
            - {name: NAMESPACE, valueFrom: {fieldRef: {fieldPath: metadata.namespace}}}
          ports: [{containerPort: 3000}]
---
apiVersion: v1
kind: Service
metadata: {name: app, namespace: default}
spec:
  selector: {app: app}
  ports: [{name: http, port: 8080, targetPort: 3000}]
```

The Service `app` listens on port 8080 and passes traffic to port 3000 of the pods. The later examples point routes at port 8080 of this Service.

### 2.2 Gateway and HTTPRoute

Next, a Gateway `web` that listens for HTTP on port 80, and an HTTPRoute that sends requests for `app.example.com` to the Service `app`:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs:
    - name: web
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - name: app
          port: 8080
```

The lines that matter:

- `gatewayClassName: rproxy` hands this Gateway to rproxy-gateway.
- `parentRefs` names the Gateway the route attaches to.
- `hostnames` is matched against the request's Host header. Leave it out to accept every hostname.
- `backendRefs` is the Service and port to send to.
- **rproxy-specific**: rproxy does not go through the Service's ClusterIP; it sends straight to the IPs of the ready pods in the Service's EndpointSlices. So rproxy itself balances between pods and checks their health.

### 2.3 What gets created

> **rproxy-specific**: which pods and Services a Gateway leads to differs per implementation. This section describes rproxy-gateway in managed mode.

In managed mode (the default), creating a Gateway makes the controller create a complete rproxy for that Gateway, in the Gateway's namespace. This section lists what is created.

In the names, `<id>` is `<namespace>-<name>` followed by a 6-character hash (for example `default-web-1a2b3c`). Every object has the Gateway as its owner, so deleting the Gateway deletes them too. You never create or delete them yourself.

| Created | Name | Role |
|---|---|---|
| rproxy Deployment and ServiceAccount | `rproxy-<id>` | The rproxy pods that relay traffic |
| Service (`LoadBalancer`) | `rproxy-<id>` | The Gateway's address; carries every listener port |
| Certificate Secret | `rproxy-<id>-certs` | Certificates and keys rproxy uses ([11.](#11-how-it-talks-to-rproxy)) |
| Control API Secret (certificate, token file) | `rproxy-<id>-api` | Credentials for the controller and rproxy to talk |
| NetworkPolicy | `rproxy-<id>` | Opens the control ports to the controller only ([12.](#12-security-basics)) |

You can list them together by the Gateway's label:

```bash
kubectl -n default get deploy,svc,pod -l gateway.networking.k8s.io/gateway-name=web
```

### 2.4 Check

Check that the Gateway and the route were accepted and that traffic gets through:

```bash
kubectl -n default get gateway web
# NAME   CLASS    ADDRESS        PROGRAMMED   AGE
# web    rproxy   192.0.2.100    True         40s
kubectl -n default get httproute app
# NAME   HOSTNAMES             AGE
# app    ["app.example.com"]   40s
```

- `ADDRESS` is the Gateway's address: the address of the `LoadBalancer` Service (the ClusterIP for the `ClusterIP` type).
- `PROGRAMMED` `True` means the rules are loaded on at least one rproxy pod and it can take traffic.

Take the address and send a real request. Even before DNS points at it, you can test with a Host header:

```bash
ADDR=$(kubectl -n default get gateway web -o jsonpath='{.status.addresses[0].value}')
curl -H 'Host: app.example.com' http://$ADDR/
```

`kubectl describe` shows the status in more detail:

```bash
kubectl -n default describe gateway web
```

```
Status:
  Addresses:
    Type:   IPAddress
    Value:  192.0.2.100
  Conditions:
    Reason:  Accepted      Status: True   Type: Accepted
    Reason:  Programmed    Status: True   Type: Programmed
  Listeners:
    Attached Routes:  1
    Conditions:
      Reason:  Accepted       Status: True   Type: Accepted
      Reason:  ResolvedRefs   Status: True   Type: ResolvedRefs
      Reason:  NoConflicts    Status: False  Type: Conflicted
      Reason:  Programmed     Status: True   Type: Programmed
    Name:  http
```

(Part of the output is left out.) Conditions are listed for the whole Gateway and for each listener. `Accepted` means what you wrote was accepted, `ResolvedRefs` means references such as certificates were found, and `Programmed` means it is loaded in rproxy. Only `Conflicted` is healthy when `False`. `Attached Routes` is how many routes are attached to the listener.

A route's status is under `Status.Parents` of `kubectl -n default describe httproute app`, one entry per Gateway it attaches to. `Accepted` and `ResolvedRefs` `True` mean it is healthy.

If `ADDRESS` stays empty and `PROGRAMMED` is `False` (reason `AddressNotAssigned`, message "waiting for an address (the rproxy Service)"), nothing in the cluster gives `LoadBalancer` Services addresses (MetalLB or a cloud load balancer). Provide one following [PLATFORM.md](PLATFORM.md), or set the chart's `managed.serviceType` (**rproxy-specific**) to `NodePort` or `ClusterIP` ([1.3](#13-main-chart-values)).

## 3. HTTPS

This chapter covers receiving HTTPS on a Gateway: putting a certificate you have into a Secret (3.1), letting cert-manager obtain one (3.2), and sending requests that arrive over HTTP on to HTTPS (3.3).

### 3.1 A TLS Secret in certificateRefs

Use this when you already have certificate and key files. First, put them into a Secret of type `kubernetes.io/tls`:

```bash
kubectl -n default create secret tls app-tls --cert=app.crt --key=app.key
```

Then add an HTTPS listener to the Gateway that names that Secret in `certificateRefs`, and attach the route to the HTTPS listener:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        mode: Terminate
        certificateRefs:
          - name: app-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs:
    - name: web
      sectionName: https
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - name: app
          port: 8080
```

The lines that matter:

- On a `protocol: HTTPS` listener, rproxy terminates (decrypts) TLS and routes what is inside as HTTP. `mode: Terminate` names this behaviour; leaving it out means the same.
- You can put several `HTTPS` listeners on the same port (one certificate per hostname). rproxy picks the certificate by the hostname the client sends at the start of TLS (SNI).
- The route's `sectionName` is the name of the listener to attach to. Without it, the route attaches to every listener that accepts it. Here it attaches only to `https`, so the app is not served over plain HTTP.

Two things are worth knowing about how rproxy handles certificates (**rproxy-specific**):

- Private keys never go through the control API between the controller and rproxy. The controller copies the key into a Secret named `rproxy-<id>-certs`, and the kubelet hands it to the rproxy pods as a file ([11.](#11-how-it-talks-to-rproxy)).
- When you rewrite (renew) the certificate Secret, the new certificate is used from the next connection on, without cutting existing connections ([10.](#10-changing-things-while-they-run)).

To check, look at the listeners' `ResolvedRefs`. When the certificate is found, the reason is `ResolvedRefs` too:

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{.conditions[?(@.type=="ResolvedRefs")].reason}{"\n"}{end}'
# http ResolvedRefs
# https ResolvedRefs
curl --resolve app.example.com:443:$ADDR https://app.example.com/
```

If the Secret is missing or broken, the listener becomes `ResolvedRefs: False` (reason `InvalidCertificateRef`) and `Programmed: False`, and no rule is made for that port.

### 3.2 cert-manager

To obtain and renew certificates automatically from Let's Encrypt or another CA, use [cert-manager](https://cert-manager.io/). Point `certificateRefs` at the Secret cert-manager creates, just as in 3.1. rproxy-gateway only reads the Secret; there is no special link between it and cert-manager.

There are two ways to write it.

**Writing a Certificate yourself**: this is the form the acceptance tests check, renewal included. You write a ClusterIssuer (where and how to obtain certificates) and a Certificate (which names go into which Secret):

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: {name: letsencrypt}
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@example.com
    privateKeySecretRef: {name: letsencrypt-account}
    solvers:
      - http01:
          gatewayHTTPRoute:
            parentRefs:
              - {name: web, namespace: default, kind: Gateway, sectionName: http}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: app-tls, namespace: default}
spec:
  secretName: app-tls
  dnsNames: [app.example.com]
  issuerRef: {name: letsencrypt, kind: ClusterIssuer, group: cert-manager.io}
```

- The Gateway is the same as in 3.1 (`certificateRefs: [{name: app-tls}]`). The listener stays `ResolvedRefs: False` until the Secret exists, then turns `True` by itself. You do not need to reapply anything.
- With `gatewayHTTPRoute`, cert-manager creates an HTTPRoute on the Gateway's `http` listener to answer Let's Encrypt's HTTP-01 check (the challenge). For this, turn on cert-manager's Gateway API support (for example `config.enableGatewayAPI: true` in cert-manager's chart; check cert-manager's documentation). With DNS-01 the Gateway is not involved.
- Putting the redirect route from 3.3 on the same hostname is fine: the challenge path (`/.well-known/acme-challenge/...`) is a longer match, so it is chosen first.

**Writing an annotation on the Gateway**: this is neither standard Gateway API nor rproxy; it is a cert-manager feature. With one annotation on the Gateway, cert-manager creates the Certificate from the listener's `hostname` and `certificateRefs`, so you do not write the Certificate yourself.

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: web
  namespace: default
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
```

The `cert-manager.io/cluster-issuer` annotation names the ClusterIssuer to use.

Either way, check that the certificate was issued:

```bash
kubectl -n default get certificate app-tls     # READY True
kubectl -n default get secret app-tls
```

**rproxy's ACME is not used** (**rproxy-specific**): rproxy has its own ACME (getting certificates from Let's Encrypt and similar CAs by itself), but Gateway API does not use it. Certificates come from the Secrets in `certificateRefs`. The reason is that rproxy's ACME orders a certificate from every pod; with several pods, the HTTP-01 challenge request can reach another pod and fail. Get your certificates with cert-manager. The only exception is migrating Traefik's `certResolver` ("Mapping" in [MIGRATION.md](MIGRATION.md)).

Even if cert-manager does not build in the DNS provider you use for DNS-01 (OCI and others), a cert-manager DNS-01 webhook can often cover it. Webhooks are separate per provider, and this project has not tested them.

### 3.3 Redirect HTTP to HTTPS

Once HTTPS is set up, you normally send requests that arrive over HTTP (80) on to HTTPS. Attach a route that only redirects to the HTTP listener:

```yaml
# everything that comes to HTTP (80) goes to HTTPS
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: to-https, namespace: default}
spec:
  parentRefs:
    - name: web
      sectionName: http
  hostnames: [app.example.com]
  rules:
    - filters:
        - type: RequestRedirect
          requestRedirect:
            scheme: https
            statusCode: 301
```

- `sectionName: http` attaches this route to the HTTP listener only. The app's route (`app` in 3.1) stays attached to `sectionName: https` only.
- Changing the scheme with `scheme: https` also changes the port to that scheme's default (443). You do not need to write the port.

A 301 whose `Location` header holds the HTTPS URL means it works:

```bash
curl -sI -H 'Host: app.example.com' http://$ADDR/path
# HTTP/1.1 301 Moved Permanently
# location: https://app.example.com/path
```

## 4. HTTP routing

This chapter covers routing HTTP requests with HTTPRoutes: by hostname, path and headers, splitting by weight, filters such as header rewriting, retries and timeouts, and gRPC. Every HTTPRoute in this chapter attaches to the Gateway `web` from 2.2.

### 4.1 Hostnames

This is how one Gateway serves sites under several hostnames. A request reaches a route only if it matches both the listener's `hostname` and the route's `hostnames`. In the example below, one HTTPS listener with a wildcard certificate carries two routes, `app.example.com` and `api.example.com`:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: "*.example.com"
      tls:
        certificateRefs:
          - name: wildcard-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: api, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [api.example.com]
  rules:
    - backendRefs: [{name: api, port: 8080}]
```

The hostname rules:

- `*.example.com` matches every name under `example.com` (deeper ones such as `a.b.example.com` too). It does not match `example.com` itself.
- If the listener's and the route's hostnames do not overlap, the route is not accepted: `Accepted: False` (reason `NoMatchingListenerHostname`).
- A request that matches no route gets a 404.
- If the same port has a listener with a more specific hostname (say `app.example.com`), requests for that name reach only that listener's routes, not the wildcard listener's. This is a Gateway API rule (listener isolation).

### 4.2 Path, header, query, method

This is how to send requests for the same hostname to different places by path or header. The example puts the commonly used kinds of match in one HTTPRoute:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    # starting with /api/ (and /api itself)
    - matches:
        - path: {type: PathPrefix, value: /api}
      backendRefs: [{name: api, port: 8080}]
    # exactly /healthz
    - matches:
        - path: {type: Exact, value: /healthz}
      backendRefs: [{name: app, port: 8080}]
    # header X-Debug: 1 and POST
    - matches:
        - headers:
            - {name: X-Debug, value: "1"}
          method: POST
      backendRefs: [{name: app-v2, port: 8080}]
    # query ?beta=true (one item of matches is enough: OR)
    - matches:
        - queryParams:
            - {name: beta, value: "true"}
        - path: {type: RegularExpression, value: "/v[0-9]+/beta/.*"}
      backendRefs: [{name: app-v2, port: 8080}]
    # anything else
    - backendRefs: [{name: app, port: 8080}]
```

How to read it:

- All the conditions inside one `matches` entry must match (AND). With several entries in `matches`, any one of them is enough (OR). The third rule needs both the header and POST; the fourth is satisfied by either the query or the regular-expression path.
- `PathPrefix: /api` matches `/api` and `/api/...` but not `/apis`, because paths are compared segment by segment at `/`.
- Rules are not chosen in the order you write them but in Gateway API's precedence (Exact → longer PathPrefix → method → number of headers → number of queries → older route...). Details: "Rule precedence" in "Mapping (Gateway API → rproxy)" of [DESIGN.md](DESIGN.md).
- Headers and queries can use `type: RegularExpression` too.

### 4.3 Weights and canaries

Use this when you want to send only part of the requests to a new version of an app and watch how it does (a canary). List several Services in one rule's `backendRefs` and set the split with `weight`:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080, weight: 90}
        - {name: app-v2, port: 8080, weight: 10}
```

- In this example, about one request in ten goes to `app-v2`.
- **rproxy-specific**: the controller divides each Service's weight by its number of pods to get per-pod weights, so the ratio between the Services holds even when they have different numbers of pods.
- A Service with `weight: 0` gets nothing.
- Changing weights does not cut existing connections ([10.](#10-changing-things-while-they-run)). Steps for raising the weight bit by bit are in [14.5](#145-canary-by-weight).

### 4.4 Filters

Filters rewrite headers or paths before a request is sent on (or before the response is returned), or answer with a redirect. The example uses the three most common kinds, one per rule:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    # add and remove headers
    - matches: [{path: {type: PathPrefix, value: /}}]
      filters:
        - type: RequestHeaderModifier
          requestHeaderModifier:
            set: [{name: X-Env, value: prod}]
            add: [{name: X-Via, value: rproxy}]
            remove: [X-Internal-Token]
        - type: ResponseHeaderModifier
          responseHeaderModifier:
            set: [{name: Strict-Transport-Security, value: max-age=31536000}]
      backendRefs: [{name: app, port: 8080}]
    # send /old/... as /new/... (the client does not see it)
    - matches: [{path: {type: PathPrefix, value: /old}}]
      filters:
        - type: URLRewrite
          urlRewrite:
            hostname: internal.example.com
            path: {type: ReplacePrefixMatch, replacePrefixMatch: /new}
      backendRefs: [{name: app, port: 8080}]
    # redirect /docs to another site (the client gets a 302)
    - matches: [{path: {type: PathPrefix, value: /docs}}]
      filters:
        - type: RequestRedirect
          requestRedirect:
            hostname: docs.example.com
            path: {type: ReplaceFullPath, replaceFullPath: /}
            statusCode: 302
```

- In `RequestHeaderModifier` and `ResponseHeaderModifier`, `set` replaces a value, `add` appends to an existing value separated by `,`, and `remove` deletes the header.
- `URLRewrite` rewrites the path or hostname only on the way to the backend; the client does not see it. `ReplacePrefixMatch` can be used only when the rule has exactly one path match (a `PathPrefix`).
- `RequestRedirect` sends nothing to the backend and answers the client with a redirect.

`CORS`, `RequestMirror` (sending a copy of the request to another Service) and per-backendRef `filters` work too. **rproxy-specific**: rproxy's own features such as rate limits and authentication are used through the standard extension point `ExtensionRef`, which names an RproxyMiddleware ([8.2](#82-rproxymiddleware)).

### 4.5 Retries and timeouts

This is how to decide, per rule, what happens when a backend is slow or fails for a moment. `timeouts` caps how long to wait, and `retry` says when to send again:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
      timeouts:
        request: 30s          # the client's whole request
        backendRequest: 10s   # one send to the backend
      retry:                  # experimental CRDs in Gateway API v1.6, standard from v1.7
        attempts: 2           # retries on top of the first try
        codes: [502, 503, 504]
        backoff: 100ms
```

- `timeouts.request` is how long the client's whole request may take. `timeouts.backendRequest` is how long one send to the backend may take until its response. Past either, rproxy answers 504.
- **rproxy-specific**: rules without `timeouts` get a default of waiting up to 30 seconds for the backend's response headers (the chart's `backends.responseTimeout`, [1.3](#13-main-chart-values)). This default does not cut response bodies (large downloads, SSE) or WebSockets. For apps that take longer to answer, write `backendRequest` or use RproxyPolicy's `responseTimeout` ([8.1](#81-rproxypolicy)).
- `retry`'s `attempts` is how many times to send again on top of the first try.
- **rproxy-specific**: rproxy sends again when it could not connect, when there was no response, or when one of the status codes in `codes` came back. It does so only for methods that give the same result however often they are sent (GET, HEAD, OPTIONS, PUT, DELETE, TRACE) and only for requests without a body. POST and similar requests are not sent again, since they could be processed twice.
- **Gateway API experimental**: in Gateway API v1.6, `retry` needs the experimental CRDs ([1.1](#11-gateway-apis-crds)). From v1.7 it is in standard.

### 4.6 GRPCRoute

To publish a gRPC service, use a GRPCRoute instead of an HTTPRoute. It routes by gRPC service and method names instead of paths:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GRPCRoute
metadata: {name: echo, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [grpc.example.com]
  rules:
    - matches:
        - method: {service: echo.Echo, method: Ping}
      backendRefs: [{name: grpc-echo, port: 9000}]
    - matches:
        - method: {service: echo.Echo}
          headers:
            - {name: x-canary, value: "true"}
      backendRefs: [{name: grpc-echo, port: 9000}]
```

- The first rule matches calls to `echo.Echo/Ping` only. The second matches calls to any method of `echo.Echo` that carry the header `x-canary: true`.
- A GRPCRoute attaches to `HTTP` and `HTTPS` listeners, and can share a port with HTTPRoutes.
- **rproxy-specific**: rproxy talks to the backend in h2c (unencrypted HTTP/2). If the backend accepts TLS, use a BackendTLSPolicy ([7.1](#71-backendtlspolicy)).
- **rproxy-specific**: GRPCRoutes do not get the 30-second response default that HTTPRoutes get, because gRPC streams may hold their response headers back for a long time. If you need a time limit, use RproxyPolicy's `responseTimeout`.

If an HTTPRoute's backend is an app that speaks h2c, write `appProtocol: kubernetes.io/h2c` on the Service port.

## 5. L4 forwarding

This chapter covers forwarding traffic that is not HTTP (databases, DNS, mail and so on) as TCP, UDP or TLS. L4 here means forwarding connections or packets as they are, without reading their contents as HTTP.

### 5.1 TCPRoute and UDPRoute

This forwards a TCP or UDP port to a Service as it is. The example receives PostgreSQL (TCP 5432) and DNS (UDP 53) on one Gateway:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: l4, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: postgres
      port: 5432
      protocol: TCP
      allowedRoutes:
        kinds: [{kind: TCPRoute}]
    - name: dns
      port: 53
      protocol: UDP
      allowedRoutes:
        kinds: [{kind: UDPRoute}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: postgres, namespace: default}
spec:
  parentRefs:
    - {name: l4, sectionName: postgres}
  rules:
    - backendRefs:
        - {name: postgres, port: 5432}
---
apiVersion: gateway.networking.k8s.io/v1
kind: UDPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs:
    - {name: l4, sectionName: dns}
  rules:
    - backendRefs:
        - {name: dns, port: 53}
```

- `allowedRoutes.kinds` is the kind of route the listener accepts. You can leave it out: a `TCP` listener then accepts TCPRoutes and a `UDP` listener UDPRoutes.
- L4 routes have no hostnames, so a route names its listener with `sectionName`.
- **rproxy-specific**: if you attach several TCPRoutes (or UDPRoutes) to one listener, all of them are `Accepted`, but traffic goes only to the oldest route. Attach one route per listener.
- **rproxy-specific**: a `TCP` or `UDP` listener with no route attached gets no rproxy rule (the Service still has the port).
- With older Gateway API CRDs (before v1.6), TCPRoute and UDPRoute are `v1alpha2`. Write the version your API server serves.

Check the route status and a real connection:

```bash
kubectl -n default get tcproute,udproute
kubectl -n default get tcproute postgres -o jsonpath='{.status.parents[0].conditions[*].reason}'
# Accepted ResolvedRefs
psql -h $ADDR -p 5432 -U app
dig @$ADDR example.com
```

### 5.2 TLSRoute (passthrough and terminate)

This forwards TLS traffic chosen by hostname (SNI). There are two forms, depending on whether rproxy decrypts:

- **Passthrough** (no decryption): the backend pods hold the certificate, and rproxy relays the encrypted traffic as it is. Use it when you want end-to-end encryption or do not want to hand the certificate to the Gateway.
- **Terminate** (rproxy decrypts): rproxy terminates TLS with a certificate and sends the contents to the backend as plain TCP. Use it to let rproxy take care of TLS in front of an app that does not speak TLS.

The example puts listeners of both forms on one Gateway:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: tls, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    # not decrypted (the backend pods hold the certificate)
    - name: passthrough
      port: 443
      protocol: TLS
      hostname: "*.example.com"
      tls:
        mode: Passthrough
    # rproxy decrypts and sends plain TCP to the backend
    - name: terminate
      port: 8443
      protocol: TLS
      hostname: mail.example.com
      tls:
        mode: Terminate
        certificateRefs:
          - name: mail-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: secure-app, namespace: default}
spec:
  parentRefs:
    - {name: tls, sectionName: passthrough}
  hostnames: [secure.example.com]
  rules:
    - backendRefs:
        - {name: secure-app, port: 8443}
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: mail, namespace: default}
spec:
  parentRefs:
    - {name: tls, sectionName: terminate}
  hostnames: [mail.example.com]
  rules:
    - backendRefs:
        - {name: mail, port: 443}
```

- With **Passthrough**, rproxy looks only at the SNI in TLS's first message (the ClientHello) and sends the traffic on, still encrypted. It cannot see the HTTP inside, so it cannot rewrite headers and so on. Connections that match no route's hostname are refused.
- With **Terminate**, rproxy terminates TLS with the certificate and sends the contents on as TCP. It does not route the contents as HTTP. If you want HTTP routing, use an `HTTPS` listener and an HTTPRoute ([3.1](#31-a-tls-secret-in-certificaterefs)).
- `HTTPS` and `TLS` (Passthrough) listeners can share a port. The hostname decides which names are decrypted and which are not ([14.4](#144-sni-passthrough-on-443)).

### 5.3 The listener for each route

A quick reference for which route attaches to which listener. The listener's `protocol` and `tls.mode` decide which kinds of route it accepts and whether it needs a certificate.

| Route | Listener `protocol` | `tls.mode` | Certificate (`certificateRefs`) |
|---|---|---|---|
| HTTPRoute, GRPCRoute | `HTTP` | — | — |
| HTTPRoute, GRPCRoute | `HTTPS` | `Terminate` (may be left out) | needed |
| TLSRoute | `TLS` | `Passthrough` | not needed (the backend has it) |
| TLSRoute | `TLS` | `Terminate` | needed |
| TCPRoute | `TCP` | — | — |
| UDPRoute | `UDP` | — | — |

Two things are worth knowing about ports with rproxy (**rproxy-specific**):

- The managed Service carries every listener port of the Gateway. To put TCP and UDP on the same port (such as 53 for DNS) in one `LoadBalancer` Service, the load balancer implementation must support mixing TCP and UDP.
- Ports below 1024 work as they are. The rproxy pods do not run as root; a safe sysctl lets them open low ports.

## 6. Other namespaces

This chapter covers using a Gateway across namespaces. It is for setups where the infrastructure team owns one shared Gateway and each team attaches routes or listeners from its own namespace.

### 6.1 allowedRoutes: which namespaces' routes a Gateway takes

By default a Gateway accepts only routes from its own namespace (`from: Same`). To let teams in other namespaces use it, widen the accepted namespaces with the listener's `allowedRoutes`. The example accepts routes only from namespaces labelled `gateway-access: "true"`:

```yaml
# a shared Gateway of infra: takes routes of namespaces labelled gateway-access: "true"
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: shared, namespace: infra}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: "*.example.com"
      tls:
        certificateRefs:
          - name: wildcard-tls
      allowedRoutes:
        namespaces:
          from: Selector
          selector:
            matchLabels:
              gateway-access: "true"
```

- `from` is one of `Same` (the default: the same namespace only), `All` (every namespace) or `Selector` (namespaces chosen by label).
- Routes from namespaces that are not accepted get `Accepted: False` (reason `NotAllowedByListeners`).
- If you use `Selector`, make sure tenants cannot change namespace labels themselves. Otherwise they could attach routes from namespaces you did not allow.

### 6.2 ReferenceGrant: using another namespace's Services and Secrets

A route normally points at Services in its own namespace. To point at a Service in another namespace, put a ReferenceGrant in the namespace **being pointed at**. A ReferenceGrant is a permission issued by the owner of what is referred to: "this thing in my namespace may be used by that kind of resource in that namespace". It keeps anyone from sending traffic to another team's Service uninvited.

In the example, an HTTPRoute in `team-a` uses the Service `api` in `default`:

```yaml
# team-a's route: names the Gateway of another namespace with its namespace
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: team-a}
spec:
  parentRefs:
    - {name: shared, namespace: infra}
  hostnames: [app.example.com]
  rules:
    - matches: [{path: {type: PathPrefix, value: /api}}]
      backendRefs:
        # a Service of another namespace (needs the ReferenceGrant below)
        - {name: api, namespace: default, port: 8080}
    - backendRefs:
        - {name: app, port: 8080}
---
# default's owner lets team-a's HTTPRoutes use the Service api
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata: {name: team-a-to-api, namespace: default}
spec:
  from:
    - {group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: team-a}
  to:
    - {group: "", kind: Service, name: api}
```

- `namespace: infra` in `parentRefs` points at the shared Gateway from 6.1. Pointing at a Gateway needs no ReferenceGrant (the Gateway side allows it with `allowedRoutes`).
- The ReferenceGrant's `from` is "who" (HTTPRoutes in `team-a`) and `to` is "what" (the Service `api`).
- Without the ReferenceGrant, the route becomes `ResolvedRefs: False` (reason `RefNotPermitted`, message "Service default/api: no ReferenceGrant allows the reference"), and requests meant for that backend get a 500.
- The same applies when a Gateway's `certificateRefs` names a Secret in another namespace (`from` is `kind: Gateway`, `to` is `kind: Secret`). **rproxy-specific**: in managed mode, that private key is copied into a Secret in the Gateway's namespace ("Keys from other namespaces (M7)" in [SECURITY.md](SECURITY.md)).
- In Gateway API v1.6, a ReferenceGrant can be written as either `v1` or `v1beta1`.

### 6.3 ListenerSet: listeners from other namespaces

This lets tenants add listeners with their own hostnames and certificates to a shared Gateway. Use it when each team should keep its own certificate in its own namespace, instead of the Gateway's owner providing one certificate for everyone as with the wildcard certificate in 6.1.

The Gateway's owner decides with `allowedListeners` which namespaces may add listeners (the default is `None`: nobody may):

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: shared, namespace: infra}
spec:
  gatewayClassName: rproxy
  allowedListeners:
    namespaces:
      from: Selector
      selector:
        matchLabels:
          gateway-access: "true"
  listeners:
    - name: http
      port: 80
      protocol: HTTP
---
# team-a adds a listener with its own hostname and certificate
apiVersion: gateway.networking.k8s.io/v1
kind: ListenerSet
metadata: {name: team-a, namespace: team-a}
spec:
  parentRef:
    name: shared
    namespace: infra
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls      # team-a's Secret
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: team-a}
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: ListenerSet
      name: team-a
      sectionName: https
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080}
```

- The ListenerSet's `parentRef` names the Gateway the listeners are added to.
- Certificates for the ListenerSet's listeners are looked up in the ListenerSet's namespace (here `team-a`). `Same` in `allowedRoutes` also means the ListenerSet's namespace.
- A route attaches to a ListenerSet's listener only when its `parentRefs` says `kind: ListenerSet`. Routes that point at the Gateway do not attach to the ListenerSet's listeners.
- When settings on the same port conflict, the older ListenerSet wins.

Check that the added listeners were accepted:

```bash
kubectl -n team-a get listenerset team-a
# NAME     ACCEPTED   PROGRAMMED   AGE
# team-a   True       True         30s
kubectl -n infra get gateway shared -o jsonpath='{.status.attachedListenerSets}'
```

## 7. Other Gateway API features

This chapter covers the standard Gateway API features the earlier chapters of Part 2 did not. TLS to backends (7.1) and client certificate validation (7.2) are standard-channel features. Listener filters (7.3) and address routability (7.4) are features of Gateway API v1.7's experimental channel.

### 7.1 BackendTLSPolicy

BackendTLSPolicy is the Gateway API setting for sending from rproxy to the backend pods over TLS (HTTPS). Use it when the backend app accepts only HTTPS, or when you want traffic inside the cluster encrypted too. You write which name's certificate to expect and which CA to check it with.

```yaml
# HTTPS to port 8443 of the Service secure-app, its certificate checked with a ConfigMap's CA
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {name: secure-app, namespace: default}
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: secure-app
  validation:
    hostname: secure-app.default.svc
    caCertificateRefs:
      - group: ""
        kind: ConfigMap
        name: secure-app-ca     # PEM in the key ca.crt
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: secure, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [secure.example.com]
  rules:
    - backendRefs: [{name: secure-app, port: 8443}]
```

- `targetRefs` names the Service to send to over TLS. Nothing is added on the HTTPRoute side.
- `validation.hostname` is the name sent as TLS SNI and used to check the backend's certificate. To check a different name, use `subjectAltNames`.
- `caCertificateRefs` is the CA that checks the backend's certificate, as PEM under the key `ca.crt` of a ConfigMap. For certificates from a public CA, you can write `wellKnownCACertificates: System` instead.
- With `sectionName` in `targetRefs`, the policy applies only to a particular port of the Service. A policy naming a port wins over one naming the whole Service.
- If no usable CA is found, it becomes `Accepted: False` (reason `NoValidCACertificate`), and requests to that backend get a 500.
- If the backend asks for a client certificate, put the Secret of the certificate to present in the Gateway's `spec.tls.backend.clientCertificateRef`.

Check it in `Status.Ancestors` (status per Gateway) of `kubectl -n default describe backendtlspolicy secure-app`.

### 7.2 ClusterTrustBundle and client certificate validation

This section covers ClusterTrustBundle, which shares CA certificates across the cluster, and asking clients for certificates (mTLS).

A ClusterTrustBundle is a Kubernetes resource that holds a bundle of CA certificates for the whole cluster (`certificates.k8s.io/v1` in Kubernetes 1.37; before that, when the beta API is turned on). `caCertificateRefs` can name it, and since it belongs to no namespace, no ReferenceGrant is needed. It is handy when you keep your internal CA in one place and use it from many Gateways and BackendTLSPolicies.

In the example, the internal CA becomes a ClusterTrustBundle, used both to check backend certificates (BackendTLSPolicy) and to check client certificates (Gateway):

```yaml
apiVersion: certificates.k8s.io/v1
kind: ClusterTrustBundle
metadata: {name: internal-ca}
spec:
  trustBundle: |
    -----BEGIN CERTIFICATE-----
    ...(the internal CA's PEM)...
    -----END CERTIFICATE-----
---
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {name: secure-app, namespace: default}
spec:
  targetRefs:
    - {group: "", kind: Service, name: secure-app}
  validation:
    hostname: secure-app.default.svc
    caCertificateRefs:
      - group: certificates.k8s.io
        kind: ClusterTrustBundle
        name: internal-ca
---
# ask for client certificates (mTLS); the CA is a ConfigMap or a ClusterTrustBundle
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  tls:
    frontend:
      default:
        validation:
          caCertificateRefs:
            - {group: certificates.k8s.io, kind: ClusterTrustBundle, name: internal-ca}
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
```

- The Gateway's `spec.tls.frontend.default.validation` asks clients of its HTTPS listeners for a certificate and checks it with that CA.
- `perPort` in `spec.tls.frontend` sets different settings per port.
- With `mode: AllowInsecureFallback`, connections are accepted even when the client presents no certificate or it fails the check, and the result is passed to the backend in the `X-Client-Verify` header (this header is **rproxy-specific**). Use it when the app should decide what to do.
- If the API server does not serve ClusterTrustBundle, the reference gets reason `InvalidKind` (`InvalidCACertificateKind` on a Gateway).

### 7.3 Listener filters

> **Gateway API experimental**: needs Gateway API v1.7's experimental CRDs.

Listener filters are processing applied to every request that arrives at a listener, **before** a route is chosen. Use them to apply authentication or rate limits to a whole port, whatever the hostname or path. It is a feature of Gateway API v1.7's experimental CRDs (GEP-5224): you write `filters.requests` on the listener. The filters run in the order written and become rproxy v0.4.22's `http.pre_routing`.

In the example, every request to port 80 is first checked with an authorization server, then put through a rate limit:

```yaml
# needs Gateway API v1.7's experimental CRDs
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
      filters:
        requests:
          # 1. before any route, ask the authorization server (Service ext-authz)
          - type: ExternalAuth
            externalAuth:
              protocol: HTTP
              backendRef: {name: ext-authz, port: 9001}
              http:
                path: /check
                allowedResponseHeaders: [X-User]
          # 2. then a rate limit on every request
          - type: ExtensionRef
            extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: rate-limit}
---
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: rate-limit, namespace: default}
spec:
  rate_limit: {average: 10, period: 1s, burst: 20}
```

- `ExternalAuth` asks `/check` on port 9001 of the Service `ext-authz` and lets the request continue only when allowed. `allowedResponseHeaders` are headers from the authorization server's answer passed on to the backend.
- **rproxy-specific**: the second filter uses the same RproxyMiddleware as in 8.2. In other implementations, what `ExtensionRef` can name differs per implementation.

What rproxy supports, and its rules (**rproxy-specific**):

- The only filters supported are `ExternalAuth` and `ExtensionRef` (RproxyMiddleware), on `HTTP` and `HTTPS` listeners only.
- These RproxyMiddleware settings cannot be used: `retry`, `mirror` and `replace_host`, which rproxy's `pre_routing` does not accept; settings that name a service (such as `forward_auth`'s `service`), because only a route's rules can be named and a listener has none (for authentication use `ExternalAuth` or `forward_auth`'s `address`); and `headers` that change Host, refused because the listener is not chosen again after such a change.
- In rproxy, the listeners on one port form a single rule, so give every listener on a port the same filters. If they differ, the listeners with filters become `Accepted: False` (reason `UnsupportedValue`).
- If a reference does not resolve (the Service or RproxyMiddleware is missing), the listener becomes `ResolvedRefs: False` (reason `InvalidListenerFilterRef`) and answers 500 at that filter. It never skips authentication and lets the request through.
- Until all of the Gateway's rproxy pods report that they support `pre_routing`, the listener is `Accepted: False` (reason `UnsupportedValue`, message "not known (yet)"). The bundled rproxy v0.4.22 supports it, so normally the listener is accepted once the pods start.
- Details: 17. of [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md).

### 7.4 Address routability

> **Gateway API experimental**: needs Gateway API v1.7's experimental CRDs.

This gives a Gateway an address reachable only from inside the cluster, in addition to its outside address. Use it when the same Gateway serves both outside clients and apps in the cluster, and traffic from inside should not go around through the outside. With Gateway API v1.7's experimental CRDs you can write `spec.addresses[].routability` (GEP-5093). For each entry with `routability: Cluster`, the controller creates a ClusterIP Service for use inside the cluster in the Gateway's namespace.

```yaml
# needs Gateway API v1.7's experimental CRDs
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  addresses:
    - type: IPAddress                       # default: the Service's outside address (LoadBalancer)
    - type: IPAddress
      routability: Cluster                  # and a cluster-only ClusterIP Service
  listeners:
    - name: http
      port: 80
      protocol: HTTP
```

- The first entry has neither a value nor `routability`, so it is the Service's outside address (`LoadBalancer`) as usual.
- The second entry becomes the cluster-only ClusterIP Service.

rproxy's rules (**rproxy-specific**):

- If every entry is `Cluster`, the Gateway's Service itself becomes `ClusterIP` and nothing is exposed outside.
- Giving a `Cluster` entry a value (a fixed ClusterIP) is not allowed by default (`AddressesAssigned: False`). The administrator can open it with the class's `policy.clusterIPs: true`.
- Changing the order of the entries recreates the Services and changes their ClusterIPs. Add new entries at the end.
- Fleet mode does not assign `Cluster` addresses.
- A `type: IPAddress` with a value (a fixed outside address) becomes the Service's `externalIPs` in managed mode, and only addresses inside the chart's `managed.addressCIDRs` ([1.3](#13-main-chart-values)) can be used. Details in [PLATFORM.md](PLATFORM.md).

## Part 3: rproxy-only settings

Part 3 (chapters 8 to 12) covers settings and behaviour that only rproxy-gateway has. None of it carries over to other Gateway API implementations. Each section starts by saying why the setting exists (what standard Gateway API cannot express) and, if there is one, the standard alternative.

## 8. rproxy's CRDs

> **rproxy-specific**: the four CRDs in this chapter are rproxy-gateway's extensions and do not carry over to other implementations.

Gateway API puts meaning the same on every implementation first, so features whose capabilities vary between implementations, such as limiting client addresses or rate limiting, are not in the standard. Instead, it gives implementations fixed entry points for adding their own features: the `ExtensionRef` route filter, the Gateway's `spec.infrastructure.parametersRef`, and the pattern of attaching a policy to a Gateway or Service (Policy Attachment: naming the target with `targetRefs` and reporting status in `status.ancestors`). rproxy-gateway's CRDs use these entry points to make rproxy's features available from Kubernetes.

The CRDs' API group is `rproxy.max3584.net/v1beta1` (`v1alpha1` works with the same shape but is deprecated). `kubectl get` also takes the short names.

| CRD | Short name | What it is for | Standard entry point |
|---|---|---|---|
| `RproxyPolicy` | `rppol` | Adding client limits, connection caps, timing and more to Gateways, listeners and Services ([8.1](#81-rproxypolicy)) | Policy Attachment pattern |
| `RproxyMiddleware` | `rpmw` | Using rproxy middlewares (rate limits, authentication...) from HTTPRoutes and GRPCRoutes ([8.2](#82-rproxymiddleware)) | `ExtensionRef` filter |
| `RproxyRule` | `rprule` | Adding rproxy rules Gateway API cannot express to a Gateway, as they are ([8.3](#83-rproxyrule)) | none (rproxy-gateway's own) |
| `RproxyGatewayParameters` | `rpgwp` | Changing the shape of the managed rproxy pods and Service (pod count, resources, Service settings...) ([8.4](#84-rproxygatewayparameters)) | Gateway and GatewayClass `parametersRef` |

### 8.1 RproxyPolicy

RproxyPolicy adds rproxy limits and timing settings to Gateways, listeners and Services. For example: "the database port accepts connections only from inside the company", "allow idle database connections to stay open for a long time", or "wait up to 2 minutes for this Gateway's backends to answer".

**Why it is rproxy-specific**: standard Gateway API has no fields for allowing clients by address, capping connections or bandwidth, L4 connection timing, or passive health checks. The only timing you can write in the standard is HTTP timing with HTTPRoute `timeouts` ([4.5](#45-retries-and-timeouts)). If all you want is to decide how long to wait for HTTP responses, consider `timeouts` first, since it carries over to other implementations.

Write what it applies to in `spec.targetRefs`. You can name a Gateway or a Service **in the same namespace as the policy**. A Gateway alone means all its listeners; with `sectionName` too, only that listener. A Service means the rules that send to that Service.

The fields are below. The "Applies to" column says which rproxy rule the field ends up on ("rule" is rproxy's rule for a listener port; "HTTP service" is the per-backend setting of an HTTPRoute).

| Field | What it decides | Applies to |
|---|---|---|
| `allowFrom` | The client address ranges (a list of CIDRs) to accept connections from; others are refused | rule |
| `limits` | Caps on concurrent connections and on the rate of new ones (rproxy's `limits` shape) | rule |
| `bandwidth` | Bandwidth caps (rproxy's `bandwidth` shape) | rule |
| `geoip` | Allowing connections by country or ASN (rproxy's `geoip` shape; rproxy needs a GeoIP database) | rule |
| `crowdsec` | Refusing connections by CrowdSec decisions (rproxy needs `global.crowdsec`) | rule |
| `outlierDetection` | Passive health checks: when to leave out a pod that keeps failing (rproxy's `outlier_detection` shape) | L4 rules, HTTP services |
| `connectTimeout` | How long to wait when connecting to a backend (100ms to 10m) | L4 tcp rules, HTTP services |
| `responseTimeout` | How long to wait for a backend's response headers (up to 1h) | HTTP services |
| `idleTimeout` | How long an L4 connection that carries nothing stays open (`0s`: never closed; rproxy's default is 1h) | L4 tcp rules |
| `halfCloseTimeout` | How long to wait for the other side to close after one side has finished sending (`0s`: wait forever; default 1m) | L4 tcp rules |

`limits`, `bandwidth`, `geoip` and `outlierDetection` take rproxy's rule shapes (snake_case) as they are. The shapes are described in rproxy-api docs/API.md ("L4 の制限と帯域", "GeoIP", "受け身のヘルスチェック").

**Example 1**: the PostgreSQL listener of the L4 Gateway `l4` ([5.1](#51-tcproute-and-udproute)) accepts connections only from internal addresses, with limited connection counts. Also, connections sent to the Service `postgres` may sit idle for up to 8 hours.

```yaml
# one listener of the Gateway (postgres): only from internal ranges, with connection limits
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: postgres-listener, namespace: default}
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: l4
      sectionName: postgres
  allowFrom: [10.0.0.0/8, 192.168.0.0/16]
  limits:
    max_connections: 500
    per_source:
      max_connections: 20
      new_connections: {average: 5, period: 1s, burst: 10}
---
# rules sending to the Service postgres: idle connections may stay up to 8 hours
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: postgres-service, namespace: default}
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: postgres
  connectTimeout: 2s
  idleTimeout: 8h
  halfCloseTimeout: 1m
```

- The first policy applies to that listener only, through `sectionName: postgres`. Its `limits` mean: up to 500 concurrent connections on the whole listener, up to 20 concurrent connections per client, and new connections per client at 5 a second on average (10 at once at most).
- The second policy names a Service, so it applies to connections sent to the Service `postgres` whatever listener they came from. It keeps long-idle connections, such as those of a connection pool, from being cut at the default of one hour.

**Example 2**: on every listener of the HTTP Gateway `web`, wait up to 2 minutes for backends to answer, and relax the passive health checks:

```yaml
# every listener of the Gateway web: wait up to 2 minutes for backends, other passive health checks
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: web-backends, namespace: default}
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: web
  responseTimeout: 2m
  outlierDetection:
    consecutive_gateway_failures: 5
    ejection_time: 30s
```

The `outlierDetection` here leaves out a pod for 30 seconds at first after 5 gateway failures in a row (502, 503, 504, no connection and so on); the default is 3 failures and 10 seconds.

How it applies:

- Naming a Service applies the policy to the L4 rules that send to that Service. `outlierDetection`, `connectTimeout` and `responseTimeout` also apply to HTTP services whose backend is that Service.
- If two policies set the same field, the older policy wins.
- Writing `outlierDetection` drops the chart's `backends.outlierDetection` default ([1.3](#13-main-chart-values)). To turn leaving pods out off entirely, write `outlierDetection: {max_ejected_percent: 0}`.
- `idleTimeout` and `halfCloseTimeout` do not apply to HTTP and HTTPS rules or to UDP. HTTP connection timing is set with `rproxy.http` in [8.4](#84-rproxygatewayparameters).
- Rules of an HTTPRoute with `timeouts` do not get `responseTimeout` (what the route says wins).

Check that a policy took effect in `status.ancestors[]`, which lists conditions per Gateway the policy applies to:

```bash
kubectl -n default get rproxypolicy postgres-service -o jsonpath='{range .status.ancestors[*]}{.ancestorRef.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# l4 Accepted=True(Accepted) rproxy.max3584.net/RelayTimeouts=True(Applied)
```

- If a time is written wrongly, the policy becomes `Accepted: False` (reason `Invalid`) and is not used at all.
- If the listener named by `sectionName` does not exist, it becomes `Accepted: False` (reason `TargetNotFound`).

### 8.2 RproxyMiddleware

RproxyMiddleware lets HTTPRoutes and GRPCRoutes use rproxy's HTTP middlewares (processing placed before and after a request is forwarded). This gives you features the standard filters do not have: rate limits, allowing clients by address, security headers, authentication and more.

**Why it is rproxy-specific**: the standard filters ([4.4](#44-filters)) cover header rewriting, redirects, URL rewriting, mirroring, CORS, and the experimental `ExternalAuth`. Rate limits, allowing by address and the like are not in the standard, so you use them by naming an RproxyMiddleware from the standard extension point `ExtensionRef`. For authentication alone there is a standard alternative: the `ExternalAuth` filter (**Gateway API experimental**).

`spec` holds one rproxy middleware in the form `{kind: {settings}}`. The kinds and settings are the same as `http.middlewares` in rproxy-api docs/API.md ("v0.3 の設定"). A route uses it by name through a filter of `type: ExtensionRef`.

The example creates three middlewares: `/admin` is limited to internal ranges, and everything else gets a rate limit and security headers.

```yaml
# per client IP, 10 requests a second on average and 20 at once (beyond: 429)
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: rate-limit, namespace: default}
spec:
  rate_limit: {average: 10, period: 1s, burst: 20}
---
# internal ranges only (others: 403)
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: office-only, namespace: default}
spec:
  ip_allow: {source_range: [10.0.0.0/8, 192.168.0.0/16]}
---
# security headers on responses
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: secure-headers, namespace: default}
spec:
  headers: {frame_deny: true, content_type_nosniff: true, referrer_policy: same-origin}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - matches: [{path: {type: PathPrefix, value: /admin}}]
      filters:
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: office-only}
      backendRefs: [{name: admin, port: 8080}]
    - filters:
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: rate-limit}
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: secure-headers}
      backendRefs: [{name: app, port: 8080}]
```

- `group`, `kind` and `name` in `extensionRef` name the RproxyMiddleware to use. A rule can list several.
- Only RproxyMiddlewares in the route's own namespace can be named. If one is not found, the rule answers 500 and the route becomes `ResolvedRefs: False`.
- If a shape or value does not fit rproxy's rules, the route becomes `Accepted: False` (reason `UnsupportedValue`, with the reason in the message). Other routes on the same port keep working.
- Per-backendRef `filters` cannot hold `ExtensionRef` (it becomes `UnsupportedValue`).

**Counts start over**: `rate_limit`, `in_flight` and `circuit_breaker` decide by counting requests inside rproxy. Those counts start over whenever the `http` part of rproxy's rule for that port changes. The backend pod IPs are also inside the rule's `http` (`servers` of `services`), so the counts start over not only when any route on the same port changes, but also whenever backend pods are added, removed or replaced (scaling, rollouts).

**Settings that name files are hard to use**: fields that name a file (`basic_auth`'s `users_file` and others) can name only the Gateway's certificate files ([SECURITY.md](SECURITY.md)). A user list file cannot be handed to rproxy, so authenticate with `forward_auth` or Gateway API's `ExternalAuth`.

#### Authentication (`forward_auth`)

`forward_auth` asks an authentication server about every request and lets it through to the backend only when the server answers 2xx. It puts a sign-in such as oauth2-proxy or Authelia in front of an app.

If the authentication server runs in the cluster, name it with `service`. You can name only rules of the HTTPRoute that uses the middleware (the rule's `name`, or `r<number>` if it has none). In the example, oauth2-proxy sits in the same HTTPRoute as the rule `oauth2`, and the app's rule asks `login` first:

```yaml
# asks oauth2-proxy each time and lets 2xx through; service is a rule name of the same HTTPRoute
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: login, namespace: default}
spec:
  forward_auth:
    service: oauth2
    path: /oauth2/auth
    response_headers: [X-Auth-Request-User, X-Auth-Request-Email]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    # oauth2-proxy itself (sign-in pages and callback)
    - name: oauth2
      matches: [{path: {type: PathPrefix, value: /oauth2}}]
      backendRefs: [{name: oauth2-proxy, port: 4180}]
    # the app: checked by login first
    - name: app
      filters:
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: login}
      backendRefs: [{name: app, port: 8080}]
```

- `path` is the path on the authentication server to ask.
- `response_headers` are headers from the authentication server's answer that are passed on to the backend. The app learns who signed in from them.
- Steps for using it are in [14.6](#146-an-auth-middleware-via-extensionref).

**Authentication servers outside the cluster**: settings that ask a URL outside the cluster (`forward_auth.address`, `oidc.issuer`) are off by default, so that tenants cannot make Gateway pods send requests to any URL they like. To use them, the administrator allows hosts in the GatewayClass parameters (the chart's `managed.parameters`):

```yaml
# values.yaml
managed:
  parameters:
    policy:
      middlewares:
        outboundHosts: [auth.example.com]       # *, a hostname, *.example.com, an IP range
        # allowedKinds: [headers, rate_limit, forward_auth]   # to limit the kinds
```

- `outboundHosts` takes `*` (anywhere), hostnames, wildcards such as `*.example.com`, and IP ranges.
- `allowedKinds` limits which middleware kinds tenants may use (all by default).

An RproxyMiddleware that asks an allowed host looks like this:

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: authelia, namespace: default}
spec:
  forward_auth:
    address: https://auth.example.com/api/verify
    response_headers: [Remote-User, Remote-Groups]
```

Naming a host that is not allowed makes the route `Accepted: False` (reason `UnsupportedValue`, message "auth.example.com is not allowed by the GatewayClass (policy.middlewares.outboundHosts; by default no URL out)").

### 8.3 RproxyRule

RproxyRule adds an rproxy rule as it is (the same shape as the body of the control API's `POST /rules`) to a Gateway's rule set. It is the way out for things Gateway API cannot express, such as forwarding to servers outside the cluster or using rproxy's finer settings.

**Why it is rproxy-specific**: the backend every implementation supports for standard Gateway API routes is a Kubernetes Service, and rproxy's finer rule fields (such as `source_ip`, which passes the client address on with PROXY protocol) cannot be written. RproxyRule is rproxy-gateway's own way around that; there is no standard alternative.

The example adds one TCP forward to an SSH server outside the cluster to the rproxy of the Gateway `web`:

```yaml
# adds a TCP forward to an SSH server outside the cluster to the Gateway web's rproxy
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyRule
metadata: {name: bastion-ssh, namespace: default}
spec:
  parentRef: {name: web}
  rule:
    protocol: tcp
    listen_addr: 0.0.0.0
    listen_port: 2222
    remote_addr: 192.0.2.50
    remote_port: 22
    allow_from: [203.0.113.0/24]
```

- `parentRef` names the Gateway the rule is added to. Inside `rule` is rproxy's rule shape (snake_case). This example forwards connections received on port 2222 to `192.0.2.50:22`, accepting them only from `203.0.113.0/24`.
- The rule's listening port is added to the managed Service automatically (port ranges up to 100 ports).
- To add a rule to a Gateway in another namespace, that Gateway's namespace needs a ReferenceGrant (`from` `group: rproxy.max3584.net, kind: RproxyRule`, `to` `kind: Gateway`).
- If the Gateway already has a rule with the same key (protocol, address, port), it becomes `Accepted: False` (reason `Conflicted`). If rproxy refuses the rule, it becomes `Accepted: False` (reason `Invalid`); the other rules keep working.

**A note on trust**: RproxyRules are outside the tenant limits ([SECURITY.md](SECURITY.md)). They can name any destination. If you cannot trust the people who can write RproxyRules as much as the Gateway's owner, turn them off with `policy.rproxyRules: false` in the GatewayClass parameters. Fleet mode does not read RproxyRules by default (the chart's `fleet.rproxyRules`).

Check that an added rule runs in the `PROGRAMMED` column:

```bash
kubectl -n default get rproxyrule
# NAME          GATEWAY   PROGRAMMED
# bastion-ssh   web       True
```

### 8.4 RproxyGatewayParameters

RproxyGatewayParameters changes the shape of the managed rproxy pods and Service (pod count, resources, Service settings, how rproxy behaves and so on).

**Why it is rproxy-specific**: Gateway API defines only the entry point that Gateways and GatewayClasses use to name settings (`parametersRef`), plus the labels and annotations in `spec.infrastructure`. What is named is up to each implementation, and RproxyGatewayParameters is rproxy-gateway's version. If you only want labels and annotations on the pods and Service, the standard `spec.infrastructure.labels` and `annotations` can add them too (though rproxy-gateway limits which annotations reach the Service; [12.](#12-security-basics)).

There are two ways to use it:

- **Per Gateway** (the Gateway's reference): write it in the Gateway's namespace and name it in the Gateway's `spec.infrastructure.parametersRef`. It applies to that Gateway only.
- **Defaults for every Gateway** (the class's reference): write it in the chart's `managed.parameters`. What tenants may change in their Gateway references (`policy`) is decided here too.

First a per-Gateway example. The rproxy of the Gateway `web` gets three pods spread over zones, its Service accepts only certain client ranges, and it waits longer for connections when stopping:

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyGatewayParameters
metadata: {name: web, namespace: default}
spec:
  replicas: 3
  podDisruptionBudget: {maxUnavailable: 1}
  pod:
    resources:
      rproxy: {requests: {cpu: 500m, memory: 128Mi}, limits: {memory: 512Mi}}
    topologySpreadConstraints:
      - {maxSkew: 1, topologyKey: topology.kubernetes.io/zone, whenUnsatisfiable: ScheduleAnyway}
  service:
    externalTrafficPolicy: Local
    loadBalancerSourceRanges: [203.0.113.0/24]
  rproxy:
    logLevel: info
    shutdown: {delay: 15s, drain: 60s}
    http:
      http2MaxStreams: 1000
      timeouts: {writeStall: 2m, upgradedIdle: 0s}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  infrastructure:
    parametersRef:
      group: rproxy.max3584.net
      kind: RproxyGatewayParameters
      name: web
  listeners:
    - name: http
      port: 80
      protocol: HTTP
```

- The Gateway's `infrastructure.parametersRef` names the RproxyGatewayParameters `web` in the same namespace. One in another namespace cannot be named (a ReferenceGrant does not open it either).
- `rproxy.shutdown.drain: 60s` makes a stopping pod wait up to 60 seconds for existing connections (the default is 25 seconds; [10.3](#103-pod-replacement-and-the-readiness-gate)).
- `rproxy.http` caps HTTP connections with clients. Here HTTP/2 allows up to 1000 concurrent streams, a client connection that receives nothing of the response for 2 minutes is closed, and upgraded connections such as WebSockets are never closed for being idle (`0s`).

The main fields follow (all of them: 2.2 of [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md)). The right-hand column says whether tenants may set the field in a Gateway reference by default. Fields marked "may not" can still be opened by the administrator with `policy.gatewayOverrides` in the class's reference (fields marked "class reference only" cannot be opened).

**Pod count and placement**

| Field | What it decides | Tenants may set it |
|---|---|---|
| `replicas` | The number of rproxy pods, from 1 to `policy.maxReplicas` (default 10) | may |
| `podDisruptionBudget` | The PDB's `minAvailable` or `maxUnavailable` (one of them). If unset: `maxUnavailable: 1` with 2 or more pods | may |
| `pod.resources.rproxy`, `.certsync` | Resources of rproxy and of certsync (the side container that prepares the certificate files) | may |
| `pod.labels`, `pod.annotations`, `pod.topologySpreadConstraints` | The pods' labels, annotations and spreading. Without a spread, 2 or more pods are spread over nodes | may |
| `pod.nodeSelector`, `tolerations`, `affinity`, `priorityClassName` | Which nodes the pods run on and their priority | may not (the class can open it) |

`nodeSelector` and the like are closed by default so that tenants cannot put pods on dedicated nodes or the control plane, or push other pods out with priority.

**Service**

| Field | What it decides | Tenants may set it |
|---|---|---|
| `service.type` | The Service type (`LoadBalancer`, `NodePort`, `ClusterIP`) | may not (the class can open it) |
| `service.externalTrafficPolicy` | `Local` or `Cluster` (meaning as for `managed.externalTrafficPolicy` in [1.3](#13-main-chart-values)) | may |
| `service.loadBalancerSourceRanges` | Client ranges the load balancer accepts (set only for `LoadBalancer`) | may |
| `service.ipFamilyPolicy`, `labels`, `annotations` | IPv4/IPv6 handling, the Service's labels and annotations | may (annotations that choose addresses only from the allow list) |
| `service.loadBalancerClass` | Which load balancer implementation handles it; cannot change after the Service is created | may not (the class can open it) |

`service.type` and `loadBalancerClass` are closed by default because they can take outside addresses. Changing `service.type` changes the Service, and with it the load balancer address.

**How rproxy behaves**

| Field | What it decides | Tenants may set it |
|---|---|---|
| `rproxy.logLevel` | How detailed rproxy's logs are (`error` to `trace`) | may |
| `rproxy.performance` | rproxy's performance settings (number of workers and so on) | may |
| `rproxy.shutdown` | `delay` and `drain` when stopping (meaning as for `managed.shutdown` in [1.3](#13-main-chart-values)) | may |
| `rproxy.http` | Caps on HTTP connections with clients (`http2MaxStreams`, `timeouts`) | may |
| `rproxy.image`, `rproxy.extraEnv` | The rproxy image and extra environment variables (`RPROXY_*`) | may not (class reference only) |
| `proxyProtocol` | Reading the PROXY protocol header the load balancer in front adds ([PLATFORM.md](PLATFORM.md)) | may not (the class can open it) |

`rproxy.image` and `extraEnv` would let anyone run any image or settings in a pod that holds the Gateway's control API credentials, so they cannot be opened to tenants. `proxyProtocol` decides which sources may claim to be any client through the header, so by default only the administrator can set it.

**Other**

| Field | What it decides | Tenants may set it |
|---|---|---|
| `ui.visible` | Whether the management UI shows this Gateway | may (cannot be `true` when the class says `false`) |
| `policy` | What tenants are allowed | may not (class reference only) |

**Defaults for every Gateway (the class's reference)**: what you write in the chart's `managed.parameters` becomes the `RproxyGatewayParameters` `rproxy-default` (in the release namespace), which the GatewayClass names in its `parametersRef`. What tenants are allowed (`policy`) can be written only here. The example gives every Gateway two rproxy pods and narrows what tenants may change:

```yaml
# values.yaml
managed:
  parameters:
    replicas: 2
    pod:
      resources: {rproxy: {requests: {cpu: 100m, memory: 64Mi}}}
    rproxy:
      extraEnv:
        - {name: RPROXY_MAX_RULES, value: "20000"}
    policy:
      maxReplicas: 5
      # replaces the default list when written (DESIGN-v0.4.x.md 2.5)
      gatewayOverrides: [replicas, podDisruptionBudget, pod.resources, pod.tolerations, rproxy.logLevel]
```

- `extraEnv` raises rproxy's limit on the number of rules (`RPROXY_MAX_RULES`). The value is a string, so it is quoted.
- `policy.maxReplicas: 5` caps the `replicas` tenants may write at 5.
- With `policy.gatewayOverrides`, tenants may set only the fields listed (it replaces the default "may" list as a whole). This example opens `pod.tolerations` and in exchange closes `pod.topologySpreadConstraints`, `service.*`, `rproxy.shutdown` and `rproxy.http`. So with this class, the Gateway `web`'s reference above becomes `InvalidParameters`.

How values are combined:

- Values are layered in the order controller flags (the chart's `managed.*`) → class reference → Gateway reference; later ones win.
- Lists and objects (`tolerations`, `resources.rproxy` and so on) are replaced as a whole. Maps such as labels are merged key by key.
- On clusters upgraded from `v0.4.1`, apply rproxy-gateway's CRDs before writing `managed.parameters`. `helm upgrade` does not install new CRDs ([13.1](#131-upgrades)).

Check the RproxyGatewayParameters and the Deployment:

```bash
kubectl -n default get rpgwp
# NAME   REPLICAS
# web    3
kubectl -n default get deploy -l gateway.networking.k8s.io/gateway-name=web
```

What happens on mistakes:

- Writing a field that is not allowed or a wrong value makes the Gateway `Accepted: False` (reason `InvalidParameters`). The message names the field (for example "spec.service.type: not allowed by the GatewayClass (policy.gatewayOverrides)").
- **A Gateway that is already running is not stopped**: a Gateway whose rproxy Deployment already exists keeps running in its last correct shape (`Programmed: True`), so that one typo does not stop traffic. Once you fix it, the next reconcile applies the new shape.
- Whether `rproxy.http` took effect shows in the Gateway's condition `rproxy.max3584.net/HTTPSettings` (reasons `Applied`, `Kept`, `UnsupportedRproxy`).

**Not usable in fleet mode**: in fleet mode, Gateway references cannot be used. A Gateway with a `parametersRef` becomes `Accepted: False` (reason `InvalidParameters`, message "not used in fleet mode"), and the rules of a Gateway that is not accepted are not sent to the fleet pods. "Keep running in the last shape" above is managed-only behaviour, so in fleet mode **that Gateway's traffic stops** (changes do not arrive either, and restarted pods or pods on new nodes get nothing). In fleet mode, remove `parametersRef`.

## 9. managed and fleet

> **rproxy-specific**: Gateway API leaves how a Gateway is run (where the proxy pods go) to the implementation. managed and fleet are rproxy-gateway's ways of placing them; there is no standard alternative.

rproxy-gateway can place rproxy's pods in two ways. This chapter explains how they differ, how to choose, and what to watch out for when you switch. It is worth deciding this once before you install.

- **managed** (the default): for each Gateway you create, the controller makes a Deployment and a Service of rproxy just for that Gateway, in the Gateway's namespace. Each Gateway has its own pods and its own keys, which suits a PaaS where several teams share a cluster.
- **fleet**: one DaemonSet installed by the chart (rproxy with `hostNetwork: true`, using the node's network directly) serves every Gateway. Adding Gateways adds no pods. This mode is meant for a cluster run by a single administrator.

| | managed (default) | fleet |
|---|---|---|
| rproxy | a Deployment and a Service per Gateway, in the Gateway's namespace | the chart's DaemonSet (`hostNetwork: true`) serves every Gateway |
| The Gateway's address | the Service's load balancer address (MetalLB, cloud...) | node IPs or `fleet.addresses` (the platform provides VIPs) |
| Keys and tokens | separate per Gateway | shared by every Gateway |
| Separates tenants | yes | no (for one administrator) |
| `RproxyGatewayParameters` | usable | a Gateway's reference is not usable: the Gateway becomes `InvalidParameters`, its rules are not sent and its traffic stops (the DaemonSet's shape comes from the chart's `fleet.*`) |
| RproxyRule | usable | not read by default (`fleet.rproxyRules`) |
| One port used by two Gateways | works (the Services are separate) | the older Gateway holds it; the later Gateway's listener is `Accepted: False` (`PortUnavailable`). With `fleet.listen: addresses`, Gateways on different addresses can share it |
| NetworkPolicy | one per Gateway | has no effect (protect the nodes with a firewall) |

**Which to choose**: pick managed unless you have a reason not to. Fleet fits when there is no load balancer implementation, when you want to take traffic on the node addresses directly, or when you run a single-administrator cluster and want fewer pods.

A `values.yaml` for fleet:

```yaml
# values.yaml
fleet:
  enabled: true
  addresses: [192.0.2.10]    # the address written to Gateway status (a VIP or the load balancer in front)
  # listen: addresses        # listen per Gateway spec.addresses (inside managed.addressCIDRs)
  # nodeSelector: {node-role.example.com/edge: "true"}
```

- `addresses` is the address written to the Gateways' `status`. Put the VIP your platform provides, or the address of the load balancer in front. Without it, the node IPs are written.
- With `listen: addresses`, a Gateway listens only on the addresses in its `spec.addresses` (inside the `managed.addressCIDRs` ranges). Gateways on different addresses can then use the same port.
- `nodeSelector` picks the nodes rproxy runs on.

Point the health check of the load balancer or VIP in front at `https://<node IP>:9443/readyz`. rproxy answers 503 once it starts stopping, so traffic can move to another node before it stops. How to set it up: "fleet" in [PLATFORM.md](PLATFORM.md).

**Switching**: changing `fleet.enabled` with `helm upgrade` switches the mode. Either way, traffic is interrupted and the Gateways' addresses change, so do it in a maintenance window, together with the DNS and firewall changes.

- **managed → fleet**: the controller deletes the managed Deployments and Services and sends the rules to the fleet's pods. A Gateway's address changes from the load balancer address to the node IPs (or `fleet.addresses`).
  - A Gateway with a `parametersRef` becomes `InvalidParameters`, its rules are not sent and its traffic stops (fleet does not keep the previous shape running, as managed does). **Remove `spec.infrastructure.parametersRef` from every Gateway before you switch.** The pods' shape comes from the chart's `fleet.*`.
  - RproxyRules are only used with `fleet.rproxyRules: true`.
- **fleet → managed**: helm deletes the DaemonSet and the controller creates a Deployment and a Service per Gateway. The addresses become the new load balancer addresses.

```bash
helm upgrade rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway -n rproxy-gateway-system -f values.yaml
kubectl get gateway -A        # check that ADDRESS changed
```

## 10. Changing things while they run

> **rproxy-specific**: Gateway API does not define what happens to existing connections on a change; it differs per implementation. This chapter describes rproxy-gateway and rproxy. The shape of the conditions is standard, but some reasons and messages are rproxy-specific.

This chapter explains what happens to existing connections when you change a Gateway or a route that is carrying traffic. Use it to check, before a change, whether the change will cut connections.

On every change, the controller renders the Gateway's whole rule set again and sends it to rproxy ([11.](#11-how-it-talks-to-rproxy)). rproxy applies only the difference from the previous rule set. rproxy has three ways of applying the difference to a rule (a listening port), and which one it uses decides what happens to existing connections ("ルールの組" in rproxy-api's docs/API.md).

| How rproxy applies it | What it does | Existing connections |
|---|---|---|
| `in_place` | keeps the listener and swaps its contents in place | not cut; new connections and requests get the new settings |
| `recreate` | closes the listener and opens it again | that port's connections are cut |
| removal | closes the listener of a rule that left the rule set | that port's connections are cut |

Most changes are `in_place`. Connections are cut when you change a port or a protocol, and when you delete a listener or a Gateway.

### 10.1 Common changes

What happens with common changes.

**Changes that cut no connection (`in_place`)**

| Change | What happens |
|---|---|
| Adding, changing or removing routes (paths, headers, weights, filters) | No connection is cut. However, the counts of that port's `rate_limit`, `in_flight` and `circuit_breaker` start again from zero ([8.2](#82-rproxymiddleware)) |
| Backend pods coming and going (scaling, rollouts) | Pods that are ending get no new requests. On HTTP ports the pod IPs are inside the rule's `http`, so here too the port's `rate_limit`, `in_flight` and `circuit_breaker` counts start again |
| A certificate Secret renewed (by cert-manager, for example) | The certificate files are named by a hash of their contents, so new connections use the new certificate |
| RproxyPolicy (`allowFrom`, `limits`, timeouts...) | Takes effect from the next connection |
| Adding or removing a listener (hostname) on the same port | That port's rule is changed `in_place`, so connections to the other hostnames are not cut |

**Changes that cut connections**

| Change | What happens |
|---|---|
| Changing a listener's port | The old port's rule goes and its connections are cut. A rule for the new port is created, and the managed Service's port changes too |
| Removing a listener (the last one on its port) | That port's rule goes and its connections are cut |
| Changing the protocol on a port (`HTTP` ↔ `TCP` and so on) | `recreate`: that port's connections are cut |
| Deleting a Gateway | managed: the Deployment and Service, owned by the Gateway, are deleted; every connection is cut and the address goes away. fleet: the Gateway's rule set is removed from rproxy and that Gateway's connections are cut |

**Changes that replace pods or the Service**

| Change | What happens |
|---|---|
| `RproxyGatewayParameters` (the pods' shape other than replicas), labels and annotations of `spec.infrastructure` | A rolling update of the pods (the mechanisms in 10.3 keep traffic flowing). A replicas change only adds or removes pods |
| Values of `rproxy.http` | The pods are not replaced: rproxy reads its settings file again (the kubelet takes about a minute to update the file). Only setting it for the first time, or removing all of it, causes a rolling update |
| Changing `service.type` | The Service changes and the load balancer address changes |
| Upgrading the controller (the rproxy or certsync image changes) | A rolling update of every managed Gateway |

### 10.2 Errors show in status

When some rules or routes cannot be accepted, rproxy leaves out just those and runs the rest. One error does not stop the whole Gateway. What was left out shows in the status (conditions) of the Kubernetes objects, as follows.

| What happened | Where it shows |
|---|---|
| rproxy refused a route | That route is `Accepted: False` (reason `UnsupportedValue`, rproxy's answer in the message). The other routes on the port keep working |
| rproxy refused an RproxyRule | That RproxyRule is `Accepted: False` (reason `Invalid`) |
| rproxy refused the whole rule set (a limit, for example) | The Gateway and its listeners are `Programmed: False` (reason `Invalid`, message "rproxy refused the rule set: ...") |
| The certificate files have not reached the pod yet | `Programmed: False` (reason `Pending`); `True` once they arrive |
| A listener cannot be opened (the port is in use) | The listener is `Programmed: False` |
| A wrong reference (no such Service or Secret) | The route is `ResolvedRefs: False` (reason `BackendNotFound`, `RefNotPermitted`), the listener `ResolvedRefs: False` (reason `InvalidCertificateRef`) |

What was refused is not sent again until its contents change or 5 minutes pass. Once you fix it, it goes out on the next reconcile.

### 10.3 Pod replacement and the readiness gate

managed rproxy pods keep traffic flowing even while they are replaced by an upgrade or a node drain, thanks to the following ("rproxy availability (managed)" in [DESIGN.md](DESIGN.md)).

- **Readiness gate** (`rproxy.max3584.net/ruleset-applied`): a readiness gate adds a condition a pod must meet before Kubernetes considers it Ready. An rproxy pod is not Ready just because its process started: it becomes Ready, and joins the Service, only after the controller has sent its rule set and set this condition to `True`. So traffic never reaches a freshly started rproxy that has no rules.
- **Rolling update**: the Deployment's `maxUnavailable: 0` makes a new pod Ready before an old one stops.
- **How it stops**: on SIGTERM, rproxy keeps accepting for `delay` (15 seconds by default) while `/readyz` answers 503, waiting to leave the Service. Then it closes its listeners and waits up to `drain` (25 seconds by default) for existing connections to end. Connections still open after `drain` (long WebSockets, DB connections) are cut. To wait for long connections, lengthen `rproxy.shutdown.drain` ([8.4](#84-rproxygatewayparameters)) or the chart's `managed.shutdown`.
- **PDB and spreading**: with 2 or more `replicas`, a PDB (`maxUnavailable: 1`) and spreading over nodes are added, so even a node drain stops at most one pod at a time. With 1 replica, there is a moment during a replacement when the only pod is stopped.

You can see whether the readiness gate is met in the pod list:

```bash
kubectl -n default get pod -l gateway.networking.k8s.io/gateway-name=web -o wide
# READINESS GATES 1/1: the rule set is on the pod
```

### 10.4 Restarts

What happens when the controller or rproxy restarts.

| What restarts | What happens |
|---|---|
| The controller | rproxy keeps running with its current rules (traffic does not stop). The new leader reads rproxy's rule set and sends it again if it differs |
| The rproxy container | The rule set lives only in rproxy's memory, so it is lost. The controller sets the readiness gate back to `False` (taking the pod out of the Service), waits for rproxy's `/readyz`, sends the rule set again and sets it back to `True` |
| An rproxy pod (deleted, drained) | The same as the rolling update in 10.3 |

While the controller is down, changes do not reach rproxy. rproxy keeps running in the shape it had before.

### 10.5 Who changes things, and how

This section explains how a route change or pods coming and going reach rproxy.

Nobody gives rproxy instructions directly. Changing Kubernetes objects is the instruction. The controller watches the following kinds of objects (`src/controller/cache.rs`), and whenever one of them changes, it renders the Gateway's rule set again and sends it.

- Gateway API: GatewayClass, Gateway, HTTPRoute, GRPCRoute, TLSRoute, TCPRoute, UDPRoute, ReferenceGrant, ListenerSet, BackendTLSPolicy
- rproxy's CRDs: RproxyMiddleware, RproxyPolicy, RproxyRule, RproxyGatewayParameters
- Kubernetes: Pod (the rproxy pods' state and readiness gate), Service, EndpointSlice, Secret, ConfigMap (only the referenced ones are read), Namespace (for the `allowedRoutes` selector), ClusterTrustBundle; during a migration also Ingress and Traefik's CRDs

What changes these objects, for example:

| Trigger | What changes |
|---|---|
| A person runs `kubectl apply` | Gateways, routes, rproxy's CRDs |
| GitOps (Argo CD, Flux) applies what is in Git | The same (the controller does not tell it apart from `kubectl apply`) |
| Pods come, go, fail or are replaced | The EndpointSlice changes, and rproxy's backends (`servers`) change automatically. Nobody has to do anything |
| cert-manager renews a certificate | The Secret changes and the certificate files are replaced automatically |
| A progressive delivery tool (Argo Rollouts, Flagger) changes weights | An HTTPRoute's `backendRefs[].weight` |

Progressive delivery tools only rewrite HTTPRoutes through their Gateway API plugin, so in principle they should work with rproxy-gateway too. This project has not tested them, though.

When such a tool sets the weights to 90 / 10, what it writes is the same as this, written by hand:

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080, weight: 90}      # the current version
        - {name: app-v2, port: 8080, weight: 10}   # the new version
```

**Who may change what** is decided by Kubernetes RBAC. For example:

- Gateways (ports, certificates, addresses): only infrastructure administrators may write them (put them in an `infra` namespace, for example).
- Routes: each team writes them in its own namespace. Which namespaces' routes a Gateway takes is decided by the administrator with the Gateway's `allowedRoutes` ([6.1](#61-allowedroutes-which-namespaces-routes-a-gateway-takes)).
- Only the controller holds a token that can write to rproxy's control API (when the chart's `ui.namespace` is set, the management UI holds a read-only token). Users, CI and GitOps tools never call rproxy's API directly; they only write Kubernetes objects.

## 11. How it talks to rproxy

> **rproxy-specific**: how a controller talks to its proxies differs per implementation.

This chapter explains how the controller hands rules to rproxy. You do not need it for day-to-day operation, but it helps when you read rproxy's logs or the output of `render` in [15.3](#153-checking-without-a-cluster).

The controller turns one Gateway into one rproxy rule set and sends it through rproxy-api's control API. This is the same API you use with `curl` against an rproxy running on a VM, and the rules' JSON has the same shape.

```
PUT https://<rproxy pod IP>:9443/rulesets/k8s/<Gateway namespace>/<Gateway name>
Authorization: Bearer <that Gateway's token>
If-Match: <current etag>
{"generation": <the Gateway's metadata.generation>, "rules": [ ... ]}
```

- The rule set is named `k8s/<namespace>/<name>`, and each Gateway's token can write only the rule set of that name.
- rproxy has one rule per (protocol, address, port). Listeners and routes on the same port are merged into one rule.
- Nothing is sent when the contents are the same as before. Even without changes, the controller renders again every `--resync-secs` (30 seconds by default) to check.

For example, the Gateway `web` and HTTPRoute `app` of 2.2 become this rule set (`rproxy-gateway render -f <file>` draws it without a cluster):

```json
{
  "rules": [
    {
      "protocol": "tcp",
      "listen_addr": "0.0.0.0",
      "listen_port": 80,
      "http": {
        "routes": [
          {
            "match": "Host(`app.example.com`)",
            "name": "default/app/r0/m0/h0",
            "priority": 1,
            "service": "default/app/r0"
          }
        ],
        "services": {
          "default/app/r0": {
            "servers": [
              {"url": "http://10.244.1.10:3000"},
              {"url": "http://10.244.2.11:3000"}
            ]
          }
        }
      },
      "labels": {
        "app.kubernetes.io/managed-by": "rproxy-gateway",
        "gateway.networking.k8s.io/gateway-name": "web",
        "gateway.networking.k8s.io/gateway-namespace": "default"
      }
    }
  ]
}
```

- The port-80 listener has become one rule with `listen_port: 80`. The HTTPRoute's rules go into `http.routes` and the backends into `http.services`. A name such as `default/app/r0` means "rule 0 of the HTTPRoute `app` in the namespace `default`".
- `servers` are the pod IPs in the Service's EndpointSlice, with the pods' port (the Service's `targetPort`). The Service of 2.1 maps 8080 → 3000, hence `:3000`.
- The real controller adds the chart's `backends` defaults to this (the services' `outlier_detection`, `timeouts.connect: 1s`, `timeouts.response: 30s`) before sending it.

**What does not go through the control API**: to keep private keys off the API, and so that rproxy's pods need no Kubernetes API, the following are passed as files or environment variables.

| What | How |
|---|---|
| Certificates and keys (`certificateRefs`, BackendTLSPolicy CAs...) | The controller gathers them into the Secret `rproxy-<id>-certs`, and the kubelet mounts it into the pod at `/var/run/rproxy-gateway/certs` as files. Rules name the files (a hash of the contents) in `cert_file` and similar fields. The pod's `certsync` container tells the controller when the files are in place |
| The token file (`tokens.yaml`, which holds only the tokens' hashes) and the control API's certificate | The Secret `rproxy-<id>-api`, mounted as a volume |
| rproxy's settings file (`config.json`, when `rproxy.http` is set) | An item of the same `rproxy-<id>-api` and the environment variable `RPROXY_CONFIG` |
| Log level, performance, shutdown, `extraEnv` | The pod's environment variables |

In fleet mode, certificates are in the Secret `rproxy-fleet-certs`, the token file in the Secret `rproxy-gateway-token`, and the `fleet.http` settings in the ConfigMap `rproxy-fleet-config`.

## 12. Security basics

> **rproxy-specific**: this chapter is about how rproxy-gateway protects things. The standard Gateway API parts involved are the permissions for references across namespaces (`allowedRoutes`, ReferenceGrant, `allowedListeners`; [6.](#6-other-namespaces)).

This chapter briefly sums up how rproxy-gateway keeps Gateways and tenants apart, and what operators should watch out for. Tenant separation is explained in detail in [SECURITY.md](SECURITY.md).

- **A token per Gateway**: managed rproxy runs with a control API certificate and a token of its own Gateway (derived from the controller's master token). The token can write only its own Gateway's rule set (`k8s/<namespace>/<name>`). So someone who can read the Secrets of one Gateway's namespace can do nothing to another Gateway's rproxy. The master token and the CA key exist only in the controller's namespace (`rproxy-gateway-system`).
- **rproxy's pods do not use the Kubernetes API**: no ServiceAccount token is mounted into rproxy's pods, and they have no RBAC permissions. Even a compromised rproxy cannot touch the Kubernetes API.
- **There is no Service for the control API**: each Gateway's Service has only the listener ports (and RproxyRules' listening ports). No Service is made for the control API (9443) or certsync (9444). The controller PUTs to each pod directly at `https://<pod IP>:9443`. Each pod holds its own rule set, so going through a Service would reach only one pod and would not work. Users need not create Services or NetworkPolicies for 9443 and 9444.
- **NetworkPolicy (managed)**: one is created automatically per Gateway. The control API (9443) and certsync (9444) are reachable only from the controller's pods (when the chart's `ui.namespace` is set, 9443 of the Gateways shown to the UI is also reachable from the UI's pods); the listener ports are reachable from anyone. It has no effect if the CNI does not support NetworkPolicy.
- **fleet and hostNetwork**: NetworkPolicy has no effect in fleet mode. The control API (9443) and certsync (9444) listen on the node IPs, so restrict traffic to the nodes' 9443 and 9444 with the nodes' firewall. Not only outside clients but also pods inside the cluster can reach hostNetwork's 9443 and 9444, and NetworkPolicy cannot stop them.
  - Open 9443 only to the controller and to the load balancer's health checks on `/readyz`. Open 9444 (certsync, which listens on `0.0.0.0`) only to the controller.
  - The controller's traffic often comes from its pod IP (the source address of pod-to-node traffic depends on the CNI). Allow the controller's pod range (or the controller's nodes) and deny the rest.
  - Fleet is for one trust domain; it does not separate tenants.
- **Closed by default**: the following are off by default. Open them only in the classes of tenants you trust.
  - Gateways' `spec.addresses` (the chart's `managed.addressCIDRs`)
  - ExternalName Services (they can point anywhere)
  - Middlewares that call outside URLs (`policy.middlewares.outboundHosts`)
  - Load balancer annotations that pick addresses (`serviceAnnotationPrefixes`)
- **Private keys from other namespaces**: when you use another namespace's certificate through a ReferenceGrant, managed mode copies its private key into the Gateway's namespace. If that is a problem, set the chart's `controller.crossNamespaceSecrets: false`.
- **Narrowing the controller's permissions**: limiting the watched namespaces with the chart's `controller.watchNamespaces` also narrows its permissions to Roles in those namespaces. In addition, `rbac.admissionPolicy.enabled: true` (Kubernetes 1.30 or later) uses a ValidatingAdmissionPolicy to limit what the controller may change to the objects it made itself.

## Part 4: Operations and troubleshooting

Part 4 (chapters 13 to 15) covers running rproxy-gateway after installing it, recipes for common setups, and what to look at when things go wrong. It touches both standard Gateway API and rproxy-specific topics.

## 13. Operations

This chapter covers day-to-day operation after installing: upgrades, monitoring, redundancy, backups and logs.

### 13.1 Upgrades

How to move rproxy-gateway to a new version. Helm does not update CRDs when it upgrades a chart, so apply rproxy-gateway's CRDs yourself first, then upgrade the chart.

```bash
# 1. apply rproxy's CRDs first (helm upgrade does not update CRDs)
kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.22/rproxy.max3584.net.yaml
# 2. upgrade the chart
helm upgrade rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway --version 0.4.22 \
  -n rproxy-gateway-system -f values.yaml
# 3. check
kubectl -n rproxy-gateway-system rollout status deploy/rproxy-gateway
kubectl get gateway -A
```

What an upgrade does, and what to keep in mind:

- Each chart version comes with a fixed rproxy version (the chart's `rproxy.image.tag`; chart 0.4.22 has rproxy 0.4.22). When the rproxy image (or certsync, which is in the controller's image) changes, the pods of every managed Gateway are replaced by a rolling update (10.3). With 2 or more replicas, the gap is normally about a second at most ("Availability" in the [README](../../README.en.md)). In fleet mode the DaemonSet is replaced.
- Check each version's changes in the release notes (GitHub Releases). Within v0.4.x, changes do not break your current values or Gateways ([DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) 1.).
- If you installed with Kustomize or `install.yaml`, `kubectl apply --server-side` the new version's `install.yaml` (it includes the CRDs).
- To upgrade Gateway API's CRDs, `kubectl apply --server-side` the new version's `standard-install.yaml` (or experimental). The controller looks for new kinds every 30 seconds, so it does not need a restart. Check which Kubernetes versions go with it in "Supported versions" in the [README](../../README.en.md).

### 13.2 Monitoring

This section explains what to watch so that you notice when a Gateway goes wrong.

**Status (conditions)**: look here first. Alerting when the following conditions are not `True` catches configuration mistakes and failures to apply.

| Object | Conditions to watch |
|---|---|
| Gateway | `Accepted`, `Programmed` |
| Listeners (`status.listeners[]`) | `Accepted`, `ResolvedRefs`, `Programmed` |
| Routes (`status.parents[]`) | `Accepted`, `ResolvedRefs` |
| RproxyPolicy, BackendTLSPolicy (`status.ancestors[]`) | `Accepted` |
| RproxyRule | `Accepted`, `Programmed` |
| rproxy's own Gateway conditions | `rproxy.max3584.net/HTTPSettings`, `rproxy.max3584.net/ProxyProtocol` (alert on these when they are `False`) |

Commands that list the conditions that are not `True`:

```bash
# conditions of Gateways and listeners that are not True
kubectl get gateways -A -o json | jq -r '.items[] | "\(.metadata.namespace)/\(.metadata.name)" as $n
  | (.status.conditions[]?, (.status.listeners[]? | .conditions[]?))
  | select(.status != "True" and .type != "Conflicted") | "\($n) \(.type)=\(.status) \(.reason): \(.message)"'
# conditions of routes that are not True
kubectl get httproutes,grpcroutes,tlsroutes,tcproutes,udproutes -A -o json | jq -r '.items[]
  | "\(.kind) \(.metadata.namespace)/\(.metadata.name)" as $n | .status.parents[]? | .conditions[]?
  | select(.status != "True") | "\($n) \(.type)=\(.status) \(.reason): \(.message)"'
```

A listener's `Conflicted` is the one condition that is normally `False`, so the first command leaves it out. To watch these in Prometheus, kube-state-metrics' Custom Resource State feature can turn the conditions into metrics.

**Pods**: watch whether the controller's Deployment (`rproxy-gateway`) and the rproxy pods are Ready, and whether their restart counts grow. If an rproxy pod's READINESS GATES stays at `0/1`, its rule set is not on it (10.3).

**Logs**: for the controller's logs, see 13.5 and [15.2](#152-controller-logs). rproxy's logs are read with `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy` (`ruleset.apply`, `conn.*`, `http.access` and so on; their meaning is in rproxy-api's docs/API.md).

**rproxy's `/metrics`**: rproxy serves Prometheus metrics at `GET /metrics` on its control API (9443); it needs a token with `metrics:read`. rproxy-gateway gives such a token only to the management UI (the chart's `ui.namespace`), and the NetworkPolicy lets only the UI's pods through. This version has no setting for Prometheus to read them directly, so view the metrics in the management UI.

### 13.3 Scaling and HA

This section sums up how many controllers and rproxy pods to run, and where each number is set.

| What | Set by | What happens |
|---|---|---|
| Controller | `controller.replicas` (default 2) | One replica becomes the leader through the Lease `rproxy-gateway` (in the controller's namespace), and only it applies changes. If the leader stops, another takes over in about 15 seconds (at once if it stopped on SIGTERM). To run a single replica, set `controller.leaderElection: false` |
| rproxy (default for every Gateway) | `managed.replicas` or `managed.parameters.replicas` | With 2 or more, a PDB (`maxUnavailable: 1`) and spreading over `kubernetes.io/hostname` (topologySpread, `ScheduleAnyway`) are added |
| rproxy (per Gateway) | `replicas`, `podDisruptionBudget`, `pod.topologySpreadConstraints`, `pod.resources` of `RproxyGatewayParameters` | Tenants can set `replicas` up to `policy.maxReplicas` (default 10) |
| fleet | DaemonSet (one per node) | `fleet.nodeSelector` and `tolerations` pick the nodes |

To see the current leader and a Gateway's PDB:

```bash
kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}'   # the leader
kubectl -n default get pdb -l gateway.networking.k8s.io/gateway-name=web
```

- In production, run 2 or more rproxy pods. With one, traffic is interrupted while the pod is replaced or its node is drained.
- Measured gaps for each setup are in "Availability" in the [README](../../README.en.md) and in [PLATFORM.md](PLATFORM.md).
- A fleet rproxy holds up to 1000 rule sets (`RPROXY_MAX_RULESETS`) and 10000 rules (`RPROXY_MAX_RULES`). In fleet mode every Gateway lives on the same rproxy, so with more Gateways than that, raise the limits with `fleet.extraEnv` (quote the values, as in `"5000"`).

### 13.4 Backup

rproxy's rules live only in rproxy's memory; the source of truth is the Kubernetes objects. So what you back up is Kubernetes objects and the chart's values. What to keep:

| What | Needed |
|---|---|
| Gateway API objects (Gateways, every kind of route, ReferenceGrants, ListenerSets, BackendTLSPolicies) | yes |
| Objects of rproxy's CRDs (RproxyPolicy, RproxyMiddleware, RproxyRule, RproxyGatewayParameters) | yes |
| Certificate Secrets (those `certificateRefs` name), CA ConfigMaps | yes (cert-manager can issue its own again) |
| The chart's values (`values.yaml`) | yes |
| The controller's Secrets (`rproxy-gateway-ca`, `rproxy-gateway-api-tls`, `rproxy-gateway-token`) | optional. Without them the controller makes new ones and the rproxy pods are replaced once. The management UI's discovery Secret is rewritten too |
| Each Gateway's `rproxy-<id>-certs` and `rproxy-<id>-api`, rproxy's Deployments and Services | no (the controller makes them) |

With kubectl:

```bash
kubectl get gateways,httproutes,grpcroutes,tlsroutes,tcproutes,udproutes,referencegrants,listenersets,backendtlspolicies -A -o yaml > gateway-api.yaml
kubectl get rproxypolicies,rproxymiddlewares,rproxyrules,rproxygatewayparameters -A -o yaml > rproxy-crs.yaml
```

To restore, install Gateway API's CRDs and the chart first, then apply what you saved. With Velero or other per-namespace backups, make sure the kinds above are included. On a new cluster, the Services' load balancer addresses may change. To pin them, set them with load balancer annotations or `spec.addresses` ([PLATFORM.md](PLATFORM.md)).

### 13.5 Log levels

How detailed the logs are is set separately for the controller and for rproxy.

- **Controller**: the format is the chart's `controller.logFormat` (default `json`). The level is the environment variable `RPROXY_GATEWAY_LOG` (default `info`; `debug` and others work too). To raise it only while you investigate:

  ```bash
  kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG=debug
  ```

  `RPROXY_GATEWAY_LOG` is not a chart value, so `helm upgrade` does not remove it (Helm only fixes the fields it wrote itself). When you are done, remove it with `kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG-`.
- **rproxy**: set by `rproxy.logLevel` (`error` to `trace`) of `RproxyGatewayParameters` ([8.4](#84-rproxygatewayparameters)).

## 14. Recipes

This chapter collects common setups in a form you can use as they are. Each recipe combines explanations from earlier chapters, so see the referenced sections for the details.

### 14.1 A web app with HTTPS and cert-manager

Serves the app of 2.1 over HTTPS with a Let's Encrypt certificate, and redirects what comes over HTTP to HTTPS. Enable cert-manager's Gateway API support first (3.2). It can be written with standard Gateway API and cert-manager alone.

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: {name: letsencrypt}
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@example.com
    privateKeySecretRef: {name: letsencrypt-account}
    solvers:
      - http01:
          gatewayHTTPRoute:
            parentRefs:
              - {name: web, namespace: default, kind: Gateway, sectionName: http}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: app-tls, namespace: default}
spec:
  secretName: app-tls
  dnsNames: [app.example.com]
  issuerRef: {name: letsencrypt, kind: ClusterIssuer, group: cert-manager.io}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: to-https, namespace: default}
spec:
  parentRefs:
    - {name: web, sectionName: http}
  hostnames: [app.example.com]
  rules:
    - filters:
        - type: RequestRedirect
          requestRedirect: {scheme: https, statusCode: 301}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs:
    - {name: web, sectionName: https}
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080}
```

It is the ClusterIssuer and Certificate of 3.2, the Gateway of 3.1, the redirect route of 3.3 and the app's route. Steps:

1. Apply the YAML above. Look up `ADDRESS` with `kubectl -n default get gateway web` and point the DNS name `app.example.com` at it.
2. Wait until `kubectl -n default get certificate app-tls` shows `READY True` (until then the `https` listener is `ResolvedRefs: False`).
3. You are done when `curl -I http://app.example.com/` returns 301 and `curl https://app.example.com/` returns 200.

### 14.2 TCP to a DB with a long idleTimeout

Exposes a database such as PostgreSQL over TCP. By default rproxy closes an L4 connection that carries nothing for an hour. For databases with connections that stay open and quiet for a long time, such as connection pools, lengthen the RproxyPolicy's `idleTimeout`.

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: db, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: postgres
      port: 5432
      protocol: TCP
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: postgres, namespace: default}
spec:
  parentRefs:
    - {name: db, sectionName: postgres}
  rules:
    - backendRefs:
        - {name: postgres, port: 5432}
---
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: db, namespace: default}
spec:
  targetRefs:
    - {group: gateway.networking.k8s.io, kind: Gateway, name: db, sectionName: postgres}
  allowFrom: [10.0.0.0/8]
  idleTimeout: 8h
  halfCloseTimeout: 0s
```

- **rproxy-specific**: the RproxyPolicy accepts connections only from `10.0.0.0/8`, allows quiet connections for up to 8 hours, and does not close half-closed connections on a timer ([8.1](#81-rproxypolicy)).
- It is in effect when `status.ancestors[].conditions` of `kubectl -n default get rproxypolicy db -o yaml` shows `rproxy.max3584.net/RelayTimeouts: True` (reason `Applied`).
- If you expose the database outside the cluster, restrict the sources with `allowFrom` or `service.loadBalancerSourceRanges` ([8.4](#84-rproxygatewayparameters)).
- When rproxy's pods are replaced, connections still open after `drain` (25 seconds by default) are cut. Make sure the app can reconnect.

### 14.3 UDP

Exposes a service that uses the same port over UDP and TCP, such as DNS.

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: dns, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: dns-udp
      port: 53
      protocol: UDP
    - name: dns-tcp
      port: 53
      protocol: TCP
---
apiVersion: gateway.networking.k8s.io/v1
kind: UDPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs:
    - {name: dns, sectionName: dns-udp}
  rules:
    - backendRefs:
        - {name: dns, port: 53}
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs:
    - {name: dns, sectionName: dns-tcp}
  rules:
    - backendRefs:
        - {name: dns, port: 53}
```

- Port 53 has one UDP and one TCP listener, with a UDPRoute and a TCPRoute attached to them.
- Give the backend Service `dns` both the TCP and the UDP port 53.
- The load balancer implementation must support mixing TCP and UDP in one Service.

**Passing the client address to the backend** (**rproxy-specific**): with `externalTrafficPolicy: Local` (the `LoadBalancer` default), the client address reaches rproxy, but no further. rproxy uses it for `allowFrom`, `geoip` and logs. The TCP and UDP rules rendered from Gateway routes, however, do not pass the source address on to the backend (they have no `source_ip`), so the backend sees the rproxy pod's IP. To pass the client address on, write the rproxy rule itself in an RproxyRule ([8.3](#83-rproxyrule)) with `source_ip: proxy_v2` (works for UDP too) or `proxy_v1` (TCP only). The backend must be able to read PROXY protocol ("ルール" in rproxy-api's docs/API.md). RproxyPolicy has no field for passing the source.

### 14.4 SNI passthrough on 443

On the same port 443, rproxy decrypts some names and routes them as HTTP, and sends other names to the backend as they are, without decrypting. This lets you serve a service whose certificate you do not want to hand to the Gateway and an ordinary web app on the same address and port.

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: edge, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    # app.example.com: rproxy decrypts and routes as HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
    # secure.example.com: sent to the backend undecrypted
    - name: passthrough
      port: 443
      protocol: TLS
      hostname: secure.example.com
      tls:
        mode: Passthrough
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: edge, sectionName: https}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: secure-app, namespace: default}
spec:
  parentRefs: [{name: edge, sectionName: passthrough}]
  hostnames: [secure.example.com]
  rules:
    - backendRefs: [{name: secure-app, port: 8443}]
```

- rproxy takes port 443 with one rule and sends only connections with the SNI `secure.example.com` on without decrypting them ([5.2](#52-tlsroute-passthrough-and-terminate)).
- The `secure-app` pods hold the certificate for `secure.example.com`.
- To check: `openssl s_client -connect $ADDR:443 -servername secure.example.com` shows `secure-app`'s certificate, and `-servername app.example.com` shows the `app-tls` certificate.

### 14.5 Canary by weight

Steps to move requests to a new version of the app little by little (how to write weights: [4.3](#43-weights-and-canaries)).

1. Create the Deployment and Service of the new version `app-v2`.
2. Apply the HTTPRoute of 4.3 with `app: 90` and `app-v2: 10`.
3. While watching status and errors, rewrite the weights and apply again (`90/10` → `50/50` → `0/100`). No step cuts connections.
4. To roll back if something is wrong, set `app-v2`'s weight to 0.

A command that changes only the weights, and one that shows the actual split:

```bash
# change only the weights (the two backendRefs of rules[0])
kubectl -n default patch httproute app --type=json -p '[
  {"op": "replace", "path": "/spec/rules/0/backendRefs/0/weight", "value": 50},
  {"op": "replace", "path": "/spec/rules/0/backendRefs/1/weight", "value": 50}]'
# see the split (echo answers with its pod name)
for i in $(seq 100); do curl -s -H 'Host: app.example.com' http://$ADDR/ | jq -r .pod; done | sort | uniq -c
```

A canary that sends only testers to the new version (chosen by a header) can be written with the `headers` match of 4.2.

### 14.6 An auth middleware via ExtensionRef

> **rproxy-specific**: this uses an RproxyMiddleware. To write it in standard Gateway API, use the `ExternalAuth` filter (**Gateway API experimental**).

Puts an oauth2-proxy sign-in in front of the app. It uses the RproxyMiddleware `login` (`forward_auth`) and the HTTPRoute of 8.2 as they are.

1. Run oauth2-proxy as the Service `oauth2-proxy` (port 4180). Set the redirect URL to `https://app.example.com/oauth2/callback` and add `--reverse-proxy` and `--set-xauthrequest` (check oauth2-proxy's documentation).
2. Apply the RproxyMiddleware `login` and the HTTPRoute `app` of 8.2 (the Gateway is `web` of 14.1).
3. Without a sign-in, oauth2-proxy's `/oauth2/auth` answers 401, and that goes back to the client as it is (rproxy returns the authentication server's non-2xx answers unchanged). Signing in starts at `https://app.example.com/oauth2/start`. For sending browsers to the sign-in page automatically, see oauth2-proxy's documentation (its forward auth setup).

Notes:

- `forward_auth`'s `service` is a rule `name` of the same HTTPRoute (`oauth2` here). Rules of other routes cannot be named.
- For an authentication server outside the cluster, use `address` instead of `service`, and allow its host in the GatewayClass's `policy.middlewares.outboundHosts` (8.2).
- Gateway API's `ExternalAuth` filter (experimental CRDs) does the same (in rproxy it becomes `forward_auth`).

### 14.7 Listener filters

> Combines **Gateway API experimental** (listener filters) with **rproxy-specific** parts (the rate-limit RproxyMiddleware).

Applies authentication and a rate limit to every request on a port, whatever the hostname or path. It uses the YAML of 7.3 as it is (Gateway API v1.7's experimental CRDs are needed).

After applying it, check that the listener was accepted and that authentication comes first even on a path with no route:

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# http Accepted=True(Accepted) ResolvedRefs=True(ResolvedRefs) Conflicted=False(NoConflicts) Programmed=True(Programmed)
curl -i -H 'Host: anything.example.com' http://$ADDR/no-such-path
# if the authorization server refuses, its 403 (or such) comes first, even without a route
```

- If the same RproxyMiddleware is on both a route's `ExtensionRef` and the listener, it runs twice, separately.
- If you delete the authorization server's Service, the listener becomes `ResolvedRefs: False` (reason `InvalidListenerFilterRef`) and answers 500. Requests are never let through without authentication.

## 15. Troubleshooting

This chapter collects where to look when things do not work, what the common conditions mean, and how to fix them. First look up the condition's reason in the tables of 15.1; if that does not explain it, read the controller's logs as in 15.2.

### 15.1 Common conditions

What each condition and reason you see in `kubectl describe` and similar means, and what to do about it.

**GatewayClass and Gateway**

| Where | Condition (reason) | Meaning | What to do |
|---|---|---|---|
| GatewayClass | `Accepted: False` (`InvalidParameters`) | The class's parameters (the chart's `managed.parameters`) are wrong | Fix the field named in the message. The class's parameters can only name one in the controller's namespace |
| Gateway | `Programmed: False` (`AddressNotAssigned`) | The Service gets no address | Install a load balancer implementation ([PLATFORM.md](PLATFORM.md)) or change `managed.serviceType`. `EXTERNAL-IP` in `kubectl get svc` shows `<pending>` |
| Gateway | `Accepted: False` (`InvalidParameters`) | `RproxyGatewayParameters` is wrong or sets a field that is not allowed | Fix the field named in the message. A Gateway that was already running keeps its previous shape (`Programmed: True`) |
| Gateway | `Accepted: False` (`UnsupportedAddress`) | A `spec.addresses` type other than `IPAddress` | Use `IPAddress` |
| Gateway | `Programmed: False` (`AddressNotUsable`) | `spec.addresses` is outside `managed.addressCIDRs`, or is an unusable IP | Ask the administrator to open the range, or fix the address |
| Gateway, listener | `Programmed: False` (`Invalid`, "rproxy refused the rule set: 409 limit ...") | rproxy hit one of its count limits | Raise `RPROXY_MAX_RULESETS` / `RPROXY_MAX_RULES` ("Where rproxy runs" in [DESIGN.md](DESIGN.md)) |
| Gateway, listener | `Programmed: False` (`Pending`) | Waiting to apply (certificate files, pod start) | Wait a minute or two. If it lasts, read the controller's logs |
| Gateway, listener | `Programmed: False` ("rproxy ... has no rule sets (features.rulesets; rproxy v0.4.0 or later)") | rproxy is too old (`rproxy.image` was changed) | Go back to the chart's image |

**Listeners**

| Condition (reason) | Meaning | What to do |
|---|---|---|
| `ResolvedRefs: False` (`InvalidCertificateRef`) | The certificate Secret is missing, or has no `tls.crt` / `tls.key` | Create the Secret (with cert-manager, look at the Certificate's status) |
| `ResolvedRefs: False` (`RefNotPermitted`) | Another namespace's Secret is used without a ReferenceGrant | Put a ReferenceGrant in the Secret's namespace |
| `ResolvedRefs: False` (`InvalidRouteKinds`) | `allowedRoutes.kinds` has a kind the protocol cannot take | Use a kind that matches the protocol ([5.3](#53-the-listener-for-each-route)) |
| `Accepted: False` (`PortUnavailable`) | fleet: an older Gateway holds the port | Use another port, or `fleet.listen: addresses` |
| `Accepted: False` (`UnsupportedProtocol`) | An unknown protocol | Use one of `HTTP`, `HTTPS`, `TLS`, `TCP`, `UDP` |
| `Accepted: False` (`UnsupportedValue`) | Listener filters cannot be used, or do not match across the port | Read the message ([7.3](#73-listener-filters)) |

**Routes**

| Condition (reason) | Meaning | What to do |
|---|---|---|
| `Accepted: False` (`NotAllowedByListeners`) | The listener's `allowedRoutes` does not allow the route's namespace or kind | Review the Gateway's `allowedRoutes` and the route's `sectionName` |
| `Accepted: False` (`NoMatchingListenerHostname`) | The route's and the listener's hostnames do not overlap | Fix the hostnames |
| `Accepted: False` (`NoMatchingParent`) | The Gateway or listener in `parentRefs` does not exist | Fix the name, namespace or `sectionName` |
| `Accepted: False` (`UnsupportedValue`) | A value rproxy does not take, a wrong RproxyMiddleware, an outside URL that is not allowed | Fix the field named in the message |
| `ResolvedRefs: False` (`BackendNotFound`) | The Service does not exist | Fix the Service's name and port. Until then, that backend's share gets 500 |
| `ResolvedRefs: False` (`RefNotPermitted`) | Another namespace's Service is used without a ReferenceGrant | Put a ReferenceGrant in the Service's namespace ([6.2](#62-referencegrant-using-another-namespaces-services-and-secrets)) |

**Policies and RproxyRules**

| Where | Condition (reason) | Meaning | What to do |
|---|---|---|---|
| RproxyPolicy | `Accepted: False` (`Invalid`, `TargetNotFound`) | A wrongly written duration, or the named listener does not exist | Fix the value or `sectionName` |
| RproxyRule | `Accepted: False` (`Conflicted`, `Invalid`, `NotAllowed`) | A rule with the same key exists, rproxy refused it, or the class turned RproxyRules off | Change the port, read the message, check `policy.rproxyRules` |
| BackendTLSPolicy | `Accepted: False` (`NoValidCACertificate`, `Conflicted`) | No usable CA, or an older policy exists for the same target | Fix the CA ConfigMap (`ca.crt`), or keep only one policy |

**When the status looks fine but traffic does not**

- **404**: the hostname (`Host` header) matches no route. Try other hostnames with `curl -H 'Host: ...'`.
- **500**: there is no backend to send to. The Service's pods are not Ready, or the route is `BackendNotFound` or `RefNotPermitted`.
- **502, 504**: rproxy cannot connect to the backend, or the backend answers too slowly. Look at `backends.connectTimeout` and `responseTimeout` ([1.3](#13-main-chart-values)).
- **No connection (it times out)**: review the Service's `EXTERNAL-IP`, firewalls, `loadBalancerSourceRanges` and RproxyPolicy's `allowFrom`. Also check whether the rproxy pods are Ready (READINESS GATES).

### 15.2 Controller logs

When the status does not reveal the cause, read the controller's logs. Two controllers run, but only the leader applies changes, so read the leader's logs.

```bash
# the leader's logs (of the two, only the leader applies)
LEADER=$(kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}')
kubectl -n rproxy-gateway-system logs $LEADER --since=1h
# everything but info, easier to read
kubectl -n rproxy-gateway-system logs $LEADER --since=1h \
  | jq -r 'select(.level != "INFO") | "\(.timestamp) \(.level) \(.fields.message) \(.fields | del(.message) | tostring)"'
```

Each line is one JSON object (`timestamp`, `level`, `fields.message` and other fields). Common messages and what they mean:

| message | Meaning |
|---|---|
| `became the leader`, `lost the Lease to another replica` | The leader changed (normal when the controller is replaced) |
| `could not renew the Lease in time; stepping down` | It could not reach the API server and stepped down as leader (the API server or etcd is slow) |
| `rproxy refused a rule; applying the others` | rproxy refused one rule. It also shows in status |
| `rproxy refused routes; applying the others` | rproxy refused routes; they were left out and the rest applied |
| `rproxy refused the rule set` | The whole rule set was refused (a count limit, for example) |
| `listener port taken by another Gateway` | fleet: two Gateways want the same port |
| `rule not running` | A rule is not running in rproxy (it cannot open the port, cannot resolve a name, and so on) |
| `cannot deploy rproxy` | The Deployment or Service cannot be created (RBAC, admission, ResourceQuota) |
| `reconcile failed` | One reconcile failed. It keeps retrying |
| `not served by the API server (CRD not installed); looked for again later` | That kind's CRD is missing. Once installed, it is found within about 30 seconds |
| `migration: not converted` | Migration could not convert something ([MIGRATION.md](MIGRATION.md)) |

For more detail, set `RPROXY_GATEWAY_LOG=debug` as in 13.5. rproxy's own logs are read with `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy`, and the certificate file handover's with `-c certsync`.

### 15.3 Checking without a cluster

`rproxy-gateway render` draws rproxy's rule sets and the status from a file of manifests. It needs neither a cluster nor rproxy, so use it to check YAML before applying it, or to see what the conversion produces. Put the GatewayClass, Services and EndpointSlices in the same file.

```bash
docker run --rm -v $PWD:/w ghcr.io/max3584/rproxy-gateway:0.4.22 render -f /w/manifests.yaml
```

The `conditions` of `listeners` and `routes` in the output show the same reasons as in a cluster. Items decided by asking rproxy's pods (listener filters, for example) show as "not known (yet)".
