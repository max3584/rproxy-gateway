#!/bin/bash
# Gateway API conformance on kind (after the same setup as scripts/e2e.sh).
# Writes the report to $REPORT (default conformance-report.yaml) and a summary to
# $GITHUB_STEP_SUMMARY. The test run's exit status is kept in $work/status.
set -euo pipefail
cd "$(dirname "$0")/.."
GATEWAY_API_VERSION=${GATEWAY_API_VERSION:-v1.6.3}
REPORT=${REPORT:-$PWD/conformance-report.yaml}
PROFILES=${PROFILES:-GATEWAY-HTTP,GATEWAY-GRPC,GATEWAY-TLS,GATEWAY-TCP,GATEWAY-UDP}
# the extended features rproxy-gateway claims (src/controller/mod.rs SUPPORTED_FEATURES)
# GatewayStaticAddresses: a usable address becomes the Service's externalIPs; 0.0.0.0 is refused
FEATURES=${FEATURES:-Gateway,GatewayPort8080,GatewayHTTPListenerIsolation,HTTPRoute,ReferenceGrant,HTTPRouteMethodMatching,HTTPRouteQueryParamMatching,HTTPRouteResponseHeaderModification,HTTPRoutePortRedirect,HTTPRouteSchemeRedirect,HTTPRoutePathRedirect,HTTPRoutePathRewrite,TLSRoute,TLSRouteModeTerminate,TLSRouteModeMixed,TCPRoute,UDPRoute,HTTPRouteParentRefPort,HTTPRouteDestinationPortMatching,HTTPRouteNamedRouteRule,HTTPRouteBackendProtocolWebSocket,HTTPRouteBackendTimeout,GatewayStaticAddresses,GatewayAddressEmpty,GatewayInfrastructure,HTTPRoute303RedirectStatusCode,HTTPRoute307RedirectStatusCode,HTTPRoute308RedirectStatusCode,HTTPRouteRequestTimeout,HTTPRouteHostRewrite,HTTPRouteBackendRequestHeaderModification,HTTPRouteCORS,HTTPRouteRetry,HTTPRouteRetryBackendTimeout,HTTPRouteRetryConnectionError,HTTPRouteRequestMirror,HTTPRouteRequestMultipleMirrors,HTTPRouteRequestPercentageMirror,HTTPRouteBackendProtocolH2C,GatewayFrontendClientCertificateValidation,ListenerSet,GRPCRoute,GRPCRouteNamedRouteRule}
work=${RUNNER_TEMP:-/tmp}/rproxy-gateway-conformance
mkdir -p "$work"

SETUP_ONLY=1 scripts/e2e.sh

if [ ! -d "$work/gateway-api" ]; then
  git clone -q --depth 1 --branch "$GATEWAY_API_VERSION" https://github.com/kubernetes-sigs/gateway-api.git "$work/gateway-api"
fi
status=0
(
  cd "$work/gateway-api"
  GOTOOLCHAIN=auto go test ./conformance -run TestConformance -count=1 -timeout 100m -v -args \
    --gateway-class=rproxy \
    --conformance-profiles="$PROFILES" \
    --supported-features="$FEATURES" \
    --report-output="$REPORT" \
    --organization=max3584 --project=rproxy-gateway --url=https://github.com/max3584/rproxy-gateway \
    --version="${VERSION:-v0.4.0-dev}" --contact=https://github.com/max3584/rproxy-gateway/issues \
    --cleanup-base-resources=false \
    --usable-address="${USABLE_ADDRESS:-192.0.2.10}" --unusable-address="${UNUSABLE_ADDRESS:-0.0.0.0}"
) 2>&1 | tee "$work/conformance.log" || status=$?
echo "$status" > "$work/status"

# a short summary: top-level and sub-test results
passed=$(grep -cE '^\s*--- PASS' "$work/conformance.log" || true)
failed=$(grep -cE '^\s*--- FAIL' "$work/conformance.log" || true)
skipped=$(grep -cE '^\s*--- SKIP' "$work/conformance.log" || true)
{
  echo "## Gateway API conformance ($GATEWAY_API_VERSION)"
  echo
  echo "Profiles: \`$PROFILES\`. Tests (including sub-tests): $passed passed, $failed failed, $skipped skipped."
  echo
  if [ "$failed" -gt 0 ]; then
    echo "### Failed"
    grep -E '^\s*--- FAIL' "$work/conformance.log" | sed 's/^\s*--- FAIL: /- /' | head -100
    echo
  fi
  if [ -f "$REPORT" ]; then
    echo "### Report"
    echo '```yaml'
    cat "$REPORT"
    echo '```'
  fi
} >> "${GITHUB_STEP_SUMMARY:-/dev/stdout}"
