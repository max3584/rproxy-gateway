#!/bin/bash
# End-to-end test on kind (runs on the CI runner VM: kind needs Docker). Uses the
# binaries the Alpine jobs built: dist/amd64/rproxy-gateway and dist/amd64/rproxy-api.
#   scripts/e2e.sh            # create the cluster, install, test
#   SETUP_ONLY=1 scripts/e2e.sh   # stop after installing (conformance.sh uses this)
set -euo pipefail
cd "$(dirname "$0")/.."
CLUSTER=${CLUSTER:-rproxy-gateway}
GATEWAY_API_VERSION=${GATEWAY_API_VERSION:-v1.6.3}
# standard (what README installs) or experimental (conformance.sh)
GATEWAY_API_CHANNEL=${GATEWAY_API_CHANNEL:-standard}
TRAEFIK_CRDS=${TRAEFIK_CRDS:-https://raw.githubusercontent.com/traefik/traefik/v3.7.14/docs/content/reference/dynamic-configuration/kubernetes-crd-definition-v1.yml}
NS=rproxy-gateway-system
work=${RUNNER_TEMP:-/tmp}/rproxy-gateway-e2e
mkdir -p "$work"

dump() {
  echo "::group::controller log"; kubectl -n $NS logs -l app.kubernetes.io/name=rproxy-gateway --prefix --tail=300 || true; echo "::endgroup::"
  echo "::group::rproxy pods"; kubectl get pods -A -l app.kubernetes.io/name=rproxy -o wide || true
  kubectl get pods -A -l app.kubernetes.io/name=rproxy --no-headers -o custom-columns=NS:.metadata.namespace,NAME:.metadata.name 2>/dev/null |
    while read -r ns p; do kubectl -n "$ns" logs "$p" --all-containers --tail=200 || true; done
  echo "::endgroup::"
  echo "::group::Gateway API objects"; kubectl get gateways,httproutes,tcproutes,udproutes -A -o yaml || true; echo "::endgroup::"
}
trap 'dump' ERR
set -E

retry() {  # retry <seconds> <command...>
  local until=$((SECONDS + $1)); shift
  until "$@"; do
    if [ $SECONDS -ge $until ]; then echo "timed out: $*"; return 1; fi
    sleep 2
  done
}

command -v dig > /dev/null || { sudo apt-get update -q && sudo apt-get install -yq dnsutils; }

echo "== images"
chmod +x dist/amd64/*
docker build -q -t rproxy-gateway:e2e --build-arg TARGETARCH=amd64 -f Dockerfile .
docker build -q -t rproxy:e2e --build-arg TARGETARCH=amd64 -f Dockerfile.rproxy .

echo "== cluster"
kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --wait 180s
kind load docker-image --name "$CLUSTER" rproxy-gateway:e2e rproxy:e2e
kubectl apply --server-side -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/$GATEWAY_API_VERSION/$GATEWAY_API_CHANNEL-install.yaml" > /dev/null
# the runner reaches ClusterIPs (Gateway addresses) through the kind node (kube-proxy there)
node_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$CLUSTER-control-plane")
svc_cidr=$(kubectl get svc kubernetes -o jsonpath='{.spec.clusterIP}' | awk -F. '{print $1"."$2".0.0/16"}')
sudo ip route replace "$svc_cidr" via "$node_ip"

echo "== install"
helm upgrade --install rproxy-gateway charts/rproxy-gateway -n $NS --create-namespace --wait \
  --set controller.image.repository=rproxy-gateway --set controller.image.tag=e2e --set controller.image.pullPolicy=Never \
  --set rproxy.image.repository=rproxy --set rproxy.image.tag=e2e --set rproxy.image.pullPolicy=Never \
  --set managed.serviceType=ClusterIP --set controller.logFormat=text --set controller.resyncSeconds=10 \
  ${MIGRATE:+--set migration.migrateTo=$MIGRATE} \
  ${CONTROLLER_ARGS:+--set "controller.extraArgs={$CONTROLLER_ARGS}"}
kubectl wait --for=condition=Accepted gatewayclass/rproxy --timeout=120s
# installed after the controller: it finds them without a restart (discovery every 30 s)
kubectl apply --server-side -f "$TRAEFIK_CRDS" > /dev/null
kubectl wait --for=condition=Established crd/ingressroutes.traefik.io crd/middlewares.traefik.io --timeout=60s > /dev/null

if [ -n "${SETUP_ONLY:-}" ]; then exit 0; fi

echo "== Gateway"
kubectl create namespace e2e --dry-run=client -o yaml | kubectl apply -f - > /dev/null
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=secure.example.com -addext subjectAltName=DNS:secure.example.com \
  -keyout "$work/tls.key" -out "$work/tls.crt" 2> /dev/null
kubectl -n e2e create secret tls secure-cert --cert="$work/tls.crt" --key="$work/tls.key" --dry-run=client -o yaml | kubectl apply -f - > /dev/null
kubectl apply -f test/e2e/manifests.yaml > /dev/null
kubectl -n e2e rollout status deploy/echo --timeout=180s
kubectl -n e2e wait --for=condition=Programmed gateway/e2e --timeout=240s
addr=$(kubectl -n e2e get gateway e2e -o jsonpath='{.status.addresses[0].value}')
echo "Gateway address: $addr"

echo "== HTTP"
retry 60 sh -c "curl -sf -H 'Host: e2e.example.com' http://$addr/hello > $work/http.json"
jq -e '.path == "/hello" and (.headers["X-E2e"] == ["rproxy"])' "$work/http.json" > /dev/null || { cat "$work/http.json"; exit 1; }
test "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: other.example.com' http://"$addr"/)" = 404

echo "== HTTPS"
retry 60 sh -c "curl -sf --cacert $work/tls.crt --resolve secure.example.com:443:$addr https://secure.example.com/s > $work/https.json"
jq -e '.path == "/s"' "$work/https.json" > /dev/null

echo "== certificate rotation: the new certificate is served soon (mounted Secret, pod annotation)"
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj /CN=secure.example.com -addext subjectAltName=DNS:secure.example.com \
  -keyout "$work/tls2.key" -out "$work/tls2.crt" 2> /dev/null
kubectl -n e2e create secret tls secure-cert --cert="$work/tls2.crt" --key="$work/tls2.key" --dry-run=client -o yaml | kubectl apply -f - > /dev/null
retry 60 sh -c "curl -sf --cacert $work/tls2.crt --resolve secure.example.com:443:$addr https://secure.example.com/rotated > /dev/null"

echo "== rproxy next to the Gateway: infrastructure metadata, no Secret access"
sa=$(kubectl -n e2e get pods -l app.kubernetes.io/name=rproxy -o jsonpath='{.items[0].spec.serviceAccountName}')
# can-i exits 1 for "no" (and the ERR trap runs in command substitutions)
test "$(kubectl auth can-i get secrets -n e2e --as="system:serviceaccount:e2e:$sa" || true)" = no
test "$(kubectl auth can-i get secrets -n $NS --as="system:serviceaccount:e2e:$sa" || true)" = no
test "$(kubectl -n e2e get pods -l app.kubernetes.io/name=rproxy -o jsonpath='{.items[0].spec.automountServiceAccountToken}')" = false
test "$(kubectl -n e2e get pods -l gateway.networking.k8s.io/gateway-name=e2e,team=e2e -o jsonpath='{.items[0].metadata.annotations.e2e\.example\.com/note}')" = infrastructure
test "$(kubectl -n e2e get svc -l gateway.networking.k8s.io/gateway-name=e2e,team=e2e -o name | wc -l)" = 1

echo "== RproxyMiddleware (rate_limit)"
curl -s -o /dev/null -H 'Host: e2e.example.com' "http://$addr/limited/1"
test "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: e2e.example.com' "http://$addr/limited/2")" = 429

echo "== TCP"
retry 60 sh -c "curl -sf http://$addr:9000/tcp | jq -e '.path == \"/tcp\"' > /dev/null"

echo "== UDP"
retry 60 sh -c "dig +short +time=2 +tries=1 @$addr -p 5353 kubernetes.default.svc.cluster.local | grep -q '^[0-9]'"

echo "== status"
test "$(kubectl -n e2e get httproute echo -o jsonpath='{.status.parents[0].conditions[?(@.type=="Accepted")].status}')" = True
test "$(kubectl -n e2e get httproute echo -o jsonpath='{.status.parents[0].conditions[?(@.type=="ResolvedRefs")].status}')" = True
test "$(kubectl -n e2e get gateway e2e -o jsonpath='{.status.listeners[?(@.name=="https")].conditions[?(@.type=="Programmed")].status}')" = True

if [ -n "${MIGRATE:-}" ]; then
  echo "== migration (Ingress, IngressRoute)"
  retry 60 sh -c "curl -sf -H 'Host: ingress.example.com' http://$addr/i | jq -e '.path == \"/i\"' > /dev/null"
  retry 90 sh -c "curl -sf -H 'Host: traefik.example.com' http://$addr/traefik/t | jq -e '.path == \"/t\"' > /dev/null"
  echo "== migration: Ingress status"
  retry 60 sh -c "test \"\$(kubectl -n e2e get ingress -o jsonpath='{.items[0].status.loadBalancer.ingress[0].ip}')\" = $addr"
fi

echo "== rproxy restart: the controller applies the rule set again"
kubectl -n e2e delete pod -l app.kubernetes.io/name=rproxy --wait=true > /dev/null
retry 120 sh -c "curl -sf -H 'Host: e2e.example.com' http://$addr/again > /dev/null"

echo "== controller failover: another replica takes the Lease and carries on"
leader=$(kubectl -n $NS get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}')
echo "leader: $leader"
kubectl -n $NS delete pod "$leader" --wait=false > /dev/null
retry 120 sh -c "h=\$(kubectl -n $NS get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}'); test -n \"\$h\" && test \"\$h\" != $leader"
kubectl -n e2e delete pod -l app.kubernetes.io/name=rproxy --wait=true > /dev/null
retry 120 sh -c "curl -sf -H 'Host: e2e.example.com' http://$addr/after-failover > /dev/null"

echo "== Gateway deleted: its rproxy goes"
kubectl -n e2e delete gateway e2e > /dev/null
retry 120 sh -c "test -z \"\$(kubectl -n e2e get deploy,svc,sa,secret -l rproxy.max3584.net/gateway -o name)\""
echo "e2e OK"
