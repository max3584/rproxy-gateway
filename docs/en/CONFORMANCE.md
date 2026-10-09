日本語: [../CONFORMANCE.md](../CONFORMANCE.md)

# Gateway API conformance

The `Gateway API conformance` job of the CI `e2e` workflow (`scripts/conformance.sh`) runs the Gateway API v1.6.3 conformance tests on kind (rproxy built from rproxy-api master). Gateway API's CRDs are the experimental channel (`experimental-install.yaml`): some claimed features (`HTTPRouteRule.retry` of `HTTPRouteRetry*`) are experimental fields, which the API server drops with the standard CRDs. The report is the job's artifact (`conformance-report`) and its summary. The latest report: [../conformance/report.yaml](../conformance/report.yaml).

## Results (2026-10-09, rproxy-gateway v0.4.5 PRs (rproxy from the rproxy-api v0.4.3 PR branch), Gateway API experimental channel)

| Profile | core | extended |
|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 58 / 58 |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 |

Every test of the claimed features passes. Since v0.4.4's result (2026-10-07), GATEWAY-HTTP extended gained `HTTPRouteHTTPSListenerDetectMisdirectedRequests` (57 → 58); every test that passed before still passes. The CI conformance job fails when any core test fails (extended failures are reported only).

## Claimed features (`supportedFeatures`)

core (Gateway, HTTPRoute, ReferenceGrant, TLSRoute, TCPRoute, UDPRoute) plus `GatewayPort8080`, `GatewayHTTPListenerIsolation`, `HTTPRouteMethodMatching`, `HTTPRouteQueryParamMatching`, `HTTPRouteResponseHeaderModification`, `HTTPRoutePortRedirect`, `HTTPRouteSchemeRedirect`, `HTTPRoutePathRedirect`, `HTTPRoutePathRewrite`, `TLSRouteModeTerminate`, `TLSRouteModeMixed`, `HTTPRouteParentRefPort`, `HTTPRouteDestinationPortMatching`, `HTTPRouteNamedRouteRule`, `HTTPRouteBackendProtocolWebSocket`, `HTTPRouteBackendTimeout`, `GatewayStaticAddresses`, `GatewayAddressEmpty`, `GatewayInfrastructure`, `HTTPRoute303RedirectStatusCode`, `HTTPRoute307RedirectStatusCode`, `HTTPRoute308RedirectStatusCode`, `HTTPRouteRequestTimeout`, `HTTPRouteHostRewrite`, `HTTPRouteBackendRequestHeaderModification`, `HTTPRouteCORS`, `HTTPRouteRetry`, `HTTPRouteRetryBackendTimeout`, `HTTPRouteRetryConnectionError`, `HTTPRouteRequestMirror`, `HTTPRouteRequestMultipleMirrors`, `HTTPRouteRequestPercentageMirror`, `HTTPRouteBackendProtocolH2C`, `GatewayFrontendClientCertificateValidation`, `GatewayFrontendClientCertificateValidationInsecureFallback`, `ListenerSet`, `GRPCRoute`, `GRPCRouteNamedRouteRule`, `BackendTLSPolicy`, `BackendTLSPolicySANValidation`, `GatewayBackendClientCertificate`, `GatewayHTTPSListenerDetectMisdirectedRequests` (v0.4.5), `HTTPRouteExternalAuth`, `HTTPRouteExternalAuthHTTP`, `HTTPRouteExternalAuthGRPC`, `HTTPRouteExternalAuthForwardBody` (v0.4.5; Gateway API v1.6.3's conformance has neither these names nor tests, so they do not show in the report; they are tested by `tests/rproxy.rs` against a real rproxy, unit tests and rproxy-api's `tests/ext_authz.rs`). `GatewayStaticAddresses` runs with `--usable-address=192.0.2.10` and `--unusable-address=0.0.0.0`. The GatewayClass `status.supportedFeatures` and `FEATURES` of `scripts/conformance.sh` are the same (a unit test checks). The newer HTTPRoute features need the `features` of rproxy v0.4.0 (docs/en/DESIGN.md, "rproxy features"). The features claimed in v0.4.5 and the `CORS`, `RequestRedirect`, `RequestMirror` and `ExternalAuth` filters on backendRefs need rproxy v0.4.3's `features` (`misdirected` in `http_options`, `server_middleware_kinds`, `forward_auth`).

## Not claimed

| Feature | Why |
|---|---|
| Mesh (`MESH-HTTP`, `MESH-GRPC`) | needs a way to capture pod-to-pod (east-west) traffic (sidecar injection or a per-node proxy) and rproxy routing by original destination; outside this (north-south) controller. Assessment: [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) 11.5 |
