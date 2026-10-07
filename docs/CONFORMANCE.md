English: [en/CONFORMANCE.md](en/CONFORMANCE.md)

# Gateway API の conformance

CI の `e2e` ワークフローの `Gateway API conformance` ジョブ（`scripts/conformance.sh`）が、kind の上で Gateway API v1.6.3 の conformance テストを動かす（rproxy は rproxy-api の master からビルド）。レポートはジョブの成果物（`conformance-report`）とジョブのまとめに出る。最後のレポートは [conformance/report.yaml](conformance/report.yaml)。

## 結果（2026-10-07、rproxy-gateway v0.4.0-dev、rproxy-api master）

| プロファイル | core | extended |
|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 54 / 56（`HTTPRouteCORS`、`HTTPRouteRetry`） |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 |

core はすべて通り、CI の conformance のジョブは core が 1 つでも落ちると失敗する（extended は知らせるだけ）。落ちる extended の 2 つは rproxy 側（max3584/rproxy-api#238）：

- `HTTPRouteCORS`：許さないオリジンのプリフライトを rproxy が転送先へ送る（試験は rproxy が CORS のヘッダなしで答えることを求める）
- `HTTPRouteRetry`：転送先が 1 つのとき、状態コードでの送り直しが同じ転送先へ行かない

## 名乗っている機能（`supportedFeatures`）

core（Gateway、HTTPRoute、ReferenceGrant、TLSRoute、TCPRoute、UDPRoute）と、`GatewayPort8080`、`GatewayHTTPListenerIsolation`、`HTTPRouteMethodMatching`、`HTTPRouteQueryParamMatching`、`HTTPRouteResponseHeaderModification`、`HTTPRoutePortRedirect`、`HTTPRouteSchemeRedirect`、`HTTPRoutePathRedirect`、`HTTPRoutePathRewrite`、`TLSRouteModeTerminate`、`TLSRouteModeMixed`、`HTTPRouteParentRefPort`、`HTTPRouteDestinationPortMatching`、`HTTPRouteNamedRouteRule`、`HTTPRouteBackendProtocolWebSocket`、`HTTPRouteBackendTimeout`、`GatewayStaticAddresses`、`GatewayAddressEmpty`、`GatewayInfrastructure`、`HTTPRoute303RedirectStatusCode`、`HTTPRoute307RedirectStatusCode`、`HTTPRoute308RedirectStatusCode`、`HTTPRouteRequestTimeout`、`HTTPRouteHostRewrite`、`HTTPRouteBackendRequestHeaderModification`、`HTTPRouteCORS`、`HTTPRouteRetry`、`HTTPRouteRetryBackendTimeout`、`HTTPRouteRetryConnectionError`、`HTTPRouteRequestMirror`、`HTTPRouteRequestMultipleMirrors`、`HTTPRouteRequestPercentageMirror`、`HTTPRouteBackendProtocolH2C`、`GatewayFrontendClientCertificateValidation`、`ListenerSet`、`GRPCRoute`、`GRPCRouteNamedRouteRule`、`BackendTLSPolicy`、`BackendTLSPolicySANValidation`、`GatewayBackendClientCertificate`。`GatewayStaticAddresses` は `--usable-address=192.0.2.10`、`--unusable-address=0.0.0.0` で試す。GatewayClass の `status.supportedFeatures` と `scripts/conformance.sh` の `FEATURES` は同じ（単体テストが確かめる）。HTTPRoute の新しい機能は rproxy v0.4.0 の `features` が要る（docs/DESIGN.md の「rproxy の機能」）。

## 名乗っていないもの

| 機能 | 理由 |
|---|---|
| `GatewayFrontendClientCertificateValidationInsecureFallback` | rproxy にクライアント証明書を「求めるが検証しない」口がない（`client_auth` の `optional` は送られた証明書を検証する）。`AllowInsecureFallback` では証明書を求めず、Gateway に `InsecureFrontendValidationMode` を付ける |
| `HTTPRouteExternalAuth`、`GatewayHTTPSListenerDetectMisdirectedRequests`、Mesh | rproxy に口がない、またはこのコントローラの範囲の外 |
