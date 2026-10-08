日本語: [../DESIGN-v0.4.x.md](../DESIGN-v0.4.x.md)

# v0.4.x design: running on Kubernetes

Fills the gaps that the v0.4.0 and v0.4.1 acceptance tests found in running on Kubernetes. Of the design decided together with rproxy-api and the UI (approved by the owner), this page holds rproxy-gateway's parts: A, the gateway side of B, C and E, and F. rproxy's certificate API (rproxy-api #240) and rule set persistence (#241) are in rproxy-api's design; rproxy-gateway does not use them (5.).

The current decisions: [DESIGN.md](DESIGN.md); the tenant boundaries: [SECURITY.md](SECURITY.md).

## 1. Principles

| Item | Decision |
|---|---|
| Goal | managed rproxy cannot be set per Gateway, installing without Helm is awkward, the UI cannot run on Kubernetes, rproxy stops at once on SIGTERM, there is no VIP without a Service |
| Versions | **There is no v0.5.0.** Everything ships as patches of the v0.4 line (v0.4.2, v0.4.3, ...). Patches may add CRDs and values, but must not break existing installs (chart values, flags, Gateways) |
| Compatibility | Everything added can be left out; left out, the pods and Services are those of v0.4.1 |
| Boundaries | The lines drawn in the v0.4.0 security review (tenants stay in their namespace, the allow list of load balancer annotations, rproxy pods do not use the Kubernetes API, keys never cross the control API) hold. An item that crosses one is opened explicitly by the administrator |
| New rproxy endpoints | Told apart by `features` of `GET /capabilities` (the controller keeps working with older rproxy) |

### Items

| Item | What rproxy-gateway does | Version |
|---|---|---|
| A. managed rproxy per Gateway | the `RproxyGatewayParameters` CRD, merging, status, RBAC | v0.4.2 |
| B. Install with Kustomize | the chart's controller settings in a ConfigMap, `config/`, `install.yaml` on releases, CI | v0.4.2 |
| C. The UI on Kubernetes | read-only tokens for the UI and a discovery Secret (the UI's chart is in the UI's repository) | a later patch |
| E. Stopping on SIGTERM | passing rproxy's `RPROXY_SHUTDOWN_*`, the grace period | v0.4.2 (rproxy v0.4.1) |
| F. VIPs held by pods | the fleet's `vip` sidecar, Leases, status | a later patch |

### Done in v0.4.1

v0.4.1 (#32) brought the availability of managed pods and Services. A and E build on it and do not redo it.

| In v0.4.1 | In A and E |
|---|---|
| The readiness gate `rproxy.max3584.net/ruleset-applied` | kept |
| preStop (`--pre-stop-secs`, 15 s by default), `terminationGracePeriodSeconds` = preStop + 15 | only for rproxy without `features.graceful_shutdown`; with it, 6.'s shape (no preStop, delay + drain + 5) |
| With 2 or more replicas, a PDB (`maxUnavailable: 1`) and a `kubernetes.io/hostname` topology spread | the default stays; A's `podDisruptionBudget` and `pod.topologySpreadConstraints` change it |
| Probe timing (`--readiness-probe`, `--liveness-probe`) | kept; the readiness path is `/readyz` for rproxy with `features.graceful_shutdown` (6.3) |
| `externalTrafficPolicy` (`Local` for `LoadBalancer`, `Cluster` for `NodePort`) | the default stays (making NodePort `Local` is not taken: in the v0.4.1 acceptance test, NodePort behind one's own L4 balancer had shorter outages with `Cluster`); A's `service.externalTrafficPolicy` changes it |

## 2. A. managed rproxy per Gateway

The controller creates the managed Deployments and Services at run time, so neither Helm values nor Kustomize reach them. Today only flags change them (`--replicas`, `--service-type`, ...), for every Gateway at once. A Gateway with `spec.infrastructure.parametersRef` is `Accepted: False` (`InvalidParameters`).

### 2.1 Shape

A CRD `RproxyGatewayParameters` (`rproxy.max3584.net/v1alpha1`, namespaced, shortname `rpgwp`), named both by a GatewayClass's `spec.parametersRef` (the class's defaults) and by a Gateway's `spec.infrastructure.parametersRef` (that Gateway's overrides).

```yaml
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyGatewayParameters
metadata: {name: web, namespace: team-a}
spec:
  replicas: 3
  podDisruptionBudget: {maxUnavailable: 1}
  pod:
    labels: {cost-center: a}
    annotations: {prometheus.io/scrape: "true"}
    resources:
      rproxy: {requests: {cpu: 500m, memory: 128Mi}, limits: {memory: 512Mi}}
      certsync: {requests: {cpu: 5m, memory: 16Mi}}
    topologySpreadConstraints:          # without a labelSelector, one selecting the Gateway's pods is filled in
      - {maxSkew: 1, topologyKey: topology.kubernetes.io/zone, whenUnsatisfiable: ScheduleAnyway}
    nodeSelector: {}                    # the class only, by default (2.5)
    tolerations: []
    affinity: {}
    priorityClassName: ""
  service:
    type: LoadBalancer
    externalTrafficPolicy: Local
    loadBalancerClass: ""
    loadBalancerSourceRanges: [203.0.113.0/24]
    ipFamilyPolicy: PreferDualStack
    labels: {}
    annotations: {}                     # from a Gateway's reference, load balancer annotations only through the allow list (--service-annotation-prefix)
  rproxy:
    image: ""                           # the class only
    logLevel: info
    performance: {workers: 4, udpShards: auto, cpuAffinity: none, busyPollUsecs: 0, splice: {enabled: true}}
    shutdown: {delay: 15s, drain: 25s}  # E (6.)
    extraEnv: []                        # the class only; names the controller sets are refused
  ui: {visible: true}                   # C: shown to the UI (a Gateway can only turn the class's default to false)
  policy:                               # only in the class's reference (InvalidParameters from a Gateway)
    gatewayOverrides: [replicas, podDisruptionBudget, pod.labels, pod.annotations, pod.resources,
      pod.topologySpreadConstraints, service.externalTrafficPolicy, service.loadBalancerSourceRanges,
      service.ipFamilyPolicy, service.labels, service.annotations, rproxy.logLevel, rproxy.performance,
      rproxy.shutdown, ui]
    maxReplicas: 10
    allowedPriorityClasses: []
    allowedLoadBalancerClasses: []
```

### 2.2 Fields

The default column applies when neither reference sets the field; each is the v0.4.1 flag's value.

| Field | Goes to | Default | Validation |
|---|---|---|---|
| `replicas` | the Deployment's `replicas` | `--replicas` (the chart's `managed.replicas`, 1) | 1 to `policy.maxReplicas` (10 by default). Not 0 (delete the Gateway to stop it) |
| `podDisruptionBudget` | the PDB `rproxy-<id>` (one of `minAvailable`, `maxUnavailable`) | `maxUnavailable: 1` with 2 or more replicas, none with 1 (as in v0.4.1) | one of them (CEL in the CRD). `minAvailable` of replicas or more (`100%`) or `maxUnavailable` 0 (`0%`) is refused (a drain would block) |
| `pod.labels`, `pod.annotations` | the pod template | none | keys with the controller's prefixes (`rproxy.max3584.net/`, `app.kubernetes.io/`, `gateway.networking.k8s.io/`) are refused |
| `pod.resources.rproxy`, `.certsync` | each container | none (as in v0.4.1) | Kubernetes ResourceRequirements |
| `pod.topologySpreadConstraints` | the pod | with 2 or more replicas, `kubernetes.io/hostname`, `ScheduleAnyway` (as in v0.4.1) | replaces the default when set. Without `labelSelector`, the Gateway's pod selector is filled in |
| `pod.nodeSelector`, `tolerations`, `affinity` | the pod | none | Kubernetes shapes |
| `pod.priorityClassName` | the pod | none | from a Gateway's reference, one of `policy.allowedPriorityClasses` |
| `service.type` | the Service | `--service-type` (`LoadBalancer`) | `LoadBalancer`, `NodePort`, `ClusterIP` |
| `service.externalTrafficPolicy` | the Service | `--external-traffic-policy`, else `Local` for `LoadBalancer` and `Cluster` for `NodePort` (as in v0.4.1) | `Local`, `Cluster` |
| `service.loadBalancerClass` | the Service | none | from a Gateway's reference, one of `policy.allowedLoadBalancerClasses`. Cannot change once made (2.4) |
| `service.loadBalancerSourceRanges`, `ipFamilyPolicy` | the Service | none | CIDRs; `SingleStack`, `PreferDualStack`, `RequireDualStack` |
| `service.labels`, `service.annotations` | the Service | none | labels as `pod.labels`. Annotations from a Gateway's reference go through the allow list of `spec.infrastructure.annotations` (2.5) |
| `rproxy.image` | the rproxy container | `--rproxy-image` | an image reference (`repo:tag` or `repo@sha256:...`) |
| `rproxy.logLevel` | `RPROXY_LOG_LEVEL` | none | `error`, `warn`, `info`, `debug`, `trace` |
| `rproxy.performance` | `RPROXY_WORKERS`, `RPROXY_UDP_SHARDS`, `RPROXY_CPU_AFFINITY`, `RPROXY_BUSY_POLL_USECS`, `RPROXY_SPLICE`, `RPROXY_SPLICE_AFTER`, `RPROXY_SPLICE_FULL_READS`, `RPROXY_SPLICE_PIPE_SIZE` | none | the ranges of rproxy's `global.performance` (managed rproxy has no settings file, so environment variables) |
| `rproxy.shutdown.delay`, `.drain` | `RPROXY_SHUTDOWN_DELAY`, `RPROXY_SHUTDOWN_DRAIN` and the grace period (6.) | `--shutdown-delay` (15 s), `--shutdown-drain` (25 s) | `0s` to `10m`. Not put on rproxy without `features.graceful_shutdown` (it keeps the preStop) |
| `rproxy.extraEnv` | the rproxy container | none | `RPROXY_*` only. Names the controller sets (`RPROXY_API_*`, `RPROXY_TOKEN_FILE`, `RPROXY_TLS_*`, `RPROXY_FILES_*`, `RPROXY_CONFIG`, `RPROXY_DATABASE_URL`, `RPROXY_UPDATE*`, `RPROXY_HANDOFF*`, `RPROXY_STATIC_RULES`, `RPROXY_SHUTDOWN_*`, and those of the fields above) are refused |
| `ui.visible` | whether C's discovery Secret lists the Gateway | the class's value, else `true` | a Gateway cannot make it `true` when the class says `false`. Goes nowhere until C |

### 2.3 References and merging

- Order (later wins): the controller's flags → the GatewayClass's reference → the Gateway's reference → the Gateway's `spec.infrastructure.labels` and `annotations` → the controller's labels (the selector).
- Merging: scalars replace. Maps (`labels`, `annotations`, `nodeSelector`) merge by key. Lists (`tolerations`, `topologySpreadConstraints`, `loadBalancerSourceRanges`, `extraEnv`) and objects (`affinity`, `resources.rproxy`, `podDisruptionBudget`, `performance`, ...) replace as a whole (as in Gateway API's GEP-1867: no partial merge rules to learn).
- A GatewayClass's `parametersRef`: `group: rproxy.max3584.net`, `kind: RproxyGatewayParameters`, `namespace` **only the controller's namespace** (anything else: the GatewayClass is `Accepted: False`, `InvalidParameters`). The class's reference belongs to the administrator and may have `policy`.
- A Gateway's `infrastructure.parametersRef`: by Gateway API, in the same namespace (`LocalParametersReference`). Other namespaces cannot be named (not even with a ReferenceGrant).
- When a referenced object changes, its GatewayClass and Gateways are rendered again (watched). A changed pod template rolls the pods (`maxUnavailable: 0`, the readiness gate and preStop keep traffic flowing).
- fleet: a Gateway's reference is `Accepted: False` (`InvalidParameters`, "not used in fleet mode"). A class's reference is validated but does not touch the fleet's DaemonSet (the chart or Kustomize does: B).

### 2.4 Validation and status

- The CRD's OpenAPI schema and CEL refuse shapes first (one of `minAvailable` and `maxUnavailable`, enumerations, the minimum of `replicas`). Fields of Kubernetes shapes (`resources`, `tolerations`, `affinity`, `topologySpreadConstraints`, `extraEnv`) are not constrained in the CRD, to keep it small; the controller checks it can read them. The controller also checks the relations (the range of `policy`, the allow lists).
- A reference that is missing, of another kind, invalid, or sets a field the Gateway may not: the Gateway is `Accepted: False`, reason `InvalidParameters`, the message names the field (e.g. `spec.pod.tolerations: not allowed by the GatewayClass (policy.gatewayOverrides)`). An invalid GatewayClass reference makes the GatewayClass `Accepted: False` (`InvalidParameters`) and its Gateways get the same reason.
- **A Gateway that ran before is not stopped** (10. Q2): up to v0.4.1, the Deployment and Service of a Gateway that is not accepted are collected as garbage (`collect_garbage`). So that one mistyped reference does not stop traffic, a Gateway whose Deployment exists keeps its last good shape (its Deployment, Service and PDB are left alone; the certificate Secret and the rule set PUTs go on). Its status is `Accepted: False` (`InvalidParameters`) and `Programmed: True` (message "the previous parameters are kept"). A Gateway without a Deployment yet gets none. Fixing the reference brings the new shape at the next pass.
- Fields that cannot change: changing `service.loadBalancerClass` is `InvalidParameters` ("create the Gateway again") and the old Service stays. Changing `service.type` goes through (documented: the load balancer address changes).
- The referenced CR gets no `status` (v1alpha1; the Gateways' status tells which use it).

### 2.5 Who decides what

| Field | From a Gateway's reference (tenant) | Why |
|---|---|---|
| replicas, PDB, resources, pod labels and annotations, topology spread, externalTrafficPolicy, source ranges, ipFamilyPolicy, Service labels, logLevel, performance, shutdown, ui | allowed by default | stays in their namespace; amounts are bounded by ResourceQuota, LimitRange and `maxReplicas` |
| Service annotations | allowed, but prefixes that pick load balancer addresses (`metallb.universe.tf/`, ...) only through the allow list (`--service-annotation-prefix`); others are `InvalidParameters` | the same line as v0.4's `spec.infrastructure.annotations` (the CVE-2020-8554 pattern). A reference gets an error instead of a silent drop |
| `service.type`, `loadBalancerClass` | not by default | can take outside addresses (the class's `policy` opens them) |
| nodeSelector, tolerations, affinity, priorityClassName | not by default | dedicated nodes, the control plane, evicting others (the class's `policy` opens them) |
| `rproxy.image`, `extraEnv` | never (not even through `policy`) | would run any image or setting in a pod holding the Gateway's control API credentials |
| `policy` | never | the class's reference's |

- `policy.gatewayOverrides` opens and closes fields (left out: "allowed by default" above). `rproxy.image`, `rproxy.extraEnv`, `policy` and unknown names cannot be listed (the class's reference is then invalid).
- Values in the class's reference (the administrator's) are not bound by `allowedPriorityClasses` and the like.

### 2.6 RBAC

- The controller: get, list, watch on `rproxygatewayparameters` (Roles with `watchNamespaces`). It has the PDB permissions since v0.4.1. Garbage collection is that of v0.4.1 (PDBs included) but spares Gateways kept in their last good shape (2.4).
- Tenants: the chart makes a ClusterRole `rproxy-gateway-parameters-edit` (read and write `rproxygatewayparameters`) with `rbac.authorization.k8s.io/aggregate-to-admin: "true"` (namespace admins get it; edit does not). `rbac.aggregateToAdmin: false` turns it off.
- managed pods' permissions do not change (no token, no RBAC).

### 2.7 The chart and compatibility

- The chart value `managed.parameters` (`{}` by default): when not empty, the chart renders the class's default `RproxyGatewayParameters` (`rproxy-default`, in the release namespace) and names it in the GatewayClass's `parametersRef`. **When empty, nothing is rendered and the GatewayClass is that of v0.4.1** (no `parametersRef`).
  - Not rendered by default because `helm upgrade` does not install new CRDs from the chart's `crds/`: a cluster upgraded from v0.4.1 has no `RproxyGatewayParameters` CRD, and rendering one would fail the upgrade. Apply the CRDs first to use it (`kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.2/rproxy.max3584.net.yaml`). A new `helm install` installs them from `crds/`.
- The flags (`--replicas`, `--service-type`, ...) stay and become "the default of fields no reference sets".
- Gateways and GatewayClasses without a reference get the pods and Services of v0.4.1.
- The controller runs without the CRD (as with the other CRDs, installing it later is picked up without a restart). A Gateway referring to it meanwhile is `InvalidParameters` (not found).

### 2.8 Tests

- Unit: merging (order, maps, lists), what `policy` allows, the annotation allow list, the names refused in `extraEnv`, the PDB against replicas, `InvalidParameters` messages. The `render` test (`InvalidParameters` in `src/render/tests.rs`) split into "a reference that works" and "an invalid reference".
- Keeping the last good shape: changing a valid reference to an invalid one leaves a Gateway with a Deployment alone (not re-rendered, not collected).
- e2e (kind): a reference changes replicas, resources and the topology spread, and the Deployment follows. With a namespace admin's permissions (the aggregated ClusterRole), writing tolerations gives `InvalidParameters` while the old pods stay and traffic goes on.
- conformance: `GatewayInfrastructurePropagation` passes as before (Gateways without a reference do not change).
- Acceptance: scenarios h and i (8.).

## 3. B. Install with Kustomize

### 3.1 What ships

| What | Contents | Made by |
|---|---|---|
| `install.yaml` on releases | managed (the chart's defaults): the Namespace `rproxy-gateway-system`, rproxy's CRDs, RBAC, the controller, the GatewayClass | `helm template` |
| `install-fleet.yaml` on releases | the same in fleet mode (`fleet.enabled=true`) | `helm template` |
| `crds.yaml` on releases | rproxy's CRDs only (for GitOps that applies CRDs first; Gateway API's CRDs are not included, as today) | `rproxy-gateway crds` |
| `config/` in the repository | `config/crd/` (CRDs), `config/default/` (the managed base: `kustomization.yaml` and the rendered `rproxy-gateway.yaml`), `config/fleet/` (the fleet base), `config/samples/` (overlay examples: image digests, `watchNamespaces`, resources, one replica, the class's default parameters) | `scripts/render-config.sh` (`helm template` → files) |

- Use: `kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.2/install.yaml`, or `resources: [github.com/max3584/rproxy-gateway//config/default?ref=v0.4.2]`.
- The rendered files carry no Helm labels (`helm.sh/chart`, `app.kubernetes.io/managed-by: Helm`): a chart value `rendered: true` drops them in `_helpers.tpl`.
- Only fixed values are rendered (the chart uses no randomness; secrets are made by the controller on its first start), so the same chart gives the same files.

### 3.2 Keeping them in step

- `config/` is not edited by hand. A CI job `config (kustomize)` (Alpine, `apk add helm kustomize`): `scripts/render-config.sh` → `git diff --exit-code config/`, `kustomize build` of `config/default`, `config/fleet`, `config/samples/*`, kubeconform (with Gateway API's CRD schemas). The same shape as today's CRD check (compared with `cargo run -- crds`).
- It becomes a required check only after the job is on main.
- A PR that changes the chart renders `config/` again (CI finds the drift).
- The release workflow (`release.yml`) makes `install.yaml`, `install-fleet.yaml` and `crds.yaml` and attaches them (next to today's chart `.tgz` and CRDs).

### 3.3 Controller settings that Kustomize can change

- The chart passes the controller's settings as `args`, so with Kustomize one would JSON-patch the n-th element of the list. Every flag can also be read from an environment variable (`RPROXY_GATEWAY_*`, clap's `env`), so the chart puts the settings in a ConfigMap `rproxy-gateway-config` passed with `envFrom`, and `args` is `[controller]` alone. Kustomize changes it with `configMapGenerator` (`behavior: merge`). **The chart's values do not change** (existing values keep working).
- Note: in clap, arguments win over environment variables. The chart's `controller.extraArgs` is appended to the args as it is (stronger than the environment).
- A changed ConfigMap rolls the pods (Helm: a hash annotation on the template; Kustomize: `configMapGenerator`'s hashed name).

### 3.4 What to change where

| To change | Helm | Kustomize |
|---|---|---|
| The controller (replicas, resources, nodeSelector, flags) | `controller.*`, `managed.*` | a patch on `config/default`, `configMapGenerator` |
| The fleet's DaemonSet | `fleet.*` | a patch on `config/fleet` |
| managed pods and Services (every Gateway's default) | `managed.parameters` (2.7) | add an `RproxyGatewayParameters` `rproxy-default` and patch the GatewayClass's `parametersRef` (an example in `config/samples/`) |
| managed pods and Services (one Gateway) | the tenant writes an `RproxyGatewayParameters` in the Gateway's namespace and names it in `infrastructure.parametersRef` (A) | the same |

### 3.5 Tests

- The CI job `config (kustomize)` (above).
- Acceptance (`acceptance.yml`) gets an input `install: helm | kustomize`. `kustomize` installs `config/default` with an overlay (replicas, images) and runs the same scenarios (after B's PR).

## 4. C. The UI on Kubernetes (the gateway side)

The UI's image, chart (`oci://ghcr.io/max3584/charts/rproxy-ui`), migration Job and usage collection are in the UI repository's design. The UI's chart is not a subchart of rproxy-gateway's (separate versions; the UI also works with VM rproxy alone). rproxy-gateway's chart only gets the values that connect it to the UI.

### 4.1 The UI watching rproxy on Kubernetes

Today's credentials: managed rproxy has a token per Gateway (derived from the master token with HMAC, scopes `rules:read`, `rules:write`, `acme:write`) and a control API certificate issued by the CA. Both are the controller's; given to the UI, they could write.

**The controller makes read-only credentials for the UI and hands them over in one Secret in the UI's namespace** (both sides must opt in).

1. The administrator sets `ui.namespace: rproxy-ui` (`--ui-namespace`) and `ui.podSelector` (default `app.kubernetes.io/name: rproxy-ui`) in rproxy-gateway's chart. Unset, nothing is made (as today).
2. Each Gateway's `tokens.yaml` gets a second token `rproxy-ui`: derived as `HMAC(master, "rproxy-gateway-ui/<id>")` (a value different from the controller's), scopes **`rules:read` and `metrics:read` only**. rproxy refuses writes with `403`, so this does not rely on the UI. rproxy reads the token file again, so pods are not replaced.
3. The controller writes a Secret `rproxy-ui-discovery` in the UI's namespace: `nodes.yaml` (a group `k8s:<ns>/<name>` per Gateway and a node per pod, `url: https://<pod IP>:9443`, `tls_server_name: <id>.rproxy-api.rproxy-gateway.internal`, `readonly: true`), `ca.crt` (the CA certificate only, no key), `token-<id>`. Gateways whose parameters say `ui.visible: false` are left out. In fleet mode, every fleet pod (the token is added to `rproxy-gateway-token`). Stopping pods stay listed until they are gone (usage to the end).
4. NetworkPolicy: each managed Gateway's NetworkPolicy also lets `ui.podSelector` in the UI's namespace reach 9443 (only Gateways with `ui.visible`). Not certsync (9444).
5. The UI reads `RPROXY_UI_K8S_DISCOVERY=/etc/rproxy-ui/k8s` (the Secret's volume) and reads it again when the files change.

- Delay: the kubelet updates Secret volumes within 1-2 minutes. Right after a pod IP changes, the UI asks the old IP, fails, and recovers at the next reload (acceptable for read-only pages and usage).
- Boundary: whoever can read Secrets in the UI's namespace can read the rules (targets, labels) and statistics of every Gateway shown to the UI. Not write, no keys, no other Gateway's rproxy. Written in SECURITY.md.
- Not chosen:

| Option | Why not |
|---|---|
| The UI reads pods and Gateway Secrets through the Kubernetes API | the UI would need to read Secrets cluster-wide (`resourceNames` cannot narrow list and watch) |
| The UI asks the controller, which relays to rproxy | the controller becomes the UI's data path and needs a new entry point with UI authentication, for the same read-only data. Considered if the pod IP delay hurts |
| Give the UI the controller's token | it writes (`rules:write`); the rule set owner would be the same and `409 owned` would no longer protect |

### 4.2 Tests (the gateway side)

- Unit: how the UI token is derived and its scopes, the discovery Secret (no keys, `ui.visible: false` not listed), the NetworkPolicy.
- Acceptance: an input `ui: true` also installs the UI's chart: the discovery Secret's pods show in the UI, the UI's token gets `403` on `PUT /rulesets`.

## 5. rproxy-api #240 and #241 (not used by rproxy-gateway)

- #240 the certificate API (`PUT /certs/{name}`): keys cross the control API. rproxy-gateway keeps mounted Secrets and certsync (rproxy-api's design 3.3; "Designs not chosen" in [DESIGN.md](DESIGN.md)). managed pods have a read-only root and nowhere to write.
- #241 rule set persistence: the source of truth is etcd; after an rproxy restart the controller waits for `/readyz` and PUTs again. Persisting would bring back the rules of Gateways deleted while rproxy was down. The controller's token has no `persist`, so nothing is persisted without doing anything.

## 6. E. Stopping on SIGTERM (the gateway side)

rproxy-api adds `--shutdown-delay` / `RPROXY_SHUTDOWN_DELAY` (after SIGTERM, `/readyz` says `draining` and rproxy keeps accepting) and `--shutdown-drain` / `RPROXY_SHUTDOWN_DRAIN` (stop listening and wait for the open connections) (both 0 by default in the binary; `features.graceful_shutdown`).

### 6.1 When to wire them

- rproxy-api v0.4.1 has them. The chart's rproxy image (`rproxy.image.tag`, the default of `--rproxy-image`, `RPROXY_VERSION` in release.yml) moved to 0.4.1.
- An older rproxy image through `rproxy.image` ignores `RPROXY_SHUTDOWN_*` and stops at once on SIGTERM. So the new shape (no preStop, `/readyz`) is used only once the pod's rproxy is known to have `features.graceful_shutdown`: the image the controller ships (`--rproxy-image`'s default) at once, another image once a pod running it answers in `/capabilities` (its pods roll once more; the controller remembers such images while it runs). While unknown, v0.4.1's shape (preStop, `/healthz`, no `RPROXY_SHUTDOWN_*`).

### 6.2 Shape

- managed (rproxy with `features.graceful_shutdown`): `RPROXY_SHUTDOWN_DELAY` and `RPROXY_SHUTDOWN_DRAIN` from A's `rproxy.shutdown`, else the controller's `--shutdown-delay` (15 s by default) and `--shutdown-drain` (25 s by default) (the chart's `managed.shutdown`). No preStop; `terminationGracePeriodSeconds` is delay + drain + 5 (45 by default). Readiness on `/readyz` (6.3).
- `managed.preStopSeconds` / `--pre-stop-secs` is unset by default now (no preStop for a new rproxy, 15 s for an older one). An install that sets it (a value written for 0.4.1) keeps it, as a preStop on every pod (the grace period is preStop + delay + drain + 5).
- fleet: the chart's `fleet.shutdown` (delay 5 s and drain 25 s by default; the grace period is delay + drain + 5). It is hostNetwork without Service endpoints, so point the outside load balancer's or VIP's health check at `https://<node>:9443/readyz` and make the delay longer than it takes to let go (interval x failures) (README and values say so).
- The controller does not PUT to pods being deleted (`deletionTimestamp`; as before. rproxy also refuses changes with `503 shutting_down` during the delay and drain).

#### Acceptance comparison

rproxy v0.4.1, 2 replicas, l2-local (MetalLB L2 + `Local`) and l2-cluster (MetalLB L2 + `Cluster`), six combinations, one run each. Values are the scenarios' longest time without a 200 (seconds): b1/b2 a pod deleted (not on / on the announcing node), c a drain, d a rollout restart, h a parameters change, f the controllers and rproxy restarted together (no limit). Failed: the failed requests of b1 to h together.

| shape | l2-local b1/b2/c/d/h | failed | f | l2-cluster b1/b2/c/d/h | failed |
|---|---|---|---|---|---|
| A. preStop 15, delay 0, drain 0, `/healthz` (v0.4.1) | 0.3/1.2/1.2/0.1/1.2 | 9 | 1.3 | 0.1/0.1/0.1/0.2/0.2 | 0 |
| B. preStop 15, delay 0, drain 25, `/healthz` | 0.2/**4.7**/0.2/0.2/0.2 | 122 | 6.1 | 0.2/0.2/0.2/0.2/0.2 | 0 |
| C. preStop 0, delay 15, drain 25, `/healthz` | 0.1/**6.1**/1.2/0.2/1.2 | 140 | 5.8 | 0.2/0.2/0.3/1.2/0.2 | 1 |
| **D. preStop 0, delay 15, drain 25, `/readyz` (chosen)** | 1.2/0.2/1.2/1.2/1.2 | 12 | 0.2 | 0.1/0.2/0.2/0.2/0.2 | 0 |
| E. preStop 15, delay 0, drain 25, `/readyz` | 0.2/**4.0**/0.2/1.2/0.2 | 104 | 2.6 | 0.2/0.2/0.2/0.2/0.2 | 0 |
| F. preStop 10, delay 5, drain 25, `/healthz` | 0.1/**6.0**/1.2/1.2/0.2 | 139 | 4.6 | 0.1/0.1/0.2/0.1/1.2 | 1 |

- The shapes that add a drain without taking the endpoint out before the listeners close (B, C, E, F) lost 4–6 s in l2-local's b2 (deleting the pod on the announcing node), over `GAP_LIMIT` (3 s). From the moment the listeners close until the pod is gone (the few seconds connections take to end), the endpoint stays `serving`, and MetalLB does not move until the pod is gone. E uses `/readyz` but with no delay, the listeners close before readiness fails (up to 4 s).
- With D, `/readyz` says `draining` on SIGTERM, and during the delay (rproxy still accepting) the endpoint goes and MetalLB moves. Gaps are like v0.4.1's (A) (one request, 1.2 s at most in l2-local), and open connections get to finish in the drain. In l2-cluster no shape differs.
- A 5 s delay (the design's first idea) was not measured. Readiness takes up to 4 s to fail (2 s x 2), so the default is 15 s, like v0.4.1's preStop.

### 6.3 Readiness on `/readyz`? (10. Q4)

**`/readyz` for rproxy with `features.graceful_shutdown`** (`managed.readinessProbe.path` / `--readiness-path` can set `/healthz`). Older rproxy stays on `/healthz`.

- At design time, `/readyz` `draining` dropping the endpoint's `serving` first looked like v0.4.1's rejected shape (a stopping pod's gate set to `False` first), which made MetalLB L2 + `Local` gaps longer. Measured, rproxy keeps accepting during the delay, so D (`/readyz`) was like A and shorter than the shapes adding a drain on `/healthz` (B, C) (6.2's table).
- Liveness stays on `/healthz` (a `draining` rproxy is not restarted).
- At start the readiness gate (rule set applied) is a stronger condition than `/readyz`, so start-up is no faster or slower.
- bgp and nodeport-lb were not compared (bgp is recorded only). If needed, `managed.readinessProbe.path` sets `/healthz` back.

### 6.4 Tests

- Unit: the environment variables and the grace period (flags, parameters, rounding); without `features.graceful_shutdown` the preStop and `/healthz` stay and no `RPROXY_SHUTDOWN_*` is passed; how an image becomes known (the shipped image, pods' `/capabilities`, remembered).
- e2e: `rproxy:e2e` (rproxy-api's master) is not the shipped image, so its pods roll to the new shape (no preStop, `/readyz`, `RPROXY_SHUTDOWN_*`, a 45 s grace period) once they answer `graceful_shutdown`.
- Acceptance: failed requests have been in the summary already. Input `strict` (false by default): the scenarios `GAP_LIMIT` applies to (b, c, d, h, i) fail on any failed request (bgp stays record-only). With `source=checkout`, if the chart's rproxy image is not published yet, it is built from rproxy-api's release binary of the same version.

## 7. F. VIPs held by rproxy pods

rproxy pods hold VIPs without a Service or LoadBalancer, failing over within seconds. Today managed relies on the Service (MetalLB, ...) to fail over, and fleet only writes node IPs (`--fleet-address`) with no way to move a VIP.

### 7.1 Options

| Option | What | Good | Bad |
|---|---|---|---|
| 1. fleet + a VIP sidecar (Lease) | a `vip` container in the fleet DaemonSet's pods. The pod holding a VIP's Lease adds the VIP to the node's interface and sends gratuitous ARP (unsolicited NA for IPv6); stopping, it releases the Lease and removes the VIP | planned moves (rollout, drain) under 1 s. One Lease in the API server decides the holder. Tied to rproxy's readiness | depends on the API server (7.4). Losing a node takes until the Lease expires (3 s by default) |
| 1a. kube-vip inside it | kube-vip as a DaemonSet sidecar | a widely used implementation with ARP, NDP, BGP | cannot decide holding by rproxy's readiness (rule sets applied). More images and permissions; one more thing to track |
| 1b. our own `rproxy-gateway vip` (Rust) | a new subcommand of the same image. netlink to add and remove addresses, a packet socket for ARP, an ICMPv6 raw socket for NA | can require rproxy's `/readyz` and the controller's "applied". Lease permissions narrowed by name. Status on the Gateway | code (about 1,000 lines) and dependencies (`rtnetlink`, ...; `cargo deny`) |
| 2. VRRP (keepalived) among fleet pods | the VM's act / stb shape | no API server | both MASTER in a partition; multicast or `unicast_peer` (DaemonSet pod IPs change); VRID clashes; config generation |
| 3. CNI BGP advertising pod or LB IPs (Calico, Cilium) | managed as is, the CNI advertises /32s | no hostNetwork; ECMP active-active | depends on the CNI; nothing for us to build |
| 4. hostNetwork for managed, with a VIP sidecar | per-Gateway pods on the node network | a VIP per Gateway | hostNetwork in tenant namespaces (PodSecurity privileged); port clashes between Gateways on a node; breaks "managed separates tenants" |

**Chosen: 1b (fleet + our own `vip` sidecar, Leases).** 3 is documentation only ("to fail over fast with managed and no Service"). 2 and 4 are not built.

### 7.2 Shape

```yaml
fleet:
  enabled: true
  hostNetwork: true
  vip:
    enabled: false
    addresses: [192.0.2.10, 192.0.2.11, "2001:db8::10"]   # set by the administrator; the fleet's addresses (Gateway status)
    interface: ""                 # empty: the interface with a route to the VIP's subnet
    leaseDuration: 3s
    renewInterval: 1s
    retryInterval: 500ms
    garp: {count: 3, interval: 200ms}   # sent right after taking a VIP (NA for IPv6)
    onApiUnreachable: hold        # hold | release (7.4)
```

- Holder: a Lease per VIP `rproxy-vip-<hash of the VIP>` (in the controller's namespace; the chart makes them, and the `vip` container gets get, update and watch narrowed with `resourceNames`). Expired Leases go first to pods holding fewer VIPs (they wait "VIPs held × 200 ms" before trying).
- Condition to hold: the same pod's rproxy `/readyz` is ready, and the controller has told it "every Gateway's rule set is applied to this pod" (received on the pod IP like certsync, with a token derived from the master token). When either fails (E's `draining` included), the VIP is released at once.
- Release: clear the Lease's `holderIdentity`, then remove the VIP from the interface. Other pods watch the Lease, take it at once, add the VIP and send gratuitous ARP / NA.
- Addresses: `/32` for IPv4, `/128` for IPv6, added with `nodad`.
- rproxy does not change: fleet rules listen on `0.0.0.0` (`--listen-addr`), so an added VIP reaches them. UDP answers from the address it arrived on (`IP_PKTINFO` / `IPV6_RECVPKTINFO`). Using one port for different Gateways per VIP needs `IP_FREEBIND`, an rproxy-api change (10. Q19).
- Status: the Gateway's `status.addresses` are the VIPs. With no holder, the Gateway is `Programmed: False` (`AddressNotUsable`, "no pod holds VIP 192.0.2.10"). `/metrics` (the `vip` container, port 9445 on the pod IP): `rproxy_vip_held{vip}`, `rproxy_vip_transitions_total{vip,reason}`. Logs: `vip.acquire`, `vip.release` (`shutdown`, `not_ready`, `lease_lost`, `conflict`).

### 7.3 The Gateway's `spec.addresses` and boundaries

- In fleet mode, `spec.addresses` must already be one of the fleet's addresses (else `AddressNotUsable`). With VIPs, the VIPs are the fleet's addresses. A Gateway can choose among them but not create new ones (the administrator lists them in the chart).
- At start, the controller checks the VIPs are within `--address-cidr` (the chart passes `managed.addressCIDRs` in fleet mode too) and overlap no Service ClusterIP, externalIP, load balancer IP or node IP; VIPs that fail are not used. With `addressCIDRs` empty, no VIPs (the same "off by default").
- Rules listen on `0.0.0.0`, so traffic to any VIP reaches the Gateway owning the port. fleet is one trust domain ([SECURITY.md](SECURITY.md)), so this is allowed.
- managed keeps Services. To use both in one cluster, use two GatewayClasses (two controllers).

### 7.4 When the API server is unreachable

- `hold` (default): keep the VIP while rproxy is ready. Other pods cannot take the Lease without the API server either, so there are no two holders. For the case where only the holder lost the API server and another took the Lease, the `vip` container listens to ARP / NA and **releases at once when another MAC announces the VIP**.
- `release`: let go when the Lease expires (as kube-vip). A control plane outage takes the data plane down.

### 7.5 Permissions and PodSecurity

- The `vip` container: root, `capabilities: {drop: [ALL], add: [NET_ADMIN, NET_RAW]}`, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`.
- Only the `vip` container has Kubernetes API credentials: the pod keeps `automountServiceAccountToken: false` and a `projected` `serviceAccountToken` volume is mounted into the `vip` container only. ServiceAccount `rproxy-gateway-vip`; a Role with get, update, patch, watch on `leases` (the VIP Leases by `resourceNames`).
- fleet is already hostNetwork, so its namespace is PodSecurity `privileged` (as today).

### 7.6 Failover times (expected)

| Event | Option 1b | MetalLB L2 | MetalLB BGP |
|---|---|---|---|
| Planned move (rollout, drain, pod deletion) | under 1 s | seconds (v0.4.1 measured 0.2-1.2 s with the preStop) | route withdrawal (seconds; under 1 s with BFD) |
| Node lost | `leaseDuration` (3 s by default) + about 0.5 s | memberlist detection (5-8 s) | the BGP hold timer (about 1 s with BFD) |
| rproxy alone fails | as soon as `/readyz` fails | once it leaves the Service's endpoints | the same |

- Whatever the method, TCP connections open when a VIP moves break (the new node does not know them). UDP sessions start over on the new node. E's `drain` does not help a VIP move.
- active-active: several VIPs spread over nodes, handed out with DNS round robin. To split one VIP across nodes, use BGP (MetalLB, the CNI).

### 7.7 IPv6

- `/128` with `nodad`; an unsolicited NA (Override) sent 3 times right after taking it (`garp.count`).
- Fleet rproxy takes IPv6 VIPs only with `::` in `--listen-addr`; an IPv6 VIP without `::` is an error at start.

### 7.8 Tests

- Unit: taking Leases (expiry, waiting by VIPs held, release), conditions (`/readyz`, the applied notice), `hold` and releasing on another MAC, VIP checks (`addressCIDRs`, overlaps), ARP and NA packet bytes.
- Acceptance: an input `mode: managed | fleet-vip`. `fleet-vip` picks VIPs from kind's docker network and measures HTTP, TCP and UDP from the runner to the VIPs. Scenarios j. delete the holder pod, k. `kubectl rollout restart ds/rproxy`, l. drain the holder's node, m. `docker stop <holder node>`, n. `docker pause <holder node>` (no two holders when it comes back), o. `docker pause` the control plane container (`hold` keeps traffic flowing). The summary compares with MetalLB L2.

## 8. Changes to the acceptance test (`acceptance.yml`)

| Input | Default | What | PR |
|---|---|---|---|
| `managed_replicas` | `2` | as today (`managed.replicas`) | — |
| `install` | `helm` | `kustomize` installs `config/default` with an overlay | after B |
| `ui` | `false` | also installs the UI's chart and checks 4.2 | C |
| `strict` | `false` | fails if E's checks see any failed request | E |
| `mode` | `managed` | `fleet-vip` runs 7.8's scenarios j-o | F |

The gap limit (`GAP_LIMIT`) fails a run only for what rproxy-gateway controls: pod deletions, drains, rollouts and parameters changes measured in l2-local, l2-cluster and nodeport-lb. `bgp` gaps (route withdrawal, BFD) and lost nodes are the CNI's, the load balancer's or the network's time, so they are recorded only (`info (network-dependent)`).

Scenarios added (A), run after today's a-g; SKIP when the `RproxyGatewayParameters` CRD is not installed (an older published chart).

- h. Change a Gateway's parameters: with a namespace admin's permissions, write an `RproxyGatewayParameters` and name it in the Gateway's `infrastructure.parametersRef` (one more replica, resources). The pods are replaced under traffic, `Accepted` stays True, the longest gap stays within `GAP_LIMIT`.
- i. Invalid parameters: with the same permissions, write tolerations → the Gateway is `InvalidParameters` and `Programmed: True`, the Deployment does not change and traffic goes on (gap within `GAP_LIMIT`). Fixing it brings `Accepted: True` back.

## 9. Order

1. This document (a docs PR).
2. A: `RproxyGatewayParameters` (v0.4.2).
3. B: the chart's ConfigMap, `config/`, `install.yaml` on releases (v0.4.2). A and B both change the chart; whichever goes in second renders `config/` again with `scripts/render-config.sh`.
4. E: wire rproxy-api v0.4.1's `RPROXY_SHUTDOWN_*` (6.).
5. C: after the UI's chart, the discovery Secret and the UI token.
6. F: the fleet VIP; large, so a patch of its own.
7. Acceptance (8.) is run by hand on each PR's branch to check nothing regresses from v0.4.1.

## 10. Decisions

| # | Question | Decision |
|---|---|---|
| Q1 | One CRD for A, or a separate cluster-scoped kind for classes | **One** (namespaced; the class's reference only in the controller's namespace; `policy` only from the class's reference) |
| Q2 | What happens to a Gateway that ran when its reference becomes invalid | **It keeps its last good shape** (`Accepted: False`, `Programmed: True`). A Gateway's rproxy with a Deployment is never deleted because of an invalid reference |
| Q3 | The default `drain` of the rproxy binary | decided in rproxy-api's design (0 in the binary). The managed default is delay `15s` and drain `25s` (decided with the acceptance test; 6.) |
| Q4 | Defaults that change: the PDB with 2+ replicas, `externalTrafficPolicy: Local` for NodePort, readiness on `/readyz`, the grace period | the PDB was done in v0.4.1. NodePort stays `Cluster` (v0.4.1's measurements). readiness is on `/readyz` (decided with E's acceptance test; 6.3). The grace period is delay + drain + 5 (6.2) |
| Q5 | What tenants may set by default | the table of 2.5. `service.type`, `loadBalancerClass`, nodeSelector, tolerations, affinity, priorityClass open through the class's `policy`. `image` and `extraEnv` never open to tenants |
| Q6 | Move the chart's controller settings from args to a ConfigMap (`envFrom`) | **Yes** (chart values unchanged) |
| Q7 | The UI's chart as a subchart of rproxy-gateway's | **No** (separate chart and versions). rproxy-gateway's chart only gets values such as `ui.namespace` |
| Q8 | How the UI watches rproxy on Kubernetes | **The controller writes read-only tokens and a discovery Secret into the UI's namespace** (the administrator's `ui.namespace` and the Gateway's `ui.visible`) |
| Q9 | Show Kubernetes rules to UI users who are not administrators | **Administrators only**; mapping namespaces to Keycloak groups later |
| Q17 | F's approach | **1b: fleet + our own `rproxy-gateway vip` (Leases, gratuitous ARP / NA)** |
| Q18 | VIPs while the API server is unreachable | **`hold`** (released at once when another MAC announces the VIP) |
| Q19 | Separate listeners per VIP | **No** (`0.0.0.0` as is); if needed, an rproxy-api issue for `IP_FREEBIND` |
| Q20 | Can a Gateway ask for a new VIP | **Only the administrator's list** (Gateways choose from it) |
| Q21 | Lease defaults | **3 s expiry, 1 s renewal, 0.5 s retry** |
| Q22 | When F ships | a separate patch of the v0.4 line (does not hold A-E back) |
| — | Versions | **No v0.5.0**; patches from v0.4.2 on |
| — | The class's default parameters in the chart | **Rendered only when `managed.parameters` is not empty** (`helm upgrade` does not install new CRDs; 2.7) |

Q10-Q16 (the UI's migrations and MariaDB, rproxy-api #240 and #241, usage) are in the UI's and rproxy-api's designs.
