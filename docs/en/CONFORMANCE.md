日本語: [../CONFORMANCE.md](../CONFORMANCE.md)

# Gateway API conformance

The `Gateway API conformance` job of the CI `e2e` workflow (`scripts/conformance.sh`) runs the Gateway API v1.6.3 conformance tests on kind (rproxy built from rproxy-api master). The report is the job's artifact (`conformance-report`) and its summary. The latest report: [../conformance/report.yaml](../conformance/report.yaml).

## Results (2026-10-07, rproxy-gateway v0.4.0-dev, rproxy-api master)

| Profile | core | extended |
|---|---|---|
| GATEWAY-HTTP | 35 / 36 (failing: `HTTPRouteRequestHeaderModifier`) | 14 / 16 (`HTTPRouteResponseHeaderModifier`, `HTTPRouteRewritePath`) |
| GATEWAY-TLS | 19 / 19 | 4 / 4 (`TLSRouteModeTerminate`, `TLSRouteModeMixed` and others) |
| GATEWAY-TCP | 18 / 18 | |
| GATEWAY-UDP | 19 / 19 | |

All three failures are the `add` of HeaderModifier (append after an existing value). rproxy's `headers` middleware has only `set` and `remove`, and `add` is applied as `set` for now (max3584/rproxy-api#224). Once rproxy has `add`, the controller follows and core passes completely.

## Claimed features (`supportedFeatures`)

core (Gateway, HTTPRoute, ReferenceGrant, TLSRoute, TCPRoute, UDPRoute) plus `GatewayPort8080`, `GatewayHTTPListenerIsolation`, `HTTPRouteMethodMatching`, `HTTPRouteQueryParamMatching`, `HTTPRouteResponseHeaderModification`, `HTTPRoutePortRedirect`, `HTTPRouteSchemeRedirect`, `HTTPRoutePathRedirect`, `HTTPRoutePathRewrite`, `TLSRouteModeTerminate`, `TLSRouteModeMixed`, `HTTPRouteParentRefPort`, `HTTPRouteDestinationPortMatching`, `HTTPRouteNamedRouteRule`, `HTTPRouteBackendProtocolWebSocket`, `HTTPRouteBackendTimeout`, `GatewayStaticAddresses`, `GatewayAddressEmpty`, `GatewayInfrastructure`, `HTTPRoute303RedirectStatusCode`, `HTTPRoute307RedirectStatusCode`, `HTTPRoute308RedirectStatusCode`, `HTTPRouteRequestTimeout`, `HTTPRouteHostRewrite`, `HTTPRouteBackendRequestHeaderModification`, `HTTPRouteCORS`, `HTTPRouteRetry`, `HTTPRouteRetryBackendTimeout`, `HTTPRouteRetryConnectionError`, `HTTPRouteRequestMirror`, `HTTPRouteRequestMultipleMirrors`, `HTTPRouteRequestPercentageMirror`, `HTTPRouteBackendProtocolH2C`, `GatewayFrontendClientCertificateValidation`, `GatewayFrontendClientCertificateValidationInsecureFallback`, `ListenerSet`, `GRPCRoute`, `GRPCRouteNamedRouteRule`, `BackendTLSPolicy`, `BackendTLSPolicySANValidation`, `GatewayBackendClientCertificate`. `GatewayStaticAddresses` runs with `--usable-address=192.0.2.10` and `--unusable-address=0.0.0.0`. The GatewayClass `status.supportedFeatures` and `FEATURES` of `scripts/conformance.sh` are the same (a unit test checks). The newer HTTPRoute features need the `features` of rproxy v0.4.0 (docs/en/DESIGN.md, "rproxy features").

## Not claimed

| Feature | Why |
|---|---|
| `HTTPRouteExternalAuth`, `GatewayHTTPSListenerDetectMisdirectedRequests`, Mesh | no way in rproxy, or outside this controller |
