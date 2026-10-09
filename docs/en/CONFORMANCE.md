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

## The report from the released artifacts (for submission to Gateway API)

The badge (Conformant) in Gateway API's implementations list comes from a report submitted to kubernetes-sigs/gateway-api under `conformance/reports/<v1.x>/<organization>-<project>/`. That report is made from the released chart and images (nothing is built here): running the `e2e` workflow by hand with `report_version` (`gh workflow run e2e.yml -f report_version=0.4.5`) runs only the `conformance report (published)` job, `scripts/conformance-report.sh`. It installs Gateway API v1.6.3's experimental-channel CRDs on kind, installs `oci://ghcr.io/max3584/charts/rproxy-gateway` at that version (default images, `managed.serviceType=ClusterIP`, `managed.addressCIDRs={192.0.2.0/24}`) and runs the Gateway API v1.6.3 suite with the five profiles. The claimed features are not passed with `--supported-features`: the suite reads them from the GatewayClass's `status.supportedFeatures` that the controller writes. The artifact `conformance-report-v<version>` holds the report as the suite wrote it (`experimental-v<version>-default-report.yaml`), the suite's and the controller's logs and the environment (versions, image digests). The job succeeds only when every profile is core and extended success with no skipped test.

v0.4.5 (2026-10-09, chart 0.4.5, `ghcr.io/max3584/rproxy-gateway:0.4.5`, `ghcr.io/max3584/rproxy-gateway/rproxy:0.4.3`, kind v0.33.0, Kubernetes v1.37.0):

| Profile | core | extended | claimed extended features |
|---|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 58 / 58 | 38 / 38 |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 | 10 / 10 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 | 12 / 12 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 | 11 / 11 |

Nothing failed or was skipped (only Mesh, outside the profiles, was skipped). Every extended feature of each profile is claimed (no `unsupportedFeatures`). What to submit (the report, the README with the steps, the implementations list's `details.yaml`, the PR draft, what the owner does) is in [../conformance/submission/](../conformance/submission/).

## Claimed features (`supportedFeatures`)

core (Gateway, HTTPRoute, ReferenceGrant, TLSRoute, TCPRoute, UDPRoute) plus `GatewayPort8080`, `GatewayHTTPListenerIsolation`, `HTTPRouteMethodMatching`, `HTTPRouteQueryParamMatching`, `HTTPRouteResponseHeaderModification`, `HTTPRoutePortRedirect`, `HTTPRouteSchemeRedirect`, `HTTPRoutePathRedirect`, `HTTPRoutePathRewrite`, `TLSRouteModeTerminate`, `TLSRouteModeMixed`, `HTTPRouteParentRefPort`, `HTTPRouteDestinationPortMatching`, `HTTPRouteNamedRouteRule`, `HTTPRouteBackendProtocolWebSocket`, `HTTPRouteBackendTimeout`, `GatewayStaticAddresses`, `GatewayAddressEmpty`, `GatewayInfrastructure`, `HTTPRoute303RedirectStatusCode`, `HTTPRoute307RedirectStatusCode`, `HTTPRoute308RedirectStatusCode`, `HTTPRouteRequestTimeout`, `HTTPRouteHostRewrite`, `HTTPRouteBackendRequestHeaderModification`, `HTTPRouteCORS`, `HTTPRouteRetry`, `HTTPRouteRetryBackendTimeout`, `HTTPRouteRetryConnectionError`, `HTTPRouteRequestMirror`, `HTTPRouteRequestMultipleMirrors`, `HTTPRouteRequestPercentageMirror`, `HTTPRouteBackendProtocolH2C`, `GatewayFrontendClientCertificateValidation`, `GatewayFrontendClientCertificateValidationInsecureFallback`, `ListenerSet`, `GRPCRoute`, `GRPCRouteNamedRouteRule`, `BackendTLSPolicy`, `BackendTLSPolicySANValidation`, `GatewayBackendClientCertificate`, `GatewayHTTPSListenerDetectMisdirectedRequests` (v0.4.5), `HTTPRouteExternalAuth`, `HTTPRouteExternalAuthHTTP`, `HTTPRouteExternalAuthGRPC`, `HTTPRouteExternalAuthForwardBody` (v0.4.5; Gateway API v1.6.3's conformance has neither these names nor tests, so they do not show in the report; they are tested by `tests/rproxy.rs` against a real rproxy, unit tests and rproxy-api's `tests/ext_authz.rs`). `GatewayStaticAddresses` runs with `--usable-address=192.0.2.10` and `--unusable-address=0.0.0.0`. The GatewayClass `status.supportedFeatures` and `FEATURES` of `scripts/conformance.sh` are the same (a unit test checks). The newer HTTPRoute features need the `features` of rproxy v0.4.0 (docs/en/DESIGN.md, "rproxy features"). The features claimed in v0.4.5 and the `CORS`, `RequestRedirect`, `RequestMirror` and `ExternalAuth` filters on backendRefs need rproxy v0.4.3's `features` (`misdirected` in `http_options`, `server_middleware_kinds`, `forward_auth`).

## Not claimed

| Feature | Why |
|---|---|
| Mesh (`MESH-HTTP`, `MESH-GRPC`) | needs a way to capture pod-to-pod (east-west) traffic (sidecar injection or a per-node proxy) and rproxy routing by original destination; outside this (north-south) controller. Assessment: [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) 11.5 |
