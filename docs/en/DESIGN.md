日本語: [../DESIGN.md](../DESIGN.md)

# rproxy-gateway design

The implementation of rproxy-api docs/en/DESIGN-v0.4.md 3. (#28). This page records the decisions on the controller's side.

## Overview

```
Gateway API / CRDs ──watch──▶ rproxy-gateway ──PUT /rulesets/k8s/<ns>/<name>──▶ rproxy (control API, HTTPS + token)
                                    │                                             ▲
                                    └──certificate Secret (<id>-certs)──▶ certsync ──files──┘ (emptyDir in the same pod)
```

- The controller talks to rproxy only through the control API (`GET /capabilities`, `GET /readyz`, `GET` / `PUT` / `DELETE /rulesets/{name}`). rproxy knows nothing of the Kubernetes API.
- One Gateway is one rproxy rule set (`k8s/<Gateway namespace>/<Gateway name>`). One (protocol, address, port) is one rule; listeners on the same port (different host names) merge into one rule.
- One loop renders every Gateway it handles on each change (debounced) and every `--resync-secs` (30 seconds by default). When what it renders is the same as last time and the pod's set still has the etag of the last PUT, nothing is PUT.

## Where rproxy runs (`--mode`)

| mode | rproxy | Address (Gateway `status.addresses`) |
|---|---|---|
| `managed` (default) | The controller creates, in its own namespace, one Deployment and Service per Gateway (`rproxy-<id>`, type `--service-type`, `LoadBalancer` by default), and deletes them when the Gateway goes | The Service's load balancer address (the ClusterIP for type `ClusterIP`) |
| `fleet` | rproxy pods deployed beforehand (e.g. the chart's DaemonSet with `hostNetwork: true`, `--fleet-selector`) serve every Gateway. The controller PUTs the same set to every pod | `--fleet-address`, else the pods' node IPs |

- `<id>` is `<namespace>-<name>` (up to 40 characters) and a 6-digit hash.
- Managed pods run as non-root (65532) and take ports below 1024 through `net.ipv4.ip_unprivileged_port_start=0` (a namespaced, safe sysctl).
- In fleet mode, when two Gateways use the same port, rproxy refuses the later one's rule with `409 already_exists` and that listener's `Programmed` is `False`.

## Control API connection

On its first start the controller creates these Secrets in its namespace (reading them when they exist).

| Secret | Contents | Read by |
|---|---|---|
| `rproxy-gateway-ca` | A CA certificate and key | the controller |
| `rproxy-gateway-api-tls` | rproxy's control API certificate (issued by the CA for `rproxy-api.rproxy-gateway.internal`; pods are reached by IP, so the name is fixed) | rproxy |
| `rproxy-gateway-token` | The controller's token (`token`) and the token file rproxy reads (`tokens.yaml`, only the SHA-256; scopes `rules:read`, `rules:write`, `acme:write`) | `token`: the controller; `tokens.yaml`: rproxy |

rproxy runs with `RPROXY_API_ADDR=0.0.0.0`, `RPROXY_API_PORT=9443`, `RPROXY_TOKEN_FILE` and `RPROXY_TLS_CERT` / `RPROXY_TLS_KEY` (the three rproxy requires for a control API on a non-loopback address).

## Certificates (certsync)

- `certificateRefs` Secrets become files on the rproxy host, referenced with `cert_file` / `key_file` (key material never goes over the control API; rproxy-api design 3.3).
- Files are named by their content's hash (`<first 16 hex digits of sha256>.crt` / `.key`). New content is a new path, which reaches rproxy as a rule change.
- The controller puts each Gateway's certificates into one Secret (`rproxy-<id>-certs`, label `rproxy.max3584.net/certs-for`). `certsync` in the rproxy pod (`rproxy-gateway certsync` of this image) watches it and writes the files into an `emptyDir` shared with rproxy (`/var/run/rproxy-gateway/certs`).
- Before a PUT the controller checks with certsync's `GET /files` that the files are there (until then `Programmed: False`, reason `Pending`). Files no Secret holds any more stay 5 minutes before they are removed (so old rules reading them again do not break).

## Applying (rule sets)

1. The pod's `GET /capabilities` (without `features.rulesets`: `Programmed: False`, "needs rproxy v0.4.0 or later"). Without `features.labels`, `labels` are left out.
2. Wait until `GET /readyz` is ready (not for an rproxy without `features.readyz`).
3. `GET /rulesets/{name}`. When the content and the etag are what was last PUT, nothing happens. Otherwise (the content changed, rproxy restarted and lost the set, someone else changed it) `PUT /rulesets/{name}`: `generation` is the Gateway's `metadata.generation` (or rproxy's, if larger, so a Gateway created again does not get `stale_generation`), `If-Match` is the current etag.
4. Status is written from the `conditions` of the rules in `GET /rulesets/{name}` after the PUT. When a rule is `failed`, the set is PUT again a little later (e.g. a file certsync has just written).

## Mapping (Gateway API → rproxy)

| Gateway API | rproxy |
|---|---|
| Listener `HTTP` | a tcp rule with `http` (plain) |
| Listener `HTTPS` (`tls.mode: Terminate`) | a tcp rule, `tls.mode: terminate` (the certificates of the listeners on the port together; rproxy picks by SNI), `http` |
| HTTPRoute `matches` | the `match` expression: `Host` (`*.example.com` as `**.example.com`), `Path` (Exact), `Path(p) \|\| PathPrefix(p/)` (PathPrefix, matched at `/` boundaries), `PathRegexp(^(?:re)$)`, `Method`, `Header` / `HeaderRegexp`, `Query` / `QueryRegexp` |
| Rule precedence | Gateway API's order (host name specificity → Exact → longest PathPrefix → method → number of header matches → number of query matches → oldest route → namespace/name → order written) as `priority` |
| A listener with a more specific host name on the same port | routes of wildcard and host-less listeners get `!Host(...)` so they do not take that host name (listener isolation) |
| `backendRefs` | the ready pod IPs of the Service's EndpointSlices (not the ClusterIP) as `servers`; `weight` is spread over the pods. ExternalName as the name |
| No usable backend | `respond` (500) |
| `RequestHeaderModifier` / `ResponseHeaderModifier` | `headers` (`set`, `remove`; `add` becomes `set`) |
| `RequestRedirect` | `redirect_regex` (301 / 302; the port follows Gateway API: the scheme's default when the scheme changes, else the listener's port) |
| `URLRewrite` path | `replace_path` (ReplaceFullPath), `replace_path_regex` (ReplacePrefixMatch) |
| `ExtensionRef` (`RproxyMiddleware`) | that middleware (`spec` as it is) |
| `timeouts.backendRequest` (else `request`) | the service's `timeouts.response` |
| Listener `TLS` (`tls.mode: Passthrough`) | a tcp rule, `tls.mode: sni`, `unmatched: reject`; `tls.routes` by TLSRoute host name |
| `HTTPS` and `TLS` (Passthrough) on the same port | `tls.routes` (`passthrough: true`) of the `http` rule: only those names are not decrypted |
| Listener `TLS` (`tls.mode: Terminate`) | a tcp rule, `tls.mode: terminate`, `tls.routes` by TLSRoute host name (those of Passthrough listeners on the same port with `passthrough: true`) |
| TLSRoute destination | a `tls.routes` entry has one destination, so the Service's ClusterIP (kube-proxy spreads over the pods; the first pod for a headless Service). With several backendRefs, the one with the largest weight |
| Listener `TCP` / `UDP` | a tcp / udp rule, `targets` (the pod IPs of all backends of the TCPRoutes / UDPRoutes, weights spread over the pods) |
| A `TLS` / `TCP` / `UDP` listener nothing attaches to | no rule (the port stays closed; the listener is `Programmed: True`) |

### rproxy's CRDs (`rproxy.max3584.net/v1alpha1`)

| CRD | Use |
|---|---|
| `RproxyMiddleware` | `spec` has the shape of rproxy's `http.middlewares.<name>` (e.g. `{rate_limit: {average: 10}}`). Used through an HTTPRoute `ExtensionRef` filter. When it is missing, that rule answers 500 and `ResolvedRefs: False` |
| `RproxyPolicy` | Adds a rule's `limits`, `bandwidth`, `geoip`, `outlierDetection`, `allowFrom`, `crowdsec` to `spec.targetRefs` (a Gateway in the same namespace, a listener of it with `sectionName`, a Service) (GEP-713). A Service means the L4 rules sending to it (and, for `outlierDetection`, the `http` services). When several policies set the same key, the oldest wins. Status in `status.ancestors[]` (a missing listener: `Accepted: False`, `TargetNotFound`) |
| `RproxyRule` | `spec.rule` is a rule verbatim (the body of `POST /rules`), added to the rule set of the `spec.parentRef` Gateway. A Gateway in another namespace needs a ReferenceGrant there (from `rproxy.max3584.net/RproxyRule`, to `Gateway`). When a rule with the same key exists: `Accepted: False` (`Conflicted`). Status copies the rproxy rule's `conditions` |

Not supported (the route gets `Accepted: False`, reason `UnsupportedValue`): `URLRewrite` hostname, `RequestMirror`, the `CORS` filter, filters on a backendRef, redirects with 303 / 307 / 308.

## Status

| Where | Contents |
|---|---|
| GatewayClass | `Accepted`, `SupportedVersion`, `supportedFeatures` |
| Gateway | `addresses`, `Accepted`, `Programmed` (an address, and applied to at least one pod). `observedGeneration` is the Gateway's generation |
| Listeners | `Accepted` (`UnsupportedProtocol`, `ProtocolConflict`, `HostnameConflict`, no certificate), `ResolvedRefs` (`InvalidCertificateRef`, `RefNotPermitted`, `InvalidRouteKinds`), `Conflicted`, `Programmed` (the rproxy rule's `Programmed`), `supportedKinds`, `attachedRoutes` |
| Route `status.parents[]` | `Accepted` (`NotAllowedByListeners`, `NoMatchingListenerHostname`, `NoMatchingParent`, `UnsupportedValue`), `ResolvedRefs` (`BackendNotFound`, `RefNotPermitted`, `InvalidKind`). When the rproxy rule's `Accepted` / `ResolvedRefs` is `False`, that too. Entries of other controllers are kept |

`lastTransitionTime` changes only when the status changes. Nothing is written when the content is the same.
