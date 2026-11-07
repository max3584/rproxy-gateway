#!/bin/bash
# Listener filters (GEP-5224, GatewayListenerFilters) on the conformance cluster (scripts/conformance.sh
# runs it after the suite): Gateway API v1.7.0 has no conformance test for the feature yet. A listener's
# ExtensionRef (an RproxyMiddleware setting a header) runs before route matching, and the route matches
# on that header; the GatewayClass claims the feature. Skipped (exit 0) with Gateway API CRDs without
# listeners[].filters (the standard channel, before v1.7) and with an rproxy without http.pre_routing
# (before v0.4.22: the listener is not accepted, never served without its filters).
set -euo pipefail
cd "$(dirname "$0")/.."
ns=listener-filters
out=${RUNNER_TEMP:-/tmp}/listener-filters.json

retry() {  # retry <seconds> <command...>
  local until=$((SECONDS + $1)); shift
  until "$@"; do
    if [ $SECONDS -ge $until ]; then echo "timed out: $*"; return 1; fi
    sleep 2
  done
}

fields=$(kubectl get crd gateways.gateway.networking.k8s.io -o json \
  | jq '[.spec.versions[] | select(.name == "v1") | .schema.openAPIV3Schema.properties.spec.properties.listeners.items.properties.filters // empty] | length')
if [ "$fields" = 0 ]; then
  echo "listener filters: skipped (the Gateway CRD has no listeners[].filters)"
  exit 0
fi

kubectl apply -f - > /dev/null <<'YAML'
apiVersion: v1
kind: Namespace
metadata: {name: listener-filters}
---
apiVersion: apps/v1
kind: Deployment
metadata: {name: echo, namespace: listener-filters}
spec:
  replicas: 1
  selector: {matchLabels: {app: echo}}
  template:
    metadata: {labels: {app: echo}}
    spec:
      containers:
        - name: echo
          image: registry.k8s.io/gateway-api/echo-basic:v1.5.1
          env:
            - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
            - {name: NAMESPACE, valueFrom: {fieldRef: {fieldPath: metadata.namespace}}}
---
apiVersion: v1
kind: Service
metadata: {name: echo, namespace: listener-filters}
spec:
  selector: {app: echo}
  ports: [{name: http, port: 8080, targetPort: 3000}]
---
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: tag, namespace: listener-filters}
spec: {headers: {request: {set: {x-tag: tagged}}}}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: lf, namespace: listener-filters}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
      filters:
        requests:
          - type: ExtensionRef
            extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: tag}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: tagged, namespace: listener-filters}
spec:
  parentRefs: [{name: lf}]
  hostnames: [lf.example.com]
  rules:
    - matches: [{path: {type: PathPrefix, value: /lf}, headers: [{name: x-tag, value: tagged}]}]
      backendRefs: [{name: echo, port: 8080}]
YAML
kubectl -n $ns rollout status deploy/echo --timeout=180s > /dev/null
# the Gateway's rproxy pods: the listener is accepted once every one of them reports http_options pre_routing
retry 180 sh -c "kubectl -n $ns get deploy -l gateway.networking.k8s.io/gateway-name=lf -o name | grep -q ."
kubectl -n $ns rollout status deploy -l gateway.networking.k8s.io/gateway-name=lf --timeout=180s > /dev/null
cond() { kubectl -n $ns get gateway lf -o jsonpath="{.status.listeners[0].conditions[?(@.type==\"Accepted\")].$1}"; }
accepted() { test "$(cond status)" = True; }
if ! retry 60 accepted; then
  message=$(cond message)
  echo "listener Accepted: $(cond status) $(cond reason): $message"
  if grep -q "http_options pre_routing" <<< "$message"; then
    echo "listener filters: skipped (this rproxy has no http.pre_routing, rproxy v0.4.22)"
    kubectl delete namespace $ns --wait=false > /dev/null
    exit 0
  fi
  exit 1
fi
kubectl -n $ns wait --for=condition=Programmed gateway/lf --timeout=120s > /dev/null
addr=$(kubectl -n $ns get gateway lf -o jsonpath='{.status.addresses[0].value}')
# the client sends no x-tag: the route matches the one the listener's filter set
retry 60 sh -c "curl -sf -H 'Host: lf.example.com' http://$addr/lf > $out"
jq -e '.path == "/lf" and .headers["X-Tag"] == ["tagged"]' "$out" > /dev/null || { cat "$out"; exit 1; }
claims() { kubectl get gatewayclass rproxy -o json | jq -e '[.status.supportedFeatures[]?.name] | index("GatewayListenerFilters") != null' > /dev/null; }
retry 60 claims
kubectl delete namespace $ns --wait=false > /dev/null
echo "listener filters: passed (filters.requests ran before route matching; GatewayListenerFilters claimed)"
