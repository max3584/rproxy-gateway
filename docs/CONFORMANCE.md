English: [en/CONFORMANCE.md](en/CONFORMANCE.md)

# Gateway API の conformance

CI の `e2e` ワークフローの `Gateway API conformance` ジョブ（`scripts/conformance.sh`）が、kind の上で Gateway API v1.6.3 の conformance テストを動かす（rproxy は rproxy-api の master からビルド）。レポートはジョブの成果物（`conformance-report`）とジョブのまとめに出る。最後のレポートは [conformance/report.yaml](conformance/report.yaml)。

## 結果（2026-10-07、rproxy-gateway v0.4.0-dev、rproxy-api master）

| プロファイル | core | extended |
|---|---|---|
| GATEWAY-HTTP | 35 / 36（落ちるのは `HTTPRouteRequestHeaderModifier`） | 14 / 16（`HTTPRouteResponseHeaderModifier`、`HTTPRouteRewritePath`） |
| GATEWAY-TLS | 19 / 19 | 4 / 4（`TLSRouteModeTerminate`、`TLSRouteModeMixed` ほか） |
| GATEWAY-TCP | 18 / 18 | |
| GATEWAY-UDP | 19 / 19 | |

落ちる 3 つはどれも HeaderModifier の `add`（既にある値の後ろに足す）。rproxy の `headers` ミドルウェアには `set` と `remove` しかなく、いまは `add` を `set` にしている（max3584/rproxy-api#224）。rproxy に `add` が入れば、コントローラをそれに合わせて core がそろう。

## 名乗っている機能（`supportedFeatures`）

core（Gateway、HTTPRoute、ReferenceGrant、TLSRoute、TCPRoute、UDPRoute）と、`GatewayPort8080`、`GatewayHTTPListenerIsolation`、`HTTPRouteMethodMatching`、`HTTPRouteQueryParamMatching`、`HTTPRouteResponseHeaderModification`、`HTTPRoutePortRedirect`、`HTTPRouteSchemeRedirect`、`HTTPRoutePathRedirect`、`HTTPRoutePathRewrite`、`TLSRouteModeTerminate`、`TLSRouteModeMixed`、`HTTPRouteParentRefPort`、`HTTPRouteDestinationPortMatching`、`HTTPRouteNamedRouteRule`、`HTTPRouteBackendProtocolWebSocket`、`HTTPRouteBackendTimeout`、`GatewayStaticAddresses`、`GatewayAddressEmpty`、`GatewayInfrastructure`、`HTTPRoute303RedirectStatusCode`、`HTTPRoute307RedirectStatusCode`、`HTTPRoute308RedirectStatusCode`、`HTTPRouteRequestTimeout`、`HTTPRouteHostRewrite`、`HTTPRouteBackendRequestHeaderModification`、`HTTPRouteCORS`、`HTTPRouteRetry`、`HTTPRouteRetryBackendTimeout`、`HTTPRouteRetryConnectionError`、`HTTPRouteRequestMirror`、`HTTPRouteRequestMultipleMirrors`、`HTTPRouteRequestPercentageMirror`、`HTTPRouteBackendProtocolH2C`、`GatewayFrontendClientCertificateValidation`。`GatewayStaticAddresses` は `--usable-address=192.0.2.10`、`--unusable-address=0.0.0.0` で試す。GatewayClass の `status.supportedFeatures` と `scripts/conformance.sh` の `FEATURES` は同じ（単体テストが確かめる）。HTTPRoute の新しい機能は rproxy v0.4.0 の `features` が要る（docs/DESIGN.md の「rproxy の機能」）。

## 名乗っていないもの

| 機能 | 理由 |
|---|---|
| `ListenerSet`、`BackendTLSPolicy`、`GRPCRoute` | まだ |
| `GatewayFrontendClientCertificateValidationInsecureFallback` | rproxy にクライアント証明書を「求めるが検証しない」口がない（`client_auth` の `optional` は送られた証明書を検証する）。`AllowInsecureFallback` では証明書を求めず、Gateway に `InsecureFrontendValidationMode` を付ける |
| `HTTPRouteExternalAuth`、`GatewayHTTPSListenerDetectMisdirectedRequests`、Mesh | rproxy に口がない、またはこのコントローラの範囲の外 |
