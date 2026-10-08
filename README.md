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

決めごとと変換の表は [docs/DESIGN.md](docs/DESIGN.md)。Gateway API の conformance の結果は [docs/CONFORMANCE.md](docs/CONFORMANCE.md)。テナントの分け方・既定で止めているもの・権限は [docs/SECURITY.md](docs/SECURITY.md)。

## 入れ方

```bash
# Gateway API の CRD（standard channel。HTTPRoute の retry を使うなら experimental-install.yaml）
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
- chart の値は [charts/rproxy-gateway/values.yaml](charts/rproxy-gateway/values.yaml)。コントローラの設定は ConfigMap `rproxy-gateway-config`（`RPROXY_GATEWAY_*` の環境変数）にして渡す（`controller.extraArgs` は引数のままで、ConfigMap より強い）。

### Helm を使わずに入れる（kubectl・Kustomize）

リリースに、chart から描いたマニフェストを付けている：`install.yaml`（managed）、`install-fleet.yaml`（fleet）、`crds.yaml`（rproxy の CRD だけ。GitOps で CRD を先に当てるとき）。どれも Gateway API の CRD は含まない。

```bash
kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v<版>/install.yaml
```

Kustomize では [config/default](config/default)（fleet は [config/fleet](config/fleet)）を base にする。コントローラの設定は `rproxy-gateway controller --help` の `RPROXY_GATEWAY_*` で、`configMapGenerator` の `behavior: merge` で変える（ConfigMap の名前にハッシュが付くので、変えるとコントローラが入れ替わる）。例は [config/samples](config/samples)（イメージのダイジェスト固定、managed の replicas、コントローラ 1 台）。

```yaml
apiVersion: kustomize.config.k8s.io/v1beta1
kind: Kustomization
resources:
  - github.com/max3584/rproxy-gateway//config/default?ref=v<版>
configMapGenerator:
  - name: rproxy-gateway-config
    namespace: rproxy-gateway-system
    behavior: merge
    literals: [RPROXY_GATEWAY_REPLICAS=2]
```

- `config/` は chart の既定の値を `scripts/render-config.sh` で描いたもの（手で直さない。CI が chart との食い違いを見つける）。
- RBAC の形が変わる値（`controller.watchNamespaces` の namespace ごとの Role）や、chart がほかの物を足す値（`migration.createIngressClass`）は Helm で入れる。

## 可用性（`managed.replicas` が 2 以上）

rproxy の Pod は、コントローラがルールセットを反映してから Ready になり（readiness gate）、止まるときは preStop（既定 15 秒）の間答え続ける。Gateway ごとに PodDisruptionBudget と、ノードへの分散が付く（docs/DESIGN.md の「rproxy の可用性」）。

受け入れテスト（kind 1+3 ノード、`managed.replicas=2`、HTTP・HTTPS・TCP を 100 ms ごとに新しい接続で）の、通らなかった最も長い間（秒）：

| 形（`TOPOLOGY`） | Pod の削除 | 告知するノードの Pod の削除 | drain | rollout restart | ノードが止まる |
|---|---|---|---|---|---|
| v0.4.0（MetalLB L2、Local） | 1.2 | 10.2〜14.9 | 2.0（drain 32.9 秒） | 10.8〜13.5 | 5.6 |
| MetalLB L2、`Local`（既定） | 0.2 | 0.3 | 1.2（drain 16.5 秒） | 0.2 | 7.8 |
| MetalLB L2、`Cluster` | 0.2 | 0.2 | 0.2 | 0.2 | 9.0（約 59 秒まで一部が落ちる） |
| MetalLB BGP + ECMP（BFD）、`Local` | 3.5 | — | 2.3 | 2.3 | 3.3 |
| MetalLB BGP + ECMP（BFD）、`Cluster` | 0.2 | — | 0.2 | 0.1 | 13.3（約 60 秒まで一部が落ちる） |
| NodePort + 自前の L4（HAProxy）、`Local` | 0.2 | — | 2.2 | 2.2 | 6.4 |
| NodePort + 自前の L4（HAProxy）、`Cluster` | 0.1 | — | 0.1 | 0.2 | 20.2（約 63 秒まで一部が落ちる） |

- **既定（LoadBalancer、`externalTrafficPolicy: Local`）を勧める**。クライアントの IP が rproxy に届き、Pod の入れ替え（削除・drain・rollout）の途切れは 1 秒ほどまで。MetalLB L2 は告知するノードを移すときに、その瞬間の接続を 1 つ落とすことがある（drain の 1.2 秒）。ノードが止まったときは、ロードバランサがノードの死を見つけるまで（MetalLB L2 の memberlist で 5〜8 秒）。
- **`Cluster`** は Pod の入れ替えではほぼ途切れない（どのノードも ready な Pod に送る）が、クライアントの IP は届かず、ノードが止まると、そのノードの Pod が endpoint から外れるまで（ノードが NotReady になるまで、40〜50 秒）一部の接続が落ち続ける。
- **BGP + ECMP**：`Local` では、MetalLB が終了中の Pod のあるノードの経路を Pod が消えてから取り下げる（FRR モードで 3〜4 秒かかる）ので、その間の分が落ちる（3〜4 秒）。計画した入れ替えでほぼ 0 にしたいなら `Cluster`（ノードが止まったときの尾は上と同じ）。BFD でノードの死は 1 秒ほどで経路から外れる。
- **NodePort と自前の L4 のロードバランサ**：`Local` では、ロードバランサは rproxy が止まってからでないとノードを外せない（NodePort には `healthCheckNodePort` がない）ので、止まる瞬間の接続が落ちる（2 秒ほど）。ロードバランサが Service の `healthCheckNodePort` を見られるなら `LoadBalancer` 型にするとよい（終了中の Pod だけのノードは失敗を返すので、preStop の間に外れる）。
- ノードが止まったとき、そのノードの **backend** の Pod も、ノードが NotReady になるまで EndpointSlice に残る（どの形でも、表の値の後ろに 40〜60 秒の一部の失敗が続くことがある）。backend には RproxyPolicy の `outlierDetection` を使う。backend 自身も preStop で止まるようにする（受け入れテストの backend は 5 秒。ないと drain ごとに 1〜3 秒落ちる）。

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
- 受け入れテスト（`scripts/acceptance.sh`、手動の `acceptance` ワークフロー）：公開した chart とイメージを、ノード 4 つの kind・MetalLB・cert-manager に入れ、通信を流し続けながら冗長化・証明書の更新・状態の復旧を測る。

## ライセンス

MIT（[LICENSE](LICENSE)）
