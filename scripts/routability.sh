#!/bin/bash
# Gateway address routability (GEP-5093: GatewayAddressRoutability, GatewayAddressRoutabilityCluster) on the
# cluster scripts/conformance.sh set up (Gateway API v1.7 experimental CRDs, managed.serviceType=ClusterIP).
# Gateway API v1.7.0's suite has no test for these features yet: this checks the GEP's conformance scenarios.
# Runs on the CI runner VM after the suite (it reaches ClusterIPs through the kind node).
set -euo pipefail
cd "$(dirname "$0")/.."
NS=rp-routability
work=${RUNNER_TEMP:-/tmp}/rproxy-gateway-routability
mkdir -p "$work"

dump() {
  echo "::group::Gateways and Services"; kubectl -n $NS get gateways -o yaml || true; kubectl -n $NS get svc -o wide || true; echo "::endgroup::"
  echo "::group::controller log"
  kubectl -n rproxy-gateway-system logs -l app.kubernetes.io/name=rproxy-gateway --prefix --tail=200 || true
  echo "::endgroup::"
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

# the CRDs have the field (Gateway API v1.7 experimental), else nothing to check
if ! kubectl get crd gateways.gateway.networking.k8s.io -o json \
  | jq -e '.spec.versions[] | select(.name == "v1") | .schema.openAPIV3Schema.properties.spec.properties.addresses.items.properties.routability' > /dev/null; then
  echo "the Gateway CRD has no spec.addresses[].routability: skipped"
  exit 0
fi

echo "== GatewayClass claims both features"
kubectl get gatewayclass rproxy -o json \
  | jq -e '[.status.supportedFeatures[].name] | index("GatewayAddressRoutability") != null and index("GatewayAddressRoutabilityCluster") != null' > /dev/null

kubectl create namespace $NS --dry-run=client -o yaml | kubectl apply -f - > /dev/null
svc_cidr_prefix=$(kubectl get svc kubernetes -o jsonpath='{.spec.clusterIP}' | awk -F. '{print $1"."$2}')
# a free ClusterIP in the ServiceCIDR (the /16 of the kubernetes Service) for the static Cluster request
static_ip="$svc_cidr_prefix.$((200 + RANDOM % 50)).$((10 + RANDOM % 200))"
echo "static Cluster address: $static_ip"

gateway() {  # gateway <name> <addresses (YAML flow list)>
  cat <<YAML | kubectl apply -f - > /dev/null
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: $1, namespace: $NS}
spec:
  gatewayClassName: rproxy
  addresses: $2
  listeners: [{name: http, port: 80, protocol: HTTP}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: $1, namespace: $NS}
spec:
  parentRefs: [{name: $1}]
  rules: [{filters: [{type: RequestRedirect, requestRedirect: {hostname: $1.example.com, statusCode: 302}}]}]
YAML
}
assigned() {  # assigned <gateway> <status> <reason>
  local got
  got=$(kubectl -n $NS get gateway "$1" -o jsonpath='{.status.conditions[?(@.type=="AddressesAssigned")].status}/{.status.conditions[?(@.type=="AddressesAssigned")].reason}')
  test "$got" = "$2/$3"
}
addresses() { kubectl -n $NS get gateway "$1" -o json | jq -c '[.status.addresses[]? | {value, routability}]'; }
# every address answers (the route redirects to <gateway>.example.com: rproxy itself answers)
answers() {  # answers <gateway>
  local a
  for a in $(kubectl -n $NS get gateway "$1" -o jsonpath='{.status.addresses[*].value}'); do
    retry 60 sh -c "curl -s -o /dev/null -D - --max-time 3 http://$a/ | grep -qi '^location: http://$1.example.com'"
  done
}
in_cidr() { case $1 in "$svc_cidr_prefix".*) return 0 ;; *) return 1 ;; esac; }

echo "== no spec.addresses: the ClusterIP, reported as Cluster; AddressesAssigned"
gateway none "[]"
retry 240 assigned none True AddressesAssigned
addresses none | tee "$work/none.json"
jq -e 'length == 1 and .[0].routability == "Cluster"' "$work/none.json" > /dev/null
in_cidr "$(jq -r '.[0].value' "$work/none.json")"
answers none

echo "== one empty and one Cluster request: one distinct address each"
gateway mixed "[{type: IPAddress}, {type: IPAddress, routability: Cluster}]"
retry 240 assigned mixed True AddressesAssigned
addresses mixed | tee "$work/mixed.json"
jq -e 'length == 2 and (map(.value) | unique | length == 2) and all(.routability == "Cluster")' "$work/mixed.json" > /dev/null
for v in $(jq -r '.[].value' "$work/mixed.json"); do in_cidr "$v"; done
test "$(kubectl -n $NS get svc -l rproxy.max3584.net/address=true,gateway.networking.k8s.io/gateway-name=mixed -o name | wc -l)" = 1
answers mixed

echo "== a static Cluster value in the ServiceCIDR: that ClusterIP"
gateway static "[{type: IPAddress, value: \"$static_ip\", routability: Cluster}]"
retry 240 assigned static True AddressesAssigned
addresses static | tee "$work/static.json"
jq -e --arg ip "$static_ip" '. == [{value: $ip, routability: "Cluster"}]' "$work/static.json" > /dev/null
answers static

echo "== a static Cluster value outside the ServiceCIDR: not assigned; with an empty request: partially"
gateway outside "[{type: IPAddress, value: \"192.0.2.77\", routability: Cluster}]"
retry 240 assigned outside False NotAssigned
retry 120 sh -c "test \"\$(kubectl -n $NS get gateway outside -o jsonpath='{.status.conditions[?(@.type==\"Programmed\")].reason}')\" = AddressNotAssigned"
gateway outside "[{type: IPAddress, value: \"192.0.2.77\", routability: Cluster}, {type: IPAddress}]"
retry 240 assigned outside False PartiallyAssigned
kubectl -n $NS get gateway outside -o jsonpath='{.status.conditions[?(@.type=="AddressesAssigned")].message}' | grep -q 'spec.addresses\[0\]'
addresses outside | tee "$work/outside.json"
jq -e 'length == 1 and .[0].value != "192.0.2.77"' "$work/outside.json" > /dev/null

echo "== the conformance sentinel, and an unsupported prefixed value"
gateway sentinel "[{type: IPAddress, routability: testing.x-k8s.io/sentinel}, {type: IPAddress, routability: example.com/CorpWan}]"
retry 240 assigned sentinel False PartiallyAssigned
addresses sentinel | tee "$work/sentinel.json"
jq -e 'length == 1 and .[0].routability == "testing.x-k8s.io/sentinel"' "$work/sentinel.json" > /dev/null

echo "== a Cluster request removed: its Service goes"
gateway mixed "[{type: IPAddress}]"
retry 120 sh -c "test \"\$(kubectl -n $NS get svc -l rproxy.max3584.net/address=true,gateway.networking.k8s.io/gateway-name=mixed -o name | wc -l)\" = 0"
retry 120 sh -c "test \"\$(kubectl -n $NS get gateway mixed -o json | jq '.status.addresses | length')\" = 1"

kubectl delete namespace $NS --wait=false > /dev/null
echo "routability: ok"
