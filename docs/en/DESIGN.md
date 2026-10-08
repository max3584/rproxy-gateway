日本語: [../DESIGN.md](../DESIGN.md)

# rproxy-gateway design

The implementation of rproxy-api docs/en/DESIGN-v0.4.md 3. (#28). This page records the decisions on the controller's side.

## Overview

```
Gateway API / CRDs ──watch──▶ rproxy-gateway ──PUT /rulesets/k8s/<ns>/<name>──▶ rproxy (control API, HTTPS + token)
                                    │                                             ▲
                                    └──certificate Secret (<id>-certs)──mounted by the kubelet──┘ (rproxy pods do not use the API)
```

- The controller talks to rproxy only through the control API (`GET /capabilities`, `GET /readyz`, `GET` / `PUT` / `DELETE /rulesets/{name}`). rproxy knows nothing of the Kubernetes API.
- One Gateway is one rproxy rule set (`k8s/<Gateway namespace>/<Gateway name>`). One (protocol, address, port) is one rule; listeners on the same port (different host names) merge into one rule.
- One loop renders every Gateway it handles on each change (debounced) and every `--resync-secs` (30 seconds by default). When what it renders is the same as last time and the pod's set still has the etag of the last PUT, nothing is PUT.

## High availability (leader election)

- Several controller replicas can run (the chart runs 2 by default, `controller.replicas`). Every replica keeps its watches, but only the one holding the Lease in the controller's namespace (`coordination.k8s.io/v1`, named `rproxy-gateway`, `--leader-lease`) applies rule sets, writes status and deploys rproxy.
- The leader renews the Lease every 5 seconds; it is held for 15 seconds after the last renewal. Another replica takes a Lease that has no holder or was not renewed for 15 seconds (writes carry the `resourceVersion`, so two replicas cannot both take it).
- A leader that could not renew for 10 seconds steps down by itself (it stops before another replica can take over at 15 seconds), abandoning the pass in progress. On shutdown (SIGTERM) it gives the Lease up, so another replica takes over at once.
- The new leader reads rproxy's current rule sets and PUTs again where the etag differs (`If-Match`). Should two replicas ever write at once, rproxy's `If-Match` and `generation` refuse the stale one.
- To run a single replica, use `--leader-elect=false` (chart: `controller.leaderElection: false`).

## Where rproxy runs (`--mode`)

| mode | rproxy | Address (Gateway `status.addresses`) |
|---|---|---|
| `managed` (default) | The controller creates, in the Gateway's namespace, one Deployment, Service and ServiceAccount per Gateway (`rproxy-<id>`, Service type `--service-type`, `LoadBalancer` by default) and Secrets (certificates `rproxy-<id>-certs`, control API `rproxy-<id>-api`). The Gateway owns them all (`ownerReferences`), so they go when it goes | `spec.addresses` when given, else the Service's load balancer address (the ClusterIP for type `ClusterIP`) |
| `fleet` (for one trust domain: [SECURITY.md](SECURITY.md)) | rproxy pods deployed beforehand (e.g. the chart's DaemonSet with `hostNetwork: true`, `--fleet-selector`) serve every Gateway. The controller PUTs the same set to every pod | `--fleet-address`, else the pods' node IPs |

- `<id>` is `<namespace>-<name>` (up to 40 characters) and a 6-digit hash.
- What managed mode creates carries the Gateway's `spec.infrastructure` `labels` and `annotations` (pods too) and the label `gateway.networking.k8s.io/gateway-name` (the controller's own labels win: they select the pods). The Deployment, Service, PDB and pods can be set per Gateway by the `RproxyGatewayParameters` (table below) that the GatewayClass's `parametersRef` and the Gateway's `spec.infrastructure.parametersRef` name (neither: the flags; the decisions are in [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), 2.). A Gateway naming another kind, a missing one, or setting a field it may not is `Accepted: False` (`InvalidParameters`). A Gateway whose rproxy Deployment exists keeps running in its last good shape when its reference becomes invalid (the Deployment and Service are left alone, its rule set is still applied; `Programmed: True`).
- `spec.addresses`: `IPAddress` only (other types: `Accepted: False`, `UnsupportedAddress`). In managed mode they become the Service's `externalIPs` (off by default: only within `--address-cidr`, never another Service's IP; [SECURITY.md](SECURITY.md)) (kube-proxy sends traffic for those IPs to rproxy). An entry without a value keeps the Service's address. Unspecified, loopback, link-local or multicast IPs, and IPs the cluster does not let the Service take, make `Programmed: False` (`AddressNotUsable`). In fleet mode an address must be one of the fleet's (`--fleet-address`, or the nodes' IPs), else `AddressNotUsable`.
- Managed pods run as non-root (65532) and take ports below 1024 through `net.ipv4.ip_unprivileged_port_start=0` (a namespaced, safe sysctl).
- In fleet mode, when two Gateways use the same port, rproxy refuses the later one's rule with `409 already_exists` and that listener's `Programmed` is `False`.

## rproxy availability (managed)

Keeping traffic flowing while rproxy pods are replaced (deleted, `kubectl rollout restart`, a node drain, an image or certificate update).

- **Readiness gate** (`rproxy.max3584.net/ruleset-applied`): an rproxy pod becomes Ready, and joins the Service's endpoints, only once the controller has applied its rule set to it (the `PUT` went through). `/healthz` alone would send traffic to an rproxy without rules. The controller writes the condition into the pod's `status.conditions` (`pods/status` patch). When the rproxy container restarts (rule sets live in memory) it is set back to `False`, and to `True` once the sets are applied again (the condition's `message` holds the restart count it was set for). A pod that is `True` stays so while a later update waits (for certificate files, say). A set rproxy refused (`Rejected`) counts as done: there is nothing to wait for. The fleet DaemonSet has the same gate (Ready once every Gateway's set is on the pod; its rolling update waits for that).
- **Rolling update**: `maxUnavailable: 0`. An old pod stops only after a new one is Ready (gate included).
- **Stopping on SIGTERM** (rproxy v0.4.1's `features.graceful_shutdown`): a pod marked for deletion turns `ready: false` (`terminating: true`) in its EndpointSlice at once, and the kubelet sends rproxy SIGTERM. rproxy keeps accepting for `RPROXY_SHUTDOWN_DELAY` (`managed.shutdown.delay` / `--shutdown-delay`, 15 s by default) while `/readyz` says `draining` (503). Readiness (`/readyz`) fails, so the endpoint's `serving` turns `false` too: MetalLB moves its announcement to another node, kube-proxy sends new connections to other pods, and cloud load balancers take the node out by its `healthCheckNodePort` (failing once the node's pods are all terminating). Then, for `RPROXY_SHUTDOWN_DRAIN` (`managed.shutdown.drain` / `--shutdown-drain`, 25 s by default), it closes its listeners and lets connections end (stopping sooner once they have). `terminationGracePeriodSeconds` is delay + drain + 5 s (45 by default). Per Gateway: the parameters' `rproxy.shutdown`. No preStop (no shell needed in the image, the same on every Kubernetes version). certsync stops at once on SIGTERM (the controller does not talk to pods being deleted).
  - Chosen by comparing six combinations in the acceptance test (l2-local, l2-cluster, 2 replicas; [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) 6.). Keeping v0.4.1's 15 s preStop and adding a drain left the endpoint `serving` from the moment the listeners closed until the pod was gone (the few seconds connections took to end): deleting the pod on the announcing node with MetalLB L2 + `Local` lost 4–6 s. Only `/readyz` with a delay takes the endpoint out before the listeners close.
  - An rproxy without `features.graceful_shutdown` (an older image through the parameters' `rproxy.image`) stops at once on SIGTERM, so it keeps v0.4.1's shape (a 15 s **preStop**; `terminationGracePeriodSeconds` preStop + 15 s; readiness on `/healthz`; no `RPROXY_SHUTDOWN_*`). On Kubernetes 1.30 and later (the kubelet's `sleep` action on by default) the preStop is `lifecycle.preStop.sleep`, before that `sleep` in the rproxy container (the controller picks by the API server's version at start). The rproxy image the controller ships (`--rproxy-image`'s default) is known to have it; another image gets the new shape once a pod running it answers `graceful_shutdown` in `/capabilities` (its pods roll once more; the controller remembers such images while it runs).
  - **preStop** (`managed.preStopSeconds` / `--pre-stop-secs`): unset by default from 0.4.2 (as in the two shapes above: none for a new rproxy, 15 s for an older one). An install that sets it (`managed.preStopSeconds` written for 0.4.1) keeps that value, as a preStop on every pod (with a new rproxy the delay and drain follow it; the grace period is preStop + delay + drain + 5 s). 0: no preStop.
- **Probes** (`managed.readinessProbe` / `--readiness-probe`, `managed.livenessProbe` / `--liveness-probe`): readiness every 2 s by default, out after 2 failures in a row (a hung rproxy leaves the endpoints in about 4 s); liveness every 5 s, restarted after 3 (kept slow). The form is `periodSeconds=2,timeoutSeconds=1,failureThreshold=2,successThreshold=1,initialDelaySeconds=0` (any of them). The readiness path is `/readyz` (`managed.readinessProbe.path` / `--readiness-path`; also not ready while restoring at start and while stopping) or `/healthz`. Liveness is always `/healthz` (`draining` is not dead).
- **2 or more replicas** (`managed.replicas`): a PodDisruptionBudget per Gateway (`rproxy-<id>`, `maxUnavailable: 1`, `unhealthyPodEvictionPolicy: AlwaysAllow`; owned by the Gateway; deleted when replicas go back to 1) and topologySpreadConstraints on `kubernetes.io/hostname` (`ScheduleAnyway`). A drain does not stop both pods at once, and they normally run on different nodes.
- **externalTrafficPolicy** (`managed.externalTrafficPolicy` / `--external-traffic-policy`):
  - `Local` (the default for `LoadBalancer`): clients' IPs reach rproxy. Only nodes with a ready rproxy pod take traffic; load balancers pick nodes by the Service's `healthCheckNodePort` (answered by kube-proxy; it fails once the node's pods are all terminating). MetalLB L2 announces from one of those nodes.
  - `Cluster`: every node forwards to any ready pod (kube-proxy SNATs: rproxy sees node IPs). Replacing pods changes nothing on the load balancer's side.
  - Empty (default): `Local` for `LoadBalancer`, `Cluster` for `NodePort` (as in v0.4.0). With your own L4 load balancer in front of `NodePort`, use `Local` and let it pick nodes by health checks on the node ports.
  - Kubernetes allocates `healthCheckNodePort` per Service (each Gateway's differs, so it is not a value).
- **allocateLoadBalancerNodePorts** (`managed.allocateLoadBalancerNodePorts`, true by default): false for load balancers that do not use node ports, such as MetalLB.
- The controller PUTs to a Gateway's pods 4 at a time.
- Not chosen: setting a stopping pod's gate to `False` first (while rproxy still answers in its preStop), which also drops the endpoint's `serving`. MetalLB would move sooner, but with `externalTrafficPolicy: Local` kube-proxy drops what still reaches that node (it cannot send it to other nodes' pods), so the gap until the announcement moved got worse (v0.4.1). rproxy v0.4.1's `/readyz` `draining` drops `serving` as well, but measured shorter gaps in the acceptance test (0.2–1.2 s; "Stopping on SIGTERM" above).

The acceptance test (`.github/workflows/acceptance.yml`, `TOPOLOGY` of `scripts/acceptance.sh`) measures the outages per load balancer topology. Results and recommendations: the [README](../../README.en.md), "Availability".

## Control API connection

On its first start the controller creates these Secrets in its namespace (reading them when they exist). The fleet's rproxy uses them.

| Secret | Contents | Read by |
|---|---|---|
| `rproxy-gateway-ca` | A CA certificate and key | the controller (rproxy pods cannot read it) |
| `rproxy-gateway-api-tls` | rproxy's control API certificate (issued by the CA for `rproxy-api.rproxy-gateway.internal`; pods are reached by IP, so the name is fixed) | rproxy (a volume) |
| `rproxy-gateway-token` | The controller's token (`token`) and the token file rproxy reads (`tokens.yaml`, only the SHA-256; scopes `rules:read`, `rules:write`, `acme:write`) | `token`: the controller; `tokens.yaml`: rproxy (a volume with only that key) |

Each managed rproxy uses `rproxy-<id>-api` in its Gateway's namespace: a control API certificate the CA issued for `<id>.rproxy-api.rproxy-gateway.internal`, and the token file (`tokens.yaml`, only the SHA-256) of a token derived from the master token (HMAC-SHA256 keyed with the master token over the Gateway's id). The controller connects to each pod with that name and token. Reading the Secrets of one Gateway's namespace gives no way into other Gateways' rproxy or into the controller (the master token and the CA key stay in the controller's namespace).

With `ui.namespace` (`--ui-namespace`), the token files of Gateways shown to the UI (their parameters' `ui.visible`) also get the UI's read-only token (`rules:read`, `metrics:read`), and the controller writes the Secret `rproxy-ui-discovery` (the pods, the CA certificate, the UI's tokens) in the UI's namespace ([SECURITY.md](SECURITY.md), "Showing Gateways to the UI"; [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), 4.). rproxy v0.4.2 (`features.tokens_reload`) reads a changed token file again, so the UI's token is left out of the hash on the pod template and adding or removing it does not roll the pods (a pod is listed in the discovery Secret once it is seen to take the UI's token). Older rproxy (an older image in the parameters' `rproxy.image`) reads its token file only at start and on SIGHUP, so the token is part of the hash and the pods roll once.

rproxy also gets `RPROXY_FILES_TRUSTED_DIRS` (the Secret volumes' directories, whose files the kubelet makes root's; [SECURITY.md](SECURITY.md)).

rproxy runs with `RPROXY_API_ADDR=0.0.0.0`, `RPROXY_API_PORT=9443`, `RPROXY_TOKEN_FILE` and `RPROXY_TLS_CERT` / `RPROXY_TLS_KEY` (the three rproxy requires for a control API on a non-loopback address).

## Certificates (a mounted Secret, certsync)

- `certificateRefs` Secrets become files on the rproxy host, referenced with `cert_file` / `key_file` (key material never goes over the control API; rproxy-api design 3.3).
- Files are named by their content's hash (`<first 16 hex digits of sha256>.crt` / `.key`). New content is a new path, which reaches rproxy as a rule change.
- The controller puts each Gateway's certificates into one Secret (`rproxy-<id>-certs`), which the kubelet mounts into the rproxy pod as a Secret volume (`/var/run/rproxy-gateway/certs`, read-only, mode 0440). In fleet mode every Gateway's certificates go into one Secret (`rproxy-fleet-certs`) mounted into the DaemonSet's pods (a Secret holds up to 1 MiB).
- rproxy pods do not use the Kubernetes API: no ServiceAccount token is mounted (`automountServiceAccountToken: false`) and they have no RBAC. Only the referenced certificates reach the pod, through the Secret the controller writes; other Secrets of the namespace (the CA key, the controller's token, other Gateways' certificates) cannot be read.
- When it changes the Secret, the controller sets an annotation on the pods (`rproxy.max3584.net/certs`, the content's hash). The kubelet handles the pod update by refreshing the volume at once (without the annotation, its periodic sync, about a minute, refreshes it).
- `certsync` in the same pod (`rproxy-gateway certsync` of this image) only answers `POST /files` with which of the names the controller sends (content hashes) are in that directory (it never lists them, does not use the API, and listens on the pod's IP). Before a PUT the controller checks that the files are there (until then `Programmed: False`, reason `Pending`).
- Files no rule uses any more stay in the Secret for 5 minutes before they are dropped (so old rules reading them again do not break).

### Designs not chosen

| Design | Why not |
|---|---|
| certsync watching Secrets (the previous design) | The rproxy pod needs to read every Secret of the namespace; `resourceNames` cannot narrow watch and list by name |
| Every Gateway's rproxy in one namespace (the previous design) | The CA key and the master token sit next to the rproxy pods, and every Gateway's control API opens with the same certificate and token. It does not fit Gateway API's `infrastructure` either (objects made in the Gateway's namespace) |
| Sending keys over the control API | Against rproxy-api design 3.3 (key material never crosses the network) |

## Applying (rule sets)

1. The pod's `GET /capabilities` (without `features.rulesets`: `Programmed: False`, "needs rproxy v0.4.0 or later"). Without `features.labels`, `labels` are left out.
2. Wait until `GET /readyz` is ready (not for an rproxy without `features.readyz`).
3. `GET /rulesets/{name}`. When the content and the etag are what was last PUT, nothing happens. Otherwise (the content changed, rproxy restarted and lost the set, someone else changed it) `PUT /rulesets/{name}`: `generation` is the Gateway's `metadata.generation` (or rproxy's, if larger, so a Gateway created again does not get `stale_generation`), `If-Match` is the current etag.
4. When rproxy refuses one rule (`400`, `rules[i]: ...`), that rule is left out and the rest is PUT again (one mistake in an RproxyRule or a migrated route does not stop the whole set). The refused rule is reported as `Accepted: False` (`Invalid`).
5. Status is written from the `conditions` of the rules in `GET /rulesets/{name}` after the PUT. When a rule is `failed`, the set is PUT again a little later (e.g. a file the kubelet has just written).

## Mapping (Gateway API → rproxy)

| Gateway API | rproxy |
|---|---|
| Listener `HTTP` | a tcp rule with `http` (plain) |
| Listener `HTTPS` (`tls.mode: Terminate`) | a tcp rule, `tls.mode: terminate` (the certificates of the listeners on the port together; rproxy picks by SNI), `http` |
| HTTPRoute `matches` | the `match` expression: `Host` (`*.example.com` as `**.example.com`), `Path` (Exact), `Path(p) \|\| PathPrefix(p/)` (PathPrefix, matched at `/` boundaries), `PathRegexp(^(?:re)$)`, `Method`, `Header` / `HeaderRegexp`, `Query` / `QueryRegexp` |
| Rule precedence | Gateway API's order (host name specificity → Exact → longest PathPrefix → method → number of header matches → number of query matches → oldest route → namespace/name → order written) as `priority` |
| A listener with a more specific host name on the same port | routes of wildcard and host-less listeners get `!Host(...)` so they do not take that host name (listener isolation) |
| `backendRefs` | the ready pod IPs of the Service's EndpointSlices (not the ClusterIP) as `servers`; `weight` is spread over the pods. ExternalName as the name (off by default: `--allow-external-name-services`) |
| No usable backend | `respond` (500) |
| Some backendRefs invalid | rproxy answers that backendRef's weight share with 500 (`servers[]` with `status: 500`) |
| `appProtocol: kubernetes.io/h2c` on the backend Service's port | the service's `protocol: h2c` (HTTP/2 with prior knowledge to the backend) |
| `RequestHeaderModifier` / `ResponseHeaderModifier` | `headers` (`set`, `add`, `remove`; `add` appends to a value already there with `,`) |
| `RequestRedirect` | `redirect_regex` (`status` 301, 302, 303, 307 or 308; the port follows Gateway API: the scheme's default when the scheme changes, else the listener's port) |
| `URLRewrite` | hostname: `replace_host`; path: `replace_path` (ReplaceFullPath), `replace_path_regex` (ReplacePrefixMatch) |
| `CORS` | `cors` (`allow_origins`, `allow_methods`, `allow_headers`, `expose_headers`, `allow_credentials`, `max_age`; `maxAge` defaults to 5) |
| `RequestMirror` | `mirror` (a service of the mirror backend's pod IPs; `percent` / `fraction`). A mirror backend that does not resolve: `ResolvedRefs: False`, and only the mirror is left out |
| `filters` of a backendRef | that backend's `servers[].middlewares` (`RequestHeaderModifier`, `ResponseHeaderModifier`, `URLRewrite`; ReplacePrefixMatch only when the rule has one path prefix) |
| `retry` | `retry` (`attempts` is Gateway API's count + 1, `codes` become `status`, `backoff` `initial_interval`), the last middleware |
| `ExtensionRef` (`RproxyMiddleware`) | that middleware (`spec` as it is) |
| `timeouts.request` / `timeouts.backendRequest` | the route's `timeouts.request` / `timeouts.backend_request` |
| GRPCRoute | Into the same `http` rule as HTTPRoutes on the port. A method match becomes a path match (`service` and `method`: `Path(/<service>/<method>)`; `service` only: `PathPrefix(/<service>/)`; `method` only or `RegularExpression`: `PathRegexp`). Header matches, filters (`RequestHeaderModifier`, `ResponseHeaderModifier`, `RequestMirror`, `ExtensionRef`) and backendRef filters as for HTTPRoute. h2c to the backends (the service's `protocol: h2c`). rproxy names start with `grpc:` |
| BackendTLSPolicy | `servers` to the target Service (its port with `sectionName`) become `https://`, with the service's `tls` (`server_name` from `hostname`, `ca_file` from the `ca.crt` of the `caCertificateRefs` ConfigMaps, rproxy's default roots for `wellKnownCACertificates: System`, `subject_alt_names`). Of policies with the same target the oldest wins, the others get `Accepted: False` (`Conflicted`); one for a port wins over one for the whole Service. Without a usable CA: `Accepted: False` (`NoValidCACertificate`; `ResolvedRefs` `InvalidKind` / `InvalidCACertificateRef`) and that backend's share is answered with 500. Status in `status.ancestors[]` (the Gateways with routes to that Service) |
| The Gateway's `spec.tls.backend.clientCertificateRef` | `cert_file` / `key_file` of the BackendTLSPolicy services' `tls`; the Gateway's `ResolvedRefs` (`InvalidClientCertificateRef`, `RefNotPermitted`) |
| Listener `TLS` (`tls.mode: Passthrough`) | a tcp rule, `tls.mode: sni`, `unmatched: reject`; `tls.routes` by TLSRoute host name |
| `HTTPS` and `TLS` (Passthrough) on the same port | `tls.routes` (`passthrough: true`) of the `http` rule: only those names are not decrypted |
| Listener `TLS` (`tls.mode: Terminate`) | a tcp rule, `tls.mode: terminate`, `tls.routes` by TLSRoute host name (those of Passthrough listeners on the same port with `passthrough: true`) |
| TLSRoute destination | `tls.routes[].targets`: the pod IPs of all backends (weights spread over the pods) |
| Listener `TCP` / `UDP` | a tcp / udp rule, `targets` (the pod IPs of all backends of the TCPRoutes / UDPRoutes, weights spread over the pods) |
| A `TLS` / `TCP` / `UDP` listener nothing attaches to | no rule (the listener is `Programmed: True`; the Service has the port) |
| A TLSRoute without a usable backend | its names go to `127.0.0.1:1` (accepted, then closed: Gateway API expects a reset, not a refused connection) |
| Several routes on one TCP / UDP listener | all `Accepted`, the traffic goes to the oldest |
| ListenerSet | The listeners of the ListenerSets the Gateway's `allowedListeners` allows (`None` by default) are added after the Gateway's own (oldest first, then namespace/name; on a conflict on the same port the earlier one wins). Certificate references and `allowedRoutes` `Same` are relative to the ListenerSet's namespace (a Secret in another namespace needs a ReferenceGrant from `ListenerSet`). A route attaches to them only through a parentRef with `kind: ListenerSet`. The ListenerSet's status: `Accepted` (`NotAllowed`; `ListenersNotValid` without a valid listener; `ParentNotAccepted` when the Gateway is not accepted), `Programmed`, `listeners`. The Gateway's `status.attachedListenerSets` counts the accepted ones |
| The Gateway's `spec.tls.frontend` (client certificate validation) | `tls.client_auth` of the port's rule (`mode: required`, `ca_file` a file of the `ca.crt` of the `caCertificateRefs` ConfigMaps). `perPort` for the port, else `default`. Kinds other than ConfigMap: `InvalidCACertificateKind`; missing or without `ca.crt`: `InvalidCACertificateRef`; another namespace needs a ReferenceGrant (to `ConfigMap`, else `RefNotPermitted`). With no usable one the listener is `Accepted: False` (`NoValidCACertificate`). `AllowInsecureFallback` becomes `mode: optional_no_verify` (certificates are asked for and checked, connections are accepted without one or with one that fails; the result reaches the backend in `X-Client-Verify: SUCCESS / FAILED / NONE` and `X-Forwarded-Client-Cert`), and the Gateway gets `InsecureFrontendValidationMode: True`. An rproxy without that mode (`features.client_auth_modes`) does not ask for certificates |
| An `HTTPS` listener without a usable certificate | routes attach (counted in `attachedRoutes`), no rule is made (`ResolvedRefs: False`, `Programmed: False`) |

### rproxy's CRDs (`rproxy.max3584.net/v1alpha1`)

| CRD | Use |
|---|---|
| `RproxyMiddleware` | `spec` has the shape of rproxy's `http.middlewares.<name>` (e.g. `{rate_limit: {average: 10}}`). Used through an HTTPRoute `ExtensionRef` filter. When it is missing, that rule answers 500 and `ResolvedRefs: False` |
| `RproxyPolicy` | Adds a rule's `limits`, `bandwidth`, `geoip`, `outlierDetection`, `allowFrom`, `crowdsec` to `spec.targetRefs` (a Gateway in the same namespace, a listener of it with `sectionName`, a Service) (GEP-713). A Service means the L4 rules sending to it (and, for `outlierDetection`, the `http` services). When several policies set the same key, the oldest wins. Status in `status.ancestors[]` (a missing listener: `Accepted: False`, `TargetNotFound`) |
| `RproxyGatewayParameters` | how a managed Gateway's rproxy is made (replicas, PDB; pod labels, annotations, resources, topology spread, nodeSelector, tolerations, affinity, priorityClass; the Service's type, externalTrafficPolicy, loadBalancerClass, source ranges, ipFamilyPolicy, labels, annotations; rproxy's image, logLevel, performance, extra environment variables). Named by the GatewayClass's `parametersRef` (in the controller's namespace; its `policy` decides what Gateways may set) and the Gateway's `infrastructure.parametersRef` (same namespace). Merging, validation and who decides what: [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), 2. |
| `RproxyRule` | `spec.rule` is a rule verbatim (the body of `POST /rules`), added to the rule set of the `spec.parentRef` Gateway. A Gateway in another namespace needs a ReferenceGrant there (from `rproxy.max3584.net/RproxyRule`, to `Gateway`). When a rule with the same key exists: `Accepted: False` (`Conflicted`). Status copies the rproxy rule's `conditions` |

Not supported (the route gets `Accepted: False`, reason `UnsupportedValue`): `RequestMirror`, `CORS` and `RequestRedirect` filters on a backendRef, `ExternalAuth`.

### rproxy features (`features`)

The newer settings above (`add` of `headers`, a redirect's `status`, route `timeouts`, `replace_host`, `servers[].middlewares`, `cors`, `status` of `retry`, `mirror`, a service's `protocol` and `tls`, `tls.routes[].targets`, `servers[].status`) come with rproxy v0.4.0 (rproxy-api docs/en/API.md, "L7 and TLS for Gateway API"). The controller reads `features` of `GET /capabilities` from the Gateway's rproxy pods and uses only what every pod has.

- A route that needs a missing setting gets `Accepted: False` (`UnsupportedValue`, naming the missing `features`). Other routes keep working.
- What the older shapes can do is done with them: `timeouts.backendRequest` becomes the service's `timeouts.response`, a TLSRoute goes to the Service's ClusterIP (of the backendRef with the largest weight), partly invalid backendRefs use only the valid ones.
- Pods not asked yet (just created) are rendered for all of v0.4.0 and corrected on the next pass after they are asked.

## Status

| Where | Contents |
|---|---|
| GatewayClass | `Accepted` (`InvalidParameters` for an invalid `parametersRef`), `SupportedVersion`, `supportedFeatures` |
| Gateway | `addresses`, `Accepted` (`UnsupportedAddress`, `InvalidParameters`, `ListenersNotValid`), `Programmed` (an address, and applied to at least one pod; `AddressNotUsable`), `ResolvedRefs` (with `tls.backend`), `InsecureFrontendValidationMode`, `attachedListenerSets`. `observedGeneration` is the Gateway's generation |
| ListenerSet | `Accepted`, `Programmed`, `listeners` (as the Gateway's listeners) |
| BackendTLSPolicy, RproxyPolicy | `status.ancestors[]` (per Gateway; other controllers' entries are kept) |
| Ingress (migration) | `status.loadBalancer.ingress` (the target Gateway's addresses) |
| Listeners | `Accepted` (`UnsupportedProtocol`, `ProtocolConflict`, `HostnameConflict`, no certificate), `ResolvedRefs` (`InvalidCertificateRef`, `RefNotPermitted`, `InvalidRouteKinds`), `Conflicted`, `Programmed` (the rproxy rule's `Programmed`), `supportedKinds`, `attachedRoutes` |
| Route `status.parents[]` | `Accepted` (`NotAllowedByListeners`, `NoMatchingListenerHostname`, `NoMatchingParent`, `UnsupportedValue`), `ResolvedRefs` (`BackendNotFound`, `RefNotPermitted`, `InvalidKind`). When the rproxy rule's `Accepted` / `ResolvedRefs` is `False`, that too. Entries of other controllers are kept |

`lastTransitionTime` changes only when the status changes. Nothing is written when the content is the same.
