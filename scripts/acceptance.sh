#!/bin/bash
# shellcheck disable=SC2329  # functions run through wait_for, a trap or scenario_$s
# Acceptance test of the chart and images: HA, TLS and state recovery measured under continuous
# traffic, on a multi-node kind cluster with cert-manager, in front of one of several load balancer
# topologies. Runs on the CI runner VM (the manual `acceptance` workflow; kind needs Docker).
#   CHART_VERSION=0.4.0 MANAGED_REPLICAS=2 scripts/acceptance.sh                # the published chart and images
#   SOURCE=checkout TOPOLOGY=bgp scripts/acceptance.sh                          # this checkout's chart, the controller
#                                                                               # image from dist/amd64/rproxy-gateway
# TOPOLOGY (how traffic reaches the managed rproxy pods):
#   l2-local    MetalLB L2, externalTrafficPolicy Local (the chart's default): one node announces
#   l2-cluster  MetalLB L2, externalTrafficPolicy Cluster: one node announces, any node forwards
#   bgp         MetalLB BGP (FRR mode, BFD) to an FRR router container with ECMP: every node with a
#               ready rproxy pod announces, the router splits connections across them. Record-only: its
#               gaps are the network's (route withdrawal, BFD), so they never fail the run ("info
#               (network-dependent)")
#   nodeport-lb NodePort Services (externalTrafficPolicy Local; Cluster through HELM_ARGS) behind an HAProxy container that
#               health-checks every node and sends to all healthy ones (a user's own L4 balancer)
# MODE=fleet-vip (instead of a TOPOLOGY): fleet mode with VIPs held by the fleet's pods (fleet.vip,
# docs/DESIGN-v0.4.x.md 7.): a VIP from kind's docker network, no Service or load balancer; the runner
# sends to the VIP. Scenarios j-o (the VIP holder's pod deleted, rollout restart, drain; its node killed
# or paused, the control plane paused); the pod deletion, rollout restart, drain and the paused control
# plane (hold) fail over GAP_LIMIT, a lost node is recorded only.
# MODE=fleet: fleet mode (hostNetwork DaemonSet) without VIPs, fleet.listen=addresses: a Gateway with
# spec.addresses listens on those addresses only (listen_freebind; rproxy v0.4.3). The Gateway acc has
# none (the wildcard, the nodes' addresses). Scenario q: two addresses put on one worker by hand (ip
# addr: what MetalLB, kube-vip, keepalived... do; record-only), two Gateways on the same ports, one per
# address, and a third asking for a port taken on one of them (PortUnavailable).
# RPROXY_IMAGE_FROM=dist: the rproxy image from dist/amd64/rproxy-api (the workflow's rproxy_ref) instead
# of the chart's.
# Outages are measured and reported. With MANAGED_REPLICAS >= 2, a pod deletion, a node drain, a
# rollout restart and a parameters change fail the run when the longest gap without a 200 exceeds
# GAP_LIMIT seconds (l2-local, l2-cluster, nodeport-lb: what rproxy-gateway controls; bgp and a lost
# node are recorded only: their gaps come from the network and the cluster's failure detection), and with
# STRICT=true on any failed request; otherwise the script fails only on functional breakage (routes never
# recover, the certificate never rotates, state not restored). Each scenario's timeline (EndpointSlices,
# pods, the load balancer's view, probe failures) is in $work/timeline-*.txt. Results: $work/summary.md (and $GITHUB_STEP_SUMMARY).
set -euo pipefail
cd "$(dirname "$0")/.."

SOURCE=${SOURCE:-published}
TOPOLOGY=${TOPOLOGY:-l2-local}
# managed (TOPOLOGY decides how traffic reaches the Gateway) or fleet-vip
MODE=${MODE:-managed}
case "$MODE" in
  managed) ;;
  fleet-vip) TOPOLOGY=fleet-vip ;;
  fleet) TOPOLOGY=fleet ;;
  *) echo "MODE: managed, fleet or fleet-vip"; exit 1 ;;
esac
if [ "$SOURCE" = checkout ]; then
  CHART=${CHART:-charts/rproxy-gateway}
else
  CHART=${CHART:-oci://ghcr.io/max3584/charts/rproxy-gateway}
fi
CHART_VERSION=${CHART_VERSION:-0.4.0}
MANAGED_REPLICAS=${MANAGED_REPLICAS:-2}
# more helm flags, split on spaces
HELM_ARGS=${HELM_ARGS:-}
# seconds without a 200 a pod deletion, drain or rollout may cause (MANAGED_REPLICAS >= 2)
GAP_LIMIT=${GAP_LIMIT:-3}
# true: a pod deletion, drain, rollout or parameters change also fails on any failed request (not only
# on a gap over GAP_LIMIT)
STRICT=${STRICT:-false}
# the scenarios to run (a b1 b2 c d e f g h i; u is added with UI=true; MODE=fleet-vip: j k l o n m, the
# node killed (m) last: a kind node started again may come back with another address)
if [ "$MODE" = fleet-vip ]; then
  SCENARIOS=${SCENARIOS:-j k l o n m}
elif [ "$MODE" = fleet ]; then
  SCENARIOS=${SCENARIOS:-q}
else
  SCENARIOS=${SCENARIOS:-a b1 b2 c d e f g h i}
fi
# true: also the UI chart of UI_DIR (a checkout of TCP-UDP-rproxy-ui) with the bundled MariaDB, the
# controller's ui.namespace, and scenario u
UI=${UI:-false}
UI_DIR=${UI_DIR:-../TCP-UDP-rproxy-ui}
# the UI's usage collection interval in scenario u (the chart's default is 60)
UI_USAGE_SECS=${UI_USAGE_SECS:-15}
FRR_IMAGE=${FRR_IMAGE:-quay.io/frrouting/frr:9.1.0}
HAPROXY_IMAGE=${HAPROXY_IMAGE:-haproxy:3.0-alpine}
case "$TOPOLOGY" in l2-local | l2-cluster | bgp | nodeport-lb | fleet-vip | fleet) ;; *) echo "TOPOLOGY: l2-local, l2-cluster, bgp or nodeport-lb"; exit 1 ;; esac
CLUSTER=${CLUSTER:-rproxy-gateway-acc}
GATEWAY_API_VERSION=${GATEWAY_API_VERSION:-v1.6.3}
METALLB_VERSION=${METALLB_VERSION:-v0.15.2}
CERT_MANAGER_VERSION=${CERT_MANAGER_VERSION:-v1.18.2}
# seconds a scenario may take to recover before it counts as broken
RECOVERY_TIMEOUT=${RECOVERY_TIMEOUT:-300}
# seconds a new leader may take; a route change after a failover; a certificate rotation
LEADER_TIMEOUT=${LEADER_TIMEOUT:-60}
APPLY_TIMEOUT=${APPLY_TIMEOUT:-120}
ROTATE_TIMEOUT=${ROTATE_TIMEOUT:-300}
NS=rproxy-gateway-system
APP=acc
# the rproxy pods: the Gateway's (managed) or the fleet's
if [ "$MODE" != managed ]; then
  RP_NS=$NS RP_SEL=app.kubernetes.io/name=rproxy,app.kubernetes.io/component=fleet
else
  RP_NS=$APP RP_SEL=app.kubernetes.io/name=rproxy
fi
work=${RUNNER_TEMP:-/tmp}/rproxy-gateway-acceptance
mkdir -p "$work"
results=$work/results.tsv
: > "$results"

log() { echo "[$(date -u +%H:%M:%S)] $*"; }
now_ms() { date +%s%3N; }
secs() { awk -v ms="$1" 'BEGIN { printf "%.1f", ms / 1000 }'; }

# wait_for <seconds> <command...>: retries every 0.5 s
wait_for() {
  local end=$(($(now_ms) + $1 * 1000))
  shift
  until "$@" > /dev/null 2>&1; do
    if [ "$(now_ms)" -ge "$end" ]; then return 1; fi
    sleep 0.5
  done
}

dump() {
  local d=$work/logs
  mkdir -p "$d"
  kubectl get nodes -o wide > "$d/nodes.txt" 2>&1 || true
  kubectl get pods -A -o wide > "$d/pods.txt" 2>&1 || true
  kubectl get events -A --sort-by=.lastTimestamp > "$d/events.txt" 2>&1 || true
  kubectl -n "$NS" logs -l app.kubernetes.io/name=rproxy-gateway --prefix --tail=-1 > "$d/controller.log" 2>&1 || true
  kubectl -n "$NS" get lease rproxy-gateway -o yaml > "$d/lease.yaml" 2>&1 || true
  kubectl -n "$RP_NS" logs -l "$RP_SEL" --prefix --all-containers --tail=-1 > "$d/rproxy.log" 2>&1 || true
  if [ "$MODE" = fleet-vip ]; then
    kubectl -n "$NS" logs -l "$RP_SEL" -c vip --prefix --tail=-1 --previous > "$d/vip-previous.log" 2>&1 || true
    kubectl -n "$NS" get lease -o yaml > "$d/leases.yaml" 2>&1 || true
    for n in $(kind get nodes --name "$CLUSTER" 2> /dev/null); do echo "$n: $(docker exec "$n" ip -o addr show 2> /dev/null | grep -c " ${VIP:-none}/")"; done > "$d/vip-addresses.txt" 2>&1 || true
  fi
  kubectl get gateways,httproutes,tcproutes,certificates -A -o yaml > "$d/objects.yaml" 2>&1 || true
  kubectl -n "$APP" get deploy,svc,networkpolicy,pdb,secret -o wide > "$d/managed.txt" 2>&1 || true
  docker logs "$CLUSTER-frr" > "$d/frr.log" 2>&1 || true
  docker exec "$CLUSTER-frr" vtysh -c 'show bgp summary' -c 'show bfd peers brief' -c 'show ip route' > "$d/frr-state.txt" 2>&1 || true
  docker logs "$CLUSTER-lb" > "$d/haproxy.log" 2>&1 || true
  if [ "$UI" = true ]; then
    kubectl -n rproxy-ui get all,pvc,secret,networkpolicy -o wide > "$d/ui.txt" 2>&1 || true
    kubectl -n rproxy-ui logs -l app.kubernetes.io/part-of=rproxy-ui --prefix --all-containers --tail=-1 > "$d/ui.log" 2>&1 || true
    kubectl -n "$APP" get networkpolicy -o yaml > "$d/networkpolicies.yaml" 2>&1 || true
  fi
  if [ -n "${GITHUB_ACTIONS:-}" ]; then
    echo "::group::controller log (tail)"; tail -n 200 "$d/controller.log" || true; echo "::endgroup::"
  fi
}

probe_pids=()
stop_probes() {
  for p in "${probe_pids[@]}"; do kill "$p" 2> /dev/null || true; done
}
finish() {
  stop_probes
  dump
}
trap finish EXIT

for c in kind kubectl helm jq curl openssl docker; do command -v $c > /dev/null || { echo "$c is needed"; exit 1; }; done
if [ "$UI" = true ]; then
  command -v node > /dev/null || { echo "node is needed (UI=true)"; exit 1; }
  [ -f "$UI_DIR/charts/rproxy-ui/Chart.yaml" ] || { echo "UI_DIR ($UI_DIR) has no charts/rproxy-ui"; exit 1; }
  case " $SCENARIOS " in *" u "*) ;; *) SCENARIOS="$SCENARIOS u" ;; esac
fi

# ---------------------------------------------------------------- cluster
log "== cluster: 1 control plane + 3 workers"
if ! kind get clusters | grep -qx "$CLUSTER"; then
  cat > "$work/kind.yaml" << 'YAML'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
nodes:
  - role: control-plane
  - role: worker
  - role: worker
  - role: worker
YAML
  kind create cluster --name "$CLUSTER" --config "$work/kind.yaml" --wait 300s
fi
kubectl get nodes -o wide

# the runner reaches pod IPs through each node (rule sets checked pod by pod)
for n in $(kubectl get nodes -o jsonpath='{.items[*].metadata.name}'); do
  cidr=$(kubectl get node "$n" -o jsonpath='{.spec.podCIDR}')
  ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$n")
  sudo ip route replace "$cidr" via "$ip"
done

kind_ip() { docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$1"; }
WORKERS=$(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}')
subnet=$(docker network inspect kind -f '{{range .IPAM.Config}}{{.Subnet}} {{end}}' | tr ' ' '\n' | grep -v ':' | grep . | head -1)
prefix=$(echo "$subnet" | cut -d. -f1-2)
docker rm -f "$CLUSTER-frr" "$CLUSTER-lb" > /dev/null 2>&1 || true

# metallb <manifest>: MetalLB and an address pool
metallb() {
  log "== MetalLB $METALLB_VERSION ($1)"
  kubectl apply -f "https://raw.githubusercontent.com/metallb/metallb/$METALLB_VERSION/config/manifests/$1.yaml" > /dev/null
  kubectl -n metallb-system rollout status deploy/controller --timeout=300s
  kubectl -n metallb-system rollout status ds/speaker --timeout=300s
}
apply_pool() {  # apply_pool <range> <extra YAML> (errors in $work/apply.log)
  cat << YAML | kubectl apply -f - > "$work/apply.log" 2>&1
apiVersion: metallb.io/v1beta1
kind: IPAddressPool
metadata: {name: kind, namespace: metallb-system}
spec: {addresses: ["$1"]}
---
$2
YAML
}
case "$TOPOLOGY" in
  l2-*)
    metallb metallb-native
    pool="$prefix.255.200-$prefix.255.250"
    echo "kind network $subnet, pool $pool"
    wait_for 180 apply_pool "$pool" 'apiVersion: metallb.io/v1beta1
kind: L2Advertisement
metadata: {name: kind, namespace: metallb-system}
spec: {ipAddressPools: [kind]}' || { cat "$work/apply.log"; exit 1; }
    ;;
  bgp)
    # an FRR router on the kind network: eBGP with every node's MetalLB speaker (BFD), ECMP over
    # the nodes that announce, per-connection (L4) hashing; the runner routes the pool through it
    mkdir -p "$work/frr"
    neighbors=""
    for n in $(kubectl get nodes -o jsonpath='{.items[*].metadata.name}'); do
      neighbors+=" neighbor $(kind_ip "$n") peer-group metallb"$'\n'
    done
    cat > "$work/frr/daemons" << 'EOF2'
bgpd=yes
bfdd=yes
vtysh_enable=yes
zebra_options="  -A 127.0.0.1 -s 90000000"
bgpd_options="   -A 127.0.0.1"
bfdd_options="   -A 127.0.0.1"
EOF2
    cat > "$work/frr/frr.conf" << EOF2
frr defaults traditional
hostname frr
log stdout informational
!
router bgp 64512
 no bgp ebgp-requires-policy
 no bgp default ipv4-unicast
 bgp bestpath as-path multipath-relax
 neighbor metallb peer-group
 neighbor metallb remote-as 64513
 neighbor metallb bfd
 neighbor metallb timers 3 9
$neighbors !
 address-family ipv4 unicast
  neighbor metallb activate
  maximum-paths 8
 exit-address-family
!
EOF2
    : > "$work/frr/vtysh.conf"
    chmod -R a+rwX "$work/frr"
    docker run -d --name "$CLUSTER-frr" --network kind --privileged \
      --sysctl net.ipv4.ip_forward=1 --sysctl net.ipv4.fib_multipath_hash_policy=1 \
      --sysctl net.ipv4.conf.all.send_redirects=0 --sysctl net.ipv4.conf.default.send_redirects=0 \
      -v "$work/frr:/etc/frr" "$FRR_IMAGE" > /dev/null
    docker exec "$CLUSTER-frr" sysctl -qw net.ipv4.conf.eth0.send_redirects=0 || true
    FRR_IP=$(kind_ip "$CLUSTER-frr")
    metallb metallb-frr
    # MetalLB's CRDs give ASNs a maximum beyond int32 that newer API servers refuse
    # ("Maximum boundary value must be of type integer with format int32"): drop those maximums
    kubectl get crd bgppeers.metallb.io -o json |
      jq 'walk(if type == "object" and .format? == "int32" and ((.maximum? // 0) > 2147483647) then del(.maximum) else . end)' |
      kubectl replace -f - > /dev/null
    pool=10.200.0.10-10.200.0.50
    sudo ip route replace 10.200.0.0/24 via "$FRR_IP"
    echo "FRR $FRR_IP, pool $pool (routed through FRR)"
    wait_for 180 apply_pool "$pool" "apiVersion: metallb.io/v1beta1
kind: BFDProfile
metadata: {name: fast, namespace: metallb-system}
spec: {receiveInterval: 300, transmitInterval: 300, detectMultiplier: 3}
---
apiVersion: metallb.io/v1beta2
kind: BGPPeer
metadata: {name: frr, namespace: metallb-system}
spec: {myASN: 64513, peerASN: 64512, peerAddress: $FRR_IP, bfdProfile: fast}
---
apiVersion: metallb.io/v1beta1
kind: BGPAdvertisement
metadata: {name: kind, namespace: metallb-system}
spec: {ipAddressPools: [kind]}" || { cat "$work/apply.log"; exit 1; }
    ;;
  nodeport-lb)
    HELM_ARGS="--set managed.serviceType=NodePort --set managed.externalTrafficPolicy=Local $HELM_ARGS"
    ;;
  fleet)
    # two addresses from kind's docker network for scenario q (not on any node until it adds them)
    ADDR1=${ADDR1:-$prefix.255.110} ADDR2=${ADDR2:-$prefix.255.111}
    echo "kind network $subnet, scenario q's addresses $ADDR1 $ADDR2"
    HELM_ARGS="--set fleet.enabled=true --set fleet.listen=addresses --set managed.addressCIDRs={$ADDR1/32,$ADDR2/32} $HELM_ARGS"
    ;;
  fleet-vip)
    # a VIP from kind's docker network (docker allocates from the start of the subnet); the fleet's pods
    # hold it (hostNetwork, the vip sidecar), the runner reaches it on the docker bridge
    VIP=${VIP:-$prefix.255.100}
    LEASE=rproxy-vip-$(printf '%s' "$VIP" | sha256sum | cut -c1-10)
    echo "kind network $subnet, VIP $VIP (Lease $LEASE)"
    HELM_ARGS="--set fleet.enabled=true --set fleet.vip.enabled=true --set fleet.vip.addresses={$VIP} --set managed.addressCIDRs={$VIP/32} $HELM_ARGS"
    ;;
esac
case "$TOPOLOGY" in
  l2-cluster) HELM_ARGS="--set managed.externalTrafficPolicy=Cluster $HELM_ARGS" ;;
esac
# the externalTrafficPolicy in use (the last one HELM_ARGS sets; else the chart's default for the Service type)
ETP=$(grep -o 'managed.externalTrafficPolicy=[A-Za-z]*' <<< "$HELM_ARGS" | tail -1 | cut -d= -f2 || true)
if [ -z "$ETP" ]; then [ "$TOPOLOGY" = nodeport-lb ] && ETP=Cluster || ETP=Local; fi

log "== cert-manager $CERT_MANAGER_VERSION"
kubectl apply -f "https://github.com/cert-manager/cert-manager/releases/download/$CERT_MANAGER_VERSION/cert-manager.yaml" > /dev/null
for d in cert-manager cert-manager-cainjector cert-manager-webhook; do kubectl -n cert-manager rollout status deploy/$d --timeout=300s; done

log "== Gateway API $GATEWAY_API_VERSION (experimental channel)"
kubectl apply --server-side -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/$GATEWAY_API_VERSION/experimental-install.yaml" > /dev/null

# ---------------------------------------------------------------- install
image_args=()
if [ "$SOURCE" = checkout ]; then
  log "== controller image from dist/amd64/rproxy-gateway"
  chmod +x dist/amd64/rproxy-gateway
  docker build -q -t rproxy-gateway:acc --build-arg TARGETARCH=amd64 -f Dockerfile .
  kind load docker-image --name "$CLUSTER" rproxy-gateway:acc
  image_args=(--set controller.image.repository=rproxy-gateway --set controller.image.tag=acc)
  # the chart's rproxy image before its first release (made by release.yml): built from rproxy-api's
  # release binary of the same version (Dockerfile.rproxy)
  rproxy_repo=$(sed -n '/^rproxy:/,/^[a-z]/s/^    repository: *//p' "$CHART/values.yaml")
  rproxy_tag=$(sed -n '/^rproxy:/,/^[a-z]/s/^    tag: *"\(.*\)"/\1/p' "$CHART/values.yaml")
  if ! docker pull -q "$rproxy_repo:$rproxy_tag" > /dev/null 2>&1; then
    log "== rproxy image $rproxy_repo:$rproxy_tag not published: built from rproxy-api v$rproxy_tag"
    rel=https://github.com/max3584/rproxy-api/releases/download/v$rproxy_tag
    bin=rproxy-api-v$rproxy_tag-x86_64-unknown-linux-musl
    curl -fsSLo "$work/$bin" "$rel/$bin"
    curl -fsSLo "$work/SHA256SUMS" "$rel/SHA256SUMS"
    (cd "$work" && grep " \*\?$bin\$" SHA256SUMS | sha256sum -c -)
    mkdir -p dist/amd64
    install -m 755 "$work/$bin" dist/amd64/rproxy-api
    docker build -q -t "$rproxy_repo:$rproxy_tag" --build-arg TARGETARCH=amd64 -f Dockerfile.rproxy .
  fi
  kind load docker-image --name "$CLUSTER" "$rproxy_repo:$rproxy_tag"
else
  image_args=(--version "$CHART_VERSION")
fi
RPROXY_IMAGE_FROM=${RPROXY_IMAGE_FROM:-chart}
if [ "$RPROXY_IMAGE_FROM" = dist ]; then
  # rproxy built from rproxy-api (the workflow's rproxy_ref), e.g. a branch not released yet
  log "== rproxy image from dist/amd64/rproxy-api (sha256 $(sha256sum dist/amd64/rproxy-api | cut -c1-12))"
  chmod +x dist/amd64/rproxy-api
  docker build -q -t rproxy-api:acc --build-arg TARGETARCH=amd64 -f Dockerfile.rproxy .
  kind load docker-image --name "$CLUSTER" rproxy-api:acc
  image_args+=(--set rproxy.image.repository=rproxy-api --set rproxy.image.tag=acc --set rproxy.image.digest=)
fi
ui_args=()
if [ "$UI" = true ]; then
  # the chart's Role for the discovery Secret lives in the UI's namespace: it must exist first
  kubectl create namespace rproxy-ui --dry-run=client -o yaml | kubectl apply -f - > /dev/null
  ui_args=(--set ui.namespace=rproxy-ui)
fi
log "== helm install $CHART ($SOURCE) (controller 2 replicas, managed.replicas=$MANAGED_REPLICAS) ${ui_args[*]} $HELM_ARGS"
# shellcheck disable=SC2086
helm install rproxy-gateway "$CHART" "${image_args[@]}" -n "$NS" --create-namespace --wait --timeout 10m \
  --set controller.replicas=2 --set managed.replicas="$MANAGED_REPLICAS" "${ui_args[@]}" $HELM_ARGS
helm -n "$NS" list
kubectl wait --for=condition=Accepted gatewayclass/rproxy --timeout=120s

# ---------------------------------------------------------------- Gateway, routes, certificates
log "== issuers, Gateway, routes"
kubectl create namespace $APP --dry-run=client -o yaml | kubectl apply -f - > /dev/null
apply_issuers() {
  cat << 'YAML' | kubectl apply -f -
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: {name: selfsigned}
spec: {selfSigned: {}}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: acc-ca, namespace: cert-manager}
spec:
  isCA: true
  commonName: acceptance-ca
  secretName: acc-ca
  privateKey: {algorithm: ECDSA, size: 256}
  issuerRef: {name: selfsigned, kind: ClusterIssuer, group: cert-manager.io}
---
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: {name: acc-ca}
spec: {ca: {secretName: acc-ca}}
YAML
}
wait_for 180 apply_issuers
kubectl -n cert-manager wait --for=condition=Ready certificate/acc-ca --timeout=120s
kubectl wait --for=condition=Ready clusterissuer/acc-ca --timeout=120s
kubectl -n cert-manager get secret acc-ca -o jsonpath='{.data.ca\.crt}' | base64 -d > "$work/ca.crt"

cat << YAML | kubectl apply -f - > /dev/null
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: secure-cert, namespace: $APP}
spec:
  secretName: secure-cert
  dnsNames: [secure.example.com]
  duration: 24h
  privateKey: {rotationPolicy: Always}
  issuerRef: {name: acc-ca, kind: ClusterIssuer, group: cert-manager.io}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo, namespace: $APP}
spec:
  replicas: 3
  selector: {matchLabels: {app: echo}}
  template:
    metadata: {labels: {app: echo}}
    spec:
      topologySpreadConstraints:
        - {maxSkew: 1, topologyKey: kubernetes.io/hostname, whenUnsatisfiable: ScheduleAnyway, labelSelector: {matchLabels: {app: echo}}}
      # backends stop gracefully too (as they should behind any proxy): a drain then measures
      # rproxy's own failover, not requests sent to an evicted backend before the controller
      # has PUT the rule set without it (without this, about 1-3 s of failures per drain)
      terminationGracePeriodSeconds: 15
      containers:
        - name: echo
          image: registry.k8s.io/gateway-api/echo-basic:v1.5.1
          lifecycle: {preStop: {sleep: {seconds: 5}}}
          env:
            - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
            - {name: NAMESPACE, valueFrom: {fieldRef: {fieldPath: metadata.namespace}}}
          readinessProbe: {httpGet: {path: /, port: 3000}, periodSeconds: 2}
---
apiVersion: v1
kind: Service
metadata: {name: echo, namespace: $APP}
spec:
  selector: {app: echo}
  ports: [{name: http, port: 8080, targetPort: 3000}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: acc, namespace: $APP}
spec:
  gatewayClassName: rproxy
  listeners:
    - {name: http, port: 80, protocol: HTTP}
    - name: https
      port: 443
      protocol: HTTPS
      hostname: secure.example.com
      tls: {certificateRefs: [{name: secure-cert}]}
    - {name: tcp, port: 9000, protocol: TCP}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: echo, namespace: $APP}
spec:
  parentRefs: [{name: acc, sectionName: http}, {name: acc, sectionName: https}]
  hostnames: [echo.example.com, secure.example.com]
  rules: [{backendRefs: [{name: echo, port: 8080}]}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: echo, namespace: $APP}
spec:
  parentRefs: [{name: acc, sectionName: tcp}]
  rules: [{backendRefs: [{name: echo, port: 8080}]}]
YAML

# a host-only HTTPRoute (scenarios a and f: a change applied after a failover / a restart)
route() {
  cat << YAML | kubectl apply -f - > /dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: $1, namespace: $APP}
spec:
  parentRefs: [{name: acc, sectionName: http}]
  hostnames: [$1.example.com]
  rules: [{backendRefs: [{name: echo, port: 8080}]}]
YAML
}

kubectl -n "$APP" rollout status deploy/echo --timeout=300s
kubectl -n "$APP" wait --for=condition=Ready certificate/secure-cert --timeout=180s
kubectl -n "$APP" wait --for=condition=Programmed gateway/acc --timeout=300s
LB=$(kubectl -n "$APP" get gateway acc -o jsonpath='{.status.addresses[0].value}')
if [ "$MODE" = fleet ]; then
  # every worker runs a fleet pod; the Gateway (no spec.addresses) listens on the wildcard: the nodes'
  # addresses, the first in its status
  DEPLOY=-
  MANAGED_REPLICAS=$(echo "$WORKERS" | wc -w)
  kubectl -n "$NS" rollout status ds/rproxy --timeout=300s
elif [ "$MODE" = fleet-vip ]; then
  # every worker runs a fleet pod; the Gateway's address is the VIP
  DEPLOY=-
  MANAGED_REPLICAS=$(echo "$WORKERS" | wc -w)
  kubectl -n "$NS" rollout status ds/rproxy --timeout=300s
  [ "$LB" = "$VIP" ] || { echo "the Gateway's address is $LB, not the VIP $VIP"; exit 1; }
else
  DEPLOY=$(kubectl -n "$APP" get deploy -l gateway.networking.k8s.io/gateway-name=acc -o jsonpath='{.items[0].metadata.name}')
  log "managed rproxy Deployment: $DEPLOY"
  kubectl -n "$APP" rollout status "deploy/$DEPLOY" --timeout=300s
fi
if [ "$TOPOLOGY" = nodeport-lb ]; then
  # HAProxy (TCP) in front of every worker's node ports: health checks every 500 ms (down after 2
  # failures); a connection that fails marks the node down at once and is tried again on another
  # node (a node whose last rproxy pod has stopped drops connections: externalTrafficPolicy Local)
  nodeport() { kubectl -n "$APP" get svc "$DEPLOY" -o jsonpath="{.spec.ports[?(@.port==$1)].nodePort}"; }
  {
    printf 'global\n  log stdout format raw local0\ndefaults\n  mode tcp\n  log global\n  option log-health-checks\n'
    printf '  timeout connect 200ms\n  timeout client 30s\n  timeout server 30s\n  timeout check 500ms\n  retries 3\n  option redispatch 1\n'
    printf '  default-server inter 500ms fastinter 250ms downinter 500ms fall 2 rise 2 on-marked-down shutdown-sessions observe layer4 error-limit 1 on-error mark-down\n'
    for port in 80 443 9000; do
      np=$(nodeport $port)
      printf 'frontend f%s\n  bind :%s\n  default_backend b%s\nbackend b%s\n  balance roundrobin\n' "$port" "$port" "$port" "$port"
      for n in $WORKERS; do printf '  server %s %s:%s check\n' "$n" "$(kind_ip "$n")" "$np"; done
    done
  } > "$work/haproxy.cfg"
  chmod a+r "$work/haproxy.cfg"
  docker run -d --name "$CLUSTER-lb" --network kind -v "$work/haproxy.cfg:/usr/local/etc/haproxy/haproxy.cfg:ro" "$HAPROXY_IMAGE" > /dev/null
  LB=$(kind_ip "$CLUSTER-lb")
fi
log "Gateway address ($TOPOLOGY): $LB"

# ---------------------------------------------------------------- checks
# fleet-vip: the nodes with the VIP on an interface (a paused or stopped node is not listed)
vip_nodes() {
  local n
  for n in $WORKERS; do
    if docker exec "$n" ip -o addr show 2> /dev/null | grep -q " $VIP/"; then echo "$n"; fi
  done
}
vip_once() { [ "$(vip_nodes | wc -l)" -eq 1 ]; }
# vip_moved <node>: exactly one node other than <node> has the VIP
vip_moved() { local n; n=$(vip_nodes); [ -n "$n" ] && [ "$n" != "$1" ] && [ "$(echo "$n" | wc -l)" -eq 1 ]; }
vip_holder() { kubectl -n "$NS" get lease "$LEASE" -o jsonpath='{.spec.holderIdentity}' 2> /dev/null; }
pod_node() { kubectl -n "$RP_NS" get pod "$1" -o jsonpath='{.spec.nodeName}' 2> /dev/null; }
# fleet-vip: a UDP datagram to the VIP is answered (from the VIP: the client only takes answers from where it sent)
udp_ok() { [ "$(echo 'echo vip-udp' | timeout 3 nc -u -w 1 "$LB" 9001 2> /dev/null | head -c 7)" = vip-udp ]; }
http_code() { curl -s -o /dev/null -w '%{http_code}' --connect-timeout 1 --max-time 2 "$@" || true; }
lb_host_ok() { [ "$(http_code -H "Host: $1" "http://$LB/")" = 200 ]; }
# a pod's rule set: HTTP, HTTPS (verified against the CA) and TCP straight to the pod IP
pod_serves() {
  [ "$(http_code -H 'Host: echo.example.com' "http://$1/")" = 200 ] &&
    [ "$(http_code --cacert "$work/ca.crt" --resolve "secure.example.com:443:$1" https://secure.example.com/)" = 200 ] &&
    [ "$(http_code "http://$1:9000/")" = 200 ]
}
# live rproxy pods: name ip ready(true/false)
rproxy_pods() {
  kubectl -n "$RP_NS" get pods -l "$RP_SEL" -o json |
    jq -r '.items[] | select(.metadata.deletionTimestamp == null and .status.phase == "Running" and .status.podIP != null)
      | "\(.metadata.name) \(.status.podIP) \(([.status.conditions[]? | select(.type == "Ready")][0].status // "False") == "True")"'
}
all_pods_serve() {
  local n=0 name ip ready
  while read -r name ip ready; do
    [ -n "$name" ] || continue
    pod_serves "$ip" || return 1
    n=$((n + 1))
  done < <(rproxy_pods)
  [ "$n" -eq "$MANAGED_REPLICAS" ]
}
cond() {  # cond <kind/name> <jsonpath filter>
  kubectl -n "$APP" get "$1" -o jsonpath="$2"
}
# every listener Programmed (3; fleet-vip: 4 with the UDP one)
listeners_programmed() {
  local l
  l=$(cond gateway/acc '{.status.listeners[*].conditions[?(@.type=="Programmed")].status}')
  [ -n "$l" ] && ! tr ' ' '\n' <<< "$l" | grep -qvx True && [ "$(wc -w <<< "$l")" -ge 3 ]
}
statuses_ok() {
  [ "$(cond gateway/acc '{.status.conditions[?(@.type=="Programmed")].status}')" = True ] &&
    listeners_programmed &&
    [ "$(cond httproute/echo '{.status.parents[*].conditions[?(@.type=="Accepted")].status}')" = "True True" ] &&
    [ "$(cond httproute/echo '{.status.parents[*].conditions[?(@.type=="ResolvedRefs")].status}')" = "True True" ] &&
    [ "$(cond tcproute/echo '{.status.parents[*].conditions[?(@.type=="Accepted")].status}')" = True ]
}
leader() { kubectl -n "$NS" get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}'; }
leader_changed() { local h; h=$(leader); [ -n "$h" ] && [ "$h" != "$1" ]; }
controllers_ready() { kubectl -n "$NS" rollout status deploy/rproxy-gateway --timeout=5s; }
serial_of() {  # serial_of <ip>: the certificate served for secure.example.com
  openssl s_client -connect "$1:443" -servername secure.example.com < /dev/null 2> /dev/null | openssl x509 -noout -serial 2> /dev/null | cut -d= -f2
}
secret_serial() {
  kubectl -n "$APP" get secret secure-cert -o jsonpath='{.data.tls\.crt}' | base64 -d | openssl x509 -noout -serial | cut -d= -f2
}
secret_changed() { local s; s=$(secret_serial); [ -n "$s" ] && [ "$s" != "$1" ]; }

if [ "$MODE" = fleet-vip ]; then
  # UDP through the VIP (rproxy answers from the address the datagram came to): a UDP listener and an
  # echo backend (agnhost netexec answers "echo <text>" with <text>)
  cat << YAML | kubectl apply -f - > /dev/null
apiVersion: apps/v1
kind: Deployment
metadata: {name: udp-echo, namespace: $APP}
spec:
  replicas: 2
  selector: {matchLabels: {app: udp-echo}}
  template:
    metadata: {labels: {app: udp-echo}}
    spec:
      containers:
        - name: echo
          image: registry.k8s.io/e2e-test-images/agnhost:2.53
          args: [netexec, --http-port=8080, --udp-port=8081]
          readinessProbe: {httpGet: {path: /healthz, port: 8080}, periodSeconds: 2}
---
apiVersion: v1
kind: Service
metadata: {name: udp-echo, namespace: $APP}
spec:
  selector: {app: udp-echo}
  ports: [{name: udp, port: 8081, protocol: UDP}]
---
apiVersion: gateway.networking.k8s.io/v1alpha2
kind: UDPRoute
metadata: {name: udp-echo, namespace: $APP}
spec:
  parentRefs: [{name: acc, sectionName: udp}]
  rules: [{backendRefs: [{name: udp-echo, port: 8081}]}]
YAML
  kubectl -n "$APP" patch gateway acc --type=json -p '[{"op":"add","path":"/spec/listeners/-","value":{"name":"udp","port":9001,"protocol":"UDP"}}]' > /dev/null
  kubectl -n "$APP" rollout status deploy/udp-echo --timeout=300s
  wait_for 120 udp_ok || { echo "UDP through the VIP never answered"; exit 1; }
  log "UDP through the VIP answers"
fi

log "== baseline"
wait_for 180 all_pods_serve || { echo "the rproxy pods never served"; exit 1; }
wait_for 60 lb_host_ok echo.example.com || { echo "the Gateway address never served"; exit 1; }
statuses_ok || { echo "statuses not Programmed/Accepted"; exit 1; }
kubectl -n "$APP" get pods -o wide
kubectl -n "$NS" get pods -o wide
{
  echo "controller: $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=rproxy-gateway -o jsonpath='{.items[0].status.containerStatuses[0].imageID}')"
  echo "rproxy: $(kubectl -n "$RP_NS" get pods -l "$RP_SEL" -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="rproxy")].imageID}')"
} > "$work/images.txt"
cat "$work/images.txt"

# ---------------------------------------------------------------- traffic probes
# one request every 100 ms per protocol, new connection each time: "<epoch ms> <HTTP code>" (000: no answer)
probe() {
  local name=$1
  shift
  while :; do
    echo "$(now_ms) $(http_code "$@")" >> "$work/probe-$name.log"
    sleep 0.1
  done
}
probe http -H 'Host: echo.example.com' "http://$LB/" &
probe_pids+=($!)
probe https --cacert "$work/ca.crt" --resolve "secure.example.com:443:$LB" https://secure.example.com/ &
probe_pids+=($!)
probe tcp "http://$LB:9000/" &
probe_pids+=($!)

# ---------------------------------------------------------------- timelines
# what the cluster and the load balancer see, one stamped line per event: "<epoch ms> <kind> ..."
stamp() { while IFS= read -r l; do echo "$(now_ms) $l"; done; }
# watch <name> <function>: runs it again when it ends (watches time out)
watch_bg() {
  (while :; do "$2" 2> /dev/null || true; sleep 1; done) | stamp >> "$work/watch-$1.log" &
  probe_pids+=($!)
}
w_slices() {
  kubectl -n "$APP" get endpointslices -l "kubernetes.io/service-name=$DEPLOY" -w --output-watch-events -o json |
    jq -r --unbuffered '"slice " + ([.object.endpoints[]? | ((.targetRef.name // "?") | split("-") | last) + "@" + (.nodeName // "?") + "="
      + (if .conditions.ready then "ready" elif .conditions.serving then "serving" else "down" end)
      + (if .conditions.terminating then "/terminating" else "" end)] | sort | join(" "))'
}
w_pods() {
  kubectl -n "$RP_NS" get pods -l "$RP_SEL" -w --output-watch-events -o json |
    jq -r --unbuffered '"pod " + (.object.metadata.name | split("-") | last) + " @" + (.object.spec.nodeName // "-")
      + " Ready=" + ([.object.status.conditions[]? | select(.type == "Ready") | .status][0] // "-")
      + " gate=" + ([.object.status.conditions[]? | select(.type == "rproxy.max3584.net/ruleset-applied") | .status][0] // "-")
      + (if .type == "DELETED" then " deleted" elif .object.metadata.deletionTimestamp then " terminating" else "" end)'
}
w_l2() {
  kubectl get servicel2statuses.metallb.io -A -w --output-watch-events -o json |
    jq -r --unbuffered --arg s "$DEPLOY" 'select(.object.status.serviceName == $s) | "l2 " + .type + " " + (.object.status.node // "?")'
}
w_route() {
  docker exec "$CLUSTER-frr" sh -c "while :; do echo \"route \$(ip route show $LB/32 | grep -o 'via [0-9.]*' | sort | tr '\n' ' ')\"; sleep 0.2; done"
}
# fleet-vip: the VIP's Lease holder, and the nodes that have the VIP on an interface
w_lease() {
  kubectl -n "$NS" get lease "$LEASE" -w -o json | jq -r --unbuffered '"lease holder=" + ((.spec.holderIdentity // "-") | split("-") | last)'
}
w_vip() { while :; do echo "vip on $(vip_nodes | tr '\n' ' ')"; sleep 0.2; done; }
w_lb() { docker logs -f --since 1s "$CLUSTER-lb" 2>&1 | grep --line-buffered -E ' is (UP|DOWN)' | sed -u 's/^/lb /'; }
[ "$MODE" != managed ] || watch_bg slices w_slices
watch_bg pods w_pods
case "$TOPOLOGY" in
  fleet-vip) watch_bg lease w_lease && watch_bg vip w_vip ;;
  l2-*) watch_bg l2 w_l2 ;;
  bgp) watch_bg route w_route ;;
  nodeport-lb) watch_bg lb w_lb ;;
esac
# node name ↔ address (the router's next hops, HAProxy's servers)
for n in $(kubectl get nodes -o jsonpath='{.items[*].metadata.name}'); do echo "$n $(kind_ip "$n")"; done > "$work/nodes.txt"

# probe_events <from ms> <to ms>: a probe's turns between answering 200 and not
probe_events() {
  local p
  for p in http https tcp; do
    awk -v s="$1" -v e="$2" -v p="$p" '$1 >= s && $1 <= e {
      ok = ($2 == "200"); if (n++ == 0 || ok != was) print $1, "probe", p, (ok ? "ok" : "FAIL (" $2 ")"); was = ok }' "$work/probe-$p.log"
  done
}
# timeline <key> <from ms> <to ms>: every change in the window, relative to its start, into
# $work/timeline-<key>.txt; TIMELINE gets the first of each kind (for the notes)
timeline() {
  local f=$work/timeline-$1.txt
  # earlier lines only set the state a change is told against
  { cat "$work"/watch-*.log; probe_events "$2" "$3"; } | awk -v e="$3" '$1 <= e' | sort -s -n -k1,1 |
    awk -v s="$2" '{ key = ($2 == "pod" || $2 == "probe") ? $2 " " $3 : $2; rest = substr($0, length($1) + 2)
      if (last[key] == rest) next; last[key] = rest; if ($1 < s) next; printf "+%.2fs %s\n", ($1 - s) / 1000, rest }' > "$f"
  TIMELINE=$(awk '{ k = $2; if ($2 == "probe") k = ($4 == "ok") ? "recovered" : "failed"
      if (k == "recovered") last_ok = $1 " " $3; else if (!(k in seen)) { seen[k] = 1; out = out k " " $1 "; " } }
    END { if (last_ok) out = out "last probe recovered " last_ok; print out }' "$f")
}
sleep 3

# every probe answered 200 after <ms>
probes_ok_since() {
  local p last
  for p in http https tcp; do
    last=$(tail -n 1 "$work/probe-$p.log")
    [ "${last#* }" = 200 ] && [ "${last%% *}" -ge "$1" ] || return 1
  done
}
# window <probe> <from ms> <to ms> → "<requests> <failed> <longest gap ms without a 200>"
window() {
  awk -v s="$2" -v e="$3" '
    $1 >= s && $1 <= e { n++; if ($2 != "200") f++; else { g = $1 - (last ? last : s); if (g > max) max = g; last = $1 } }
    END { if (!last) max = e - s; else if (e - last > max) max = e - last; print n + 0, f + 0, max + 0 }' "$work/probe-$1.log"
}

# new_pods_serve <old pod names> <seconds>: waits until MANAGED_REPLICAS live pods serve; for every
# pod not in the old list, records when Kubernetes first saw it Ready and when its rule set first
# answered. Sets NEW_GAPS ("pod: ready→serving") and SERVED_AT (ms)
new_pods_serve() {
  local old=" $1 " end=$(($(now_ms) + $2 * 1000)) name ip ready n t
  declare -A ready_at=() serve_at=()
  NEW_GAPS="" SERVED_AT=0
  while :; do
    t=$(now_ms)
    n=0
    local all=1
    while read -r name ip ready; do
      [ -n "$name" ] || continue
      n=$((n + 1))
      if [[ $old != *" $name "* ]]; then
        if [ "$ready" = true ] && [ -z "${ready_at[$name]:-}" ]; then ready_at[$name]=$t; fi
        if [ -z "${serve_at[$name]:-}" ]; then
          if pod_serves "$ip"; then serve_at[$name]=$(now_ms); else all=0; fi
        fi
      else
        pod_serves "$ip" || all=0
      fi
    done < <(rproxy_pods)
    if [ "$all" = 1 ] && [ "$n" -eq "$MANAGED_REPLICAS" ]; then break; fi
    if [ "$(now_ms)" -ge "$end" ]; then return 1; fi
    sleep 0.25
  done
  SERVED_AT=$(now_ms)
  for name in "${!serve_at[@]}"; do
    local r=${ready_at[$name]:-}
    if [ -z "$r" ]; then
      # Ready only after serving (or seen Ready late): read it from the pod
      local lt
      lt=$(kubectl -n "$APP" get pod "$name" -o jsonpath='{.status.conditions[?(@.type=="Ready")].lastTransitionTime}' 2> /dev/null || true)
      [ -n "$lt" ] && r=$(($(date -d "$lt" +%s) * 1000))
    fi
    if [ -n "$r" ]; then
      NEW_GAPS+="${name##*-}: Ready→serving $(secs $((serve_at[$name] - r)))s; "
    fi
  done
}

failed=0
# record <scenario> <from ms> <to ms> <recovery> <result> <notes> [limit: fail above GAP_LIMIT]
record() {
  local h s t gap=0 req=0 fails=() p n f g worst=""
  for p in http https tcp; do
    read -r n f g < <(window "$p" "$2" "$3")
    req=$((req + n))
    fails+=("$f")
    if [ "$g" -gt "$gap" ]; then gap=$g worst=$p; fi
  done
  h=${fails[0]} s=${fails[1]} t=${fails[2]}
  local result=$5 notes=$6 key=${1%%.*}
  if [ "$result" = PASS ] && [ $((h + s + t)) -gt 0 ]; then result="PASS (outage)"; fi
  if [ -n "${7:-}" ] && [ "$MANAGED_REPLICAS" -ge 2 ] && [ "$gap" -gt $((GAP_LIMIT * 1000)) ]; then
    if [ "$TOPOLOGY" = bgp ]; then
      # BGP's convergence (route withdrawal, BFD) is the network's, not rproxy-gateway's: recorded only
      [ "$result" = FAIL ] || result="info (network-dependent)"
      notes="longest gap over ${GAP_LIMIT}s (BGP convergence: network-side, tune with BFD); $notes"
    else
      result=FAIL notes="longest gap over ${GAP_LIMIT}s; $notes"
    fi
  fi
  # STRICT: any failed request in these scenarios fails the run (not in bgp: recorded only)
  if [ "$STRICT" = true ] && [ -n "${7:-}" ] && [ "$MANAGED_REPLICAS" -ge 2 ] && [ "$TOPOLOGY" != bgp ] &&
    [ $((h + s + t)) -gt 0 ] && [ "$result" != FAIL ]; then
    result=FAIL notes="failed requests (STRICT); $notes"
  fi
  [ "$result" = FAIL ] && failed=1
  timeline "$key" "$2" "$3"
  notes+="; timeline: $TIMELINE"
  printf '%s\t%s/%s/%s of %s\t%s s (%s)\t%s\t%s\t%s\n' "$1" "$h" "$s" "$t" "$req" "$(secs "$gap")" "${worst:-–}" "$4" "$result" "$notes" >> "$results"
  log "RESULT $1: failed http/https/tcp $h/$s/$t of $req, longest gap $(secs "$gap") s ($worst), recovery $4, $result. $notes"
}
# settle: every probe and every pod back to normal before the next scenario
settle() {
  wait_for "$RECOVERY_TIMEOUT" all_pods_serve || return 1
  local t
  t=$(now_ms)
  wait_for "$RECOVERY_TIMEOUT" probes_ok_since "$t" || return 1
  # pods being deleted are gone (the next scenario starts from a clean placement)
  wait_for 120 none_terminating || true
  if [ "$MODE" = fleet-vip ]; then
    wait_for 60 vip_once || return 1
    wait_for 60 udp_ok || return 1
  fi
  sleep 5
}
none_terminating() {
  [ -z "$(kubectl get pods -A -l 'app.kubernetes.io/name in (rproxy, rproxy-gateway)' -o json | jq -r '.items[] | select(.metadata.deletionTimestamp != null) | .metadata.name')" ]
}
pod_gone() { ! kubectl -n "$APP" get pod "$1" > /dev/null 2>&1; }
# the node MetalLB announces the Gateway address from (ServiceL2Status; the last event otherwise)
announcer() {
  local n
  case "$TOPOLOGY" in l2-*) ;; *) echo "-"; return ;; esac
  n=$(kubectl get servicel2statuses.metallb.io -A -o json 2> /dev/null |
    jq -r --arg s "$DEPLOY" --arg ns "$APP" '.items[] | select(.status.serviceName == $s and .status.serviceNamespace == $ns) | .status.node' | head -1)
  if [ -z "$n" ]; then
    n=$(kubectl -n "$APP" get events --field-selector "involvedObject.name=$DEPLOY,reason=nodeAssigned" --sort-by=.lastTimestamp -o jsonpath='{.items[-1:].message}' 2> /dev/null |
      sed -n 's/.*node "\([^"]*\)".*/\1/p')
  fi
  echo "${n:-unknown}"
}
# live rproxy pods with their nodes: name node
rproxy_nodes() {
  kubectl -n "$RP_NS" get pods -l "$RP_SEL" -o json |
    jq -r '.items[] | select(.metadata.deletionTimestamp == null) | "\(.metadata.name) \(.spec.nodeName)"'
}
placement() {
  kubectl get pods -A -l 'app.kubernetes.io/name in (rproxy, rproxy-gateway)' -o custom-columns=NAME:.metadata.name,NODE:.spec.nodeName --no-headers | tr '\n' ' '
}

# ---------------------------------------------------------------- a. controller leader deleted
scenario_a() {
  local t0 old new t1 t_lead t_apply ok=PASS notes
  wait_for 120 controllers_ready
  old=$(leader)
  log "== a. delete the controller leader ($old)"
  t0=$(now_ms)
  kubectl -n "$NS" delete pod "$old" --wait=false > /dev/null
  if wait_for "$LEADER_TIMEOUT" leader_changed "$old"; then
    t_lead=$(now_ms)
    new=$(leader)
  else
    t_lead=$(now_ms) new="(none)" ok=FAIL
  fi
  route changed-a
  t1=$(now_ms)
  if wait_for "$APPLY_TIMEOUT" lb_host_ok changed-a.example.com; then t_apply=$(now_ms); else t_apply=$(now_ms) ok=FAIL; fi
  settle || ok=FAIL
  notes="new leader $new after $(secs $((t_lead - t0)))s; HTTPRoute created after the failover served after $(secs $((t_apply - t1)))s"
  record "a. controller leader pod deleted" "$t0" "$(now_ms)" "$(secs $((t_apply - t0)))s" "$ok" "$notes"
}

# ---------------------------------------------------------------- b. one rproxy pod deleted
# b1: a pod on another node than the one MetalLB announces from; b2: the pod on that node
scenario_b() {
  local which=$1 t0 old victim ok=PASS ann t_gone label
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  ann=$(announcer)
  if [ "$ann" = - ]; then
    # no single announcing node (BGP, an external balancer): b1 is any pod, b2 does not apply
    if [ "$which" != other ]; then
      printf '%s\t–\t–\t–\tSKIP\tno single announcing node in topology %s\n' "b2. rproxy pod deleted (on the announcing node)" "$TOPOLOGY" >> "$results"
      return 0
    fi
    victim=$(rproxy_nodes | awk '{print $1}' | head -1)
    label="b1. rproxy pod deleted"
  elif [ "$which" = other ]; then
    victim=$(rproxy_nodes | awk -v n="$ann" '$2 != n {print $1}' | head -1)
    label="b1. rproxy pod deleted (not on the announcing node)"
  else
    victim=$(rproxy_nodes | awk -v n="$ann" '$2 == n {print $1}' | head -1)
    label="b2. rproxy pod deleted (on the announcing node)"
  fi
  if [ -z "$victim" ]; then
    printf '%s\t–\t–\t–\tSKIP\tno rproxy pod %s the announcing node %s (placement: %s)\n' "$label" "$([ "$which" = other ] && echo off || echo on)" "$ann" "$(rproxy_nodes | tr '\n' ' ')" >> "$results"
    return 0
  fi
  log "== $label: $victim; MetalLB announces from $ann; placement: $(rproxy_nodes | tr '\n' ' ')"
  t0=$(now_ms)
  kubectl -n "$APP" delete pod "$victim" --wait=false > /dev/null
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  local served=$SERVED_AT
  [ "$served" -gt 0 ] || served=$(now_ms)
  wait_for 120 pod_gone "$victim" || true
  t_gone=$(now_ms)
  settle || ok=FAIL
  record "$label" "$t0" "$(now_ms)" "$(secs $((served - t0)))s" "$ok" \
    "replacement serving after $(secs $((served - t0)))s; deleted pod gone after $(secs $((t_gone - t0)))s; announcing node $ann → $(announcer); ${NEW_GAPS}" limit
}
scenario_b1() { scenario_b other; }
scenario_b2() { scenario_b announcing; }

# ---------------------------------------------------------------- c. node drained
scenario_c() {
  local t0 old node ld ok=PASS notes ann
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  ld=$(leader)
  ann=$(announcer)
  local ld_node rp_nodes
  ld_node=$(kubectl -n "$NS" get pod "$ld" -o jsonpath='{.spec.nodeName}' 2> /dev/null || true)
  rp_nodes=$(rproxy_nodes | awk '{print $2}')
  # the node MetalLB announces from (it has an rproxy pod: externalTrafficPolicy Local), else the
  # node with the most rproxy pods
  node=$(echo "$rp_nodes" | sort | uniq -c | sort -rn | awk '{print $2}' | head -1)
  if echo "$rp_nodes" | grep -qx "$ann"; then node=$ann; fi
  local on_node also=""
  on_node=$(echo "$rp_nodes" | grep -cx "$node" || true)
  if [ "$node" = "$ld_node" ]; then also=" and the controller leader"; fi
  if [ "$node" = "$ann" ]; then also+=", the announcing node"; fi
  notes="drained $node ($on_node of $MANAGED_REPLICAS rproxy pods$also)"
  log "== c. drain $node; placement: $(placement)"
  t0=$(now_ms)
  kubectl drain "$node" --ignore-daemonsets --delete-emptydir-data --timeout=300s > "$work/drain.log" 2>&1 || { ok=FAIL; notes+="; drain failed"; }
  local t_drained
  t_drained=$(now_ms)
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  local served=$SERVED_AT
  [ "$served" -gt 0 ] || served=$(now_ms)
  settle || ok=FAIL
  record "c. node drained" "$t0" "$(now_ms)" "$(secs $((served - t0)))s" "$ok" \
    "$notes; kubectl drain took $(secs $((t_drained - t0)))s; announcing node → $(announcer); ${NEW_GAPS}" limit
  kubectl uncordon "$node" > /dev/null
}

# ---------------------------------------------------------------- d. rollout restart
scenario_d() {
  local t0 old ok=PASS
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  log "== d. rollout restart deploy/$DEPLOY"
  t0=$(now_ms)
  kubectl -n "$APP" rollout restart "deploy/$DEPLOY" > /dev/null
  kubectl -n "$APP" rollout status "deploy/$DEPLOY" --timeout="${RECOVERY_TIMEOUT}s" > /dev/null || ok=FAIL
  local t_rolled
  t_rolled=$(now_ms)
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  local served=$SERVED_AT
  [ "$served" -gt 0 ] || served=$(now_ms)
  settle || ok=FAIL
  record "d. rollout restart (rproxy)" "$t0" "$(now_ms)" "$(secs $((served - t0)))s" "$ok" "rollout complete after $(secs $((t_rolled - t0)))s; ${NEW_GAPS}" limit
}

# ---------------------------------------------------------------- e. certificate renewal
all_pods_serial() {  # every pod serves serial $1
  local name ip ready
  while read -r name ip ready; do
    [ -n "$name" ] || continue
    [ "$(serial_of "$ip")" = "$1" ] || return 1
  done < <(rproxy_pods)
}
scenario_e() {
  local t0 before after t_secret t_served ok=PASS gen notes
  before=$(serial_of "$LB")
  log "== e. renew the certificate (served serial $before)"
  gen=$(kubectl -n "$APP" get certificate secure-cert -o jsonpath='{.metadata.generation}')
  t0=$(now_ms)
  # what `cmctl renew` does: the Issuing condition
  kubectl -n "$APP" patch certificate secure-cert --subresource=status --type=json -p "[{\"op\":\"add\",\"path\":\"/status/conditions/-\",\"value\":{\"type\":\"Issuing\",\"status\":\"True\",\"reason\":\"ManuallyTriggered\",\"message\":\"acceptance test\",\"lastTransitionTime\":\"$(date -u +%Y-%m-%dT%H:%M:%SZ)\",\"observedGeneration\":$gen}}]" > /dev/null
  if wait_for "$ROTATE_TIMEOUT" secret_changed "$before"; then
    t_secret=$(now_ms)
    after=$(secret_serial)
    if wait_for "$ROTATE_TIMEOUT" all_pods_serial "$after"; then t_served=$(now_ms); else t_served=$(now_ms) ok=FAIL; fi
    notes="serial $before → $after; Secret renewed after $(secs $((t_secret - t0)))s, served by every rproxy pod $(secs $((t_served - t_secret)))s later"
  else
    t_secret=$(now_ms) t_served=$t_secret ok=FAIL notes="the Secret was never renewed"
  fi
  settle || ok=FAIL
  record "e. TLS certificate renewal" "$t0" "$(now_ms)" "$(secs $((t_served - t0)))s" "$ok" "$notes"
}

# ---------------------------------------------------------------- f. everything restarted
scenario_f() {
  local t0 old ok=PASS t_routes t_status t_new
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  log "== f. delete both controller replicas and every rproxy pod; create an HTTPRoute meanwhile"
  t0=$(now_ms)
  kubectl -n "$NS" delete pod -l app.kubernetes.io/name=rproxy-gateway --wait=false > /dev/null
  kubectl -n "$APP" delete pod -l app.kubernetes.io/name=rproxy --wait=false > /dev/null
  route changed-f
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  t_routes=$SERVED_AT
  [ "$t_routes" -gt 0 ] || t_routes=$(now_ms)
  if wait_for "$RECOVERY_TIMEOUT" lb_host_ok changed-f.example.com; then t_new=$(now_ms); else t_new=$(now_ms) ok=FAIL; fi
  if wait_for "$RECOVERY_TIMEOUT" statuses_ok; then t_status=$(now_ms); else t_status=$(now_ms) ok=FAIL; fi
  # the HTTPRoute made while no controller ran has a status from the new leader
  [ "$(cond httproute/changed-f '{.status.parents[0].conditions[?(@.type=="Accepted")].status}')" = True ] || ok=FAIL
  lb_host_ok changed-a.example.com || ok=FAIL
  settle || ok=FAIL
  local rec=$t_routes
  for x in $t_new $t_status; do [ "$x" -gt "$rec" ] && rec=$x; done
  record "f. controllers + rproxy restarted together" "$t0" "$(now_ms)" "$(secs $((rec - t0)))s" "$ok" \
    "rule sets back on every pod after $(secs $((t_routes - t0)))s; HTTPRoute created meanwhile served after $(secs $((t_new - t0)))s; statuses Programmed/Accepted after $(secs $((t_status - t0)))s; leader $(leader); ${NEW_GAPS}"
}

# ---------------------------------------------------------------- g. node lost
# probes_stable <seconds>: every probe answered, and only with 200, in the last <seconds>
probes_stable() {
  local p since=$(($(now_ms) - $1 * 1000))
  for p in http https tcp; do
    awk -v s="$since" '$1 >= s { n++; if ($2 != "200") bad = 1 } END { exit (n > 0 && !bad) ? 0 : 1 }' "$work/probe-$p.log" || return 1
  done
}
node_ready() { [ "$(kubectl get node "$1" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')" = True ]; }
scenario_g() {
  local t0 node ann ok=PASS t_ok t_end ld ld_node
  ann=$(announcer)
  ld=$(leader)
  ld_node=$(kubectl -n "$NS" get pod "$ld" -o jsonpath='{.spec.nodeName}' 2> /dev/null || true)
  # the announcing node, else a node with an rproxy pod (not the controller leader's when possible)
  node=$(rproxy_nodes | awk -v n="$ann" '$2 == n {print $2}' | head -1)
  [ -n "$node" ] || node=$(rproxy_nodes | awk -v l="$ld_node" '$2 != l {print $2}' | head -1)
  [ -n "$node" ] || node=$(rproxy_nodes | awk '{print $2}' | head -1)
  log "== g. node $node lost (docker pause); placement: $(placement)"
  t0=$(now_ms)
  docker pause "$node" > /dev/null
  # failures show within a few seconds; then wait until there have been none for 5 s
  sleep 8
  if wait_for "$RECOVERY_TIMEOUT" probes_stable 5; then t_ok=$(($(now_ms) - 5000)); else t_ok=$(now_ms) ok=FAIL; fi
  t_end=$(now_ms)
  docker unpause "$node" > /dev/null
  wait_for 300 node_ready "$node" || ok=FAIL
  settle || ok=FAIL
  record "g. node lost (docker pause)" "$t0" "$t_end" "$(secs $((t_ok - t0)))s" "$ok" \
    "paused $node (announcing node: $ann; controller leader there: $([ "$node" = "$ld_node" ] && echo yes || echo no)); steady again (5 s without a failure) by $(secs $((t_ok - t0)))s; announcing node → $(announcer)"
}

# ---------------------------------------------------------------- h, i. RproxyGatewayParameters
# a namespace admin (the chart's ClusterRole aggregated to admin) writes the Gateway's parameters
params_crd() { kubectl get crd rproxygatewayparameters.rproxy.max3584.net > /dev/null 2>&1; }
as_tenant() { kubectl --as="system:serviceaccount:$APP:tenant" "$@"; }
tenant_params() {  # tenant_params <replicas> [more pod fields]: writes RproxyGatewayParameters acc as the tenant
  cat << YAML | as_tenant apply -f - > /dev/null
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyGatewayParameters
metadata: {name: acc, namespace: $APP}
spec:
  replicas: $1
  pod:
    resources: {rproxy: {requests: {cpu: 50m, memory: 64Mi}}}
    ${2:-}
YAML
}
gw_cond() { cond gateway/acc "{.status.conditions[?(@.type==\"$1\")].$2}"; }
gw_reason_is() { [ "$(gw_cond Accepted reason)" = "$1" ]; }
skip_params() {
  printf '%s\t–\t–\t–\tSKIP\tno RproxyGatewayParameters CRD (a chart before 0.4.2)\n' "$1" >> "$results"
}
scenario_h() {
  local label="h. Gateway parameters changed (replicas +1, resources)" t0 old ok=PASS want served
  params_crd || { skip_params "$label"; return 0; }
  kubectl -n "$APP" create serviceaccount tenant --dry-run=client -o yaml | kubectl apply -f - > /dev/null
  kubectl -n "$APP" create rolebinding tenant-admin --clusterrole=admin --serviceaccount="$APP:tenant" --dry-run=client -o yaml | kubectl apply -f - > /dev/null
  wait_for 60 sh -c "kubectl auth can-i create rproxygatewayparameters.rproxy.max3584.net -n $APP --as=system:serviceaccount:$APP:tenant | grep -qx yes" || return 1
  want=$((MANAGED_REPLICAS + 1))
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  log "== $label: replicas $MANAGED_REPLICAS → $want"
  t0=$(now_ms)
  tenant_params "$want" || ok=FAIL
  kubectl -n "$APP" patch gateway acc --type=merge \
    -p '{"spec":{"infrastructure":{"parametersRef":{"group":"rproxy.max3584.net","kind":"RproxyGatewayParameters","name":"acc"}}}}' > /dev/null
  MANAGED_REPLICAS=$want
  wait_for 60 sh -c "test \"\$(kubectl -n $APP get deploy $DEPLOY -o jsonpath='{.spec.replicas}')\" = $want" || ok=FAIL
  kubectl -n "$APP" rollout status "deploy/$DEPLOY" --timeout="${RECOVERY_TIMEOUT}s" > /dev/null || ok=FAIL
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  served=$SERVED_AT
  [ "$served" -gt 0 ] || served=$(now_ms)
  gw_reason_is Accepted || ok=FAIL
  [ "$(kubectl -n "$APP" get deploy "$DEPLOY" -o jsonpath='{.spec.template.spec.containers[0].resources.requests.cpu}')" = 50m ] || ok=FAIL
  settle || ok=FAIL
  wait_for 60 statuses_ok || ok=FAIL
  record "$label" "$t0" "$(now_ms)" "$(secs $((served - t0)))s" "$ok" "every pod replaced and serving after $(secs $((served - t0)))s; Accepted $(gw_cond Accepted status); ${NEW_GAPS}" limit
}
scenario_i() {
  local label="i. invalid Gateway parameters (the running rproxy is kept)" t0 gen ok=PASS t_bad t_fixed notes
  params_crd || { skip_params "$label"; return 0; }
  gen=$(kubectl -n "$APP" get deploy "$DEPLOY" -o jsonpath='{.metadata.generation}')
  log "== $label: tolerations (not allowed to tenants by default)"
  t0=$(now_ms)
  tenant_params "$MANAGED_REPLICAS" "tolerations: [{key: node-role.kubernetes.io/control-plane, operator: Exists}]" || ok=FAIL
  if wait_for 60 gw_reason_is InvalidParameters; then t_bad=$(now_ms); else t_bad=$(now_ms) ok=FAIL; fi
  # passes go on meanwhile: the Deployment must stay as it was, the rule set still applied
  route changed-i
  wait_for "$APPLY_TIMEOUT" lb_host_ok changed-i.example.com || { ok=FAIL; notes="a route change did not reach the kept rproxy; "; }
  sleep 10
  [ "$(gw_cond Programmed status)" = True ] || { ok=FAIL; notes+="Programmed $(gw_cond Programmed status); "; }
  [ "$(kubectl -n "$APP" get deploy "$DEPLOY" -o jsonpath='{.metadata.generation}')" = "$gen" ] || { ok=FAIL; notes+="the Deployment changed; "; }
  notes+="InvalidParameters after $(secs $((t_bad - t0)))s ($(gw_cond Accepted message)); Programmed $(gw_cond Programmed status); "
  tenant_params "$MANAGED_REPLICAS" || ok=FAIL
  if wait_for 60 gw_reason_is Accepted; then t_fixed=$(now_ms); else t_fixed=$(now_ms) ok=FAIL; fi
  settle || ok=FAIL
  [ "$(kubectl -n "$APP" get deploy "$DEPLOY" -o jsonpath='{.metadata.generation}')" = "$gen" ] || { ok=FAIL; notes+="fixed: the Deployment changed; "; }
  record "$label" "$t0" "$(now_ms)" "$(secs $((t_fixed - t0)))s" "$ok" "${notes}accepted again $(secs $((t_fixed - t_bad)))s after the fix" limit
}

# ---------------------------------------------------------------- j-o. fleet-vip (MODE=fleet-vip)
# The VIP's holder: the pod named in its Lease and that pod's node; a scenario first waits for the VIP
# to be on exactly one node (moved: on another one)
vip_where() {
  HOLDER=$(vip_holder)
  HOLDER_NODE=$(pod_node "$HOLDER")
}
only_fleet() {  # only_fleet <label>: SKIP outside MODE=fleet-vip
  [ "$MODE" = fleet-vip ] && return 0
  printf '%s\t–\t–\t–\tSKIP\tMODE=fleet-vip only\n' "$1" >> "$results"
  return 1
}
# moved <from node> <since ms>: waits for the VIP on one other node; MOVED_AT (ms), MOVED_TO
moved() {
  if wait_for 60 vip_moved "$1"; then MOVED_AT=$(now_ms); else MOVED_AT=$(now_ms) MOVED_TO="(not moved)"; return 1; fi
  MOVED_TO=$(vip_nodes | tr '\n' ' ')
}
scenario_j() {
  local label="j. VIP holder pod deleted" t0 old ok=PASS
  only_fleet "$label" || return 0
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  vip_where
  log "== $label: $HOLDER on $HOLDER_NODE"
  t0=$(now_ms)
  kubectl -n "$NS" delete pod "$HOLDER" --wait=false > /dev/null
  moved "$HOLDER_NODE" || ok=FAIL
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  settle || ok=FAIL
  record "$label" "$t0" "$(now_ms)" "$(secs $((MOVED_AT - t0)))s" "$ok" \
    "VIP $HOLDER_NODE → ${MOVED_TO}after $(secs $((MOVED_AT - t0)))s (seen by polling the nodes every ~0.5 s); holder now $(vip_holder); ${NEW_GAPS}" limit
}
scenario_k() {
  local label="k. rollout restart (fleet DaemonSet)" t0 old ok=PASS t_rolled
  only_fleet "$label" || return 0
  old=$(rproxy_pods | awk '{print $1}' | tr '\n' ' ')
  vip_where
  log "== $label; VIP on $HOLDER_NODE"
  t0=$(now_ms)
  kubectl -n "$NS" rollout restart ds/rproxy > /dev/null
  kubectl -n "$NS" rollout status ds/rproxy --timeout="${RECOVERY_TIMEOUT}s" > /dev/null || ok=FAIL
  t_rolled=$(now_ms)
  new_pods_serve "$old" "$RECOVERY_TIMEOUT" || ok=FAIL
  settle || ok=FAIL
  record "$label" "$t0" "$(now_ms)" "$(secs $((t_rolled - t0)))s" "$ok" \
    "rollout complete after $(secs $((t_rolled - t0)))s; VIP $HOLDER_NODE → $(vip_nodes | tr '\n' ' ')(holder $(vip_holder)); ${NEW_GAPS}" limit
}
scenario_l() {
  local label="l. VIP holder's node drained (cordoned: the VIP moves)" t0 ok=PASS t_drained
  only_fleet "$label" || return 0
  vip_where
  log "== $label: $HOLDER_NODE; placement: $(placement)"
  t0=$(now_ms)
  # the VIP moves when the node is cordoned, while the drain evicts the other pods
  kubectl drain "$HOLDER_NODE" --ignore-daemonsets --delete-emptydir-data --timeout=300s > "$work/drain.log" 2>&1 &
  local drain=$!
  moved "$HOLDER_NODE" || ok=FAIL
  wait "$drain" || ok=FAIL
  t_drained=$(now_ms)
  settle || ok=FAIL
  record "$label" "$t0" "$(now_ms)" "$(secs $((MOVED_AT - t0)))s" "$ok" \
    "VIP $HOLDER_NODE → ${MOVED_TO}after $(secs $((MOVED_AT - t0)))s; kubectl drain took $(secs $((t_drained - t0)))s (the fleet's pods stay: a DaemonSet)" limit
  kubectl uncordon "$HOLDER_NODE" > /dev/null
}
# first_ok <from ms>: seconds from <from> to the first 200 after the first failure (any probe)
first_ok() {
  local p t best=""
  for p in http https tcp; do
    t=$(awk -v s="$1" '$1 >= s { if ($2 != "200") bad = 1; else if (bad) { print $1; exit } }' "$work/probe-$p.log")
    if [ -n "$t" ] && { [ -z "$best" ] || [ "$t" -lt "$best" ]; }; then best=$t; fi
  done
  if [ -n "$best" ]; then secs $((best - $1)); else echo "–"; fi
}
# node_lost <kill|pause> <label>: the VIP holder's node killed or paused (the Lease expires: record only)
node_lost() {
  local how=$1 label=$2 t0 ok=PASS t_ok t_end notes node
  vip_where
  node=$HOLDER_NODE
  log "== $label: $node ($HOLDER); placement: $(placement)"
  t0=$(now_ms)
  docker "$how" "$node" > /dev/null
  moved "$node" || ok=FAIL
  sleep 3
  if wait_for "$RECOVERY_TIMEOUT" probes_stable 5; then t_ok=$(($(now_ms) - 5000)); else t_ok=$(now_ms) ok=FAIL; fi
  t_end=$(now_ms)
  notes="VIP $node → ${MOVED_TO}after $(secs $((MOVED_AT - t0)))s; first 200 again after $(first_ok "$t0")s; steady again (5 s without a failure) by $(secs $((t_ok - t0)))s (requests rproxy sends to the echo pod on that node fail until the node is NotReady)"
  if [ "$how" = pause ]; then
    sleep 15
    docker unpause "$node" > /dev/null
    # the paused holder comes back believing it holds the VIP: it must let go (its Lease is another's)
    local t_back
    t_back=$(now_ms)
    sleep 1
    if wait_for 30 vip_once; then
      notes+="; unpaused after $(secs $((t_back - t0)))s: on one node again $(secs $(($(now_ms) - t_back)))s later ($(vip_nodes | tr '\n' ' '))"
    else
      ok=FAIL notes+="; unpaused: the VIP stayed on $(vip_nodes | tr '\n' ' ')"
    fi
  else
    docker start "$node" > /dev/null
    # the address the node comes back with (docker may give it another one)
    notes+="; started again as $(kind_ip "$node")"
  fi
  wait_for 300 node_ready "$node" || ok=FAIL
  settle || ok=FAIL
  record "$label" "$t0" "$t_end" "$(secs $((t_ok - t0)))s" "$ok" "$notes"
}
scenario_m() {
  # docker kill: lost at once (docker stop shuts the node down cleanly: its pods stop and the VIP is
  # handed over like a drain)
  local label="m. VIP holder's node lost (docker kill)"
  only_fleet "$label" || return 0
  node_lost kill "$label"
}
scenario_n() {
  local label="n. VIP holder's node paused (docker pause, then unpause)"
  only_fleet "$label" || return 0
  node_lost pause "$label"
}
# q. MODE=fleet, fleet.listen=addresses: see the header. Both Gateways must be Programmed and each
# address must reach its own Gateway's backend (HTTP, HTTPS, UDP); the third Gateway gets
# PortUnavailable; acc (the wildcard, other ports) is not disturbed (GAP_LIMIT)
q_gateways_programmed() {
  local g
  for g in one two; do
    [ "$(kubectl -n q get gateway "$g" -o jsonpath='{.status.conditions[?(@.type=="Programmed")].status}')" = True ] &&
      [ "$(kubectl -n q get gateway "$g" -o jsonpath='{.status.listeners[*].conditions[?(@.type=="Programmed")].status}')" = "True True True" ] || return 1
  done
}
ns_of() { curl -s --connect-timeout 1 --max-time 2 "$@" | jq -r '.namespace // empty' 2> /dev/null; }
# q_serves <address> <tag>: HTTP, HTTPS and UDP on the address reach the echo pods tagged <tag>
q_serves() {
  [ "$(ns_of -H 'Host: echo.example.com' "http://$1:8080/")" = "$2" ] &&
    [ "$(ns_of --cacert "$work/ca.crt" --resolve "q.example.com:8443:$1" https://q.example.com:8443/)" = "$2" ] &&
    [ "$(echo "echo $2" | timeout 3 nc -u -w 1 "$1" 9002 2> /dev/null | head -c ${#2})" = "$2" ]
}
q_port_refused() {
  [ "$(kubectl -n q get gateway three -o jsonpath='{.status.listeners[0].conditions[?(@.type=="Accepted")].reason}')" = PortUnavailable ]
}
scenario_q() {
  local label="q. two Gateways on the same ports on two addresses (fleet.listen=addresses)" t0 ok=PASS t_prog t_serve notes msg node
  if [ "$MODE" != fleet ]; then
    printf '%s\t–\t–\t–\tSKIP\tMODE=fleet only\n' "$label" >> "$results"
    return 0
  fi
  node=$(echo "$WORKERS" | awk '{print $1}')
  log "== $label: $ADDR1 and $ADDR2 on $node"
  t0=$(now_ms)
  kubectl create namespace q --dry-run=client -o yaml | kubectl apply -f - > /dev/null
  {
    cat << YAML
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: q-cert, namespace: q}
spec:
  secretName: q-cert
  dnsNames: [q.example.com]
  privateKey: {algorithm: ECDSA, size: 256}
  issuerRef: {name: acc-ca, kind: ClusterIssuer, group: cert-manager.io}
YAML
    local g addr
    for g in one two; do
      [ "$g" = one ] && addr=$ADDR1 || addr=$ADDR2
      cat << YAML
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo-$g, namespace: q}
spec:
  replicas: 2
  selector: {matchLabels: {app: echo-$g}}
  template:
    metadata: {labels: {app: echo-$g}}
    spec:
      containers:
        - name: echo
          image: registry.k8s.io/gateway-api/echo-basic:v1.5.1
          env:
            - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
            - {name: NAMESPACE, value: $g}
          readinessProbe: {httpGet: {path: /, port: 3000}, periodSeconds: 2}
        - name: udp
          image: registry.k8s.io/e2e-test-images/agnhost:2.53
          args: [netexec, --http-port=8081, --udp-port=8082]
---
apiVersion: v1
kind: Service
metadata: {name: echo-$g, namespace: q}
spec:
  selector: {app: echo-$g}
  ports: [{name: http, port: 8080, targetPort: 3000}, {name: udp, port: 8082, protocol: UDP}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: $g, namespace: q}
spec:
  gatewayClassName: rproxy
  addresses: [{type: IPAddress, value: $addr}]
  listeners:
    - {name: http, port: 8080, protocol: HTTP}
    - {name: https, port: 8443, protocol: HTTPS, hostname: q.example.com, tls: {certificateRefs: [{name: q-cert}]}}
    - {name: udp, port: 9002, protocol: UDP}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: $g, namespace: q}
spec:
  parentRefs: [{name: $g, sectionName: http}, {name: $g, sectionName: https}]
  hostnames: [echo.example.com, q.example.com]
  rules: [{backendRefs: [{name: echo-$g, port: 8080}]}]
---
apiVersion: gateway.networking.k8s.io/v1alpha2
kind: UDPRoute
metadata: {name: $g, namespace: q}
spec:
  parentRefs: [{name: $g, sectionName: udp}]
  rules: [{backendRefs: [{name: echo-$g, port: 8082}]}]
YAML
    done
  } | kubectl apply -f - > /dev/null
  kubectl -n q rollout status deploy/echo-one deploy/echo-two --timeout=300s > /dev/null || ok=FAIL
  if wait_for "$APPLY_TIMEOUT" q_gateways_programmed; then t_prog=$(now_ms); else t_prog=$(now_ms) ok=FAIL; fi
  # the platform's part (MetalLB, kube-vip, keepalived...): the addresses on one node
  docker exec "$node" ip addr add "$ADDR1/32" dev eth0
  docker exec "$node" ip addr add "$ADDR2/32" dev eth0
  if wait_for "$APPLY_TIMEOUT" q_serves "$ADDR1" one && wait_for 30 q_serves "$ADDR2" two; then t_serve=$(now_ms); else t_serve=$(now_ms) ok=FAIL; fi
  statuses_ok || ok=FAIL
  notes="both Gateways Programmed after $(secs $((t_prog - t0)))s (before the addresses were on any node); $ADDR1 → one, $ADDR2 → two (HTTP, HTTPS, UDP) $(secs $((t_serve - t0)))s after the start"
  # the same port on the first address: refused on that listener
  cat << YAML | kubectl apply -f - > /dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: three, namespace: q}
spec:
  gatewayClassName: rproxy
  addresses: [{type: IPAddress, value: $ADDR1}]
  listeners: [{name: http, port: 8080, protocol: HTTP}]
YAML
  if wait_for 60 q_port_refused; then
    msg=$(kubectl -n q get gateway three -o jsonpath='{.status.listeners[0].conditions[?(@.type=="Accepted")].message}')
    notes+="; three on $ADDR1:8080: PortUnavailable ($msg)"
  else
    ok=FAIL notes+="; three on $ADDR1:8080 not refused: $(kubectl -n q get gateway three -o jsonpath='{.status.listeners[0].conditions}')"
  fi
  if ! q_serves "$ADDR1" one || ! statuses_ok; then ok=FAIL notes+="; disturbed by three"; fi
  kubectl delete namespace q --wait=true --timeout=180s > /dev/null || true
  docker exec "$node" ip addr del "$ADDR1/32" dev eth0 || true
  docker exec "$node" ip addr del "$ADDR2/32" dev eth0 || true
  settle || ok=FAIL
  record "$label" "$t0" "$(now_ms)" "$(secs $((t_serve - t0)))s" "$ok" "$notes" limit
}
scenario_o() {
  local label="o. control plane paused (hold: the VIP stays)" t0 ok=PASS cp t_back notes before
  only_fleet "$label" || return 0
  cp=$(kubectl get nodes -l node-role.kubernetes.io/control-plane -o jsonpath='{.items[0].metadata.name}')
  vip_where
  before=$(vip_nodes | tr '\n' ' ')
  log "== $label: $cp for 20 s; VIP on $before"
  t0=$(now_ms)
  docker pause "$cp" > /dev/null
  sleep 20
  notes="during the pause the VIP was on $(vip_nodes | tr '\n' ' ')"
  docker unpause "$cp" > /dev/null
  t_back=$(now_ms)
  wait_for 120 kubectl get --raw /readyz || ok=FAIL
  sleep 10
  settle || ok=FAIL
  notes+="; API server back $(secs $(($(now_ms) - t_back)))s after the unpause; VIP $before→ $(vip_nodes | tr '\n' ' ')(holder $(vip_holder))"
  record "$label" "$t0" "$(now_ms)" "–" "$ok" "$notes" limit
}

# ---------------------------------------------------------------- u. the UI (UI=true)
# The UI chart of UI_DIR (a checkout of TCP-UDP-rproxy-ui; its image built from it) with the bundled
# MariaDB, reading the Gateway's rproxy pods through the controller's discovery Secret (ui.namespace).
# Checks: the UI lists the Gateway's pods read-only to an admin and refuses changes (409 readonly_node),
# the UI token reads but cannot write (rproxy 403), usage rows appear, and a UI pod restart and a
# MariaDB pod restart keep the data (PVC). Timings are recorded only.
UI_NS=rproxy-ui
ui_install() {
  log "== UI image from $UI_DIR ($(git -C "$UI_DIR" rev-parse --short HEAD 2> /dev/null || echo ?))"
  docker build -q -t rproxy-ui:acc "$UI_DIR"
  kind load docker-image --name "$CLUSTER" rproxy-ui:acc
  kubectl create namespace "$UI_NS" --dry-run=client -o yaml | kubectl apply -f - > /dev/null
  # secrets made here and never stored: the session secret also signs the test's admin session
  UI_SESSION_SECRET=$(openssl rand -hex 32)
  UI_ROOT_PW=$(openssl rand -hex 16)
  kubectl -n "$UI_NS" create secret generic rproxy-ui --from-literal=NEXTAUTH_SECRET="$UI_SESSION_SECRET" \
    --from-literal=KEYCLOAK_CLIENT_SECRET=unused --from-literal=DB_PASSWORD="$(openssl rand -hex 16)" > /dev/null
  kubectl -n "$UI_NS" create secret generic rproxy-ui-mariadb --from-literal=MARIADB_ROOT_PASSWORD="$UI_ROOT_PW" > /dev/null
  log "== helm install rproxy-ui ($UI_DIR/charts/rproxy-ui, bundled MariaDB)"
  local t0
  t0=$(now_ms)
  helm install rproxy-ui "$UI_DIR/charts/rproxy-ui" -n "$UI_NS" --wait --timeout 10m \
    --set image.repository=rproxy-ui --set image.tag=acc --set image.pullPolicy=Never \
    --set replicas=1 --set url=http://rproxy-ui.acc.invalid \
    --set keycloak.issuer=http://keycloak.acc.invalid/realms/rproxy \
    --set existingSecret=rproxy-ui --set mariadb.enabled=true --set mariadb.existingSecret=rproxy-ui-mariadb \
    --set mariadb.persistence.size=1Gi --set rproxy.discovery.enabled=true --set usage.intervalSeconds="$UI_USAGE_SECS"
  UI_INSTALL_SECS=$(secs $(($(now_ms) - t0)))
  kubectl -n "$UI_NS" get pods,pvc,secret -o wide
}

# an admin's NextAuth session cookie (next-auth v4: JWE dir + A256GCM, key HKDF-SHA256 of NEXTAUTH_SECRET)
ui_session() {
  UI_SECRET="$UI_SESSION_SECRET" node -e '
    const c = require("node:crypto");
    const b64 = (b) => Buffer.from(b).toString("base64url");
    const key = Buffer.from(c.hkdfSync("sha256", process.env.UI_SECRET, "", "NextAuth.js Generated Encryption Key", 32));
    const now = Math.floor(Date.now() / 1000);
    const claims = { sub: "acceptance-admin", name: "acceptance", roles: ["rproxy-admin"], iat: now, exp: now + 7200, jti: c.randomUUID() };
    const header = b64(JSON.stringify({ alg: "dir", enc: "A256GCM" }));
    const iv = c.randomBytes(12);
    const g = c.createCipheriv("aes-256-gcm", key, iv);
    g.setAAD(Buffer.from(header));
    const ct = Buffer.concat([g.update(JSON.stringify(claims)), g.final()]);
    process.stdout.write([header, "", b64(iv), b64(ct), b64(g.getAuthTag())].join("."));'
}
ui_pod() { kubectl -n "$UI_NS" get pods -l app.kubernetes.io/name=rproxy-ui,app.kubernetes.io/component=ui -o json | jq -r '[.items[] | select(.metadata.deletionTimestamp == null and .status.podIP != null and ([.status.conditions[]? | select(.type == "Ready")][0].status == "True"))][0] | "\(.metadata.name) \(.status.podIP)"'; }
# ui_get <path>: GET on the UI pod as the admin (the runner reaches pod IPs)
ui_get() {
  local ip
  ip=$(ui_pod | cut -d' ' -f2)
  [ -n "$ip" ] && [ "$ip" != null ] && curl -sf --max-time 10 -H "Cookie: next-auth.session-token=$UI_COOKIE" "http://$ip:3000$1"
}
ui_post_code() {
  local ip
  ip=$(ui_pod | cut -d' ' -f2)
  curl -s -o "$work/ui-post.json" -w '%{http_code}' --max-time 10 -H "Cookie: next-auth.session-token=$UI_COOKIE" \
    -H 'Content-Type: application/json' -d "$2" "http://$ip:3000$1" || true
}
ui_sql() {
  kubectl -n "$UI_NS" exec rproxy-ui-mariadb-0 -- env MYSQL_PWD="$UI_ROOT_PW" mariadb -uroot -N -B rproxy -e "$1"
}
discovery_lists() {  # the Secret names every running rproxy pod of the Gateway
  local y
  y=$(kubectl -n "$UI_NS" get secret rproxy-ui-discovery -o jsonpath='{.data.nodes\.yaml}' | base64 -d) || return 1
  local name ip ready
  while read -r name ip ready; do
    [ -n "$name" ] || continue
    grep -Eq "name: \"?k8s:$APP/acc/$name\"?\$" <<< "$y" || return 1
  done < <(rproxy_pods)
}
ui_lists() {  # the UI shows the Gateway's pods read-only
  local n
  n=$(ui_get /api/forward/nodes | jq '[.nodes[] | select(.readonly == true and (.name | startswith("k8s:'"$APP"'/acc/")))] | length') || return 1
  [ "$n" -eq "$MANAGED_REPLICAS" ]
}
ui_rules() {  # the Gateway's rules on the dashboard, marked read-only
  ui_get /api/forward/dashboard | jq -e '[.rules[] | select(.readonlyNode == true and .ruleset == "k8s/'"$APP"'/acc")] | length > 0' > /dev/null
}
usage_rows() { ui_sql "SELECT COUNT(*) FROM usage_hourly WHERE node = 'k8s:$APP/acc' AND origin = 'ruleset' AND rx_bytes > 0"; }
usage_bytes() { ui_sql "SELECT COALESCE(SUM(rx_bytes + tx_bytes), 0) FROM usage_hourly WHERE node = 'k8s:$APP/acc'"; }
usage_seen() { [ "$(usage_rows 2> /dev/null || echo 0)" -gt 0 ]; }
usage_grew() { [ "$(usage_bytes 2> /dev/null || echo 0)" -gt "$1" ]; }
ui_healthy() { [ -n "$(ui_get /api/healthz)" ]; }
mariadb_ready() { [ "$(kubectl -n "$UI_NS" get pod rproxy-ui-mariadb-0 -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2> /dev/null)" = True ]; }
# the UI token from the Secret: reads (GET /rules 200) and cannot write (PUT /rulesets/... 403), from the UI pod
# (the Gateway's NetworkPolicy lets the UI namespace's UI pods reach 9443)
ui_token_check() {
  local pod
  pod=$(ui_pod | cut -d' ' -f1)
  kubectl -n "$UI_NS" exec "$pod" -- node -e '
    const fs = require("node:fs");
    const { parse } = require("/app/node_modules/yaml");
    const { Agent, fetch } = require("/app/node_modules/undici");
    const dir = "/etc/rproxy-ui/k8s";
    const n = parse(fs.readFileSync(dir + "/nodes.yaml", "utf8")).nodes[0];
    const token = fs.readFileSync(dir + "/" + n.token_file, "utf8").trim();
    const dispatcher = new Agent({ connect: { ca: fs.readFileSync(dir + "/" + n.tls_ca), servername: n.tls_server_name } });
    const h = { Authorization: "Bearer " + token, "Content-Type": "application/json" };
    (async () => {
      const read = await fetch(n.url + "/rules", { headers: h, dispatcher });
      const write = await fetch(n.url + "/rulesets/k8s/acceptance-ui", { method: "PUT", headers: h, body: JSON.stringify({ rules: [] }), dispatcher });
      console.log(read.status + " " + write.status);
    })().catch((e) => { console.log("error " + e.message); });'
}

scenario_u() {
  local t0 t_secret t_listed t_usage ok=PASS notes="" codes before t_r t_ui t_db rows_before rows_after
  ui_install
  UI_COOKIE=$(ui_session)
  t0=$(now_ms)
  log "== u. the UI lists the Gateway's rproxy read-only"
  if wait_for 180 discovery_lists; then t_secret=$(now_ms); else ok=FAIL notes+="the discovery Secret never listed the pods; "; t_secret=$(now_ms); fi
  # the kubelet updates the mounted Secret within a minute or two
  if wait_for 240 ui_lists; then t_listed=$(now_ms); else ok=FAIL notes+="the UI never listed the pods read-only; "; t_listed=$(now_ms); fi
  wait_for 60 ui_rules || { ok=FAIL notes+="the Gateway's rules are not on the dashboard read-only; "; }
  codes=$(ui_post_code /api/forward/api-delete "{\"protocol\":\"tcp\",\"srcAddr\":\"0.0.0.0\",\"srcPort\":80,\"target\":\"$(ui_get /api/forward/nodes | jq -r '[.nodes[] | select(.readonly == true)][0].name')\"}")
  if [ "$codes" != 409 ] || ! jq -e '.code == "readonly_node"' "$work/ui-post.json" > /dev/null; then ok=FAIL notes+="a change to a pod was not refused (HTTP $codes $(cat "$work/ui-post.json")); "; fi
  codes=$(ui_token_check 2>&1 | tail -1)
  if [ "$codes" != "200 403" ]; then ok=FAIL notes+="UI token: GET /rules and PUT /rulesets gave '$codes' (want 200 403); "; fi
  log "== u. usage rows (every ${UI_USAGE_SECS}s)"
  if wait_for $((UI_USAGE_SECS * 6 + 60)) usage_seen; then t_usage=$(now_ms); else ok=FAIL notes+="no usage rows for k8s:$APP/acc; "; t_usage=$(now_ms); fi
  notes+="install ${UI_INSTALL_SECS}s; Secret $(secs $((t_secret - t0)))s, listed in the UI $(secs $((t_listed - t0)))s, first usage row $(secs $((t_usage - t0)))s after install; "
  log "== u. UI pod and MariaDB pod restarted: the data stays (PVC)"
  rows_before=$(ui_sql 'SELECT COUNT(*) FROM schema_migrations')
  before=$(usage_bytes)
  t_r=$(now_ms)
  kubectl -n "$UI_NS" delete pod "$(ui_pod | cut -d' ' -f1)" --wait=false > /dev/null
  kubectl -n "$UI_NS" delete pod rproxy-ui-mariadb-0 --wait=false > /dev/null
  sleep 2
  if wait_for 300 mariadb_ready; then t_db=$(now_ms); else ok=FAIL notes+="MariaDB not ready again; "; t_db=$(now_ms); fi
  if wait_for 300 ui_healthy && wait_for 240 ui_lists; then t_ui=$(now_ms); else ok=FAIL notes+="the UI did not come back; "; t_ui=$(now_ms); fi
  rows_after=$(ui_sql 'SELECT COUNT(*) FROM schema_migrations' 2> /dev/null || echo 0)
  if [ "$rows_after" != "$rows_before" ] || [ "$(usage_bytes 2> /dev/null || echo 0)" -lt "$before" ]; then
    ok=FAIL notes+="data lost over the restarts (schema_migrations $rows_before -> $rows_after, usage bytes $before -> $(usage_bytes 2> /dev/null || echo ?)); "
  fi
  wait_for $((UI_USAGE_SECS * 6 + 60)) usage_grew "$before" || { ok=FAIL notes+="usage did not grow after the restarts; "; }
  notes+="after the restarts: MariaDB ready $(secs $((t_db - t_r)))s, UI listing again $(secs $((t_ui - t_r)))s (record only)"
  record "u. UI (bundled MariaDB): read-only k8s rproxy, usage, restarts" "$t0" "$(now_ms)" "$(secs $((t_listed - t0)))s" "$ok" "$notes"
  kubectl -n "$UI_NS" logs -l app.kubernetes.io/name=rproxy-ui --all-containers --prefix --tail=-1 > "$work/ui.log" 2>&1 || true
}

for s in $SCENARIOS; do
  if ! "scenario_$s"; then
    echo "scenario $s aborted" >&2
    printf '%s\t–\t–\t–\tFAIL\taborted\n' "$s" >> "$results"
    failed=1
  fi
done
stop_probes

# ---------------------------------------------------------------- summary
{
  if [ "$SOURCE" = checkout ]; then
    echo "## rproxy-gateway acceptance: $TOPOLOGY (this checkout: $(git rev-parse --short HEAD 2> /dev/null || echo ?))"
  else
    echo "## rproxy-gateway acceptance: $TOPOLOGY (chart $CHART_VERSION, published images)"
  fi
  echo
  case "$TOPOLOGY" in
    l2-local) echo "MetalLB $METALLB_VERSION L2, externalTrafficPolicy Local." ;;
    l2-cluster) echo "MetalLB $METALLB_VERSION L2, externalTrafficPolicy Cluster." ;;
    bgp) echo "MetalLB $METALLB_VERSION BGP (FRR mode, BFD 300 ms x 3) to an FRR router ($FRR_IMAGE) with ECMP over the announcing nodes (externalTrafficPolicy $ETP)." ;;
    nodeport-lb) echo "NodePort Services (externalTrafficPolicy $ETP) behind HAProxy ($HAPROXY_IMAGE, TCP, checks every 500 ms, a failed connection marks the node down and goes to another)." ;;
    fleet) echo "fleet mode (a DaemonSet with hostNetwork on every worker) without VIPs, fleet.listen=addresses; scenario q's addresses $ADDR1 $ADDR2 put on one worker by hand (ip addr); rproxy image: $RPROXY_IMAGE_FROM." ;;
    fleet-vip) echo "fleet mode (a DaemonSet with hostNetwork on every worker) with the VIP $VIP held by the fleet's pods directly (the vip sidecar, Lease $LEASE; no Service or load balancer). MetalLB L2 for comparison: the managed runs (l2-local, l2-cluster; docs/DESIGN-v0.4.x.md 6.2)." ;;
  esac
  echo "kind 1 control plane + 3 workers, cert-manager $CERT_MANAGER_VERSION, Gateway API $GATEWAY_API_VERSION experimental."
  echo "Controller replicas 2, managed.replicas $MANAGED_REPLICAS. Extra helm flags: \`${HELM_ARGS:-none}\`. Probes: HTTP, HTTPS, TCP through the Gateway's address every 100 ms, a new connection each."
  echo
  echo '```'
  cat "$work/images.txt"
  echo '```'
  echo
  echo "| scenario | failed requests (HTTP/HTTPS/TCP) | longest gap without a 200 | recovery | result | notes |"
  echo "|---|---|---|---|---|---|"
  awk -F'\t' '{ printf "| %s | %s | %s | %s | %s | %s |\n", $1, $2, $3, $4, $5, $6 }' "$results"
  echo
  echo "PASS (outage): requests failed but everything recovered (recorded, not a failure). info (network-dependent): over the gap limit in the bgp topology, whose gaps are the network's convergence (recorded, not a failure); a lost node (g, m, n) is recorded only in every topology. SKIP: the placement or topology did not allow the scenario. FAIL: routes never recovered, the certificate never rotated or the state was not restored (timeouts: recovery ${RECOVERY_TIMEOUT}s, leader ${LEADER_TIMEOUT}s, route change ${APPLY_TIMEOUT}s, rotation ${ROTATE_TIMEOUT}s)$([ "$MANAGED_REPLICAS" -ge 2 ] && [ "$TOPOLOGY" != bgp ] && echo ", or a pod deletion, drain, rollout restart or parameters change left a gap over ${GAP_LIMIT}s$([ "$STRICT" = true ] && echo " or any failed request (STRICT)")")."
  echo
  echo "Nodes: $(tr '\n' ' ' < "$work/nodes.txt")"
  for f in "$work"/timeline-*.txt; do
    [ -e "$f" ] || continue
    echo
    echo "<details><summary>timeline $(basename "$f" .txt | sed 's/timeline-//')</summary>"
    echo
    echo '```'
    head -n 200 "$f"
    echo '```'
    echo "</details>"
  done
} > "$work/summary.md"
cat "$work/summary.md"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then cat "$work/summary.md" >> "$GITHUB_STEP_SUMMARY"; fi
exit $failed
