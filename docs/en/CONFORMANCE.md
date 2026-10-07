日本語: [../CONFORMANCE.md](../CONFORMANCE.md)

# Gateway API conformance

The `Gateway API conformance` job of the CI `e2e` workflow (`scripts/conformance.sh`) runs the Gateway API v1.6.3 conformance tests on kind (rproxy built from rproxy-api master). Gateway API's CRDs are the experimental channel (`experimental-install.yaml`): some claimed features (`HTTPRouteRule.retry` of `HTTPRouteRetry*`) are experimental fields, which the API server drops with the standard CRDs. The report is the job's artifact (`conformance-report`) and its summary. The latest report: [../conformance/report.yaml](../conformance/report.yaml).

## Results (2026-10-07, rproxy-gateway v0.4.0-dev, Gateway API experimental channel)

| Profile | core | extended |
|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 57 / 57 |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 |

Every test of the claimed features passes (the same result four runs in a row on the same commit). The CI conformance job fails when any core test fails (extended failures are reported only).

## Claimed features (`supportedFeatures`)

core (Gateway, HTTPRoute, ReferenceGrant, TLSRoute, TCPRoute, UDPRoute) plus `GatewayPort8080`, `GatewayHTTPListenerIsolation`, `HTTPRouteMethodMatching`, `HTTPRouteQueryParamMatching`, `HTTPRouteResponseHeaderModification`, `HTTPRoutePortRedirect`, `HTTPRouteSchemeRedirect`, `HTTPRoutePathRedirect`, `HTTPRoutePathRewrite`, `TLSRouteModeTerminate`, `TLSRouteModeMixed`, `HTTPRouteParentRefPort`, `HTTPRouteDestinationPortMatching`, `HTTPRouteNamedRouteRule`, `HTTPRouteBackendProtocolWebSocket`, `HTTPRouteBackendTimeout`, `GatewayStaticAddresses`, `GatewayAddressEmpty`, `GatewayInfrastructure`, `HTTPRoute303RedirectStatusCode`, `HTTPRoute307RedirectStatusCode`, `HTTPRoute308RedirectStatusCode`, `HTTPRouteRequestTimeout`, `HTTPRouteHostRewrite`, `HTTPRouteBackendRequestHeaderModification`, `HTTPRouteCORS`, `HTTPRouteRetry`, `HTTPRouteRetryBackendTimeout`, `HTTPRouteRetryConnectionError`, `HTTPRouteRequestMirror`, `HTTPRouteRequestMultipleMirrors`, `HTTPRouteRequestPercentageMirror`, `HTTPRouteBackendProtocolH2C`, `GatewayFrontendClientCertificateValidation`, `GatewayFrontendClientCertificateValidationInsecureFallback`, `ListenerSet`, `GRPCRoute`, `GRPCRouteNamedRouteRule`, `BackendTLSPolicy`, `BackendTLSPolicySANValidation`, `GatewayBackendClientCertificate`. `GatewayStaticAddresses` runs with `--usable-address=192.0.2.10` and `--unusable-address=0.0.0.0`. The GatewayClass `status.supportedFeatures` and `FEATURES` of `scripts/conformance.sh` are the same (a unit test checks). The newer HTTPRoute features need the `features` of rproxy v0.4.0 (docs/en/DESIGN.md, "rproxy features").

## Not claimed

| Feature | Why |
|---|---|
| `HTTPRouteExternalAuth`, `GatewayHTTPSListenerDetectMisdirectedRequests`, Mesh | no way in rproxy, or outside this controller |
