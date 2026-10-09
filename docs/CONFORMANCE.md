English: [en/CONFORMANCE.md](en/CONFORMANCE.md)

# Gateway API の conformance

CI の `e2e` ワークフローの `Gateway API conformance` ジョブ（`scripts/conformance.sh`）が、kind の上で Gateway API v1.6.3 の conformance テストを動かす（rproxy は rproxy-api の master からビルド）。Gateway API の CRD は experimental channel（`experimental-install.yaml`）：名乗る機能の一部（`HTTPRouteRetry*` の `HTTPRouteRule.retry`）は experimental の欄で、standard の CRD では API サーバが捨てる。レポートはジョブの成果物（`conformance-report`）とジョブのまとめに出る。最後のレポートは [conformance/report.yaml](conformance/report.yaml)。

## 結果（2026-10-09、rproxy-gateway v0.4.5 の PR（rproxy は rproxy-api v0.4.3 の PR のブランチ）、Gateway API の experimental channel）

| プロファイル | core | extended |
|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 58 / 58 |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 |

名乗る機能の試験はすべて通る。v0.4.4 の結果（2026-10-07）から増えたのは GATEWAY-HTTP の extended の `HTTPRouteHTTPSListenerDetectMisdirectedRequests`（57 → 58）で、前から通っていた試験はすべて通る。CI の conformance のジョブは core が 1 つでも落ちると失敗する（extended は知らせるだけ）。

## リリースした成果物でのレポート（Gateway API への提出用）

Gateway API の実装一覧のバッジ（Conformant）は、kubernetes-sigs/gateway-api の `conformance/reports/<v1.x>/<組織>-<プロジェクト>/` に出したレポートで決まる。そのレポートは、リリースした chart とイメージ（ここではなにもビルドしない）から作る：`e2e` ワークフローを `report_version` を付けて手で動かすと（`gh workflow run e2e.yml -f report_version=0.4.5`）、`conformance report (published)` のジョブだけが `scripts/conformance-report.sh` を動かす。kind に Gateway API v1.6.3 の experimental channel の CRD を入れ、`oci://ghcr.io/max3584/charts/rproxy-gateway` をその版で入れ（既定のイメージ、`managed.serviceType=ClusterIP`、`managed.addressCIDRs={192.0.2.0/24}`）、Gateway API v1.6.3 の suite を 5 つのプロファイルで動かす。名乗る機能は `--supported-features` で渡さず、suite がコントローラの書いた GatewayClass の `status.supportedFeatures` から読む。成果物 `conformance-report-v<版>` に、suite が書いたままのレポート（`experimental-v<版>-default-report.yaml`）、suite とコントローラのログ、環境（版、イメージのダイジェスト）が入る。すべてのプロファイルの core と extended が success で、飛ばした試験がないときだけ成功する。

v0.4.5（2026-10-09、chart 0.4.5、`ghcr.io/max3584/rproxy-gateway:0.4.5`、`ghcr.io/max3584/rproxy-gateway/rproxy:0.4.3`、kind v0.33.0、Kubernetes v1.37.0）：

| プロファイル | core | extended | 名乗る extended の機能 |
|---|---|---|---|
| GATEWAY-HTTP | 36 / 36 | 58 / 58 | 38 / 38 |
| GATEWAY-GRPC | 14 / 14 | 11 / 11 | 10 / 10 |
| GATEWAY-TLS | 19 / 19 | 16 / 16 | 12 / 12 |
| GATEWAY-TCP | 18 / 18 | 11 / 11 | 11 / 11 |
| GATEWAY-UDP | 19 / 19 | 11 / 11 | 11 / 11 |

失敗も飛ばしもない（飛ばしたのはプロファイルの外の Mesh だけ）。各プロファイルの extended の機能はすべて名乗っている（`unsupportedFeatures` がない）。提出するもの（レポート、手順の README、実装一覧の `details.yaml`、PR の下書き、オーナーがすること）は [conformance/submission/](conformance/submission/)。

## 名乗っている機能（`supportedFeatures`）

core（Gateway、HTTPRoute、ReferenceGrant、TLSRoute、TCPRoute、UDPRoute）と、`GatewayPort8080`、`GatewayHTTPListenerIsolation`、`HTTPRouteMethodMatching`、`HTTPRouteQueryParamMatching`、`HTTPRouteResponseHeaderModification`、`HTTPRoutePortRedirect`、`HTTPRouteSchemeRedirect`、`HTTPRoutePathRedirect`、`HTTPRoutePathRewrite`、`TLSRouteModeTerminate`、`TLSRouteModeMixed`、`HTTPRouteParentRefPort`、`HTTPRouteDestinationPortMatching`、`HTTPRouteNamedRouteRule`、`HTTPRouteBackendProtocolWebSocket`、`HTTPRouteBackendTimeout`、`GatewayStaticAddresses`、`GatewayAddressEmpty`、`GatewayInfrastructure`、`HTTPRoute303RedirectStatusCode`、`HTTPRoute307RedirectStatusCode`、`HTTPRoute308RedirectStatusCode`、`HTTPRouteRequestTimeout`、`HTTPRouteHostRewrite`、`HTTPRouteBackendRequestHeaderModification`、`HTTPRouteCORS`、`HTTPRouteRetry`、`HTTPRouteRetryBackendTimeout`、`HTTPRouteRetryConnectionError`、`HTTPRouteRequestMirror`、`HTTPRouteRequestMultipleMirrors`、`HTTPRouteRequestPercentageMirror`、`HTTPRouteBackendProtocolH2C`、`GatewayFrontendClientCertificateValidation`、`GatewayFrontendClientCertificateValidationInsecureFallback`、`ListenerSet`、`GRPCRoute`、`GRPCRouteNamedRouteRule`、`BackendTLSPolicy`、`BackendTLSPolicySANValidation`、`GatewayBackendClientCertificate`、`GatewayHTTPSListenerDetectMisdirectedRequests`（v0.4.5）、`HTTPRouteExternalAuth`・`HTTPRouteExternalAuthHTTP`・`HTTPRouteExternalAuthGRPC`・`HTTPRouteExternalAuthForwardBody`（v0.4.5。Gateway API v1.6.3 の conformance にはこの名前も試験もないので、レポートには出ない。試験は本物の rproxy での `tests/rproxy.rs` と単体、rproxy-api の `tests/ext_authz.rs`）。`GatewayStaticAddresses` は `--usable-address=192.0.2.10`、`--unusable-address=0.0.0.0` で試す。GatewayClass の `status.supportedFeatures` と `scripts/conformance.sh` の `FEATURES` は同じ（単体テストが確かめる）。HTTPRoute の新しい機能は rproxy v0.4.0 の `features` が要る（docs/DESIGN.md の「rproxy の機能」）。v0.4.5 で名乗った機能と backendRef の `CORS`・`RequestRedirect`・`RequestMirror`・`ExternalAuth` のフィルタは rproxy v0.4.3 の `features`（`http_options` の `misdirected`、`server_middleware_kinds`、`forward_auth`）が要る。

## 名乗っていないもの

| 機能 | 理由 |
|---|---|
| Mesh（`MESH-HTTP`・`MESH-GRPC`） | Pod の間の通信（east-west）を取る仕組み（サイドカーの注入かノードごとのプロキシ）と、元の宛先で振り分ける rproxy の口が要り、このコントローラ（north-south）の範囲の外。見立ては [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 11.5 |
