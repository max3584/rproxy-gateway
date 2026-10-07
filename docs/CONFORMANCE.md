English: [en/CONFORMANCE.md](en/CONFORMANCE.md)

# Gateway API の conformance

CI の `e2e` ワークフローの `Gateway API conformance` ジョブ（`scripts/conformance.sh`）が、kind の上で Gateway API v1.6.3 の conformance テストを動かす（rproxy は rproxy-api の master からビルド）。Gateway API の CRD は experimental channel（`experimental-install.yaml`）：名乗る機能の一部（`HTTPRouteRetry*` の `HTTPRouteRule.retry`）は experimental の欄で、standard の CRD では API サーバが捨てる。レポートはジョブの成果物（`conformance-report`）とジョブのまとめに出る。最後のレポートは [conformance/report.yaml](conformance/report.yaml)。

## 結果（2026-10-07、rproxy-gateway v0.4.0-dev、Gateway API の experimental channel）

| プロファイル | core | extended |
|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 57 / 57 |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 |

名乗る機能の試験はすべて通る（同じコミットで 4 回続けて同じ結果）。CI の conformance のジョブは core が 1 つでも落ちると失敗する（extended は知らせるだけ）。

## 名乗っている機能（`supportedFeatures`）

core（Gateway、HTTPRoute、ReferenceGrant、TLSRoute、TCPRoute、UDPRoute）と、`GatewayPort8080`、`GatewayHTTPListenerIsolation`、`HTTPRouteMethodMatching`、`HTTPRouteQueryParamMatching`、`HTTPRouteResponseHeaderModification`、`HTTPRoutePortRedirect`、`HTTPRouteSchemeRedirect`、`HTTPRoutePathRedirect`、`HTTPRoutePathRewrite`、`TLSRouteModeTerminate`、`TLSRouteModeMixed`、`HTTPRouteParentRefPort`、`HTTPRouteDestinationPortMatching`、`HTTPRouteNamedRouteRule`、`HTTPRouteBackendProtocolWebSocket`、`HTTPRouteBackendTimeout`、`GatewayStaticAddresses`、`GatewayAddressEmpty`、`GatewayInfrastructure`、`HTTPRoute303RedirectStatusCode`、`HTTPRoute307RedirectStatusCode`、`HTTPRoute308RedirectStatusCode`、`HTTPRouteRequestTimeout`、`HTTPRouteHostRewrite`、`HTTPRouteBackendRequestHeaderModification`、`HTTPRouteCORS`、`HTTPRouteRetry`、`HTTPRouteRetryBackendTimeout`、`HTTPRouteRetryConnectionError`、`HTTPRouteRequestMirror`、`HTTPRouteRequestMultipleMirrors`、`HTTPRouteRequestPercentageMirror`、`HTTPRouteBackendProtocolH2C`、`GatewayFrontendClientCertificateValidation`、`GatewayFrontendClientCertificateValidationInsecureFallback`、`ListenerSet`、`GRPCRoute`、`GRPCRouteNamedRouteRule`、`BackendTLSPolicy`、`BackendTLSPolicySANValidation`、`GatewayBackendClientCertificate`。`GatewayStaticAddresses` は `--usable-address=192.0.2.10`、`--unusable-address=0.0.0.0` で試す。GatewayClass の `status.supportedFeatures` と `scripts/conformance.sh` の `FEATURES` は同じ（単体テストが確かめる）。HTTPRoute の新しい機能は rproxy v0.4.0 の `features` が要る（docs/DESIGN.md の「rproxy の機能」）。

## 名乗っていないもの

| 機能 | 理由 |
|---|---|
| `HTTPRouteExternalAuth`、`GatewayHTTPSListenerDetectMisdirectedRequests`、Mesh | rproxy に口がない、またはこのコントローラの範囲の外 |
