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

core (Gateway, HTTPRoute, ReferenceGrant, TLSRoute, TCPRoute, UDPRoute) plus `GatewayPort8080`, `GatewayHTTPListenerIsolation`, `HTTPRouteMethodMatching`, `HTTPRouteQueryParamMatching`, `HTTPRouteResponseHeaderModification`, `HTTPRoutePortRedirect`, `HTTPRouteSchemeRedirect`, `HTTPRoutePathRedirect`, `HTTPRoutePathRewrite`, `HTTPRouteParentRefPort`, `HTTPRouteDestinationPortMatching`, `HTTPRouteNamedRouteRule`, `HTTPRouteBackendProtocolWebSocket`, `HTTPRouteBackendTimeout`, `TLSRouteModeTerminate`, `TLSRouteModeMixed`. The GatewayClass `status.supportedFeatures` and `FEATURES` of `scripts/conformance.sh` are the same.

## Not claimed

| Feature | Why |
|---|---|
| `HTTPRouteHostRewrite` | rproxy sets `Host` from the client or the backend URL (no way to rewrite it) |
| `HTTPRouteRequestMirror` and friends | rproxy has no mirroring |
| `HTTPRoute303/307/308RedirectStatusCode` | `redirect_regex` answers 301 / 302 (308 / 307 for methods other than GET) |
| `HTTPRouteRequestTimeout` | rproxy's `timeouts.response` runs until the headers, not for the whole request |
| `HTTPRouteBackendRequestHeaderModification` | filters per backendRef (rproxy has no per-server middlewares) |
| `HTTPRouteCORS`, `HTTPRouteRetry*`, `HTTPRouteBackendProtocolH2C` | not mapped yet (CORS and retry exist as rproxy middlewares) |
| `GatewayStaticAddresses`, `GatewayInfrastructure`, `GatewayAddressEmpty` | Gateway `addresses` / `infrastructure` not handled yet |
| `ListenerSet`, `GatewayFrontendClientCertificateValidation`, `BackendTLSPolicy`, `GRPCRoute` | not yet |
