日本語: [../SECURITY.md](../SECURITY.md)

# rproxy-gateway security

What the controller trusts, and what tenants (whoever can write Gateways and routes) cannot do. The overall design: [DESIGN.md](DESIGN.md).

## Trust boundaries

| Who | Can | Cannot |
|---|---|---|
| Cluster administrator | set the chart values and controller flags | — |
| Gateway owner (a namespace's editor) | write Gateways, routes, RproxyRules, RproxyMiddlewares in their namespace | use Services or Secrets of other namespaces without a ReferenceGrant there; take cluster IPs; use other Gateways' keys or rproxy |
| A managed rproxy pod | read its Gateway's certificates (a mounted Secret) | use the Kubernetes API (no token, no RBAC); use other Gateways' credentials |
| Whoever can read Secrets in the UI's namespace (with `ui.namespace`) | read the rules (targets, labels) and statistics of the rproxy of Gateways shown to the UI | write or delete rules; read keys; reach the rproxy of Gateways not shown |

## Managed and fleet

- **managed (default)**: one rproxy per Gateway, in the Gateway's namespace, with its own control API certificate (issued by the CA for `<id>.rproxy-api.rproxy-gateway.internal`) and token (HMAC-derived from the master token). This is the mode that separates tenants.
- **fleet**: every Gateway uses the same rproxy pods (a hostNetwork DaemonSet). **It is for one trust domain (one administrator)** and does not separate tenants: every Gateway's certificates go into one Secret (`rproxy-fleet-certs`) mounted into every pod, and a node port belongs to the Gateway that took it first (later ones get `Programmed: False`). In fleet mode RproxyRules are not read by default (`fleet.rproxyRules` / `--fleet-rproxy-rules`).

## Limits on tenant input

| Item | Default | How to change |
|---|---|---|
| `spec.addresses` (managed: the Service's `externalIPs`) | **off** (`Programmed: False`, `AddressNotUsable`). Even within allowed ranges, another Service's ClusterIP, externalIPs or LB IP cannot be taken (the CVE-2020-8554 shape) | `managed.addressCIDRs` / `--address-cidr`; never include the Service or pod ranges |
| `spec.infrastructure.annotations` onto the Service | annotations that pick addresses or load balancers (`metallb.universe.tf/`, `lbipam.cilium.io/`, `service.beta.kubernetes.io/`, ...) stay off the Service (pods and the ServiceAccount get them) | `managed.serviceAnnotationPrefixes` / `--service-annotation-prefix` |
| ExternalName Services as backends | **off** (`ResolvedRefs: False`): they can name anything (the API server, other namespaces, metadata endpoints) | `controller.allowExternalNameServices` / `--allow-external-name-services` |
| Files RproxyRules and RproxyMiddlewares name (`*_file`, `file`, `*_path`) | only the Gateway's own certificate files in the certificate directory; others get `Accepted: False` / `UnsupportedValue` | — |
| Migration (Ingress, Traefik) references to other namespaces (Services, Middlewares, TLSOptions, the errors service) | not converted without a ReferenceGrant in the target namespace (from `traefik.io` `IngressRoute*`, `Middleware`), as Traefik's `allowCrossNamespace=false` | `migration.allowCrossNamespace` / `--migration-allow-cross-namespace` |
| Ingress paths | not converted with backticks, quotes, control characters, or not starting with `/` (they are embedded in a `match` expression) | — |
| Ingress `defaultBackend` | only from the target Gateway's namespace | — |
| A Gateway's `RproxyGatewayParameters` (`infrastructure.parametersRef`, same namespace only) | only replicas (up to `policy.maxReplicas`, 10 by default), PDB, resources, pod and Service labels and annotations, topology spread, externalTrafficPolicy, source ranges, ipFamilyPolicy, logLevel, performance. Annotations that pick load balancer addresses only through the allow list above (others: `InvalidParameters`). `service.type`, `loadBalancerClass`, nodeSelector, tolerations, affinity, priorityClassName are off by default. Never rproxy's image, extra environment variables or `policy`; never labels or annotations with the controller's prefixes (`rproxy.max3584.net/`, ...) | `policy` of the GatewayClass's `RproxyGatewayParameters` (the chart's `managed.parameters`): `gatewayOverrides`, `maxReplicas`, `allowedPriorityClasses`, `allowedLoadBalancerClasses`. The image and environment variables cannot be opened ([DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), 2.5) |

## Keys from other namespaces (M7)

When `certificateRefs` (or `spec.tls.backend.clientCertificateRef`) names a Secret in another namespace with a ReferenceGrant, managed mode copies that key into a Secret in the Gateway's namespace (`rproxy-<id>-certs`; an rproxy pod can only mount Secrets of its own namespace). So **whoever can read Secrets in the Gateway's namespace can read that key**: the ReferenceGrant, a permission to refer, ends up being a permission to read. If that matters (a shared wildcard certificate, say):

- `controller.crossNamespaceSecrets: false` (`--cross-namespace-secrets=false`) refuses keys of other namespaces (`ResolvedRefs: False`, `RefNotPermitted`); Gateway API's conformance tests of cross-namespace certificates then fail
- or keep the key in the Gateway's namespace

## The controller's permissions (M10)

- By default the controller has a ClusterRole over Gateway API kinds, Secrets, Services, Deployments, ServiceAccounts, NetworkPolicies, PodDisruptionBudgets and Pods (patch; `pods/status` patch for rproxy pods' readiness gate) in every namespace (it deploys rproxy into Gateways' namespaces and certificates may be referenced from anywhere). It sets the readiness gate condition only on pods that declare it. A compromised controller can read every Secret in the cluster.
- The chart aggregates a ClusterRole `rproxy-gateway-parameters-edit` (read and write `rproxygatewayparameters`) to namespace admins (`rbac.authorization.k8s.io/aggregate-to-admin`; not to edit; `rbac.aggregateToAdmin: false` turns it off). The class's `policy` bounds what they can set. A GatewayClass's `parametersRef` is used only in the controller's namespace, so tenants cannot change the class's defaults.
- With `controller.watchNamespaces` (`--watch-namespaces`) it watches only those namespaces (and its own), and the chart makes a Role in each. The ClusterRole keeps only GatewayClasses and namespaces (allowedRoutes selectors).

## Network

- managed: a NetworkPolicy per Gateway (`managed.networkPolicy`, default true): rproxy's control API (9443) and certsync (9444) only from the controller's pods, listener ports from anyone. It has no effect if the CNI does not implement NetworkPolicy.
- certsync never lists its files: the controller sends names (content hashes) and gets back which are there. It listens on the pod's IP.
- fleet (hostNetwork): NetworkPolicy does not apply. The control API and certsync listen on the pod's IP (the node's), so firewall traffic from outside to the nodes.

## Control API credentials and rotation

- The CA (`rproxy-gateway-ca`) has pathLen 0, the name constraint `rproxy-gateway.internal`, and 10 years. Control API certificates last 1 year, are issued again 30 days before they end, and the rproxy pods roll (an annotation on the pod template).
- The controller reads at most 8 MiB of rproxy's answers and 1 MiB of certsync's. It talks only to managed pods owned by the Gateway's Deployment's ReplicaSet (`rproxy-<id>-...`).
- Rotation:
  - CA: `kubectl -n rproxy-gateway-system delete secret rproxy-gateway-ca rproxy-gateway-api-tls`, then restart the controller: every control API certificate is issued again by the new CA and the rproxy pods roll
  - master token: `kubectl -n rproxy-gateway-system delete secret rproxy-gateway-token`, then restart the controller: the per-Gateway tokens change and the rproxy pods roll (restart the DaemonSet in fleet mode). The UI's tokens (`ui.namespace`) change too and `rproxy-ui-discovery` is written again

## Showing Gateways to the UI (`ui.namespace`)

How the management UI (TCP-UDP-rproxy-ui's chart, installed apart) reads the rproxy on Kubernetes ([DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), 4.). **Off by default** (`ui.namespace: ""`: nothing is made). Both sides must opt in: the administrator sets the chart's `ui.namespace` (`--ui-namespace`), and the Gateway's `RproxyGatewayParameters` do not set `ui.visible` to false (default true; when the GatewayClass sets false, a Gateway cannot set true).

- The controller writes the Secret `rproxy-ui-discovery` in the UI's namespace: `nodes.yaml` (each shown Gateway's rproxy pods, `https://<pod IP>:9443`, and their certificate's name), `ca.crt` (the control API CA's certificate only: **no key**), and per Gateway the UI's token `token-<id>`. It deletes the Secret when no Gateway is shown.
- The UI's token is derived from the master token (HMAC-SHA256 of `rproxy-gateway-ui/<id>`; not the controller's token). It is added to each Gateway's rproxy token file as `rproxy-ui` with the scopes **`rules:read` and `metrics:read` only**: rproxy refuses writes (rules, rule sets, ACME) with `403`, whatever the UI does. rproxy (v0.4.1) reads its token file only at start and on SIGHUP, so adding or removing the token (setting `ui.namespace`, showing or hiding a Gateway) rolls that Gateway's rproxy pods once (the rollout keeps traffic flowing: readiness gate, graceful shutdown; with `ui.namespace` empty the pods stay as they are).
- **Whoever can read this Secret can read the rules (targets, labels) and statistics of every Gateway shown.** They cannot write, get no key, and get no token of Gateways not shown. Limit who can read Secrets in the UI's namespace (no `get secrets` beyond the UI chart's ServiceAccount). Hide a Gateway with `ui: {visible: false}` in its parameters; hide a class's Gateways by default with `ui: {visible: false}` in the chart's `managed.parameters`.
- NetworkPolicy: a shown Gateway's NetworkPolicy also lets the pods of `ui.podSelector` (default `app.kubernetes.io/name: rproxy-ui`, `app.kubernetes.io/component: ui`) in the UI's namespace reach the control API (9443) only (not certsync's 9444).
- RBAC: the chart makes a Role `rproxy-gateway-ui` in the UI's namespace (Secrets `create`; `get`, `update`, `patch`, `delete` of `rproxy-ui-discovery` only). It works with `controller.watchNamespaces` too.
- fleet: fleet pods hold every Gateway's rules, so they are listed (and the UI's token `rproxy-gateway-ui/fleet` added to `rproxy-gateway-token`'s `tokens.yaml`) only when **every** Gateway they serve is shown. The fleet's DaemonSet is the chart's: after the token is added or removed, have rproxy read it with `kubectl -n rproxy-gateway-system rollout restart daemonset -l app.kubernetes.io/component=fleet`. The fleet's control API is not protected by a NetworkPolicy ("Network" above).
- Turning it off: set `ui.namespace` back to empty (`helm upgrade`). The UI's tokens leave the token files and rproxy refuses them at once. The controller no longer looks at the old namespace: delete `rproxy-ui-discovery` there by hand (`kubectl -n <UI namespace> delete secret rproxy-ui-discovery`).
- Rotation: the UI's tokens derive from the master token, so rotating it ("Control API credentials and rotation" below) changes them too, and the controller writes the rproxy token files and `rproxy-ui-discovery` again (the rproxy pods roll; the UI reads the Secret volume again when it changes, so until the kubelet updates it, 1 to 2 minutes, the old token is refused).

## fleet VIPs (`fleet.vip`)

When the fleet's pods hold VIPs directly ([DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) 7.):

- Only the `vip` container runs as root with `NET_ADMIN` and `NET_RAW` (everything else dropped, `readOnlyRootFilesystem`, `allowPrivilegeEscalation: false`). It adds and removes addresses on the node's network (hostNetwork) and sends and hears ARP and NDP. The fleet uses hostNetwork already, so the namespace's PodSecurity stays `privileged` (`baseline` does not allow hostNetwork).
- Only the `vip` container has Kubernetes API credentials: the pod keeps `automountServiceAccountToken: false`, and the token of the ServiceAccount `rproxy-gateway-vip` is mounted (`projected`) into `vip` alone (rproxy and certsync still do not use the API). Its permissions: get, list, watch and update on the VIPs' Leases (named in `resourceNames`), reading pods in the controller's namespace (its readiness gate) and nodes (labels, cordon). The chart makes the Leases; `vip` cannot create any. A compromised `vip` can change which node has a VIP and read that namespace's pods and the nodes.
- VIPs are only those the administrator lists in the chart, inside `managed.addressCIDRs` and not another Service's or a node's address (the controller checks; `vip` does not hold a VIP outside the ranges either). Gateways can pick VIPs but not create them.
- The fleet's rules listen on `0.0.0.0`, so traffic to any VIP reaches a Gateway whose port matches (the fleet is one trust domain).
- `/metrics` (9445) answers on the node's IP without authentication (whether the pod holds each VIP and how often they moved). Filter traffic to the nodes from outside with a firewall ("Network" above).

## Owners of rproxy's rule sets

rproxy makes a rule set belong to the name of the token that created it (other non-admin tokens cannot change it). The controller's token is always named `rproxy-gateway` whatever its value, so after a token rotation (and after re-PUTs once rproxy restarts) the owner stays the same. The tokens' `allow_rulesets` are `k8s/` for the fleet's and, for each managed Gateway's, only that Gateway's rule set (`k8s/<namespace>/<name>`).

## rproxy's file owner check

rproxy checks that the certificate and key files rules and settings name belong to its user (`global.files.owner_check`). The kubelet mounts Secret volumes as root's files (group fsGroup, mode 0440), so the controller gives rproxy `RPROXY_FILES_TRUSTED_DIRS` for those directories (managed: `/var/run/rproxy-gateway/certs`, `/etc/rproxy-gateway/api`; fleet: also `/etc/rproxy-gateway/api-tls`, `/etc/rproxy-gateway/token`), where root's files are accepted too. The mode checks (not writable by the group or others, keys not readable by others) stay.

## Images

`controller.image.digest` and `rproxy.image.digest` (`sha256:...`) pin the images (over the tags).
