# rproxy-gateway

[![CI](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml)
[![e2e](https://github.com/max3584/rproxy-gateway/actions/workflows/e2e.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/e2e.yml)
[![cargo-deny](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Renovate](https://img.shields.io/badge/renovate-enabled-brightgreen?logo=renovatebot)](https://github.com/max3584/rproxy-gateway/issues?q=is%3Aissue+is%3Aopen+%22Dependency+Dashboard%22)

English: [README.en.md](README.en.md)

[rproxy](https://github.com/max3584/rproxy-api) の Kubernetes コントローラ。Gateway API（v1.6、standard channel）のリソースを rproxy のルールにして、rproxy の制御 API（ルールセット、`PUT /rulesets/{name}`）で反映する（max3584/rproxy-api#28、設計は rproxy-api の docs/DESIGN-v0.4.md 3.）。

- rproxy は Kubernetes の API を知らない。このコントローラと rproxy は制御 API（rproxy-api の `docs/openapi.json`）だけでつながる。rproxy v0.4.0 以降（`features.rulesets`）が要る。
- 最初のリリースは rproxy v0.4.0 と一緒に出す（マイルストーン v0.4.0）。

## できること

| リソース | rproxy |
|---|---|
| `GatewayClass`（`controllerName: rproxy.max3584.net/gateway-controller`） | `Accepted`、`supportedFeatures` |
| `Gateway` のリスナー `HTTP`・`HTTPS`・`TLS`（Passthrough / Terminate）・`TCP`・`UDP` | (プロトコル, アドレス, ポート) ごとに 1 つのルール。同じポートのリスナーはまとめる。`spec.addresses`、`infrastructure`、クライアント証明書の検証（`tls.frontend`）、backend へのクライアント証明書（`tls.backend`） |
| `ListenerSet` | Gateway のリスナーに足す（`allowedListeners`） |
| `HTTPRoute`・`GRPCRoute` | `http.routes`（path・header・query・method の一致、Gateway API の優先の順）、ヘッダの書き換え（`add` も）、リダイレクト（301〜308）、URL・Host の書き換え、CORS、ミラー、retry、タイムアウト、backendRef ごとのフィルタ、重み、h2c の backend、`RproxyMiddleware`（ExtensionRef） |
| `BackendTLSPolicy` | backend への TLS（CA・SNI・SAN） |
| `TLSRoute`・`TCPRoute`・`UDPRoute` | `tls.routes`（SNI、名前ごとに Pod の `targets`）、`targets` |
| `ReferenceGrant` | ほかの namespace の Service・Secret |
| backend | EndpointSlice の Pod の IP（rproxy が振り分けとヘルスチェックをする） |
| 状態 | Gateway・リスナー・ルートの `Accepted`・`Programmed`・`ResolvedRefs`（rproxy のルールの `conditions` から） |
| `RproxyMiddleware`・`RproxyPolicy`・`RproxyRule`（`rproxy.max3584.net/v1alpha1`） | Gateway API にない設定（ミドルウェア、L4 の制限・帯域・GeoIP・受け身のヘルスチェック、ルールそのもの） |
| 移行（`--migrate-to`） | Ingress と Traefik の IngressRoute・IngressRouteTCP・IngressRouteUDP・Middleware・TLSOption を読む（[docs/MIGRATION.md](docs/MIGRATION.md)） |

決めごとと変換の表は [docs/DESIGN.md](docs/DESIGN.md)。Gateway API の conformance の結果は [docs/CONFORMANCE.md](docs/CONFORMANCE.md)。

## 入れ方

```bash
# Gateway API の CRD（standard channel）
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml
# コントローラ（CRD、RBAC、GatewayClass rproxy）
helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway -n rproxy-gateway-system --create-namespace
```

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - {name: http, port: 80, protocol: HTTP}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
```

- 既定（managed）では、Gateway ごとに rproxy の Deployment と `LoadBalancer` の Service を Gateway の namespace に作る（`managed.serviceType`、`managed.replicas`）。Gateway の `spec.infrastructure` のラベル・注釈と `spec.addresses`（Service の `externalIPs`）を使う。
- コントローラは既定で 2 レプリカ。Lease でリーダーを選び、1 つだけが反映する（docs/DESIGN.md の「冗長化」）。
- `fleet.enabled=true` では、chart の DaemonSet（`hostNetwork: true`）の rproxy がすべての Gateway を受け持つ。
- chart の値は [charts/rproxy-gateway/values.yaml](charts/rproxy-gateway/values.yaml)。

## コマンド

| コマンド | 内容 |
|---|---|
| `rproxy-gateway controller` | コントローラ（フラグは `--help`。すべて `RPROXY_GATEWAY_*` の環境変数でも指定できる） |
| `rproxy-gateway certsync` | rproxy の Pod の中で、ボリュームの証明書のファイルが揃ったかをコントローラに答える（API は使わない） |
| `rproxy-gateway crds` | 自前の CRD の YAML を出す（chart の `crds/` と同じ） |
| `rproxy-gateway render -f <files>` | マニフェストからルールセットを描く（クラスタも rproxy も要らない。移行の下見にも） |

## 開発

```bash
cargo build
cargo test                                   # 単体テスト（偽の rproxy を含む）
RPROXY_BIN=/path/to/rproxy-api cargo test --test rproxy   # 本物の rproxy（v0.4、ルールセット）で
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

- e2e（`scripts/e2e.sh`）と conformance（`scripts/conformance.sh`）は kind で動く（Docker が要る。CI の `e2e` ワークフロー）。rproxy は rproxy-api の master（手動の実行ではほかの ref も）からビルドする。

## ライセンス

MIT（[LICENSE](LICENSE)）
