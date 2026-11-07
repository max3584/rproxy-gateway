#!/bin/bash
# Listener filters (GEP-5224, GatewayListenerFilters) on the conformance cluster (scripts/conformance.sh
# runs it after the suite): Gateway API v1.7.0 has no conformance test for the feature yet. A listener's
# ExternalAuth (the echo server, which allows everything) and ExtensionRef (an RproxyMiddleware setting a
# header) run before route matching, and the route matches on that header; the GatewayClass claims the
# feature. With the ExternalAuth's Service gone, the listener reports InvalidListenerFilterRef and its
# requests get 500, never the route. Skipped (exit 0) with Gateway API CRDs without
# listeners[].filters (the standard channel, before v1.7) and with an rproxy without http.pre_routing
# (before v0.4.22: the listener is not accepted, never served without its filters).
set -euo pipefail
cd "$(dirname "$0")/.."
ns=listener-filters
out=${RUNNER_TEMP:-/tmp}/listener-filters.json
# the namespace goes however the script ends (its Gateway and rproxy with it)
cleanup() { kubectl delete namespace "$ns" --wait=false > /dev/null 2>&1 || true; }

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

trap cleanup EXIT
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
          - type: ExternalAuth
            externalAuth: {protocol: HTTP, backendRef: {name: echo, port: 8080}, http: {path: /auth, allowedResponseHeaders: [x-auth-none]}}
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
# the listener's Accepted first: accepted once every rproxy pod of the Gateway reports http_options
# pre_routing; "lacks it" once one answered without it, or with the bundled rproxy before v0.4.22 (then
# nothing may be placed, and there is no Deployment to wait for); "not known (yet)" until then
cond() { kubectl -n $ns get gateway lf -o jsonpath="{.status.listeners[0].conditions[?(@.type==\"Accepted\")].$1}"; }
settled() { [ "$(cond status)" = True ] || grep -q "lacks it" <<< "$(cond message)"; }
if ! retry 300 settled; then
  echo "listener Accepted: $(cond status) $(cond reason): $(cond message)"
  exit 1
fi
if [ "$(cond status)" != True ]; then
  echo "listener Accepted: $(cond status) $(cond reason): $(cond message)"
  echo "listener filters: skipped (this rproxy lacks http.pre_routing, rproxy v0.4.22)"
  exit 0
fi
kubectl -n $ns wait --for=condition=Programmed gateway/lf --timeout=120s > /dev/null
addr=$(kubectl -n $ns get gateway lf -o jsonpath='{.status.addresses[0].value}')
# the client sends no x-tag: the route matches the one the listener's filter set
retry 60 sh -c "curl -sf -H 'Host: lf.example.com' http://$addr/lf > $out"
jq -e '.path == "/lf" and .headers["X-Tag"] == ["tagged"]' "$out" > /dev/null || { cat "$out"; exit 1; }
claims() { kubectl get gatewayclass rproxy -o json | jq -e '[.status.supportedFeatures[]?.name] | index("GatewayListenerFilters") != null' > /dev/null; }
retry 60 claims
# the ExternalAuth's Service gone: the filter answers 500 in its place (never skipped)
kubectl -n $ns patch gateway lf --type=json \
  -p '[{"op": "replace", "path": "/spec/listeners/0/filters/requests/0/externalAuth/backendRef/name", "value": "missing"}]' > /dev/null
refs() { test "$(kubectl -n $ns get gateway lf -o jsonpath='{.status.listeners[0].conditions[?(@.type=="ResolvedRefs")].reason}')" = InvalidListenerFilterRef; }
retry 60 refs
status() { test "$(curl -s -o /dev/null -w '%{http_code}' -H 'Host: lf.example.com' "http://$addr/lf")" = 500; }
retry 60 status
echo "listener filters: passed (ExternalAuth and ExtensionRef ran before route matching; an unresolved filter answered 500; GatewayListenerFilters claimed)"
