日本語: [../SECURITY.md](../SECURITY.md)

# rproxy-gateway security

What the controller trusts, and what tenants (whoever can write Gateways and routes) cannot do. The overall design: [DESIGN.md](DESIGN.md).

## Trust boundaries

| Who | Can | Cannot |
|---|---|---|
| Cluster administrator | set the chart values and controller flags | — |
| Gateway owner (a namespace's editor) | write Gateways, routes, RproxyRules, RproxyMiddlewares in their namespace | use Services or Secrets of other namespaces without a ReferenceGrant there; take cluster IPs; use other Gateways' keys or rproxy |
| A managed rproxy pod | read its Gateway's certificates (a mounted Secret) | use the Kubernetes API (no token, no RBAC); use other Gateways' credentials |

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

## Keys from other namespaces (M7)

When `certificateRefs` (or `spec.tls.backend.clientCertificateRef`) names a Secret in another namespace with a ReferenceGrant, managed mode copies that key into a Secret in the Gateway's namespace (`rproxy-<id>-certs`; an rproxy pod can only mount Secrets of its own namespace). So **whoever can read Secrets in the Gateway's namespace can read that key**: the ReferenceGrant, a permission to refer, ends up being a permission to read. If that matters (a shared wildcard certificate, say):

- `controller.crossNamespaceSecrets: false` (`--cross-namespace-secrets=false`) refuses keys of other namespaces (`ResolvedRefs: False`, `RefNotPermitted`); Gateway API's conformance tests of cross-namespace certificates then fail
- or keep the key in the Gateway's namespace

## The controller's permissions (M10)

- By default the controller has a ClusterRole over Gateway API kinds, Secrets, Services, Deployments, ServiceAccounts, NetworkPolicies and Pods (patch) in every namespace (it deploys rproxy into Gateways' namespaces and certificates may be referenced from anywhere). A compromised controller can read every Secret in the cluster.
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
  - master token: `kubectl -n rproxy-gateway-system delete secret rproxy-gateway-token`, then restart the controller: the per-Gateway tokens change and the rproxy pods roll (restart the DaemonSet in fleet mode)

## Owners of rproxy's rule sets

rproxy makes a rule set belong to the name of the token that created it (other non-admin tokens cannot change it). The controller's token is always named `rproxy-gateway` whatever its value, so after a token rotation (and after re-PUTs once rproxy restarts) the owner stays the same. The tokens' `allow_rulesets` are `k8s/` for the fleet's and, for each managed Gateway's, only that Gateway's rule set (`k8s/<namespace>/<name>`).

## Images

`controller.image.digest` and `rproxy.image.digest` (`sha256:...`) pin the images (over the tags).
