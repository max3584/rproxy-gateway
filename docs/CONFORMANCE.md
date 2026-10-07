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

core（Gateway、HTTPRoute、ReferenceGrant、TLSRoute、TCPRoute、UDPRoute）と、`GatewayPort8080`、`GatewayHTTPListenerIsolation`、`HTTPRouteMethodMatching`、`HTTPRouteQueryParamMatching`、`HTTPRouteResponseHeaderModification`、`HTTPRoutePortRedirect`、`HTTPRouteSchemeRedirect`、`HTTPRoutePathRedirect`、`HTTPRoutePathRewrite`、`HTTPRouteParentRefPort`、`HTTPRouteDestinationPortMatching`、`HTTPRouteNamedRouteRule`、`HTTPRouteBackendProtocolWebSocket`、`HTTPRouteBackendTimeout`、`TLSRouteModeTerminate`、`TLSRouteModeMixed`、`GatewayStaticAddresses`（`--usable-address=192.0.2.10`、`--unusable-address=0.0.0.0`）、`GatewayAddressEmpty`、`GatewayInfrastructure`。GatewayClass の `status.supportedFeatures` と `scripts/conformance.sh` の `FEATURES` は同じ。

## 名乗っていないもの

| 機能 | 理由 |
|---|---|
| `HTTPRouteHostRewrite` | rproxy は `Host` をクライアントのものか backend の URL のものにする（書き換える口がない） |
| `HTTPRouteRequestMirror` 系 | rproxy にミラーがない |
| `HTTPRoute303/307/308RedirectStatusCode` | `redirect_regex` は 301 / 302（GET 以外は 308 / 307） |
| `HTTPRouteRequestTimeout` | rproxy の `timeouts.response` はヘッダまでで、リクエスト全体の時間ではない |
| `HTTPRouteBackendRequestHeaderModification` | backendRef ごとのフィルタ（rproxy の servers ごとのミドルウェアがない） |
| `HTTPRouteCORS`、`HTTPRouteRetry*`、`HTTPRouteBackendProtocolH2C` | 変換がまだ（CORS・retry は rproxy のミドルウェアにある） |
| `ListenerSet`、`GatewayFrontendClientCertificateValidation`、`BackendTLSPolicy`、`GRPCRoute` | まだ |
