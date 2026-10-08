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
#               ready rproxy pod announces, the router splits connections across them
#   nodeport-lb NodePort Services (externalTrafficPolicy Local) behind an HAProxy container that
#               health-checks every node and sends to all healthy ones (a user's own L4 balancer)
# Outages are measured and reported. With MANAGED_REPLICAS >= 2, a pod deletion, a node drain and a
# rollout restart fail the run when the longest gap without a 200 exceeds GAP_LIMIT seconds; otherwise
# the script fails only on functional breakage (routes never recover, the certificate never rotates,
# state not restored). Each scenario's timeline (EndpointSlices, pods, the load balancer's view,
# probe failures) is in $work/timeline-*.txt. Results: $work/summary.md (and $GITHUB_STEP_SUMMARY).
set -euo pipefail
cd "$(dirname "$0")/.."

SOURCE=${SOURCE:-published}
TOPOLOGY=${TOPOLOGY:-l2-local}
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
FRR_IMAGE=${FRR_IMAGE:-quay.io/frrouting/frr:9.1.0}
HAPROXY_IMAGE=${HAPROXY_IMAGE:-haproxy:3.0-alpine}
case "$TOPOLOGY" in l2-local | l2-cluster | bgp | nodeport-lb) ;; *) echo "TOPOLOGY: l2-local, l2-cluster, bgp or nodeport-lb"; exit 1 ;; esac
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
  kubectl -n "$APP" logs -l app.kubernetes.io/name=rproxy --prefix --all-containers --tail=-1 > "$d/rproxy.log" 2>&1 || true
  kubectl get gateways,httproutes,tcproutes,certificates -A -o yaml > "$d/objects.yaml" 2>&1 || true
  kubectl -n "$APP" get deploy,svc,networkpolicy,pdb,secret -o wide > "$d/managed.txt" 2>&1 || true
  docker logs "$CLUSTER-frr" > "$d/frr.log" 2>&1 || true
  docker exec "$CLUSTER-frr" vtysh -c 'show bgp summary' -c 'show bfd peers brief' -c 'show ip route' > "$d/frr-state.txt" 2>&1 || true
  docker logs "$CLUSTER-lb" > "$d/haproxy.log" 2>&1 || true
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
apply_pool() {  # apply_pool <range> <extra YAML>
  cat << YAML | kubectl apply -f -
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
spec: {ipAddressPools: [kind]}'
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
spec: {ipAddressPools: [kind]}"
    ;;
  nodeport-lb)
    HELM_ARGS="--set managed.serviceType=NodePort --set managed.externalTrafficPolicy=Local $HELM_ARGS"
    ;;
esac
case "$TOPOLOGY" in
  l2-cluster) HELM_ARGS="--set managed.externalTrafficPolicy=Cluster $HELM_ARGS" ;;
esac

log "== cert-manager $CERT_MANAGER_VERSION"
kubectl apply -f "https://github.com/cert-manager/cert-manager/releases/download/$CERT_MANAGER_VERSION/cert-manager.yaml" > /dev/null
for d in cert-manager cert-manager-cainjector cert-manager-webhook; do kubectl -n cert-manager rollout status deploy/$d --timeout=300s; done

log "== Gateway API $GATEWAY_API_VERSION (experimental channel)"
kubectl apply --server-side -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/$GATEWAY_API_VERSION/experimental-install.yaml" > /dev/null

# ---------------------------------------------------------------- install
image_args=()
if [ "$SOURCE" = checkout ]; then
  log "== controller image from dist/amd64/rproxy-gateway"
  docker build -q -t rproxy-gateway:acc --build-arg TARGETARCH=amd64 -f Dockerfile .
  kind load docker-image --name "$CLUSTER" rproxy-gateway:acc
  image_args=(--set controller.image.repository=rproxy-gateway --set controller.image.tag=acc)
else
  image_args=(--version "$CHART_VERSION")
fi
log "== helm install $CHART ($SOURCE) (controller 2 replicas, managed.replicas=$MANAGED_REPLICAS) $HELM_ARGS"
# shellcheck disable=SC2086
helm install rproxy-gateway "$CHART" "${image_args[@]}" -n "$NS" --create-namespace --wait --timeout 10m \
  --set controller.replicas=2 --set managed.replicas="$MANAGED_REPLICAS" $HELM_ARGS
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
      containers:
        - name: echo
          image: registry.k8s.io/gateway-api/echo-basic:v1.5.1
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
DEPLOY=$(kubectl -n "$APP" get deploy -l gateway.networking.k8s.io/gateway-name=acc -o jsonpath='{.items[0].metadata.name}')
log "managed rproxy Deployment: $DEPLOY"
kubectl -n "$APP" rollout status "deploy/$DEPLOY" --timeout=300s
if [ "$TOPOLOGY" = nodeport-lb ]; then
  # HAProxy (TCP) in front of every worker's node ports: health checks every 500 ms (down after 2
  # failures), a failed connection is tried again on another node
  nodeport() { kubectl -n "$APP" get svc "$DEPLOY" -o jsonpath="{.spec.ports[?(@.port==$1)].nodePort}"; }
  {
    printf 'global\n  log stdout format raw local0\ndefaults\n  mode tcp\n  log global\n  option log-health-checks\n'
    printf '  timeout connect 1s\n  timeout client 30s\n  timeout server 30s\n  timeout check 500ms\n  retries 2\n  option redispatch 1\n'
    printf '  default-server inter 500ms fastinter 250ms downinter 500ms fall 2 rise 2 on-marked-down shutdown-sessions\n'
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
  kubectl -n "$APP" get pods -l app.kubernetes.io/name=rproxy -o json |
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
statuses_ok() {
  [ "$(cond gateway/acc '{.status.conditions[?(@.type=="Programmed")].status}')" = True ] &&
    [ "$(cond gateway/acc '{.status.listeners[*].conditions[?(@.type=="Programmed")].status}')" = "True True True" ] &&
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

log "== baseline"
wait_for 180 all_pods_serve || { echo "the rproxy pods never served"; exit 1; }
wait_for 60 lb_host_ok echo.example.com || { echo "the Gateway address never served"; exit 1; }
statuses_ok || { echo "statuses not Programmed/Accepted"; exit 1; }
kubectl -n "$APP" get pods -o wide
kubectl -n "$NS" get pods -o wide
{
  echo "controller: $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=rproxy-gateway -o jsonpath='{.items[0].status.containerStatuses[0].imageID}')"
  echo "rproxy: $(kubectl -n "$APP" get pods -l app.kubernetes.io/name=rproxy -o jsonpath='{.items[0].status.containerStatuses[?(@.name=="rproxy")].imageID}')"
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
  kubectl -n "$APP" get pods -l app.kubernetes.io/name=rproxy -w --output-watch-events -o json |
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
w_lb() { docker logs -f --since 1s "$CLUSTER-lb" 2>&1 | grep --line-buffered -E ' is (UP|DOWN)' | sed -u 's/^/lb /'; }
watch_bg slices w_slices
watch_bg pods w_pods
case "$TOPOLOGY" in
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
    result=FAIL notes="longest gap over ${GAP_LIMIT}s; $notes"
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
  kubectl -n "$APP" get pods -l app.kubernetes.io/name=rproxy -o json |
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

for s in a b1 b2 c d e f g; do
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
    bgp) echo "MetalLB $METALLB_VERSION BGP (FRR mode, BFD 300 ms x 3) to an FRR router ($FRR_IMAGE) with ECMP over the announcing nodes, externalTrafficPolicy Local." ;;
    nodeport-lb) echo "NodePort Services (externalTrafficPolicy Local) behind HAProxy ($HAPROXY_IMAGE, TCP, checks every 500 ms, redispatch)." ;;
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
  echo "PASS (outage): requests failed but everything recovered (recorded, not a failure). SKIP: the placement or topology did not allow the scenario. FAIL: routes never recovered, the certificate never rotated or the state was not restored (timeouts: recovery ${RECOVERY_TIMEOUT}s, leader ${LEADER_TIMEOUT}s, route change ${APPLY_TIMEOUT}s, rotation ${ROTATE_TIMEOUT}s)$([ "$MANAGED_REPLICAS" -ge 2 ] && echo ", or a pod deletion, drain or rollout restart left a gap over ${GAP_LIMIT}s")."
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
