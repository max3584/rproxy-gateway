日本語: [../GUIDE.md](../GUIDE.md)

# rproxy-gateway configuration guide

How to create Gateways with rproxy-gateway, send traffic through them and run them. Each task comes with YAML you can paste as it is and the commands to check it.

- The detailed decisions are in [DESIGN.md](DESIGN.md) and [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), tenant separation in [SECURITY.md](SECURITY.md), providing addresses (MetalLB and others) in [PLATFORM.md](PLATFORM.md), migration from Ingress and Traefik in [MIGRATION.md](MIGRATION.md).
- The names in the examples (`app.example.com`, the `default` namespace, the Service `app`) and addresses (`192.0.2.0/24`, `203.0.113.0/24`) are for documentation. Replace them with yours.
- The examples were checked with this version (v0.4.22, bundled rproxy v0.4.22): every YAML against the CRD schemas, and the conversion with `rproxy-gateway render`.

## Contents

0. [Gateway API in brief](#0-gateway-api-in-brief)
1. [Install](#1-install)
2. [A first Gateway and HTTPRoute](#2-a-first-gateway-and-httproute)
3. [HTTPS](#3-https)
4. [HTTP routing](#4-http-routing)
5. [L4 forwarding](#5-l4-forwarding)
6. [Other namespaces](#6-other-namespaces)
7. [rproxy's CRDs and other settings](#7-rproxys-crds-and-other-settings)
8. [managed and fleet](#8-managed-and-fleet)
9. [Changing things while they run](#9-changing-things-while-they-run)
10. [How it talks to rproxy](#10-how-it-talks-to-rproxy)
11. [Security basics](#11-security-basics)
12. [Operations](#12-operations)
13. [Recipes](#13-recipes)
14. [Troubleshooting](#14-troubleshooting)

## 0. Gateway API in brief

Gateway API is the Kubernetes API for "where traffic from outside goes". Its resources are split by role.

| Resource | Written by | Holds |
|---|---|---|
| `GatewayClass` | the cluster admin (the chart creates `rproxy`) | which controller handles it |
| `Gateway` | the Gateway owner | listeners: port, protocol, hostname, certificate |
| `HTTPRoute`, `GRPCRoute`, `TLSRoute`, `TCPRoute`, `UDPRoute` | the app owner | which requests and connections go to which Service |
| `ReferenceGrant` | the owner of the namespace referred to | permission for other namespaces to use its Services and Secrets |

```
client ──▶ Gateway listener (:80 / :443) ──▶ route (hostname, path) ──▶ the Service's pods
            └─ an rproxy pod (per Gateway) listens
```

rproxy-gateway reads these, turns them into rproxy rules and sends them to each Gateway's rproxy. rproxy does not know Kubernetes ([10.](#10-how-it-talks-to-rproxy)).

## 1. Install

### 1.1 Gateway API's CRDs

Gateway API's CRDs are not in the chart. Install them first. There are two channels.

| Channel | What it has (v1.6.3) |
|---|---|
| standard | GatewayClass, Gateway, HTTPRoute, GRPCRoute, TLSRoute, TCPRoute, UDPRoute, ListenerSet, ReferenceGrant, BackendTLSPolicy |
| experimental | all of standard, and fields still experimental |

What needs the experimental CRDs:

| Feature | CRDs |
|---|---|
| HTTPRoute `retry` | experimental in v1.6; standard from v1.7 |
| HTTPRoute `ExternalAuth` filter | experimental |
| Listener filters (`filters.requests`, [7.5](#75-listener-filters)) | v1.7 experimental |
| `spec.addresses[].routability` ([7.8](#78-address-routability)) | v1.7 experimental |

```bash
# standard (usually enough)
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml
# experimental (for the features above; for v1.7's features, v1.7.0's experimental-install.yaml)
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/experimental-install.yaml
```

Which Kubernetes and Gateway API versions go together: "Supported versions" in the [README](../../README.en.md). Gateway API v1.6 needs Kubernetes 1.32 or later.

Check:

```bash
kubectl get crd | grep gateway.networking.k8s.io
```

### 1.2 helm install

```bash
helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway \
  -n rproxy-gateway-system --create-namespace -f values.yaml
```

`-f values.yaml` may be left out (the defaults work). The chart installs rproxy's CRDs (`rproxy.max3584.net`), RBAC, the controller (2 replicas) and the GatewayClass `rproxy`.

Check:

```bash
kubectl -n rproxy-gateway-system get pods
# NAME                              READY   STATUS    RESTARTS   AGE
# rproxy-gateway-6c9d8f7b5d-4xk2p   1/1     Running   0          1m
# rproxy-gateway-6c9d8f7b5d-9qm7r   1/1     Running   0          1m
kubectl get gatewayclass rproxy
# NAME     CONTROLLER                              ACCEPTED   AGE
# rproxy   rproxy.max3584.net/gateway-controller   True       1m
```

`ACCEPTED` `True` means the controller handles the GatewayClass. The features it claims: `kubectl get gatewayclass rproxy -o jsonpath='{.status.supportedFeatures[*].name}'`.

Without Helm (`install.yaml`, Kustomize): "Installing without Helm (kubectl, Kustomize)" in the [README](../../README.en.md).

### 1.3 Main chart values

All of them, with comments, are in [values.yaml](../../charts/rproxy-gateway/values.yaml).

| Value | Default | What |
|---|---|---|
| `controller.replicas` | `2` | Controllers. One becomes the leader through a Lease |
| `managed.replicas` | `1` | rproxy pods per Gateway. 2 or more get a PDB and spreading over nodes |
| `managed.serviceType` | `LoadBalancer` | Type of each Gateway's Service (`LoadBalancer`, `NodePort`, `ClusterIP`) |
| `managed.externalTrafficPolicy` | `""` | Empty: `Local` for `LoadBalancer`, `Cluster` for `NodePort` |
| `managed.allocateLoadBalancerNodePorts` | `true` | `false` for load balancers that do not use node ports, like MetalLB |
| `managed.addressCIDRs` | `[]` | Ranges Gateways' `spec.addresses` may use. Empty: none |
| `managed.networkPolicy` | `true` | A NetworkPolicy per Gateway ([11.](#11-security-basics)) |
| `managed.shutdown` | `{delay: 15s, drain: 25s}` | How rproxy stops ([9.](#9-changing-things-while-they-run)) |
| `managed.parameters` | `{}` | Every Gateway's default `RproxyGatewayParameters` and what tenants may set ([7.4](#74-rproxygatewayparameters)) |
| `fleet.enabled` | `false` | Fleet mode ([8.](#8-managed-and-fleet)) |
| `backends.connectTimeout` | `{http: 1s, l4: 1s}` | Time to connect to a backend pod |
| `backends.responseTimeout` | `30s` | Time to wait for an HTTPRoute backend's response headers (when the route has no `timeouts`) |
| `backends.outlierDetection` | (set) | Passive health checks: failing pods are left out |
| `controller.watchNamespaces` | `[]` | Watch only these namespaces (permissions become Roles too) |
| `controller.crossNamespaceSecrets` | `true` | Use certificate Secrets of other namespaces with a ReferenceGrant |
| `ui.namespace` | `""` | Show Gateways' rproxy to the management UI |
| `migration.migrateTo` | `""` | Migration from Ingress and Traefik ([MIGRATION.md](MIGRATION.md)) |

Example: two rproxy pods each, with MetalLB.

```yaml
# values.yaml
managed:
  replicas: 2
  allocateLoadBalancerNodePorts: false
```

## 2. A first Gateway and HTTPRoute

### 2.1 The backend app

The app used in the examples: the echo used by Gateway API's tests (answers with the request as JSON).

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

### 2.2 Gateway and HTTPRoute

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

- `parentRefs` names the Gateway to attach to. `hostnames` are the names the Host header is matched against (all when left out).
- `backendRefs` is a Service and its port. rproxy sends not to the Service's ClusterIP but straight to the ready pod IPs of its EndpointSlices, and balances and health-checks them itself.

### 2.3 What gets created

In managed mode (the default) the controller creates these in the Gateway's namespace. `<id>` is `<namespace>-<name>` and a 6-digit hash (e.g. `default-web-1a2b3c`). The Gateway owns all of them: deleting the Gateway deletes them.

| Object | Name |
|---|---|
| rproxy's Deployment and ServiceAccount | `rproxy-<id>` |
| Service (`LoadBalancer`) | `rproxy-<id>` |
| Certificate Secret | `rproxy-<id>-certs` |
| Control API Secret (certificate, token file) | `rproxy-<id>-api` |
| NetworkPolicy | `rproxy-<id>` |

```bash
kubectl -n default get deploy,svc,pod -l gateway.networking.k8s.io/gateway-name=web
```

### 2.4 Check

```bash
kubectl -n default get gateway web
# NAME   CLASS    ADDRESS        PROGRAMMED   AGE
# web    rproxy   192.0.2.100    True         40s
kubectl -n default get httproute app
# NAME   HOSTNAMES             AGE
# app    ["app.example.com"]   40s
```

- `ADDRESS` is the Gateway's address: the one given to the `LoadBalancer` Service (the ClusterIP for a `ClusterIP` Service).
- `PROGRAMMED` `True` means the rules are on at least one rproxy pod.

Take just the address and try it:

```bash
ADDR=$(kubectl -n default get gateway web -o jsonpath='{.status.addresses[0].value}')
curl -H 'Host: app.example.com' http://$ADDR/
```

Read the status with `kubectl describe`:

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

(Shortened.) A route's status is in `Status.Parents` (per Gateway) of `kubectl -n default describe httproute app`: `Accepted` and `ResolvedRefs`.

An empty `ADDRESS` with `PROGRAMMED` `False` (`AddressNotAssigned`, "waiting for an address (the rproxy Service)") means nothing in the cluster hands out `LoadBalancer` addresses (MetalLB, a cloud load balancer...). Provide one ([PLATFORM.md](PLATFORM.md)) or use `managed.serviceType: NodePort` / `ClusterIP`.

## 3. HTTPS

### 3.1 A TLS Secret in certificateRefs

Put the certificate and key in a `kubernetes.io/tls` Secret.

```bash
kubectl -n default create secret tls app-tls --cert=app.crt --key=app.key
```

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

- rproxy terminates TLS on `HTTPS` listeners (`mode: Terminate`, the same when left out). One port may have several `HTTPS` listeners (a certificate per hostname); rproxy picks the certificate by SNI.
- `sectionName` is the listener's name. Left out, the route attaches to every listener that takes it.
- Keys do not go through the control API. The controller copies them into `rproxy-<id>-certs` and the kubelet gives them to the rproxy pods as files ([10.](#10-how-it-talks-to-rproxy)).
- Rewriting the certificate Secret (a renewal) switches to the new certificate without cutting connections ([9.](#9-changing-things-while-they-run)).

Check:

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{.conditions[?(@.type=="ResolvedRefs")].reason}{"\n"}{end}'
# http ResolvedRefs
# https ResolvedRefs
curl --resolve app.example.com:443:$ADDR https://app.example.com/
```

With the Secret missing or broken the listener is `ResolvedRefs: False` (`InvalidCertificateRef`) and `Programmed: False`, and that port gets no rule.

### 3.2 cert-manager

Name the Secret [cert-manager](https://cert-manager.io/) makes in `certificateRefs`. rproxy-gateway only reads the Secret; there is no special link to cert-manager.

**With a Certificate** (the form the acceptance tests check, renewals included):

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

- The Gateway is the one in 3.1 (`certificateRefs: [{name: app-tls}]`). The listener is `ResolvedRefs: False` until the Secret exists, and becomes `True` by itself once it does.
- With HTTP-01's `gatewayHTTPRoute`, cert-manager attaches an HTTPRoute for the challenge to the Gateway's `http` listener. Turn on cert-manager's Gateway API support (`config.enableGatewayAPI: true` in cert-manager's chart, or as cert-manager's documentation says). DNS-01 does not involve the Gateway.
- The redirect route of 3.3 on the same hostname does not get in the way: the challenge path (`/.well-known/acme-challenge/...`) is the longer match and wins.

**With an annotation on the Gateway** (a cert-manager feature: cert-manager makes the Certificate from the listeners' `hostname` and `certificateRefs`):

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

Check:

```bash
kubectl -n default get certificate app-tls     # READY True
kubectl -n default get secret app-tls
```

**rproxy's ACME is not used**: rproxy has ACME (getting certificates from Let's Encrypt by itself), but Gateway API does not use it. Certificates come from the Secrets in `certificateRefs`. rproxy's ACME would order a certificate from every pod, so with several pods an HTTP-01 challenge can reach another pod and fail. Get certificates with cert-manager. The only exception is migrating Traefik's `certResolver` ("Mapping" in [MIGRATION.md](MIGRATION.md)). DNS providers cert-manager does not build in (OCI and others) can often be covered by cert-manager DNS-01 webhooks (a separate webhook per provider; this project has not tested them).

### 3.3 Redirect HTTP to HTTPS

Attach a redirect-only route to the HTTP listener.

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

Attach the app's route (`app` of 3.1) to `sectionName: https` only. Changing the scheme sets the port to the scheme's default (443).

```bash
curl -sI -H 'Host: app.example.com' http://$ADDR/path
# HTTP/1.1 301 Moved Permanently
# location: https://app.example.com/path
```

## 4. HTTP routing

The HTTPRoutes of this section attach to the Gateway `web` of 2.2.

### 4.1 Hostnames

Only what matches both the listener's `hostname` and the route's `hostnames` arrives.

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

- `*.example.com` is every name under `example.com` (`a.b.example.com` too), not `example.com` itself.
- A route whose hostnames do not meet the listener's is `Accepted: False` (`NoMatchingListenerHostname`).
- A request matching no route gets a 404.
- With a listener of a more specific hostname on the same port, that name reaches only the specific listener's routes (Gateway API's listener isolation).

### 4.2 Path, header, query, method

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

- All conditions within one item of `matches` must match (AND). Items of `matches` are ORed.
- `PathPrefix: /api` matches `/api` and `/api/...`, not `/apis` (matched on `/` separators).
- The order is not the written one but Gateway API's (Exact → longer PathPrefix → method → number of headers → number of queries → older route...). "Rule precedence" in "Mapping (Gateway API → rproxy)" of [DESIGN.md](DESIGN.md).
- Headers and queries take `type: RegularExpression` too.

### 4.3 Weights and canaries

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

- About 1 in 10 requests goes to `app-v2`. The controller divides each weight by the number of pods into per-pod weights (the ratio holds with different pod counts).
- A backend with `weight: 0` gets nothing.
- Changing weights cuts no connection ([9.](#9-changing-things-while-they-run)). Steps in [13.5](#135-canary-by-weight).

### 4.4 Filters

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

- `set` replaces, `add` appends to an existing value with `,`, `remove` removes.
- `URLRewrite`'s `ReplacePrefixMatch` works only when the rule has one path match (`PathPrefix`).
- `CORS`, `RequestMirror` (a copy to another Service) and per-backendRef `filters` work too. rproxy's own features (rate limits, authentication...) go through `ExtensionRef`: [7.2](#72-rproxymiddleware).

### 4.5 Retries and timeouts

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

- Rules without `timeouts` get a default of 30 s for the backend's response headers (the chart's `backends.responseTimeout`). Response bodies (big downloads, SSE) and WebSockets are not cut. For apps that take longer, write `backendRequest` or an RproxyPolicy's `responseTimeout` ([7.1](#71-rproxypolicy)).
- rproxy retries when it cannot connect or gets no response, and on the `codes`, only for idempotent methods (GET, HEAD, OPTIONS, PUT, DELETE, TRACE) without a body.
- A timeout is a 504.

### 4.6 GRPCRoute

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

- Attaches to `HTTP` and `HTTPS` listeners, on the same port as HTTPRoutes.
- Talks h2c (plaintext HTTP/2) to the backend. For a TLS backend, BackendTLSPolicy ([7.6](#76-backendtlspolicy)).
- GRPCRoutes do not get the 30 s response default (streams). Use an RproxyPolicy's `responseTimeout` if you need one.

For an HTTPRoute backend speaking h2c, set `appProtocol: kubernetes.io/h2c` on the Service port.

## 5. L4 forwarding

### 5.1 TCPRoute and UDPRoute

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

- `allowedRoutes.kinds` may be left out (`TCP` listeners take TCPRoutes, `UDP` ones UDPRoutes).
- With several TCPRoutes or UDPRoutes on one listener, all are `Accepted` but traffic goes to the oldest.
- A `TCP` or `UDP` listener with nothing attached gets no rule (the Service port is there).
- With older Gateway API CRDs (before v1.6) TCPRoute and UDPRoute are `v1alpha2`. Write the version the API server serves.

Check:

```bash
kubectl -n default get tcproute,udproute
kubectl -n default get tcproute postgres -o jsonpath='{.status.parents[0].conditions[*].reason}'
# Accepted ResolvedRefs
psql -h $ADDR -p 5432 -U app
dig @$ADDR example.com
```

### 5.2 TLSRoute (passthrough and terminate)

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

- **Passthrough**: rproxy reads only the SNI of TLS's first message (ClientHello) and sends the encrypted stream to the backend. It cannot see HTTP inside (no header rewriting). Connections matching no name are refused.
- **Terminate**: rproxy terminates TLS with the certificate and sends the contents as TCP (not routed as HTTP). To route as HTTP, use an `HTTPS` listener and HTTPRoutes.
- `HTTPS` and `TLS` (Passthrough) listeners may share a port: hostnames decide which names are decrypted and which are not ([13.4](#134-sni-passthrough-on-443)).

### 5.3 The listener for each route

| Route | Listener `protocol` | `tls.mode` | Certificate (`certificateRefs`) |
|---|---|---|---|
| HTTPRoute, GRPCRoute | `HTTP` | — | — |
| HTTPRoute, GRPCRoute | `HTTPS` | `Terminate` (may be left out) | needed |
| TLSRoute | `TLS` | `Passthrough` | not needed (the backend has it) |
| TLSRoute | `TLS` | `Terminate` | needed |
| TCPRoute | `TCP` | — | — |
| UDPRoute | `UDP` | — | — |

- The managed Service has all the Gateway's listener ports. TCP and UDP on the same port (DNS's 53) in one `LoadBalancer` Service need a load balancer that supports mixed protocols.
- Ports below 1024 work as they are (rproxy's pods run as non-root and open them through a safe sysctl).

## 6. Other namespaces

### 6.1 allowedRoutes: which namespaces' routes a Gateway takes

By default a Gateway takes routes of its own namespace only (`from: Same`). A shared Gateway opens up with `allowedRoutes`.

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

- `from` is `Same` (default), `All` or `Selector`.
- Routes of other namespaces are `Accepted: False` (`NotAllowedByListeners`).
- Keep tenants from writing namespace labels.

### 6.2 ReferenceGrant: using another namespace's Services and Secrets

A route names Services of its own namespace. To name another namespace's Service, put a ReferenceGrant **in the namespace referred to**.

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

- Without the ReferenceGrant the route is `ResolvedRefs: False` (`RefNotPermitted`, "Service default/api: no ReferenceGrant allows the reference") and that backend's share gets 500.
- The same for a Gateway's `certificateRefs` naming another namespace's Secret (`from` `kind: Gateway`, `to` `kind: Secret`). In managed mode the key is copied into a Secret of the Gateway's namespace ("Keys from other namespaces (M7)" in [SECURITY.md](SECURITY.md)).
- In Gateway API v1.6 ReferenceGrant may be written as `v1` or `v1beta1`.

### 6.3 ListenerSet: listeners from other namespaces

A tenant adds a listener with its own hostname and certificate to a shared Gateway. The Gateway's `allowedListeners` (default `None`) allows it.

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

- A ListenerSet's certificates and `allowedRoutes` `Same` are taken from the ListenerSet's namespace.
- A route attaches to its listeners only with `kind: ListenerSet` in `parentRefs`.
- On a conflict on the same port, the older ListenerSet wins.

Check:

```bash
kubectl -n team-a get listenerset team-a
# NAME     ACCEPTED   PROGRAMMED   AGE
# team-a   True       True         30s
kubectl -n infra get gateway shared -o jsonpath='{.status.attachedListenerSets}'
```

## 7. rproxy's CRDs and other settings

rproxy-gateway's CRDs are `rproxy.max3584.net/v1beta1` (`v1alpha1` works with the same shape but is deprecated).

| CRD | Short name | Use |
|---|---|---|
| `RproxyPolicy` | `rppol` | adds L4 limits and timeouts to a Gateway, listener or Service |
| `RproxyMiddleware` | `rpmw` | rproxy middlewares through `ExtensionRef` in HTTPRoutes and GRPCRoutes |
| `RproxyRule` | `rprule` | adds an rproxy rule as it is to a Gateway |
| `RproxyGatewayParameters` | `rpgwp` | the shape of managed rproxy pods and Services |

### 7.1 RproxyPolicy

`spec.targetRefs` names a Gateway (with `sectionName`, just that listener) or a Service, **in the same namespace**.

| Field | What | Applies to |
|---|---|---|
| `allowFrom` | CIDRs connections are accepted from | the rule |
| `limits` | connection counts and rates (rproxy's `limits`) | the rule |
| `bandwidth` | bandwidth (rproxy's `bandwidth`) | the rule |
| `geoip` | allow by country or ASN (rproxy's `geoip`; rproxy needs a GeoIP database) | the rule |
| `crowdsec` | refuse by CrowdSec's decisions (rproxy needs `global.crowdsec`) | the rule |
| `outlierDetection` | passive health checks (rproxy's `outlier_detection`) | L4 rules, HTTP services |
| `connectTimeout` | time to connect to a backend (100ms-10m) | L4 tcp rules, HTTP services |
| `responseTimeout` | time to wait for a backend's response headers (up to 1h) | HTTP services |
| `idleTimeout` | until an L4 connection carrying nothing is closed (`0s`: none; rproxy's default 1h) | L4 tcp rules |
| `halfCloseTimeout` | until closing after one side has finished sending (`0s`: none; default 1m) | L4 tcp rules |

`limits`, `bandwidth`, `geoip` and `outlierDetection` take rproxy's rule shapes (snake_case) as they are: rproxy-api docs/API.md ("L4 の制限と帯域", "GeoIP", "受け身のヘルスチェック").

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

- Naming a Service applies to the L4 rules sending to it (`outlierDetection`, `connectTimeout` and `responseTimeout` to HTTP services too).
- When two policies set the same field, the older wins.
- With `outlierDetection` set, the chart's `backends.outlierDetection` default is not added. To turn it off: `outlierDetection: {max_ejected_percent: 0}`.
- `idleTimeout` and `halfCloseTimeout` do not go on HTTP/HTTPS rules or UDP (HTTP connections: `rproxy.http` in [7.4](#74-rproxygatewayparameters)).
- Rules with HTTPRoute `timeouts` do not get `responseTimeout` (the route wins).

Check (the status is per Gateway in `status.ancestors[]`):

```bash
kubectl -n default get rproxypolicy postgres-service -o jsonpath='{range .status.ancestors[*]}{.ancestorRef.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# l4 Accepted=True(Accepted) rproxy.max3584.net/RelayTimeouts=True(Applied)
```

- A wrong duration: `Accepted: False` (`Invalid`), and the policy is not used.
- The listener named does not exist: `Accepted: False` (`TargetNotFound`).

### 7.2 RproxyMiddleware

`spec` is one rproxy middleware (`{kind: {settings}}`), used through `ExtensionRef` filters of HTTPRoutes and GRPCRoutes. The kinds and settings are those of `http.middlewares` in rproxy-api docs/API.md ("v0.3 の設定").

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

- The RproxyMiddleware is in the route's namespace. If it is missing the rule answers 500 and the route is `ResolvedRefs: False`.
- A shape or value rproxy does not take makes the route `Accepted: False` (`UnsupportedValue`, the reason in the message); the other routes of the port keep working.
- `rate_limit`, `in_flight` and `circuit_breaker` counts start over whenever the `http` of the port's rproxy rule changes. Backend pod IPs are inside the rule's `http` too (the `servers` of `services`), so this happens not only when any route on the port changes but also whenever backend pods come, go or are replaced (scaling, rollouts).
- backendRef `filters` cannot have `ExtensionRef` (`UnsupportedValue`).
- Fields naming files (`basic_auth`'s `users_file` and others) can name only the Gateway's certificate files ([SECURITY.md](SECURITY.md)). A user list file cannot be given, so authenticate with `forward_auth` or Gateway API's `ExternalAuth`.

**Authentication (`forward_auth`)**: asks an authentication server for each request and lets 2xx through. The service it may name is a rule of the HTTPRoute using it (the rule's `name` or `r<number>`).

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

Asking an outside URL (`forward_auth.address`, `oidc.issuer`) is off by default (so Gateway pods cannot be made to request any URL). Allow it in the GatewayClass's parameters (the chart's `managed.parameters`):

```yaml
# values.yaml
managed:
  parameters:
    policy:
      middlewares:
        outboundHosts: [auth.example.com]       # *, a hostname, *.example.com, an IP range
        # allowedKinds: [headers, rate_limit, forward_auth]   # to limit the kinds
```

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: authelia, namespace: default}
spec:
  forward_auth:
    address: https://auth.example.com/api/verify
    response_headers: [Remote-User, Remote-Groups]
```

Without it the route is `Accepted: False` (`UnsupportedValue`, "auth.example.com is not allowed by the GatewayClass (policy.middlewares.outboundHosts; by default no URL out)").

### 7.3 RproxyRule

`spec.rule` is an rproxy rule as it is (the body of the control API's `POST /rules`). It adds what Gateway API cannot express (forwarding outside the cluster, rproxy's finer settings) to a Gateway's rule set.

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

- The rule's listen port is added to the managed Service too (up to 100 ports of a range).
- For a Gateway of another namespace, that namespace needs a ReferenceGrant (from `group: rproxy.max3584.net, kind: RproxyRule`, to `kind: Gateway`).
- A rule with the same key (protocol, address, port) already in the Gateway: `Accepted: False` (`Conflicted`). Refused by rproxy: `Accepted: False` (`Invalid`), the other rules work.
- RproxyRules are outside the tenant limits ([SECURITY.md](SECURITY.md)): they can name any destination. If you cannot trust their writers like the Gateway's owner, turn them off with the class's `policy.rproxyRules: false`. Fleet mode does not read them by default (`fleet.rproxyRules`).

Check:

```bash
kubectl -n default get rproxyrule
# NAME          GATEWAY   PROGRAMMED
# bastion-ssh   web       True
```

### 7.4 RproxyGatewayParameters

Changes the shape of the managed rproxy pods and Service per Gateway. Write it in the Gateway's namespace and name it in the Gateway's `spec.infrastructure.parametersRef`.

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

Main fields (all of them: 2.2 of [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md)):

| Field | What | Tenants (a Gateway's reference) by default |
|---|---|---|
| `replicas` | pods (1 to `policy.maxReplicas`, default 10) | may set |
| `podDisruptionBudget` | `minAvailable` or `maxUnavailable` | may set |
| `pod.resources.rproxy`, `.certsync` | the containers' resources | may set |
| `pod.labels`, `pod.annotations`, `pod.topologySpreadConstraints` | pod | may set |
| `pod.nodeSelector`, `tolerations`, `affinity`, `priorityClassName` | pod | may not (the class can open them) |
| `service.type` | `LoadBalancer`, `NodePort`, `ClusterIP` | may not (the class can open it) |
| `service.externalTrafficPolicy`, `loadBalancerSourceRanges`, `ipFamilyPolicy`, `labels`, `annotations` | Service | may set (annotations deciding addresses: allow list only) |
| `service.loadBalancerClass` | Service (cannot change after creation) | may not (the class can open it) |
| `rproxy.logLevel`, `performance`, `shutdown` | rproxy's environment | may set |
| `rproxy.http` | limits of HTTP client connections (`http2MaxStreams`, `timeouts`) | may set |
| `rproxy.image`, `rproxy.extraEnv` | rproxy's image, more environment variables (`RPROXY_*`) | may not (the class's reference only) |
| `proxyProtocol` | read PROXY protocol from the load balancer in front ([PLATFORM.md](PLATFORM.md)) | may not (the class can open it) |
| `ui.visible` | show to the management UI | may set (cannot be true when the class says false) |
| `policy` | what tenants may set | may not (the class's reference only) |

**Every Gateway's defaults (the class's reference)**: write them in the chart's `managed.parameters`. The chart creates the `RproxyGatewayParameters` `rproxy-default` (in the release namespace) and names it in the GatewayClass's `parametersRef`. `policy` can be written only here.

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

- With `gatewayOverrides` written, tenants may set only the fields listed. With this example class, the Gateway's reference above (`pod.topologySpreadConstraints`, `service.*`, `rproxy.shutdown`, `rproxy.http`) becomes `InvalidParameters`.
- Merge order: the controller's flags → the class's reference → the Gateway's reference (the later wins). Lists and objects are replaced whole, maps such as labels key by key.
- On a cluster upgraded from `v0.4.1`, apply rproxy's CRDs before writing `managed.parameters` (`helm upgrade` does not install new CRDs; [12.1](#121-upgrades)).

Check:

```bash
kubectl -n default get rpgwp
# NAME   REPLICAS
# web    3
kubectl -n default get deploy -l gateway.networking.k8s.io/gateway-name=web
```

- A field not allowed or a wrong value: the Gateway is `Accepted: False` (`InvalidParameters`), the message names the field (e.g. "spec.service.type: not allowed by the GatewayClass (policy.gatewayOverrides)").
- **A Gateway already running does not stop**: a Gateway with an rproxy Deployment keeps running in its last correct shape (`Programmed: True`). Fix it and the next pass applies the new shape.
- `rproxy.http` shows in the condition `rproxy.max3584.net/HTTPSettings` (`Applied`, `Kept`, `UnsupportedRproxy`).
- In fleet mode a Gateway's reference cannot be used. A Gateway with `parametersRef` is `Accepted: False` (`InvalidParameters`, "not used in fleet mode"), and the rules of a Gateway that is not accepted are not sent to the fleet's pods. Keeping the last shape running is for managed mode only, so in fleet mode its traffic stops (no changes arrive, and restarted pods or pods on new nodes get nothing). Remove `parametersRef` in fleet mode.

### 7.5 Listener filters

With Gateway API v1.7's experimental CRDs, listeners take `filters.requests` (GEP-5224). They run **before** route matching, on every request of the listener, in the order written. They become rproxy v0.4.22's `http.pre_routing`.

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

- Only `ExternalAuth` and `ExtensionRef` (RproxyMiddleware), on `HTTP` and `HTTPS` listeners.
- RproxyMiddleware's `retry`, `mirror` and `replace_host`, settings naming a service (`forward_auth`'s `service` and others) and `headers` changing Host cannot be used.
- Listeners on one port are one rproxy rule, so their filters must be the same on every listener of the port. If not, the listeners with filters are `Accepted: False` (`UnsupportedValue`).
- An unresolved reference (no Service or RproxyMiddleware) makes the listener `ResolvedRefs: False` (`InvalidListenerFilterRef`) and answers 500 in the filter's place (never skipped).
- Until every rproxy pod of the Gateway reports `pre_routing`, the listener is `Accepted: False` (`UnsupportedValue`, "not known (yet)"). The bundled rproxy v0.4.22 has it, so it is normally accepted once the pods start.
- Details: 17. of [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md).

### 7.6 BackendTLSPolicy

rproxy sends to the backend over TLS.

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

- `hostname` is the name for SNI and the certificate check. `subjectAltNames` changes the names checked. For a public CA, `wellKnownCACertificates: System` instead of `caCertificateRefs`.
- `sectionName` in `targetRefs` picks a Service port (a port's policy wins over the whole Service's).
- No usable CA: `Accepted: False` (`NoValidCACertificate`), and that backend's share gets 500.
- To present a client certificate to backends, the Gateway's `spec.tls.backend.clientCertificateRef` (a Secret).

Check: `Status.Ancestors` (per Gateway) of `kubectl -n default describe backendtlspolicy secure-app`.

### 7.7 ClusterTrustBundle and client certificate validation

Kubernetes' ClusterTrustBundle (a cluster-wide CA bundle; `certificates.k8s.io/v1` in Kubernetes 1.37, before that when the beta API is turned on) can be named in `caCertificateRefs`. No ReferenceGrant is needed.

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

- If the API server does not serve ClusterTrustBundle, the reference is `InvalidKind` (the Gateway: `InvalidCACertificateKind`).
- `perPort` in `spec.tls.frontend` changes it per port. `mode: AllowInsecureFallback` connects without a certificate or with a failing one, and passes the result to the backend in the `X-Client-Verify` header.

### 7.8 Address routability

With Gateway API v1.7's experimental CRDs, `spec.addresses[].routability` can be written (GEP-5093). Each `Cluster` entry gets a cluster-only ClusterIP Service in the Gateway's namespace.

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

- With every entry `Cluster`, the Gateway's Service becomes `ClusterIP` too (nothing outside).
- Entries with values (fixed ClusterIPs) are off by default (`AddressesAssigned: False`); the class's `policy.clusterIPs: true` opens them.
- Reordering entries recreates the Services and changes their ClusterIPs. Add at the end.
- Fleet mode does not assign `Cluster`.
- A `type: IPAddress` with a value (a static address) becomes the Service's `externalIPs` in managed mode, only inside the chart's `managed.addressCIDRs`. Details in [PLATFORM.md](PLATFORM.md).

## 8. managed and fleet

| | managed (default) | fleet |
|---|---|---|
| rproxy | a Deployment and Service per Gateway, in the Gateway's namespace | the chart's DaemonSet (`hostNetwork: true`) serves every Gateway |
| Address | the Service's load balancer address (MetalLB, cloud...) | node IPs or `fleet.addresses` (the platform provides VIPs) |
| Keys and tokens | per Gateway | shared by every Gateway |
| Separates tenants | yes | no (for one admin) |
| `RproxyGatewayParameters` | usable | a Gateway's reference is not: the Gateway becomes `InvalidParameters`, its rules are not sent and its traffic stops (the DaemonSet takes the chart's `fleet.*`) |
| RproxyRule | usable | not read by default (`fleet.rproxyRules`) |
| One port for two Gateways | works (separate Services) | the older Gateway holds it; the later listener is `Accepted: False` (`PortUnavailable`). With `fleet.listen: addresses`, different addresses can share it |
| NetworkPolicy | one per Gateway | does not work (protect with a firewall) |

**Which**: normally managed. Fleet when there is no load balancer implementation, when you want to take traffic on node addresses directly, or in a single-admin cluster with fewer pods.

Fleet example:

```yaml
# values.yaml
fleet:
  enabled: true
  addresses: [192.0.2.10]    # the address written to Gateway status (a VIP or the load balancer in front)
  # listen: addresses        # listen per Gateway spec.addresses (inside managed.addressCIDRs)
  # nodeSelector: {node-role.example.com/edge: "true"}
```

Point the health check of the load balancer or VIP in front to `https://<node IP>:9443/readyz` (503 once stopping). How: "fleet" in [PLATFORM.md](PLATFORM.md).

**Switching** (change `fleet.enabled` with `helm upgrade`):

- managed → fleet: the controller deletes the managed Deployments and Services and sends the rules to the fleet's pods. Gateway addresses change (load balancer addresses → node IPs or `fleet.addresses`). A Gateway with `parametersRef` becomes `InvalidParameters`, its rules are not sent and its traffic stops (unlike managed mode, the last shape is not kept running). **Before switching, remove `spec.infrastructure.parametersRef` from every Gateway** (the pods' shape comes from the chart's `fleet.*`). RproxyRules are not used unless `fleet.rproxyRules: true`.
- fleet → managed: helm deletes the DaemonSet and the controller creates a Deployment and Service per Gateway. Addresses become new load balancer addresses.
- Both interrupt traffic and change addresses. Do it in a maintenance window, together with DNS and firewalls.

```bash
helm upgrade rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway -n rproxy-gateway-system -f values.yaml
kubectl get gateway -A        # check that ADDRESS changed
```

## 9. Changing things while they run

On every change the controller renders a Gateway's whole rule set again and sends it to rproxy ([10.](#10-how-it-talks-to-rproxy)). rproxy applies only the difference, in one of three ways ("ルールの組" in rproxy-api docs/API.md).

| How rproxy applies it | Current connections |
|---|---|
| `in_place` (changed where it is) | kept; new connections and requests see the change |
| `recreate` (the listener is opened again) | that port's connections are cut |
| delete (the rule left the rule set) | that port's connections are cut |

### 9.1 Common changes

| Change | What happens |
|---|---|
| Add, change, delete routes (paths, headers, weights, filters) | `in_place`; connections kept (the port's `rate_limit`/`in_flight`/`circuit_breaker` counts start over) |
| Backend pods come and go (scaling, rollouts) | `in_place`; stopping pods get no new requests. On HTTP ports pod IPs are inside the rule's `http`, so the port's `rate_limit`/`in_flight`/`circuit_breaker` counts start over |
| Certificate Secret renewed (cert-manager) | `in_place`; file names are content hashes, so new connections get the new certificate |
| RproxyPolicy (`allowFrom`, `limits`, timeouts...) | `in_place`; from the next connection |
| Add or remove a listener (hostname) on a port in use | `in_place` on that port's rule (other hostnames' connections kept) |
| Change a listener's port | the old port's rule is deleted (its connections cut), the new port's rule created; the managed Service's ports change too |
| Delete a listener (the last on its port) | that port's rule is deleted; connections cut |
| Change a port's protocol (`HTTP` ↔ `TCP`...) | `recreate`; that port's connections cut |
| Delete a Gateway | managed: the Deployment and Service go (the Gateway owns them); every connection is cut and the address goes. fleet: rproxy's rule set is deleted; that Gateway's connections are cut |
| `RproxyGatewayParameters` (pod shape other than replicas), `spec.infrastructure` labels/annotations | a pod rolling update (9.3 below). Replicas only adds or removes pods |
| `rproxy.http` values | no pod replacement (rproxy reads its settings file again; about a minute until the kubelet updates it). Setting it the first time or removing all of it rolls the pods |
| `service.type` | the Service changes and so does its load balancer address |
| Controller upgrade (a new rproxy or certsync image) | a rolling update of every managed Gateway |

### 9.2 Errors show in status

What rproxy refuses is left out and the rest keeps working: one error does not stop the whole set.

| What happened | Where it shows |
|---|---|
| rproxy refused a route | the route's `Accepted: False` (`UnsupportedValue`, rproxy's answer); the other routes of the port work |
| rproxy refused an RproxyRule | the RproxyRule's `Accepted: False` (`Invalid`) |
| rproxy refused the whole rule set (limits...) | the Gateway's and listeners' `Programmed: False` (`Invalid`, "rproxy refused the rule set: ...") |
| certificate files not on the pod yet | `Programmed: False` (`Pending`); `True` once they are |
| a listener cannot be opened (port in use) | the listener's `Programmed: False` |
| wrong references (Service or Secret missing) | the route's `ResolvedRefs: False` (`BackendNotFound`, `RefNotPermitted`), the listener's `ResolvedRefs: False` (`InvalidCertificateRef`) |

What was refused is not sent again until it changes or 5 minutes pass.

### 9.3 Pod replacement and the readiness gate

Managed rproxy pods are replaced without stopping traffic ("rproxy availability (managed)" in [DESIGN.md](DESIGN.md)):

- **Readiness gate** (`rproxy.max3584.net/ruleset-applied`): a new pod becomes Ready, and joins the Service, only after the controller has applied its rule set. No traffic reaches an rproxy without rules.
- **Rolling update**: `maxUnavailable: 0`; old pods stop after new ones are Ready.
- **Stopping**: after SIGTERM, rproxy keeps accepting for `delay` (default 15 s) while `/readyz` answers 503 so it leaves the Service, then waits up to `drain` (default 25 s) for current connections to end. Connections lasting past the drain (long WebSockets, DB connections) are cut. For long connections, raise `rproxy.shutdown.drain`.
- With `replicas` 2 or more there is a PDB (`maxUnavailable: 1`) and spreading over nodes. With one, there are moments during a replacement when the only pod is stopping.

```bash
kubectl -n default get pod -l gateway.networking.k8s.io/gateway-name=web -o wide
# READINESS GATES 1/1: the rule set is on the pod
```

### 9.4 Restarts

| What restarts | What happens |
|---|---|
| The controller | rproxy keeps running with its current rules (traffic does not stop). The new leader reads rproxy's rule sets and sends them again if they differ |
| rproxy's container | rule sets live only in memory, so they are gone. The controller sets the readiness gate back to `False` (out of the Service), waits for `/readyz`, applies the set again and sets it `True` |
| rproxy's pod (deleted, drained) | as in the rolling update of 9.3 |

While the controller is down, changes do not reach rproxy (it keeps running as it is).

### 9.5 Who changes things, and how

Nobody instructs rproxy directly: changing Kubernetes objects is the instruction. The controller watches these kinds (`src/controller/cache.rs`) and, whenever one changes, renders the Gateway's rule set again and sends it.

- Gateway API: GatewayClass, Gateway, HTTPRoute, GRPCRoute, TLSRoute, TCPRoute, UDPRoute, ReferenceGrant, ListenerSet, BackendTLSPolicy
- rproxy's CRDs: RproxyMiddleware, RproxyPolicy, RproxyRule, RproxyGatewayParameters
- Kubernetes: Pod (rproxy pods' status, the readiness gate), Service, EndpointSlice, Secret, ConfigMap (contents read only for the ones referred to), Namespace (`allowedRoutes` selectors), ClusterTrustBundle; for migration, Ingress and Traefik's CRDs

| Trigger | What changes |
|---|---|
| A person runs `kubectl apply` | Gateways, routes, rproxy's CRDs |
| GitOps (Argo CD, Flux) applies what is in Git | the same (no different from `kubectl apply`) |
| Pods scale, fail or roll | EndpointSlice → rproxy's `servers` (automatic; nobody does anything) |
| cert-manager renews a certificate | Secret → certificate files (automatic) |
| Progressive delivery tools (Argo Rollouts, Flagger) change weights | the HTTPRoute's `backendRefs[].weight` |

Progressive delivery tools only edit HTTPRoutes (through their Gateway API plugins), so they should work in principle. This project has not tested them.

A 90 / 10 weight example (what such tools do too):

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

**Who may change what** is Kubernetes RBAC:

- Gateways (ports, certificates, addresses): only infrastructure admins (for example in an `infra` namespace).
- Routes: each team in its own namespace. Which namespaces' routes a Gateway takes is the admin's `allowedRoutes` ([6.1](#61-allowedroutes-which-namespaces-routes-a-gateway-takes)).
- Only the controller holds a token that can write to rproxy's control API (with the chart's `ui.namespace` set, the management UI holds a read-only one). Users, CI and GitOps tools never call rproxy's API; they only write Kubernetes objects.

## 10. How it talks to rproxy

The controller turns each Gateway into one rproxy rule set and sends it through rproxy-api's control API: the same API you use with `curl` on an rproxy VM, with the same rule JSON.

```
PUT https://<rproxy pod IP>:9443/rulesets/k8s/<Gateway namespace>/<Gateway name>
Authorization: Bearer <that Gateway's token>
If-Match: <current etag>
{"generation": <the Gateway's metadata.generation>, "rules": [ ... ]}
```

- One (protocol, address, port) is one rule. Listeners and routes on one port go into one rule.
- Nothing is sent when the content is unchanged. It is also rendered again every `--resync-secs` (default 30 s).

The Gateway `web` and HTTPRoute `app` of 2.2 become this rule set (`rproxy-gateway render -f <file>` renders it without a cluster):

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

- `servers` are the pod IPs of the Service's EndpointSlices with the pod port (the Service's `targetPort`: `:3000` for the 8080 → 3000 Service of 2.1).
- The running controller adds the chart's `backends` defaults (the service's `outlier_detection`, `timeouts.connect: 1s`, `timeouts.response: 30s`) before sending.

**What does not go through the control API** (keys never travel over the API; rproxy's pods do not use the Kubernetes API):

| What | How it gets there |
|---|---|
| Certificates and keys (`certificateRefs`, BackendTLSPolicy CAs...) | the controller puts them in the Secret `rproxy-<id>-certs`; the kubelet mounts it at `/var/run/rproxy-gateway/certs` in the pod. Rules name the files (content hashes) in `cert_file` and the like. `certsync` in the same pod tells the controller when the files are there |
| Token file (`tokens.yaml`, hashes only) and the control API's certificate | the Secret `rproxy-<id>-api` as a volume |
| rproxy's settings file (`config.json`, with `rproxy.http`) | an item of the same `rproxy-<id>-api` and `RPROXY_CONFIG` |
| Log level, performance, shutdown, `extraEnv` | the pod's environment variables |

In fleet mode: certificates in `rproxy-fleet-certs`, the token file in `rproxy-gateway-token`, `fleet.http` in the ConfigMap `rproxy-fleet-config`.

## 11. Security basics

Details in [SECURITY.md](SECURITY.md).

- **Tokens per Gateway**: managed rproxy runs with a per-Gateway control API certificate and token (derived from the master token). The token can write only its own rule set name (`k8s/<namespace>/<name>`). Reading the Secrets of one Gateway's namespace does not reach another Gateway's rproxy. The master token and CA key are only in the controller's namespace (`rproxy-gateway-system`).
- **rproxy's pods do not use the Kubernetes API**: no ServiceAccount token mounted, no RBAC.
- **No Service for the control API**: each Gateway's Service carries only the listener ports (and the listen ports of RproxyRules); no Service is made for the control API (9443) or certsync (9444). The controller PUTs to each pod's `https://<pod IP>:9443` directly. Every pod holds its own rule set, so going through a Service would be wrong: it would reach only one pod. Users do not need to create a Service or NetworkPolicy for 9443 and 9444.
- **NetworkPolicy (managed)**: created automatically per Gateway. The control API (9443) and certsync (9444) only from the controller's pods (with the chart's `ui.namespace` set, also 9443 of the Gateways shown from the UI's pods), listener ports from anyone. It works only with a CNI that handles NetworkPolicies.
- **Fleet and hostNetwork**: NetworkPolicies do not apply. The control API (9443) and certsync (9444) listen on node IPs, so restrict the nodes' 9443 and 9444 with the node firewall. Not only outside hosts but also pods in the cluster reach hostNetwork 9443 and 9444, and NetworkPolicies cannot stop them. Let 9443 through only from the controller and the load balancer's `/readyz` health check, and 9444 (certsync, listening on `0.0.0.0`) only from the controller. The controller usually comes from its pod IP (the source of pod-to-node traffic depends on the CNI), so allow the controller pods' range (or the controller's nodes) and refuse the rest. Fleet is for one trust domain and does not separate tenants.
- **Off by default**: `spec.addresses` (`managed.addressCIDRs`), ExternalName Services, middlewares calling outside URLs (`policy.middlewares.outboundHosts`), load balancer annotations deciding addresses (`serviceAnnotationPrefixes`). Open them only on classes for trusted tenants.
- **Private keys of other namespaces**: used through a ReferenceGrant, the key is copied into the Gateway's namespace in managed mode. If that is a problem, `controller.crossNamespaceSecrets: false`.
- **Narrow the controller's permissions**: `controller.watchNamespaces` and `rbac.admissionPolicy.enabled: true` (Kubernetes 1.30 or later).

## 12. Operations

### 12.1 Upgrades

Upgrading rproxy-gateway:

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

- Each chart version comes with an rproxy version (the chart's `rproxy.image.tag`; chart 0.4.22 has rproxy 0.4.22). When the rproxy image (or the controller image, which runs certsync) changes, every managed Gateway's pods are replaced by a rolling update (9.3; with 2 or more replicas the gap is normally up to about a second, "Availability" in the [README](../../README.en.md)). In fleet mode the DaemonSet rolls.
- Read each version's changes in the release notes (GitHub Releases). Within v0.4.x, current values and Gateways keep working (1. of [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md)).
- Installed with Kustomize or `install.yaml`: `kubectl apply --server-side` the new version's `install.yaml` (CRDs included).
- To upgrade Gateway API's CRDs, `kubectl apply --server-side` the new `standard-install.yaml` (or experimental). The controller looks for new kinds every 30 s; no restart needed. Which Kubernetes versions go with it: "Supported versions" in the [README](../../README.en.md).

### 12.2 Monitoring

**Conditions** come first. To alert, alert on these not being `True`:

| Object | Conditions |
|---|---|
| Gateway | `Accepted`, `Programmed` |
| Listeners (`status.listeners[]`) | `Accepted`, `ResolvedRefs`, `Programmed` |
| Routes (`status.parents[]`) | `Accepted`, `ResolvedRefs` |
| RproxyPolicy, BackendTLSPolicy (`status.ancestors[]`) | `Accepted` |
| RproxyRule | `Accepted`, `Programmed` |
| rproxy conditions on Gateways | `rproxy.max3584.net/HTTPSettings`, `rproxy.max3584.net/ProxyProtocol` (when `False`) |

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

(A listener's `Conflicted` is normally `False`.) With Prometheus, kube-state-metrics' Custom Resource State can turn these conditions into metrics.

**Pods**: Ready and restart counts of the controller Deployment (`rproxy-gateway`) and the rproxy pods. An rproxy pod whose READINESS GATES stays `0/1` has no rule set.

**Logs**: 12.5 and [14.2](#142-controller-logs). rproxy's logs: `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy` (`ruleset.apply`, `conn.*`, `http.access`...; rproxy-api docs/API.md).

**rproxy's `/metrics`**: rproxy exposes Prometheus metrics at `GET /metrics` on the control API (9443), which needs a `metrics:read` token. rproxy-gateway gives that token only to the management UI (the chart's `ui.namespace`), and the NetworkPolicy lets only the UI's pods through. This version has no setup for Prometheus to scrape it directly; view the metrics in the management UI.

### 12.3 Scaling and HA

| What | Value | Details |
|---|---|---|
| Controller | `controller.replicas` (default 2) | One becomes the leader through the Lease `rproxy-gateway` (in the controller's namespace) and applies. If the leader stops, another takes over in about 15 s (at once on SIGTERM). For a single one, `controller.leaderElection: false` |
| rproxy (every Gateway's default) | `managed.replicas` or `managed.parameters.replicas` | 2 or more: a PDB (`maxUnavailable: 1`) and a `kubernetes.io/hostname` topology spread (`ScheduleAnyway`) |
| rproxy (per Gateway) | `RproxyGatewayParameters` `replicas`, `podDisruptionBudget`, `pod.topologySpreadConstraints`, `pod.resources` | tenants up to `policy.maxReplicas` (default 10) |
| fleet | DaemonSet (one per node) | `fleet.nodeSelector` and `tolerations` pick the nodes |

```bash
kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}'   # the leader
kubectl -n default get pdb -l gateway.networking.k8s.io/gateway-name=web
```

- In production, run 2 or more rproxy pods. With one, pod replacements and node drains interrupt traffic.
- Measured gaps per setup: "Availability" in the [README](../../README.en.md) and [PLATFORM.md](PLATFORM.md).
- Fleet rproxy holds up to 1000 rule sets (`RPROXY_MAX_RULESETS`) and 10000 rules (`RPROXY_MAX_RULES`). For more Gateways, raise them with `fleet.extraEnv` (quote the values).

### 12.4 Backup

Rules live only in rproxy's memory; the source of truth is the Kubernetes objects. Restore these:

| What | Needed |
|---|---|
| Gateway API objects (Gateways, routes, ReferenceGrants, ListenerSets, BackendTLSPolicies) | yes |
| rproxy CRD objects (RproxyPolicy, RproxyMiddleware, RproxyRule, RproxyGatewayParameters) | yes |
| Certificate Secrets (named by `certificateRefs`), CA ConfigMaps | yes (cert-manager can issue its own again) |
| The chart's values (`values.yaml`) | yes |
| The controller's Secrets (`rproxy-gateway-ca`, `rproxy-gateway-api-tls`, `rproxy-gateway-token`) | optional. Without them the controller creates new ones and the rproxy pods are replaced once; the UI's discovery Secret is rewritten |
| Per-Gateway `rproxy-<id>-certs`, `rproxy-<id>-api`, rproxy Deployments and Services | no (the controller creates them) |

```bash
kubectl get gateways,httproutes,grpcroutes,tlsroutes,tcproutes,udproutes,referencegrants,listenersets,backendtlspolicies -A -o yaml > gateway-api.yaml
kubectl get rproxypolicies,rproxymiddlewares,rproxyrules,rproxygatewayparameters -A -o yaml > rproxy-crs.yaml
```

Restore after installing Gateway API's CRDs and the chart. With Velero or other per-namespace backups, include the kinds above. On a new cluster, load balancer addresses may change (pin them with load balancer annotations or `spec.addresses`; [PLATFORM.md](PLATFORM.md)).

### 12.5 Log levels

- Controller: the chart's `controller.logFormat` (default `json`). The level is the environment variable `RPROXY_GATEWAY_LOG` (default `info`; `debug` and so on). To raise it for a while: `kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG=debug`. `RPROXY_GATEWAY_LOG` is not a chart value, so `helm upgrade` does not remove it (Helm only changes the fields it wrote). Remove it with `kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG-`.
- rproxy: `RproxyGatewayParameters` `rproxy.logLevel` (`error` to `trace`).

## 13. Recipes

### 13.1 A web app with HTTPS and cert-manager

The app of 2.1 with a Let's Encrypt certificate and an HTTP → HTTPS redirect. Turn on cert-manager's Gateway API support (3.2).

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

1. Apply it. Read `ADDRESS` from `kubectl -n default get gateway web` and point the DNS name `app.example.com` at it.
2. Wait for `kubectl -n default get certificate app-tls` to be `READY True` (until then the `https` listener is `ResolvedRefs: False`).
3. `curl -I http://app.example.com/` gives 301, `curl https://app.example.com/` 200.

### 13.2 TCP to a DB with a long idleTimeout

rproxy closes L4 connections carrying nothing after 1 hour by default. For connections that stay quiet for long, like connection pools, raise `idleTimeout`.

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

- It works once `status.ancestors[].conditions` of `kubectl -n default get rproxypolicy db -o yaml` shows `rproxy.max3584.net/RelayTimeouts: True` (`Applied`).
- If the DB is reachable from outside the cluster, narrow the sources with `allowFrom` or `service.loadBalancerSourceRanges` ([7.4](#74-rproxygatewayparameters)).
- On rproxy pod replacements, connections lasting past `drain` (default 25 s) are cut. Let the app reconnect.

### 13.3 UDP

DNS-like: UDP and TCP on the same port.

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

- The backend Service `dns` lists port 53 for both TCP and UDP.
- The load balancer implementation must support TCP and UDP mixed in one Service.
- With `externalTrafficPolicy: Local` (the `LoadBalancer` default) the client address reaches rproxy only; rproxy uses it for `allowFrom`, `geoip` and logs. The TCP and UDP rules rendered from Gateway routes pass no source to the backend (no `source_ip`), so the backend sees the rproxy pod's IP. To pass the client address on, write the rproxy rule itself in an RproxyRule with `source_ip: proxy_v2` (UDP too) or `proxy_v1` (TCP only); the backend must read PROXY protocol ("ルール" in rproxy-api docs/API.md). RproxyPolicy has no field for passing the source.

### 13.4 SNI passthrough on 443

On the same 443, rproxy decrypts some names and routes them as HTTP, and sends others to the backend undecrypted.

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

- rproxy has one rule on 443 and passes only the SNI `secure.example.com` through undecrypted.
- The `secure-app` pods hold the `secure.example.com` certificate.
- Check: `openssl s_client -connect $ADDR:443 -servername secure.example.com` shows `secure-app`'s certificate, `-servername app.example.com` the `app-tls` one.

### 13.5 Canary by weight

1. Create the Deployment and Service of `app-v2` (the new version).
2. Apply the HTTPRoute of 4.3 with `app: 90`, `app-v2: 10`.
3. Watching status and errors, rewrite the weights and apply again (`90/10` → `50/50` → `0/100`). None of it cuts connections.
4. To roll back, set `app-v2`'s weight to 0.

```bash
# change only the weights (the two backendRefs of rules[0])
kubectl -n default patch httproute app --type=json -p '[
  {"op": "replace", "path": "/spec/rules/0/backendRefs/0/weight", "value": 50},
  {"op": "replace", "path": "/spec/rules/0/backendRefs/1/weight", "value": 50}]'
# see the split (echo answers with its pod name)
for i in $(seq 100); do curl -s -H 'Host: app.example.com' http://$ADDR/ | jq -r .pod; done | sort | uniq -c
```

A header-based canary (only testers reach the new version) is a `headers` match of 4.2.

### 13.6 An auth middleware via ExtensionRef

Sign-in with oauth2-proxy, using `login` (`forward_auth`) and the HTTPRoute of 7.2 as they are.

1. Run oauth2-proxy as the Service `oauth2-proxy` (4180), with the redirect URL `https://app.example.com/oauth2/callback`, `--reverse-proxy` and `--set-xauthrequest` (check oauth2-proxy's documentation).
2. Apply the RproxyMiddleware `login` and the HTTPRoute `app` of 7.2 (the Gateway is `web` of 13.1).
3. Without a sign-in, oauth2-proxy's `/oauth2/auth` answers 401 and that goes back to the client as it is (rproxy returns non-2xx answers as they are). Sign in from `https://app.example.com/oauth2/start`. For sending browsers to the sign-in page automatically, see oauth2-proxy's documentation (its forward auth setup).

- `service` is a rule `name` of the same HTTPRoute (`oauth2` here). Rules of other routes cannot be named.
- For an authentication server outside the cluster, use `address` and allow it in the GatewayClass's `policy.middlewares.outboundHosts` (7.2).
- Gateway API's `ExternalAuth` filter (experimental CRDs) does the same (it becomes rproxy's `forward_auth`).

### 13.7 Listener filters

Authentication and a rate limit on every request of a port, whatever the host or path. Use the YAML of 7.5 as it is (Gateway API v1.7's experimental CRDs).

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# http Accepted=True(Accepted) ResolvedRefs=True(ResolvedRefs) Conflicted=False(NoConflicts) Programmed=True(Programmed)
curl -i -H 'Host: anything.example.com' http://$ADDR/no-such-path
# if the authorization server refuses, its 403 (or such) comes first, even without a route
```

- The same RproxyMiddleware on the listener and in a route's `ExtensionRef` runs twice, separately.
- Deleting the authorization server's Service makes the listener `ResolvedRefs: False` (`InvalidListenerFilterRef`) and it answers 500 (never lets requests through unauthenticated).

## 14. Troubleshooting

### 14.1 Common conditions

| Where | Condition (reason) | Meaning | What to do |
|---|---|---|---|
| GatewayClass | `Accepted: False` (`InvalidParameters`) | the class's parameters (the chart's `managed.parameters`) are wrong | fix the field in the message; only ones in the controller's namespace can be named |
| Gateway | `Programmed: False` (`AddressNotAssigned`) | the Service gets no address | install a load balancer implementation ([PLATFORM.md](PLATFORM.md)) or change `serviceType`; `EXTERNAL-IP` of `kubectl get svc` is `<pending>` |
| Gateway | `Accepted: False` (`InvalidParameters`) | wrong `RproxyGatewayParameters` or fields not allowed | fix the field in the message; a Gateway that ran keeps its last shape (`Programmed: True`) |
| Gateway | `Accepted: False` (`UnsupportedAddress`) | a `spec.addresses` type other than `IPAddress` | use `IPAddress` |
| Gateway | `Programmed: False` (`AddressNotUsable`) | `spec.addresses` outside `managed.addressCIDRs`, or an unusable IP | ask the admin to open the range, fix the address |
| Gateway, listener | `Programmed: False` (`Invalid`, "rproxy refused the rule set: 409 limit ...") | rproxy's count limits | raise `RPROXY_MAX_RULESETS` / `RPROXY_MAX_RULES` ("Where rproxy runs" in [DESIGN.md](DESIGN.md)) |
| Gateway, listener | `Programmed: False` (`Pending`) | waiting to apply (certificate files, pod start) | wait a minute or two; if it lasts, the controller's logs |
| Gateway, listener | `Programmed: False` ("rproxy ... has no rule sets (features.rulesets; rproxy v0.4.0 or later)") | an old rproxy (`rproxy.image`) | go back to the chart's image |
| Listener | `ResolvedRefs: False` (`InvalidCertificateRef`) | the certificate Secret, or its `tls.crt` / `tls.key`, is missing | create the Secret (with cert-manager, the Certificate's status) |
| Listener | `ResolvedRefs: False` (`RefNotPermitted`) | no ReferenceGrant for another namespace's Secret | a ReferenceGrant in the Secret's namespace |
| Listener | `ResolvedRefs: False` (`InvalidRouteKinds`) | `allowedRoutes.kinds` has a kind the protocol cannot take | a kind matching the protocol ([5.3](#53-the-listener-for-each-route)) |
| Listener | `Accepted: False` (`PortUnavailable`) | fleet: an older Gateway holds the port | another port, or `fleet.listen: addresses` |
| Listener | `Accepted: False` (`UnsupportedProtocol`) | an unknown protocol | `HTTP`, `HTTPS`, `TLS`, `TCP`, `UDP` |
| Listener | `Accepted: False` (`UnsupportedValue`) | listener filters cannot be used or do not match on the port | read the message ([7.5](#75-listener-filters)) |
| Route | `Accepted: False` (`NotAllowedByListeners`) | the listener's `allowedRoutes` does not allow the namespace or kind | the Gateway's `allowedRoutes`, `sectionName` |
| Route | `Accepted: False` (`NoMatchingListenerHostname`) | the route's and listener's hostnames do not meet | fix the hostnames |
| Route | `Accepted: False` (`NoMatchingParent`) | the Gateway or listener in `parentRefs` does not exist | fix the name, namespace, `sectionName` |
| Route | `Accepted: False` (`UnsupportedValue`) | a value rproxy does not take, a wrong RproxyMiddleware, an outside URL not allowed | fix the field in the message |
| Route | `ResolvedRefs: False` (`BackendNotFound`) | the Service does not exist | the Service's name and port; that backend's share gets 500 |
| Route | `ResolvedRefs: False` (`RefNotPermitted`) | no ReferenceGrant for another namespace's Service | a ReferenceGrant in the Service's namespace ([6.2](#62-referencegrant-using-another-namespaces-services-and-secrets)) |
| RproxyPolicy | `Accepted: False` (`Invalid`, `TargetNotFound`) | a wrong duration, the named listener missing | fix the value, `sectionName` |
| RproxyRule | `Accepted: False` (`Conflicted`, `Invalid`, `NotAllowed`) | a rule with the same key exists, rproxy refused it, the class turned them off | another port, read the message, `policy.rproxyRules` |
| BackendTLSPolicy | `Accepted: False` (`NoValidCACertificate`, `Conflicted`) | no usable CA, an older policy for the same target | the CA ConfigMap (`ca.crt`), keep one |

Also:

- **404**: the hostname (`Host` header) matches no route. Try with `curl -H 'Host: ...'`.
- **500**: no usable backend (the Service's pods are not ready, `BackendNotFound`, `RefNotPermitted`).
- **502, 504**: cannot connect to the backend, or it answers slowly (`backends.connectTimeout`, `responseTimeout`).
- **No connection (timeout)**: the Service's `EXTERNAL-IP`, firewalls, `loadBalancerSourceRanges`, an RproxyPolicy's `allowFrom`. Whether the rproxy pods are Ready (READINESS GATES).

### 14.2 Controller logs

```bash
# the leader's logs (of the two, only the leader applies)
LEADER=$(kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}')
kubectl -n rproxy-gateway-system logs $LEADER --since=1h
# everything but info, easier to read
kubectl -n rproxy-gateway-system logs $LEADER --since=1h \
  | jq -r 'select(.level != "INFO") | "\(.timestamp) \(.level) \(.fields.message) \(.fields | del(.message) | tostring)"'
```

One JSON object per line (`timestamp`, `level`, `fields.message`, other fields). Common ones:

| message | Meaning |
|---|---|
| `became the leader`, `lost the Lease to another replica` | the leader changed (normal when the controller is replaced) |
| `could not renew the Lease in time; stepping down` | it could not reach the API server and stepped down (API server or etcd slow) |
| `rproxy refused a rule; applying the others` | rproxy refused one rule; also in status |
| `rproxy refused routes; applying the others` | rproxy refused routes and they were left out |
| `rproxy refused the rule set` | the whole set was refused (limits...) |
| `listener port taken by another Gateway` | fleet: two Gateways want the port |
| `rule not running` | a rule is not running in rproxy (cannot open the port, name resolution) |
| `cannot deploy rproxy` | cannot create the Deployment or Service (RBAC, admission, ResourceQuota) |
| `reconcile failed` | one pass failed; it keeps retrying |
| `not served by the API server (CRD not installed); looked for again later` | that kind's CRD is missing; installed, it is found within about 30 s |
| `migration: not converted` | what migration could not convert ([MIGRATION.md](MIGRATION.md)) |

For more, `RPROXY_GATEWAY_LOG=debug` as in 12.5. On rproxy's side `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy`, the file handover `-c certsync`.

### 14.3 Checking without a cluster

`rproxy-gateway render` renders the rproxy rule sets and status from manifests (no cluster, no rproxy). Put the GatewayClass, Services and EndpointSlices in the same file.

```bash
docker run --rm -v $PWD:/w ghcr.io/max3584/rproxy-gateway:0.4.22 render -f /w/manifests.yaml
```

The `conditions` of `listeners` and `routes` in the output show the same reasons as in a cluster (what is asked of rproxy pods, such as listener filters, shows as "not known (yet)").
