#!/bin/bash
# The Gateway API conformance report for submission upstream
# (kubernetes-sigs/gateway-api conformance/reports/<minor>/max3584-rproxy-gateway/), from the
# PUBLISHED chart and images of a released version, on a fresh kind cluster. Nothing is built here:
# the chart is oci://ghcr.io/max3584/charts/rproxy-gateway at VERSION with its default images
# (ghcr.io/max3584/rproxy-gateway:<appVersion>, ghcr.io/max3584/rproxy-gateway/rproxy:<the chart's tag>).
# The suite infers the supported features from the GatewayClass status (no --supported-features),
# so the report shows what the released controller itself declares. Runs on a Linux host with
# Docker, kind, kubectl, helm, go and git (the e2e workflow run by hand with report_version; the same steps
# are in docs/conformance/submission/.../README.md for anyone to reproduce).
#   VERSION=0.4.5 scripts/conformance-report.sh
# Writes $OUT_DIR/<channel>-v<VERSION>-default-report.yaml (the upstream file name), the suite's log,
# the controller's and rproxy's logs and the environment (versions, image digests).
set -euo pipefail
cd "$(dirname "$0")/.."
VERSION=${VERSION:-0.4.5}
VERSION=${VERSION#v}
GATEWAY_API_VERSION=${GATEWAY_API_VERSION:-v1.6.3}
# experimental: some claimed features are experimental fields (HTTPRouteRule.retry); with the
# standard CRDs the API server drops them
GATEWAY_API_CHANNEL=${GATEWAY_API_CHANNEL:-experimental}
CHART=${CHART:-oci://ghcr.io/max3584/charts/rproxy-gateway}
PROFILES=${PROFILES:-GATEWAY-HTTP,GATEWAY-GRPC,GATEWAY-TLS,GATEWAY-TCP,GATEWAY-UDP}
CLUSTER=${CLUSTER:-rproxy-gateway-conformance}
# GatewayStaticAddresses: the usable address's range is allowed (static addresses are off by default)
ADDRESS_CIDR=${ADDRESS_CIDR:-192.0.2.0/24}
USABLE_ADDRESS=${USABLE_ADDRESS:-192.0.2.10}
UNUSABLE_ADDRESS=${UNUSABLE_ADDRESS:-0.0.0.0}
NS=rproxy-gateway-system
OUT_DIR=${OUT_DIR:-$PWD/conformance-report}
work=${RUNNER_TEMP:-/tmp}/rproxy-gateway-conformance-report
mkdir -p "$work" "$OUT_DIR"
OUT_DIR=$(cd "$OUT_DIR" && pwd)
REPORT=$OUT_DIR/$GATEWAY_API_CHANNEL-v$VERSION-default-report.yaml

echo "== cluster"
kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --wait 180s
kubectl config use-context "kind-$CLUSTER" > /dev/null
kubectl apply --server-side -f "https://github.com/kubernetes-sigs/gateway-api/releases/download/$GATEWAY_API_VERSION/$GATEWAY_API_CHANNEL-install.yaml" > /dev/null
# the suite (on this host) reaches the Gateways' ClusterIPs through the kind node (kube-proxy there)
node_ip=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$CLUSTER-control-plane")
svc_cidr=$(kubectl get svc kubernetes -o jsonpath='{.spec.clusterIP}' | awk -F. '{print $1"."$2".0.0/16"}')
sudo ip route replace "$svc_cidr" via "$node_ip"

echo "== install $CHART $VERSION (published chart and images)"
# kind has no load balancer: the Gateways' Services are ClusterIP (their address is the ClusterIP)
helm upgrade --install rproxy-gateway "$CHART" --version "$VERSION" -n "$NS" --create-namespace --wait --timeout 10m \
  --set managed.serviceType=ClusterIP \
  --set "managed.addressCIDRs={$ADDRESS_CIDR}"
kubectl wait --for=condition=Accepted gatewayclass/rproxy --timeout=120s

{
  echo "date: $(date -u +%Y-%m-%dT%H:%M:%SZ)"
  echo "chart: $CHART $VERSION"
  echo "gatewayAPI: $GATEWAY_API_VERSION $GATEWAY_API_CHANNEL"
  echo "kind: $(kind version)"
  echo "kubernetes: $(kubectl version -o json | jq -r .serverVersion.gitVersion)"
  echo "helm: $(helm version --short)"
  echo "go: $(go version)"
  echo "controller images:"
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=rproxy-gateway -o jsonpath='{range .items[*].status.containerStatuses[*]}  {.image} {.imageID}{"\n"}{end}'
  echo "rproxy image (chart values): $(helm get values rproxy-gateway -n "$NS" --all -o json | jq -r '.rproxy.image | "\(.repository):\(.tag)"')"
  echo "GatewayClass supportedFeatures:"
  kubectl get gatewayclass rproxy -o jsonpath='{range .status.supportedFeatures[*]}  {.name}{"\n"}{end}'
} | tee "$OUT_DIR/environment.txt"

if [ ! -d "$work/gateway-api" ]; then
  git clone -q --depth 1 --branch "$GATEWAY_API_VERSION" https://github.com/kubernetes-sigs/gateway-api.git "$work/gateway-api"
fi
status=0
(
  cd "$work/gateway-api"
  GOTOOLCHAIN=auto go test ./conformance -run TestConformance -count=1 -timeout 110m -v -args \
    --gateway-class=rproxy \
    --conformance-profiles="$PROFILES" \
    --organization=max3584 \
    --project=rproxy-gateway \
    --url=https://github.com/max3584/rproxy-gateway \
    --version="v$VERSION" \
    --contact=https://github.com/max3584/rproxy-gateway/issues \
    --report-output="$REPORT" \
    --usable-address="$USABLE_ADDRESS" \
    --unusable-address="$UNUSABLE_ADDRESS"
) 2>&1 | tee "$OUT_DIR/conformance.log" || status=$?

# the managed rproxy pods' image, now that the suite's Gateways made some (the last ones left)
{
  echo "rproxy pods' images:"
  kubectl get pods -A -l app.kubernetes.io/name=rproxy -o jsonpath='{range .items[*].status.containerStatuses[*]}  {.image} {.imageID}{"\n"}{end}' | sort -u
} | tee -a "$OUT_DIR/environment.txt" || true
kubectl -n "$NS" logs -l app.kubernetes.io/name=rproxy-gateway --prefix --tail=-1 > "$OUT_DIR/controller.log" 2>&1 || true

# every profile: core and extended success, nothing failed or skipped
check=$OUT_DIR/check.txt
if [ -f "$REPORT" ]; then
  bad=$(python3 - "$REPORT" "$check" <<'PY'
import sys, yaml
report = yaml.safe_load(open(sys.argv[1]))
bad = 0
with open(sys.argv[2], "w") as out:
    for p in report["profiles"]:
        c, e = p["core"], p.get("extended", {})
        cs, es = c.get("statistics", {}), e.get("statistics", {})
        out.write(f"{p['name']}: core {c['result']} (passed {cs.get('Passed', 0)}, failed {cs.get('Failed', 0)}, skipped {cs.get('Skipped', 0)}), "
                  f"extended {e.get('result', '-')} (passed {es.get('Passed', 0)}, failed {es.get('Failed', 0)}, skipped {es.get('Skipped', 0)}), "
                  f"supported features {len(e.get('supportedFeatures', []))}, unsupported: {', '.join(e.get('unsupportedFeatures', [])) or 'none'}\n")
        if c["result"] != "success" or e.get("result", "success") != "success" or cs.get("Skipped", 0) or es.get("Skipped", 0):
            bad += 1
print(bad)
PY
  )
else
  echo "no report" > "$check"
  bad=1
fi
{
  echo "## Gateway API conformance report: rproxy-gateway v$VERSION ($GATEWAY_API_VERSION, $GATEWAY_API_CHANNEL)"
  echo
  sed 's/^/- /' "$check"
  echo
  echo "Test run exit status: $status"
  grep -E '^\s*--- (FAIL|SKIP)' "$OUT_DIR/conformance.log" | sed 's/^\s*/    /' | head -100 || true
  if [ -f "$REPORT" ]; then
    echo
    echo '```yaml'
    cat "$REPORT"
    echo '```'
  fi
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
if [ "$status" != 0 ] || [ "$bad" != 0 ]; then
  echo "conformance report: not a full success (exit $status, $bad profile(s) not success)"
  exit 1
fi
