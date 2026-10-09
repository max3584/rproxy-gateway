日本語: [../PLATFORM.md](../PLATFORM.md)

# Giving rproxy-gateway its addresses (platform setup)

rproxy-gateway does not hold addresses (VIPs) itself. Providing addresses, bringing them to nodes or pods and moving them on failures is the platform's job (MetalLB, kube-vip, Cilium, cloud load balancers, keepalived on the nodes, your own L4 load balancer); rproxy-gateway only makes rproxy listen on them. This document lists, per common setup, what to write on the platform side (copy-paste manifests) and the outages measured by the acceptance test.

- v0.4.4's `fleet.vip` (the built-in VIP sidecar) was removed in v0.4.5 ([DESIGN-v0.4.x.md](DESIGN-v0.4.x.md), 7.). How to move off it: "[Moving off v0.4.4's `fleet.vip`](#moving-off-v044s-fleetvip)" at the end.
- Example addresses are documentation ranges (`192.0.2.0/24`, `198.51.100.0/24`). Check the API versions of the platform manifests against the docs of the version you run.

## Choosing

| Setup | What provides the address | Outage on pod replacement (delete, drain, rollout) | Losing a node | Client IP |
|---|---|---|---|---|
| [managed + LoadBalancer (MetalLB L2)](#metallb-l2) | MetalLB (ARP / NDP) | 0.2-1.2 s (`Local`) | 5-8 s (memberlist) | kept (`Local`) |
| [managed + LoadBalancer (MetalLB BGP + BFD)](#metallb-bgp--bfd) | MetalLB and the upstream router (ECMP) | 2.3-3.5 s (`Local`), 0.2 s (`Cluster`) | 3.3 s (`Local`, BFD) | kept (`Local`) |
| [managed + LoadBalancer (kube-vip)](#kube-vip-services-mode) | kube-vip (ARP, a Lease per Service) | not measured | the Lease's duration | kept (`Local`) |
| [managed + LoadBalancer (Cilium)](#cilium-lb-ipam--l2-announcements) | Cilium LB IPAM, L2 announcements (or BGP) | not measured | the Lease's duration | `Cluster` recommended (lost) |
| [managed + LoadBalancer (cloud)](#cloud-load-balancers) | the cloud's load balancer | not measured (depends on health checks) | depends on health checks | depends on the kind |
| [managed + NodePort + your own L4](#nodeport--your-own-l4-load-balancer-haproxy) | HAProxy or similar | 0.1-0.2 s (`Cluster`), 2.2 s (`Local`) | 6.4 s and more (health checks) | `Local` only |
| [fleet (hostNetwork)](#fleet-hostnetwork) | your own L4, keepalived, a MetalLB / kube-vip / Cilium Service | not measured (depends on health checks, VRRP) | same | kept (depends on the L4 forwarding) |
| [ClusterIP](#clusterip-in-cluster-only) | the Kubernetes Service | the Service's endpoints | — | kept |

The acceptance test (kind 1+3 nodes, `managed.replicas=2`, HTTP, HTTPS and TCP every 100 ms with a new connection each), longest time without a success (seconds). The same values as the README's "Availability":

| Setup (`TOPOLOGY`) | Pod deleted | Announcing node's pod deleted | Drain | Rollout restart | Node stops |
|---|---|---|---|---|---|
| MetalLB L2, `Local` (default) | 0.2 | 0.3 | 1.2 (drain 16.5 s) | 0.2 | 7.8 |
| MetalLB L2, `Cluster` | 0.2 | 0.2 | 0.2 | 0.2 | 9.0 (some fail up to ~59 s) |
| MetalLB BGP + ECMP (BFD), `Local` | 3.5 | — | 2.3 | 2.3 | 3.3 |
| MetalLB BGP + ECMP (BFD), `Cluster` | 0.2 | — | 0.2 | 0.1 | 13.3 (some fail up to ~60 s) |
| NodePort + own L4 (HAProxy), `Local` | 0.2 | — | 2.2 | 2.2 | 6.4 |
| NodePort + own L4 (HAProxy), `Cluster` | 0.1 | — | 0.1 | 0.2 | 20.2 (some fail up to ~63 s) |

- What rproxy-gateway decides is the outage of a pod replacement (the readiness gate, `/readyz` with `shutdown.delay` and `drain`, the PDB); the acceptance test fails on it over `GAP_LIMIT` (3 s). **Network-layer timings** (BGP withdrawal and BFD, ARP moves, detecting a dead node, cloud health checks) are set by the platform, so the acceptance test only records them (`bgp` and losing a node). The values are one example of that environment.
- When a node stops, the **backend** pods on it also stay in EndpointSlices until the node is NotReady (in every setup some requests may fail for 40-60 s). Use `RproxyPolicy`'s `outlierDetection` for backends.
- In every setup, make rproxy's `shutdown.delay` (managed 15 s, fleet 5 s) longer than the load balancer takes to take the pod or node out (health check interval x failures, ARP or route moves).

## managed + LoadBalancer

By default (`managed.serviceType: LoadBalancer`) each Gateway gets a `LoadBalancer` Service (`rproxy-<id>`, in the Gateway's namespace). The cluster's load balancer implementation gives the Service its address, which shows in the Gateway's `status.addresses`. `externalTrafficPolicy` is `Local` by default (client IPs are kept and only nodes with a ready pod take traffic).

Two ways to choose a Gateway's address:

- **Load balancer annotations** (`metallb.io/loadBalancerIPs`, `kube-vip.io/loadbalancerIPs`, `lbipam.cilium.io/ips`, ...) in the Gateway's `spec.infrastructure.annotations`. Annotations that pick addresses are kept off the Service by default (a tenant could take another's address; [SECURITY.md](SECURITY.md)). If you trust tenants, allow them with `managed.serviceAnnotationPrefixes` and split the address pools per namespace (MetalLB's `serviceAllocation`, Cilium's `serviceSelector` below).
- **The Gateway's `spec.addresses`**: becomes the Service's `externalIPs` (only inside `managed.addressCIDRs`). Something else brings traffic for that address to the nodes (a static route on the upstream router, Cilium L2 announcements with `externalIPs: true`, keepalived on the nodes). MetalLB and kube-vip do not announce `externalIPs`.

### MetalLB L2

```yaml
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata: {name: gateways, namespace: metallb-system}
spec:
  addresses: [192.0.2.100-192.0.2.120]
  # per namespace (MetalLB 0.14 and later)
  # serviceAllocation: {priority: 50, namespaces: [team-a]}
---
apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata: {name: gateways, namespace: metallb-system}
spec:
  ipAddressPools: [gateways]
```

```yaml
# rproxy-gateway chart values
managed:
  replicas: 2
  allocateLoadBalancerNodePorts: false   # MetalLB does not use node ports
  # let Gateways pick their address (only when you trust tenants)
  # serviceAnnotationPrefixes: [metallb.io/]
```

```yaml
# picking the address in the Gateway (once allowed by serviceAnnotationPrefixes)
spec:
  infrastructure:
    annotations: {metallb.io/loadBalancerIPs: 192.0.2.101}
```

- One node announces each Service. With `Local`, only nodes with a ready rproxy pod announce. Moving the announcement may drop the one connection in flight (the 1.2 s drain).
- A dead node is detected by memberlist (5-8 s). For faster, tune MetalLB's memberlist or use BGP + BFD.

### MetalLB BGP + BFD

```yaml
apiVersion: metallb.io/v1beta1
kind: BFDProfile
metadata: {name: fast, namespace: metallb-system}
spec: {receiveInterval: 300, transmitInterval: 300, detectMultiplier: 3}
---
apiVersion: metallb.io/v1beta2
kind: BGPPeer
metadata: {name: tor, namespace: metallb-system}
spec:
  myASN: 64512
  peerASN: 64513
  peerAddress: 198.51.100.1
  bfdProfile: fast        # BFD needs MetalLB's FRR mode (or frr-k8s)
---
apiVersion: metallb.io/v1beta1
kind: BGPAdvertisement
metadata: {name: gateways, namespace: metallb-system}
spec:
  ipAddressPools: [gateways]
```

- The router spreads the route over several nodes (ECMP; `maximum-paths` with FRR). With BFD a dead node leaves the routes in about a second.
- With `Local`, MetalLB withdraws the route of a node whose pod is terminating only once the pod is gone (3-4 s in FRR mode), and that much is lost. For close to zero on planned replacements use `Cluster` (`managed.externalTrafficPolicy: Cluster`, or per Gateway the parameters' `service.externalTrafficPolicy`), at the cost of client IPs and of some failures until a stopped node's pods leave the endpoints (40-50 s).
- Route convergence (BFD, timers, the router's ECMP handling) is network-side configuration.

### kube-vip (services mode)

Run kube-vip as a DaemonSet that announces `LoadBalancer` Services' addresses with ARP. kube-vip-cloud-provider hands out the addresses.

```bash
kubectl apply -f https://kube-vip.io/manifests/rbac.yaml
kubectl apply -f https://raw.githubusercontent.com/kube-vip/kube-vip-cloud-provider/main/manifest/kube-vip-cloud-controller.yaml
kubectl -n kube-system create configmap kubevip --from-literal range-global=192.0.2.100-192.0.2.120
```

```yaml
# also made by: kube-vip manifest daemonset --services --inCluster --arp --servicesElection
apiVersion: apps/v1
kind: DaemonSet
metadata: {name: kube-vip-ds, namespace: kube-system}
spec:
  selector: {matchLabels: {app.kubernetes.io/name: kube-vip-ds}}
  template:
    metadata: {labels: {app.kubernetes.io/name: kube-vip-ds}}
    spec:
      serviceAccountName: kube-vip
      hostNetwork: true
      containers:
        - name: kube-vip
          image: ghcr.io/kube-vip/kube-vip:v1.0.1   # the current version
          args: [manager]
          env:
            - {name: vip_arp, value: "true"}
            - {name: svc_enable, value: "true"}
            - {name: svc_election, value: "true"}   # a holder per Service (spread over nodes)
            - {name: vip_leaseduration, value: "5"}
            - {name: vip_renewdeadline, value: "3"}
            - {name: vip_retryperiod, value: "1"}
          securityContext:
            capabilities: {add: [NET_ADMIN, NET_RAW]}
```

- The annotation to pick an address in a Gateway: `kube-vip.io/loadbalancerIPs` (allow with `managed.serviceAnnotationPrefixes: [kube-vip.io/]`).
- With `svc_election`, a Lease per Service picks the announcing node; losing it takes up to the Lease's duration (5 s above). With `Local`, a node with a ready pod is chosen. Not measured by rproxy-gateway's acceptance test.

### Cilium LB IPAM + L2 announcements

Enable L2 announcements in Cilium's helm values (needs `kubeProxyReplacement: true`):

```yaml
kubeProxyReplacement: true
l2announcements:
  enabled: true
  leaseDuration: 3s
  leaseRenewDeadline: 1s
  leaseRetryPeriod: 200ms
externalIPs:
  enabled: true          # to announce Gateways' spec.addresses (externalIPs) too
k8sClientRateLimit: {qps: 50, burst: 100}   # room for the Lease renewals (scale with the number of Services)
```

```yaml
apiVersion: cilium.io/v2alpha1          # cilium.io/v2 in newer Cilium
kind: CiliumLoadBalancerIPPool
metadata: {name: gateways}
spec:
  blocks: [{start: 192.0.2.100, stop: 192.0.2.120}]
  # per namespace
  # serviceSelector: {matchLabels: {io.kubernetes.service.namespace: team-a}}
---
apiVersion: cilium.io/v2alpha1
kind: CiliumL2AnnouncementPolicy
metadata: {name: gateways}
spec:
  loadBalancerIPs: true
  externalIPs: true
  interfaces: ["^eth[0-9]+"]
  nodeSelector:
    matchExpressions: [{key: node-role.kubernetes.io/control-plane, operator: DoesNotExist}]
```

```yaml
# rproxy-gateway chart values
managed:
  replicas: 2
  externalTrafficPolicy: Cluster   # L2 announcements pick the announcing node regardless of where pods run
  allocateLoadBalancerNodePorts: false
```

- The annotation to pick an address in a Gateway: `lbipam.cilium.io/ips` (`managed.serviceAnnotationPrefixes: [lbipam.cilium.io/]`).
- A Lease per Service picks the announcing node; losing it takes up to the Lease's duration. For BGP, Cilium's BGP control plane (read it like MetalLB's BGP). Not measured by rproxy-gateway's acceptance test.

### Cloud load balancers

Cloud load balancer annotations (`service.beta.kubernetes.io/`, `cloud.google.com/`, ...) are kept from tenants by default too. To put the same ones on every Gateway, the administrator writes them in the class's default parameters (the chart's `managed.parameters`; apply the CRDs first, see the README's "Install").

```yaml
# AWS (AWS Load Balancer Controller NLB, targeting pod IPs directly)
managed:
  parameters:
    service:
      loadBalancerClass: service.k8s.aws/nlb
      annotations:
        service.beta.kubernetes.io/aws-load-balancer-nlb-target-type: ip
        service.beta.kubernetes.io/aws-load-balancer-scheme: internet-facing
```

```yaml
# GKE (backend service-based network load balancer)
managed:
  parameters:
    service:
      annotations: {cloud.google.com/l4-rbs: enabled}
```

```yaml
# Azure (internal load balancer)
managed:
  parameters:
    service:
      annotations: {service.beta.kubernetes.io/azure-load-balancer-internal: "true"}
```

- With `Local`, the load balancer checks nodes on the Service's `healthCheckNodePort` and takes out nodes without a ready pod. When pod IPs are the targets (AWS `ip`), the controller deregisters pods that left the endpoints.
- Make `managed.shutdown.delay` (15 s by default) longer than the load balancer takes to take a target out (health check interval x failures, deregistration delay). If it is not, raise it with the parameters' `rproxy.shutdown.delay`.
- The outage depends on the cloud's health check and deregistration settings (not measured by rproxy-gateway's acceptance test).

## NodePort + your own L4 load balancer (HAProxy)

```yaml
# rproxy-gateway chart values
managed:
  replicas: 2
  serviceType: NodePort   # externalTrafficPolicy is Cluster by default
```

Kubernetes picks each Gateway's node ports:

```bash
kubectl -n default get svc -l gateway.networking.k8s.io/gateway-name=web \
  -o jsonpath='{range .items[0].spec.ports[*]}{.port} -> {.nodePort}{"\n"}{end}'
```

```haproxy
# The acceptance test's settings (nodeport-lb): check every 500 ms, out after 2 failures;
# a failed connection marks the node down at once and goes to another node
defaults
  mode tcp
  timeout connect 200ms
  timeout client 30s
  timeout server 30s
  timeout check 500ms
  retries 3
  option redispatch 1
  default-server inter 500ms fastinter 250ms downinter 500ms fall 2 rise 2 on-marked-down shutdown-sessions observe layer4 error-limit 1 on-error mark-down

frontend https
  bind :443
  default_backend gw-https
backend gw-https
  balance roundrobin
  server node1 198.51.100.11:30443 check
  server node2 198.51.100.12:30443 check
  server node3 198.51.100.13:30443 check
```

- With `Cluster` (the default) every node forwards to a ready rproxy pod, so pod replacements are almost seamless (0.1-0.2 s). Client IPs are lost. When a node stops, some requests fail until its pods leave the endpoints (~60 s).
- With `Local` (to keep client IPs), NodePort has no `healthCheckNodePort`, so the load balancer cannot take a node out before rproxy stops (about 2 s lost). A `LoadBalancer` Service (even without a load balancer implementation) gets a `healthCheckNodePort`; check it over HTTP:

```haproxy
backend gw-https
  balance roundrobin
  option httpchk GET /healthz
  # 32000 is the Service's spec.healthCheckNodePort (answered by kube-proxy; 503 without a ready pod)
  server node1 198.51.100.11:30443 check port 32000
  server node2 198.51.100.12:30443 check port 32000
  server node3 198.51.100.13:30443 check port 32000
```

## fleet (hostNetwork)

With `fleet.enabled=true`, the chart's DaemonSet runs rproxy on the node's network (`hostNetwork: true`) and listens for every Gateway's listeners on `rproxy.listenAddresses` (`0.0.0.0` by default; add `::` for IPv6). Traffic reaching the node is taken for any destination address when the port matches. There is no Service, so the addresses are the nodes' IPs or ones the platform provides in front of or on the nodes.

- The addresses written to Gateways' `status.addresses` are `fleet.addresses` (empty: the nodes' IPs). Write the VIP or load balancer address there.
- When rproxy stops it keeps accepting for `fleet.shutdown.delay` (5 s by default) while `https://<node IP>:9443/readyz` returns 503 (draining). Point health checks at it and take nodes out sooner than the delay (interval x failures). 9443 is the control API port (listening on the node's IP only): firewall it so only the load balancer reaches it from outside ([SECURITY.md](SECURITY.md), "Network").
- None of these setups are measured by rproxy-gateway's acceptance test (the outage depends on health checks, VRRP and the load balancer).

### Your own L4 load balancer (HAProxy, checking `/readyz`)

```haproxy
defaults
  mode tcp
  timeout connect 200ms
  timeout client 30s
  timeout server 30s
  timeout check 500ms
  retries 3
  option redispatch 1
  default-server inter 500ms fastinter 250ms downinter 500ms fall 2 rise 2 on-marked-down shutdown-sessions

frontend https
  bind :443
  default_backend fleet-https
backend fleet-https
  balance roundrobin
  # rproxy's /readyz (503 once it starts stopping); the certificate is from the controller's CA: verify none or ca-file
  option httpchk GET /readyz
  http-check expect status 200
  server node1 198.51.100.11:443 check port 9443 check-ssl verify none
  server node2 198.51.100.12:443 check port 9443 check-ssl verify none
  server node3 198.51.100.13:443 check port 9443 check-ssl verify none
```

```yaml
# rproxy-gateway chart values
fleet:
  enabled: true
  addresses: [192.0.2.10]   # HAProxy's address
```

- Out after 500 ms x 2, well within the default 5 s delay. Client IPs are lost (no PROXY protocol).

### keepalived on the nodes (VRRP)

Install keepalived on the nodes' OS and hold the VIP with VRRP. A node whose rproxy is not ready leaves through `track_script`.

```text
# /etc/keepalived/keepalived.conf (per node; NODE_IP is that node's IP, vary priority per node)
global_defs {
  enable_script_security
  script_user root
}
vrrp_script rproxy_ready {
  script "/usr/bin/curl -skf --max-time 1 https://NODE_IP:9443/readyz"
  interval 1
  fall 2
  rise 2
}
vrrp_instance gateway {
  state BACKUP
  interface eth0
  virtual_router_id 51
  priority 100
  advert_int 1
  nopreempt
  virtual_ipaddress {
    192.0.2.10/32 dev eth0
  }
  track_script {
    rproxy_ready
  }
}
```

```yaml
# rproxy-gateway chart values
fleet:
  enabled: true
  addresses: [192.0.2.10]
```

- Planned moves (rollout, drain): the VIP moves `fall` x `interval` (2 s above) after `/readyz` turns 503, within the default 5 s delay. Losing a node: VRRP's master-down detection (about 3 x `advert_int`).
- Several VIPs spread over nodes (a `vrrp_instance` per VIP with different priorities) handed out by DNS give active-active.
- UDP replies leave from the address they arrived at (the VIP; rproxy uses `IP_PKTINFO`).

### MetalLB, kube-vip, Cilium (a Service selecting the fleet's pods)

In a cluster with a load balancer implementation, the administrator writes one `LoadBalancer` Service selecting the fleet's pods, and the implementation announces the VIP. With `externalTrafficPolicy: Local`, only nodes with a ready fleet pod take traffic (a terminating pod is no longer ready in the endpoints, so the announcement moves during the delay while rproxy still accepts).

```yaml
apiVersion: v1
kind: Service
metadata:
  name: rproxy-fleet
  namespace: rproxy-gateway-system
  annotations: {metallb.io/loadBalancerIPs: 192.0.2.10}   # kube-vip: kube-vip.io/loadbalancerIPs, Cilium: lbipam.cilium.io/ips
spec:
  type: LoadBalancer
  externalTrafficPolicy: Local
  selector: {app.kubernetes.io/name: rproxy, app.kubernetes.io/component: fleet}
  # list every Gateway listener port (update the Service when you add one)
  ports:
    - {name: http, port: 80, protocol: TCP}
    - {name: https, port: 443, protocol: TCP}
    - {name: dns, port: 53, protocol: UDP}
```

```yaml
# rproxy-gateway chart values
fleet:
  enabled: true
  addresses: [192.0.2.10]
```

- It is the administrator's Service, so address annotations are not subject to the tenant restriction (`serviceAnnotationPrefixes`).
- Failover timings read like the managed setups above (MetalLB L2's announcement moves and memberlist, BGP withdrawal, kube-vip's and Cilium's Leases). Cilium's L2 announcements pick the announcing node regardless of where pods run, so use `Cluster` there (client IPs are lost).
- Listener ports have to be listed in the Service. With many or changing ports, keepalived or your own L4 is simpler.

### Gateways' `spec.addresses` and `addressCIDRs`

- Today: a fleet Gateway's `spec.addresses` must be one of `fleet.addresses` (others: `Programmed: False`, `AddressNotUsable`). rproxy listens on `0.0.0.0`, so even with several VIPs listed in `fleet.addresses` the same port cannot go to different Gateways per VIP (any VIP with a matching port is taken).
- Coming (a PR in progress): fleet's rproxy listens per Gateway on its `spec.addresses` (rproxy's `listen_freebind`, rproxy-api #257). The addresses must be inside `managed.addressCIDRs`, and rproxy can listen before the VIP reaches that node, so the keepalived or Service VIPs above can be split per Gateway as they are. This section will be updated once it is merged.

## ClusterIP (in-cluster only)

For Gateways not exposed outside the cluster (an entry point for in-cluster clients), use `ClusterIP`.

```yaml
# every Gateway: chart values
managed:
  serviceType: ClusterIP
```

```yaml
# per Gateway: RproxyGatewayParameters (service.type is opened by the class's policy)
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyGatewayParameters
metadata: {name: internal, namespace: default}
spec:
  service: {type: ClusterIP}
```

- The Gateway's `status.addresses` is the Service's ClusterIP. To use a name, find the Service (`rproxy-<id>`) with `kubectl -n default get svc -l gateway.networking.k8s.io/gateway-name=web` and use `<Service>.<namespace>.svc`.
- Failover is the Service's endpoints (kube-proxy, the CNI); replaced pods leave through the readiness gate and `/readyz`.

## Moving off v0.4.4's `fleet.vip`

`fleet.vip` existed only in v0.4.4 (off by default). The v0.4.5 chart stops `helm upgrade` when the values have `fleet.vip` (`fleet.vip was removed in rproxy-gateway 0.4.5 ...`).

1. Provide the replacement address first (keepalived, your own L4, or a Service selecting the fleet's pods, above). To keep the same VIP, have the new mechanism hold it once `fleet.vip` is gone.
2. Delete `fleet.vip` from the values, put the VIP in `fleet.addresses`, and `helm upgrade`. Helm deletes what the chart made (the ServiceAccount, Role, RoleBinding, ClusterRole and ClusterRoleBinding `rproxy-gateway-vip`, a Lease `rproxy-vip-<hash>` per VIP). When the DaemonSet's pods are replaced, the `vip` container removes the VIP from the node's interface on SIGTERM before it exits.
3. If you installed with Kustomize (`config/samples/fleet-vip`), drop it from the overlay and delete by hand:

```bash
kubectl -n rproxy-gateway-system delete lease -l app.kubernetes.io/component=vip
kubectl -n rproxy-gateway-system delete serviceaccount,role,rolebinding rproxy-gateway-vip
kubectl delete clusterrole,clusterrolebinding rproxy-gateway-vip
```

   `RPROXY_GATEWAY_FLEET_VIPS` left in the ConfigMap `rproxy-gateway-config` is not read by the v0.4.5 controller (you can delete it). The v0.4.5 controller neither reads nor creates these Leases.

4. If a `vip` container went away without SIGTERM (a lost node, ...), check that the VIP is not left on the node:

```bash
ip -br addr | grep 192.0.2.10            # on the node
ip addr del 192.0.2.10/32 dev eth0       # if it is (/128 for IPv6)
```
