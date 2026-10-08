English: [en/DESIGN-v0.4.x.md](en/DESIGN-v0.4.x.md)

# v0.4.x の設計：Kubernetes での運用

v0.4.0・v0.4.1 の受け入れテストで分かった、Kubernetes で運用するときの穴を埋める。rproxy-api・UI と一緒に決めた設計（オーナーの承認済み）のうち、rproxy-gateway の分（A・B・C の gateway の側・E の gateway の側・F）をここに書く。rproxy の証明書の API（rproxy-api #240）と組の保存（#241）は rproxy-api の設計にあり、rproxy-gateway は使わない（5.）。

今の決めごとは [DESIGN.md](DESIGN.md)、テナントの線は [SECURITY.md](SECURITY.md)。

## 1. 方針

| 項目 | 決めたこと |
|---|---|
| 目的 | managed の rproxy を Gateway ごとに変えられない、Helm 以外で入れにくい、UI を Kubernetes に置けない、rproxy が SIGTERM ですぐ終わる、Service を通さない VIP がない |
| 版 | **v0.5.0 は作らない**。すべて v0.4 の系列のパッチ（v0.4.2、v0.4.3、…）で出す。CRD・値などを足すのはパッチでよいが、今の入れ方（chart の値・フラグ・Gateway）を壊さない |
| 互換 | 足すものはすべて省略でき、省略したときは v0.4.1 と同じ Pod・Service になる |
| 守り | v0.4.0 のセキュリティレビューで決めた線（テナントは自分の namespace だけ、LB の注釈の許可リスト、rproxy の Pod は Kubernetes の API を使わない、鍵を制御 API に流さない）を崩さない。崩す項目は管理者が明示して開ける |
| rproxy の新しい口 | `GET /capabilities` の `features` で見分ける（コントローラは古い rproxy でも動く） |

### 項目

| 項目 | rproxy-gateway でするもの | 出す版 |
|---|---|---|
| A. Gateway ごとの managed の rproxy | CRD `RproxyGatewayParameters`・合わせ方・状態・RBAC | v0.4.2 |
| B. Kustomize で入れる | chart のコントローラの設定を ConfigMap に、`config/`、リリースの `install.yaml`・CI | v0.4.2 |
| C. UI を Kubernetes で | UI 用の読むだけのトークンと発見の Secret（UI の chart は UI のリポジトリ） | 後のパッチ |
| E. SIGTERM での終わり方 | rproxy の `RPROXY_SHUTDOWN_*` を渡す、猶予の秒数 | rproxy-api のリリースの後 |
| F. Pod が直接持つ VIP | fleet の `vip` サイドカー・Lease・状態 | 後のパッチ |

### v0.4.1 で済んだもの

v0.4.1（#32）で managed の Pod・Service の可用性を入れた。A・E はこれを前提にし、作り直さない。

| v0.4.1 にあるもの | A・E での扱い |
|---|---|
| readiness gate `rproxy.max3584.net/ruleset-applied` | そのまま |
| preStop（`--pre-stop-secs`、既定 15 秒）、`terminationGracePeriodSeconds` = preStop + 15 | そのまま。E が入ったら 6. の形にする |
| replicas が 2 以上で PDB（`maxUnavailable: 1`）と `kubernetes.io/hostname` の topologySpread | 既定のまま。A の `podDisruptionBudget`・`pod.topologySpreadConstraints` で変えられる |
| プローブの秒数（`--readiness-probe`・`--liveness-probe`） | そのまま。readiness は `/healthz` のまま（6.） |
| `externalTrafficPolicy`（`LoadBalancer` は `Local`、`NodePort` は `Cluster`） | 既定のまま（NodePort を `Local` にする案は採らない。v0.4.1 の受け入れテストで NodePort + 自前の L4 は `Cluster` のほうが途切れが短い）。A の `service.externalTrafficPolicy` で変えられる |

## 2. A. managed の rproxy を Gateway ごとに変える

managed の Deployment・Service はコントローラが実行時に作るので、Helm の値も Kustomize も届かない。今変えられるのはフラグ（`--replicas`・`--service-type` など）で、全 Gateway まとめてだけ。`spec.infrastructure.parametersRef` を付けた Gateway は `Accepted: False`（`InvalidParameters`）。

### 2.1 形

CRD `RproxyGatewayParameters`（`rproxy.max3584.net/v1alpha1`、namespaced、shortname `rpgwp`）。GatewayClass の `spec.parametersRef`（クラスの既定）と、Gateway の `spec.infrastructure.parametersRef`（その Gateway の上書き）の両方から指す。

```yaml
apiVersion: rproxy.max3584.net/v1alpha1
kind: RproxyGatewayParameters
metadata: {name: web, namespace: team-a}
spec:
  replicas: 3
  podDisruptionBudget: {maxUnavailable: 1}
  pod:
    labels: {cost-center: a}
    annotations: {prometheus.io/scrape: "true"}
    resources:
      rproxy: {requests: {cpu: 500m, memory: 128Mi}, limits: {memory: 512Mi}}
      certsync: {requests: {cpu: 5m, memory: 16Mi}}
    topologySpreadConstraints:          # labelSelector を省けば、その Gateway の Pod を選ぶものを入れる
      - {maxSkew: 1, topologyKey: topology.kubernetes.io/zone, whenUnsatisfiable: ScheduleAnyway}
    nodeSelector: {}                    # 既定ではクラスだけ（2.5）
    tolerations: []
    affinity: {}
    priorityClassName: ""
  service:
    type: LoadBalancer
    externalTrafficPolicy: Local
    loadBalancerClass: ""
    loadBalancerSourceRanges: [203.0.113.0/24]
    ipFamilyPolicy: PreferDualStack
    labels: {}
    annotations: {}                     # Gateway の参照では LB の注釈は許可リスト（--service-annotation-prefix）を通ったものだけ
  rproxy:
    image: ""                           # クラスだけ
    logLevel: info
    performance: {workers: 4, udpShards: auto, cpuAffinity: none, busyPollUsecs: 0, splice: {enabled: true}}
    shutdown: {delay: 5s, drain: 25s}   # E。rproxy のリリースまでは形だけ（6.）
    extraEnv: []                        # クラスだけ。コントローラが決める名前は書けない
  ui: {visible: true}                   # C。UI に見せるか（Gateway はクラスの既定を false にだけできる）
  policy:                               # クラスの参照でだけ使える（Gateway から指すと InvalidParameters）
    gatewayOverrides: [replicas, podDisruptionBudget, pod.labels, pod.annotations, pod.resources,
      pod.topologySpreadConstraints, service.externalTrafficPolicy, service.loadBalancerSourceRanges,
      service.ipFamilyPolicy, service.labels, service.annotations, rproxy.logLevel, rproxy.performance,
      rproxy.shutdown, ui]
    maxReplicas: 10
    allowedPriorityClasses: []
    allowedLoadBalancerClasses: []
```

### 2.2 項目

既定の列は「どちらの参照にもないとき」。どれも v0.4.1 のフラグの値になる。

| 項目 | 写す先 | 既定 | 検証 |
|---|---|---|---|
| `replicas` | Deployment の `replicas` | `--replicas`（chart の `managed.replicas`、1） | 1〜`policy.maxReplicas`（既定 10）。0 は使わない（Gateway を止めるなら消す） |
| `podDisruptionBudget` | PDB `rproxy-<id>`（`minAvailable` か `maxUnavailable` の片方） | replicas が 2 以上なら `maxUnavailable: 1`、1 なら作らない（v0.4.1 と同じ） | 片方だけ（CRD の CEL）。`minAvailable` が replicas 以上（`100%`）、`maxUnavailable` が 0（`0%`）なら誤り（drain が止まるため） |
| `pod.labels`・`pod.annotations` | Pod のテンプレート | なし | コントローラの接頭辞（`rproxy.max3584.net/`・`app.kubernetes.io/`・`gateway.networking.k8s.io/`）のキーは誤り |
| `pod.resources.rproxy`・`.certsync` | 各コンテナ | なし（v0.4.1 と同じ） | Kubernetes の ResourceRequirements |
| `pod.topologySpreadConstraints` | Pod | replicas が 2 以上なら `kubernetes.io/hostname`・`ScheduleAnyway`（v0.4.1 と同じ） | 書けば既定と置き換え。`labelSelector` を省けばその Gateway の Pod の selector を入れる |
| `pod.nodeSelector`・`tolerations`・`affinity` | Pod | なし | Kubernetes の形 |
| `pod.priorityClassName` | Pod | なし | Gateway の参照では `policy.allowedPriorityClasses` の内 |
| `service.type` | Service | `--service-type`（`LoadBalancer`） | `LoadBalancer`・`NodePort`・`ClusterIP` |
| `service.externalTrafficPolicy` | Service | `--external-traffic-policy`、なければ `LoadBalancer` は `Local`・`NodePort` は `Cluster`（v0.4.1 と同じ） | `Local`・`Cluster` |
| `service.loadBalancerClass` | Service | なし | Gateway の参照では `policy.allowedLoadBalancerClasses` の内。作った後は変えられない（2.4） |
| `service.loadBalancerSourceRanges`・`ipFamilyPolicy` | Service | なし | CIDR、`SingleStack`・`PreferDualStack`・`RequireDualStack` |
| `service.labels`・`service.annotations` | Service | なし | ラベルは `pod.labels` と同じ。Gateway の参照の注釈は `spec.infrastructure.annotations` と同じ許可リスト（2.5） |
| `rproxy.image` | rproxy のコンテナ | `--rproxy-image` | 参照の形（`repo:tag` か `repo@sha256:…`） |
| `rproxy.logLevel` | `RPROXY_LOG_LEVEL` | なし | `error`・`warn`・`info`・`debug`・`trace` |
| `rproxy.performance` | `RPROXY_WORKERS`・`RPROXY_UDP_SHARDS`・`RPROXY_CPU_AFFINITY`・`RPROXY_BUSY_POLL_USECS`・`RPROXY_SPLICE`・`RPROXY_SPLICE_AFTER`・`RPROXY_SPLICE_FULL_READS`・`RPROXY_SPLICE_PIPE_SIZE` | なし | rproxy の `global.performance` と同じ範囲（managed の rproxy は設定ファイルを使わないので環境変数で渡す） |
| `rproxy.shutdown.delay`・`.drain` | E が入ったら `RPROXY_SHUTDOWN_DELAY`・`RPROXY_SHUTDOWN_DRAIN` と猶予（6.） | — | `0s`〜`10m`。**rproxy のリリースまでは確かめるだけで Pod には写さない** |
| `rproxy.extraEnv` | rproxy のコンテナ | なし | `RPROXY_*` だけ。コントローラが決める名前（`RPROXY_API_*`・`RPROXY_TOKEN_FILE`・`RPROXY_TLS_*`・`RPROXY_FILES_*`・`RPROXY_CONFIG`・`RPROXY_DATABASE_URL`・`RPROXY_UPDATE*`・`RPROXY_HANDOFF*`・`RPROXY_STATIC_RULES`・`RPROXY_SHUTDOWN_*`、上の項目で渡す名前）は誤り |
| `ui.visible` | C の発見の Secret に載せるか | クラスの値、なければ `true` | Gateway はクラスが `false` のとき `true` にできない。C が入るまでは写す先がない |

### 2.3 参照と合わせ方

- 合わせる順（後ろが勝つ）：コントローラのフラグ → GatewayClass の参照 → Gateway の参照 → Gateway の `spec.infrastructure.labels`・`annotations` → コントローラのラベル（selector に使うもの）。
- 合わせ方：スカラーは置き換え。マップ（`labels`・`annotations`・`nodeSelector`）はキーごと。リスト（`tolerations`・`topologySpreadConstraints`・`loadBalancerSourceRanges`・`extraEnv`）とオブジェクト（`affinity`・`resources.rproxy`・`podDisruptionBudget`・`performance` など）は丸ごと置き換え（Gateway API の GEP-1867 と同じ考え方。部分の合わせ方を覚えなくて済む）。
- GatewayClass の `parametersRef`：`group: rproxy.max3584.net`・`kind: RproxyGatewayParameters`・`namespace` は**コントローラの namespace だけ**（ほかは GatewayClass が `Accepted: False`、`InvalidParameters`）。クラスの参照は管理者のもので、`policy` を持てる。
- Gateway の `infrastructure.parametersRef`：Gateway API の決まりで同じ namespace（`LocalParametersReference`）。ほかの namespace は指せない（ReferenceGrant でも開けない）。
- 参照先が変われば、その GatewayClass・Gateway を描き直す（watch する）。Pod のテンプレートが変わればローリング更新（`maxUnavailable: 0`、readiness gate、preStop で通信は止めない）。
- fleet：Gateway の参照は `Accepted: False`（`InvalidParameters`、「fleet では使えない」）。クラスの参照は確かめるが、fleet の DaemonSet には効かない（chart・Kustomize で変える。B）。

### 2.4 検証と状態

- 形は CRD の OpenAPI と CEL で先に断る（`minAvailable` と `maxUnavailable` の片方だけ、列挙の値、`replicas` の最小）。Kubernetes の形の項目（`resources`・`tolerations`・`affinity`・`topologySpreadConstraints`・`extraEnv`）は CRD を小さく保つため中身を CRD で縛らず、コントローラが読めるか確かめる。参照の関係（`policy` の範囲、許可リスト）もコントローラが確かめる。
- 参照先がない・種類が違う・中身が誤り・Gateway が許されていない項目を書いた：Gateway は `Accepted: False`、reason `InvalidParameters`、message に項目の名前（例 `spec.pod.tolerations: not allowed by the GatewayClass (policy.gatewayOverrides)`）。GatewayClass の参照が誤りなら GatewayClass が `Accepted: False`（`InvalidParameters`）で、そのクラスの Gateway も同じ reason。
- **前に動いていた Gateway は止めない**（10. Q2）：v0.4.1 までは、受け付けられない Gateway の Deployment・Service をごみとして消していた（`collect_garbage`）。参照の書き間違い 1 つで通信が止まらないように、Deployment がもうある Gateway は最後に正しかった形のまま残す（Deployment・Service・PDB を変えない、証明書の Secret と組の PUT は続ける）。状態は `Accepted: False`（`InvalidParameters`）と `Programmed: True`（message「前の parameters のまま」）。Deployment がまだない Gateway は作らない。参照を直せば次の反映で新しい形になる。
- 変えられない項目：`service.loadBalancerClass` の変更は `InvalidParameters`（「Gateway を作り直す」）で前の Service のまま。`service.type` の変更は通す（LB のアドレスが変わることを文書に書く）。
- 参照先の CR に `status` は付けない（v1alpha1。どの Gateway が使っているかは Gateway の状態で分かる）。

### 2.5 誰が何を決められるか

| 項目 | Gateway の参照（テナント） | 理由 |
|---|---|---|
| replicas・PDB・resources・Pod のラベル／注釈・topologySpread・externalTrafficPolicy・sourceRanges・ipFamilyPolicy・Service のラベル・logLevel・performance・shutdown・ui | 既定で許す | 自分の namespace の中で済む。量は ResourceQuota・LimitRange と `maxReplicas` で抑える |
| Service の注釈 | 許すが、LB のアドレスを決める接頭辞（`metallb.universe.tf/` など）は許可リスト（`--service-annotation-prefix`）を通ったものだけ。ほかは `InvalidParameters` | v0.4 の `spec.infrastructure.annotations` と同じ線（CVE-2020-8554 の形）。参照では黙って落とさず誤りにする |
| `service.type`・`loadBalancerClass` | 既定で許さない | 外へのアドレスを取れる（クラスの `policy` で開ける） |
| nodeSelector・tolerations・affinity・priorityClassName | 既定で許さない | 専用ノード・コントロールプレーンに載る、ほかを追い出す（クラスの `policy` で開ける） |
| `rproxy.image`・`extraEnv` | 許さない（`policy` でも開けない） | Gateway の制御 API の資格を持つ Pod で任意のイメージ・設定を動かせる |
| `policy` | 書けない | クラスの参照のもの |

- `policy.gatewayOverrides` で開け閉めする（省略すると上の「既定で許す」）。`rproxy.image`・`rproxy.extraEnv`・`policy` と知らない名前はリストに書けない（書けばクラスの参照が誤り）。
- クラスの参照に書いた値（管理者のもの）は `allowedPriorityClasses` などに縛られない。

### 2.6 RBAC

- コントローラ：`rproxygatewayparameters` の get・list・watch（`watchNamespaces` のときは Role）。PDB の権限は v0.4.1 からある。ごみ集めは v0.4.1 と同じ（PDB を含む）だが、2.4 の「前の形のまま残す」Gateway は消さない。
- テナント：chart が ClusterRole `rproxy-gateway-parameters-edit`（`rproxygatewayparameters` の読み書き）を作り、`rbac.authorization.k8s.io/aggregate-to-admin: "true"` を付ける（namespace の admin に渡す。edit には渡さない）。`rbac.aggregateToAdmin: false` で切れる。
- managed の Pod の権限は変えない（トークンなし・RBAC なし）。

### 2.7 chart と互換

- chart の値 `managed.parameters`（既定 `{}`）：空でなければ、chart がクラスの既定の `RproxyGatewayParameters`（`rproxy-default`、リリースの namespace）を描き、GatewayClass の `parametersRef` で指す。**空なら描かず、GatewayClass も v0.4.1 と同じ**（`parametersRef` なし）。
  - 既定で描かないのは、`helm upgrade` が chart の `crds/` の新しい CRD を入れないため：v0.4.1 から上げたクラスタには `RproxyGatewayParameters` の CRD がなく、描けば upgrade が失敗する。使うときは先に CRD を入れる（`kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.2/rproxy.max3584.net.yaml`）。新しく `helm install` するときは `crds/` から入る。
- `--replicas`・`--service-type` などのフラグは残し、「参照にない項目の既定」になる。
- 参照のない Gateway・GatewayClass は v0.4.1 と同じ Pod・Service。
- コントローラは CRD がなくても動く（今の CRD と同じく、後から入れれば再起動なしで watch する）。CRD のないときに参照した Gateway は `InvalidParameters`（見つからない）。

### 2.8 試験

- 単体：合わせ方（順・マップ・リスト）、`policy` の許可、許可リストの注釈、`extraEnv` の禁止の名前、PDB と replicas、`InvalidParameters` の message。`render` の試験（`src/render/tests.rs` の `InvalidParameters`）を「動く参照」と「誤った参照」に分ける。
- 前の形のまま残すこと：正しい参照 → 誤った参照に変えても、Deployment のある Gateway は描き直さず、ごみとして消さないこと。
- e2e（kind）：参照で replicas・resources・topologySpread を変え、Deployment に写ること。namespace の admin の権限（集約した ClusterRole）で tolerations を書いて `InvalidParameters`、前の Pod が残り通信が続くこと。
- conformance：`GatewayInfrastructurePropagation` は今のまま通ること（参照のない Gateway は変わらない）。
- 受け入れ：シナリオ h・i（7.）。

## 3. B. Kustomize で入れる

### 3.1 出すもの

| もの | 中身 | 作り方 |
|---|---|---|
| リリースの `install.yaml` | managed（chart の既定）：Namespace `rproxy-gateway-system`、rproxy の CRD、RBAC、コントローラ、GatewayClass | `helm template` |
| リリースの `install-fleet.yaml` | 同じで fleet（`fleet.enabled=true`） | `helm template` |
| リリースの `crds.yaml` | rproxy の CRD だけ（GitOps で CRD を先に当てるため。Gateway API の CRD は入れない、今と同じ） | `rproxy-gateway crds` |
| リポジトリの `config/` | `config/crd/`（CRD）、`config/default/`（managed の base：`kustomization.yaml` と描いた `rproxy-gateway.yaml`）、`config/fleet/`（fleet の base）、`config/samples/`（overlay の例：イメージのダイジェスト固定、`watchNamespaces`、resources、1 台、クラスの既定の parameters） | `scripts/render-config.sh`（`helm template` → ファイル） |

- 使い方：`kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.2/install.yaml`、または `resources: [github.com/max3584/rproxy-gateway//config/default?ref=v0.4.2]`。
- 描いたものには Helm のラベル（`helm.sh/chart`・`app.kubernetes.io/managed-by: Helm`）を付けない（chart に値 `rendered: true` を足し、`_helpers.tpl` で外す）。
- 描くのは決まった値だけ（chart は乱数を使わない。秘密はコントローラが最初の起動で作る）ので、同じ chart から同じファイルになる。

### 3.2 食い違いを防ぐ

- `config/` は手で直さない。CI に `config (kustomize)` のジョブ（Alpine、`apk add helm kustomize`）：`scripts/render-config.sh` → `git diff --exit-code config/`、`kustomize build` を `config/default`・`config/fleet`・`config/samples/*` で通す、kubeconform（Gateway API の CRD のスキーマも入れる）。CRD の今の確かめ（`cargo run -- crds` との比較）と同じ形。
- 必須のチェックへはジョブが main に入ってから足す。
- chart を変える PR は `config/` も描き直す（CI が食い違いを見つける）。
- リリースのワークフロー（`release.yml`）が `install.yaml`・`install-fleet.yaml`・`crds.yaml` を作って添付する（今の chart の `.tgz` と CRD の隣）。

### 3.3 コントローラの設定を Kustomize で変えやすくする

- 今の chart はコントローラの設定を `args` で渡すので、Kustomize では args のリストの何番目かを JSON patch で直すことになる。フラグはすべて環境変数（`RPROXY_GATEWAY_*`）でも読める（clap の `env`）ので、chart は設定を ConfigMap `rproxy-gateway-config` にして `envFrom` で渡し、`args` は `[controller]` だけにする。Kustomize では `configMapGenerator`（`behavior: merge`）で変えられる。**chart の値は変えない**（今の values のまま動く）。
- 注意：clap は引数が環境変数に勝つ。chart の `controller.extraArgs` はそのまま args に足す（環境変数より強い）。
- ConfigMap の中身が変わったら Pod を入れ替える（Helm：テンプレートの注釈にハッシュ。Kustomize：`configMapGenerator` の名前のハッシュ）。

### 3.4 何をどこで変えるか

| 変えたいもの | Helm | Kustomize |
|---|---|---|
| コントローラ（replicas・resources・nodeSelector・フラグ） | `controller.*`・`managed.*` | `config/default` に patch、`configMapGenerator` |
| fleet の DaemonSet | `fleet.*` | `config/fleet` に patch |
| managed の Pod・Service（全 Gateway の既定） | `managed.parameters`（2.7） | `RproxyGatewayParameters` `rproxy-default` を足し、GatewayClass に `parametersRef` を patch（`config/samples/` に例） |
| managed の Pod・Service（1 つの Gateway） | テナントが Gateway の namespace に `RproxyGatewayParameters` を書いて `infrastructure.parametersRef` で指す（A） | 同じ |

### 3.5 試験

- CI の `config (kustomize)`（上）。
- 受け入れ（`acceptance.yml`）に入力 `install: helm | kustomize` を足す。`kustomize` では `config/default` に overlay（replicas とイメージ）を当てて入れ、同じシナリオを回す（B の PR の後で）。

## 4. C. UI を Kubernetes で（gateway の側）

UI のイメージ・chart（`oci://ghcr.io/max3584/charts/rproxy-ui`）・migration の Job・利用量は UI のリポジトリの設計に書く。UI の chart は rproxy-gateway の chart の subchart にしない（版を別々に進める、UI は VM の rproxy だけを相手にしても使える）。gateway の chart には UI との結びの値だけ足す。

### 4.1 UI が Kubernetes の rproxy を見る

今の資格：managed の rproxy は Gateway ごとのトークン（マスタートークンから HMAC で導いたもの、スコープ `rules:read`・`rules:write`・`acme:write`）と、CA が出した制御 API の証明書。どちらもコントローラ用で、UI に渡すと書き込める。

**コントローラが UI 用の読むだけの資格を作り、UI の namespace に 1 つの Secret で渡す**（両側の明示が要る）。

1. 管理者が gateway の chart で `ui.namespace: rproxy-ui`（`--ui-namespace`）と `ui.podSelector`（既定 `app.kubernetes.io/name: rproxy-ui`）を決める。決めなければ何も作らない（今と同じ）。
2. 各 Gateway の `tokens.yaml` に 2 つ目のトークン `rproxy-ui` を足す：導き方は `HMAC(master, "rproxy-gateway-ui/<id>")`（コントローラのトークンと別の値）、スコープは **`rules:read`・`metrics:read` だけ**。書き込みは rproxy が `403` で断るので、UI の作りに頼らない。rproxy はトークンファイルを読み直すので Pod の入れ替えは要らない。
3. コントローラは UI の namespace に Secret `rproxy-ui-discovery` を書く：`nodes.yaml`（Gateway ごとのグループ `k8s:<ns>/<name>` と Pod ごとのノード、`url: https://<Pod の IP>:9443`、`tls_server_name: <id>.rproxy-api.rproxy-gateway.internal`、`readonly: true`）、`ca.crt`（CA の証明書だけ。鍵は入れない）、`token-<id>`。parameters の `ui.visible: false` の Gateway は載せない。fleet ではすべての fleet の Pod（トークンは `rproxy-gateway-token` に足す）。終わりかけの Pod も消えるまで載せる（利用量を最後まで取るため）。
4. NetworkPolicy：managed の Gateway の NetworkPolicy に、UI の namespace の `ui.podSelector` から 9443 を足す（`ui.visible` の Gateway だけ）。certsync（9444）は足さない。
5. UI：`RPROXY_UI_K8S_DISCOVERY=/etc/rproxy-ui/k8s`（Secret のボリューム）を読み、ファイルの更新時刻で読み直す。

- 遅れ：kubelet が Secret のボリュームを更新するまで 1〜2 分。Pod の IP が変わってすぐは古い IP に聞いて失敗し、次の読み直しで直る（読むだけの画面と利用量なので許す）。
- 守り：UI の namespace の Secret を読める人は、UI に見せたすべての Gateway のルール（宛先・ラベル）と統計を読める。書けない、鍵はない、ほかの Gateway の rproxy に書き込めない。SECURITY.md に書く。
- 選ばなかった案：

| 案 | 選ばなかった理由 |
|---|---|
| UI が Kubernetes の API で Pod と Gateway の Secret を読む | UI にクラスタ全体の Secret を読む権限が要る（`resourceNames` では list・watch を絞れない） |
| UI がコントローラに聞き、コントローラが rproxy に取り次ぐ | コントローラが UI の通信の道になり、UI の認証という新しい口が要る。中身は同じ読むだけ。Pod の IP の遅れが困るなら考える |
| コントローラのトークンを UI に渡す | 書ける（`rules:write`）。組の持ち主も同じになり `409 owned` の守りが効かない |

### 4.2 試験（gateway の側）

- 単体：UI のトークンの導き方とスコープ、発見の Secret の中身（鍵がない、`ui.visible: false` が載らない）、NetworkPolicy。
- 受け入れ：入力 `ui: true` で UI の chart も入れ、発見の Secret の Pod が UI に出る、UI のトークンで `PUT /rulesets` が `403`。

## 5. rproxy-api #240・#241（rproxy-gateway は使わない）

- #240 証明書の API（`PUT /certs/{name}`）：鍵が制御 API を流れる。rproxy-gateway は今どおり Secret のボリュームと certsync を使う（rproxy-api の設計 3.3、[DESIGN.md](DESIGN.md) の「選ばなかった形」）。managed の Pod のルートは読むだけで、書ける場所もない。
- #241 組の保存：正は etcd にあり、rproxy の再起動後はコントローラが `/readyz` を待って PUT し直す。保存すると、rproxy が止まっている間に消した Gateway のルールが戻ってくる。コントローラのトークンは `persist` を持たないので、何もしなくても保存されない。

## 6. E. SIGTERM での終わり方（gateway の側）

rproxy-api が `--shutdown-delay` / `RPROXY_SHUTDOWN_DELAY`（SIGTERM の後、`/readyz` を `draining` にしたまま受け付け続ける）と `--shutdown-drain` / `RPROXY_SHUTDOWN_DRAIN`（待ち受けを閉じて今の接続の終わりを待つ）を足す（バイナリの既定はどちらも 0、`features.graceful_shutdown`）。

### 6.1 つなぐ時期

- **rproxy-api のリリースにこの 2 つが入ってから**つなぐ（それまでは A の `rproxy.shutdown` は確かめるだけ）。つなぐときに chart の rproxy のイメージ（`rproxy.image.tag`、`--rproxy-image` の既定）もそのリリースに上げる。
- 古い rproxy のイメージを `rproxy.image` で使うと `RPROXY_SHUTDOWN_*` は効かない（知らない環境変数は無視される）。そのため preStop を外すのは、Pod の rproxy が `features.graceful_shutdown` を持つと分かってからにする（コントローラは Pod の `/capabilities` を聞いている。分からない間は preStop を残す）。

### 6.2 形（つないだ後）

- managed：`RPROXY_SHUTDOWN_DELAY`・`RPROXY_SHUTDOWN_DRAIN` を A の `rproxy.shutdown`（既定 `5s`・`25s`）から渡す。preStop の `sleep` は E の `delay` に置き換え（イメージにシェルが要らない、Kubernetes 1.29 でも同じ形、`/readyz` が `draining` を返せる）、`terminationGracePeriodSeconds` を `delay` + `drain` + 5（既定 35）にする。置き換えは受け入れテストで v0.4.1（preStop 15 秒）と比べてから決め、preStop を残すほうが途切れが短ければ preStop + `delay` + `drain` + 5 にする。
- fleet：chart の `fleet.shutdown`（同じ既定）。hostNetwork なので外の LB・VIP の外れ方に合わせて `delay` を決めることを文書に書く。
- コントローラは終わりかけ（`deletionTimestamp` あり）の Pod に PUT しない（今と同じ）。

### 6.3 readiness を `/readyz` にするか（10. Q4）

設計の案は readiness の probe を `/healthz` から `/readyz` に変える（`draining` ですぐに外れる）ものだった。v0.4.1 の結果と合わせると、**今は `/healthz` のままにする**：

- 終わる Pod は、削除が決まったときに EndpointSlice で `ready: false` になる（readiness の結果によらない）。kube-proxy・クラウドの LB はこれで新しい接続を外す。`/readyz` を見ても、ここは早くならない。
- `/readyz` が `draining` を返すと、readiness が落ちて endpoint の `serving` も `false` になる。これは v0.4.1 で選ばなかった形（止まる Pod の gate を先に `False` にする）と同じ動きで、MetalLB L2 + `externalTrafficPolicy: Local` では告知が移るまでそのノードに来た通信を kube-proxy が落とし、途切れが長くなった（[DESIGN.md](DESIGN.md) の「rproxy の可用性」）。
- 起動のときは readiness gate（ルールセットを反映したか）が `/readyz`（rproxy の準備）より強い条件なので、`/readyz` にしても早くも安全にもならない。
- E をつなぐ PR で、受け入れテストの l2-local・bgp・nodeport-lb を `/healthz` と `/readyz` の両方で回し、途切れの短いほうを既定にする（`managed.readinessProbe` に `path` を足して選べるようにする）。

### 6.4 試験

- 単体：環境変数と猶予の秒数、`features.graceful_shutdown` のない Pod では preStop が残ること。
- 受け入れ：シナリオ b・c・d の「失敗したリクエスト」を要約に出す（今の決まりどおり、失敗だけでは落とさない。入力 `strict: true` で 0 でなければ落とす）。

## 7. F. rproxy の Pod が直接持つ VIP

Service・LoadBalancer を通さず rproxy の Pod が VIP を持ち、冗長の切り替えを数秒以内にする。今の managed は Service（MetalLB など）の切り替えに任せ、fleet はノードの IP（`--fleet-address`）を書くだけで、VIP の移し方を持たない。

### 7.1 案

| 案 | 中身 | 良いところ | 困るところ |
|---|---|---|---|
| 1. fleet + VIP のサイドカー（Lease） | fleet の DaemonSet の Pod に `vip` のコンテナ。VIP ごとの Lease を取った Pod がノードのインタフェースに VIP を足し、gratuitous ARP（IPv6 は unsolicited NA）を出す。止めるときは Lease を手放して VIP を外す | 予定の移動（rollout・drain）は 1 秒未満。持ち主は API サーバの Lease 1 つで決まる。rproxy の準備と結べる | API サーバに頼る（7.4）。予定外のノードの喪失は Lease の期限（既定 3 秒）まで |
| 1a. その中身に kube-vip を使う | kube-vip を DaemonSet のサイドカーにする | 使われている実装。ARP・NDP・BGP がある | rproxy の準備（ルールセットが入ったか）で持つかを決められない。イメージと権限が増える。版を追う相手が増える |
| 1b. 自前の `rproxy-gateway vip`（Rust） | 同じイメージの新しいサブコマンド。netlink でアドレスを足し外し、packet socket で ARP、ICMPv6 の raw socket で NA | rproxy の `/readyz` とコントローラの「反映した」を条件にできる。Lease の権限を名前で絞れる。状態を Gateway に書ける | 書く量（1,000 行ほど）と依存（`rtnetlink` など。`cargo deny`） |
| 2. VRRP（keepalived）を fleet の Pod の間で | VM の act / stb と同じ形 | API サーバに頼らない | 分断で両方が MASTER。マルチキャストか `unicast_peer`（DaemonSet の Pod の IP は変わる）。VRID の衝突。設定の生成が要る |
| 3. CNI の BGP で Pod・LB の IP を広告（Calico・Cilium） | managed のまま、CNI が /32 を広告 | hostNetwork が要らない。ECMP で active-active | CNI 次第。こちらで作るものがない |
| 4. managed で hostNetwork を選べるようにして VIP のサイドカー | Gateway ごとの Pod をノードのネットワークに | Gateway ごとの VIP | テナントの namespace に hostNetwork（PodSecurity privileged）。同じノードの Gateway どうしのポートがぶつかる。「テナントを分けるのは managed」が崩れる |

**採る案：1b（fleet + 自前の `vip` サイドカー、Lease）**。3 は文書だけ（「managed で Service を使わずに速く切り替えたいとき」）。2・4 は作らない。

### 7.2 形

```yaml
fleet:
  enabled: true
  hostNetwork: true
  vip:
    enabled: false
    addresses: [192.0.2.10, 192.0.2.11, "2001:db8::10"]   # 管理者が決める。fleet のアドレス（Gateway の status）になる
    interface: ""                 # 空なら VIP と同じサブネットの経路を持つインタフェース
    leaseDuration: 3s
    renewInterval: 1s
    retryInterval: 500ms
    garp: {count: 3, interval: 200ms}   # 取った直後に出す数（IPv6 は NA）
    onApiUnreachable: hold        # hold | release（7.4）
```

- 持ち主の決め方：VIP ごとの Lease `rproxy-vip-<VIP のハッシュ>`（コントローラの namespace。chart が先に作り、`vip` のコンテナは `resourceNames` で絞った get・update・watch だけ）。期限の切れた Lease は、持っている VIP の少ない Pod から先に取る（取りに行くまでの待ちを「持っている数 × 200ms」にする）。
- 持つ条件：同じ Pod の rproxy の `/readyz` が ready、かつコントローラから「この Pod にすべての Gateway のルールセットを反映した」の知らせ（certsync と同じく Pod の IP で受け、マスタートークンから導いたトークン付き）を受けていること。どちらかが崩れたら（E の `draining` を含む）すぐ手放す。
- 手放し方：Lease の `holderIdentity` を空にして更新 → VIP をインタフェースから外す。ほかの Pod は Lease を watch しているので、すぐ取って足し、gratuitous ARP / NA を出す。
- アドレス：IPv4 は `/32`、IPv6 は `/128` を `nodad` で足す。
- rproxy は変えない：fleet のルールは `0.0.0.0`（`--listen-addr`）で待ち受けるので、足した VIP にそのまま届く。UDP は `IP_PKTINFO` / `IPV6_RECVPKTINFO` で届いたアドレスから返す。VIP ごとに同じポートを別の Gateway に使うには `IP_FREEBIND` が要り、rproxy-api の変更になる（10. Q19）。
- 状態：Gateway の `status.addresses` は VIP。VIP の持ち主がいなければ Gateway の `Programmed: False`（`AddressNotUsable`、「VIP 192.0.2.10 を持つ Pod がない」）。`/metrics`（`vip` のコンテナ、Pod の IP の 9445）：`rproxy_vip_held{vip}`、`rproxy_vip_transitions_total{vip,reason}`。ログ：`vip.acquire`・`vip.release`（`shutdown`・`not_ready`・`lease_lost`・`conflict`）。

### 7.3 Gateway の `spec.addresses` と守り

- fleet では今も `spec.addresses` は fleet のアドレスのどれかだけで、ほかは `AddressNotUsable`。VIP を使うときは VIP が fleet のアドレスになる。Gateway は VIP を選べるが、新しい VIP を作れない（VIP の一覧は管理者が chart で決めるだけ）。
- コントローラは起動時に、VIP が `--address-cidr`（chart は fleet でも `managed.addressCIDRs` を渡す）の内にあること、Service の ClusterIP・externalIPs・LB の IP・ノードの IP と重ならないことを確かめ、外れた VIP は使わない。`addressCIDRs` が空なら VIP は使えない（「既定では使えない」と同じ）。
- `0.0.0.0` で待ち受けるので、どの VIP に来てもポートが合えばその Gateway に届く。fleet は 1 つの信頼の範囲（[SECURITY.md](SECURITY.md)）なので許す。
- managed は今のまま Service。同じクラスタで両方を使うなら GatewayClass を分ける（コントローラを 2 つ）。

### 7.4 API サーバに届かないとき

- `hold`（既定）：rproxy が ready のうちは VIP を持ち続ける。ほかの Pod も API サーバに届かなければ取れないので、二重にはならない。持ち主だけが API サーバから切れ、ほかが Lease を取った場合に備えて、`vip` のコンテナは ARP / NA を聞き、**ほかの MAC がその VIP を告げたらすぐ手放す**。
- `release`：Lease の期限で手放す（kube-vip と同じ）。コントロールプレーンが落ちるとデータプレーンも落ちる。

### 7.5 権限と PodSecurity

- `vip` のコンテナ：root、`capabilities: {drop: [ALL], add: [NET_ADMIN, NET_RAW]}`、`readOnlyRootFilesystem`、`allowPrivilegeEscalation: false`。
- Kubernetes の API の資格は `vip` のコンテナだけ：Pod の `automountServiceAccountToken: false` のまま、`projected` の `serviceAccountToken` のボリュームを `vip` のコンテナにだけつなぐ。ServiceAccount `rproxy-gateway-vip`、Role は `leases` の get・update・patch・watch（`resourceNames` で VIP の Lease だけ）。
- fleet はもう hostNetwork なので namespace は PodSecurity の `privileged`（今と同じ）。

### 7.6 切り替えの時間（見込み）

| できごと | 案 1b | MetalLB L2 | MetalLB BGP |
|---|---|---|---|
| 予定の移動（rollout・drain・Pod の削除） | 1 秒未満 | 数秒（v0.4.1 の測定では preStop で 0.2〜1.2 秒） | 経路の取り下げ（数秒、BFD なら 1 秒未満） |
| ノードの喪失 | `leaseDuration`（既定 3 秒）+ 0.5 秒ほど | memberlist の検知（5〜8 秒） | BGP の hold timer（BFD なら 1 秒ほど） |
| rproxy だけが落ちた | `/readyz` が落ちてすぐ | Service の宛先から外れてから | 同じ |

- どの方法でも、VIP が移るとそのとき張られていた TCP の接続は切れる。UDP のセッションも新しいノードで作り直し。E の `drain` は VIP の移動では効かない。
- active-active：VIP を複数にしてノードに散らし、DNS のラウンドロビンで配る。1 つの VIP の上で複数ノードに分けたいなら BGP（MetalLB・CNI）。

### 7.7 IPv6

- `/128` を `nodad` で足し、unsolicited NA（Override）を取った直後に 3 回送る（`garp.count`）。
- fleet の rproxy は `--listen-addr` に `::` があるときだけ IPv6 の VIP を受ける。IPv6 の VIP があって `::` がなければ起動時に誤り。

### 7.8 試験

- 単体：Lease の取り方（期限、持っている数の待ち、手放し）、条件（`/readyz`・反映の知らせ）、`hold` と他の MAC を見て手放すこと、VIP の確かめ（`addressCIDRs`・重なり）、ARP・NA のパケットの形。
- 受け入れ：入力 `mode: managed | fleet-vip`。`fleet-vip` では kind の docker のネットワークから VIP を選び、ランナーから VIP へ HTTP・TCP・UDP を流し続けて測る。シナリオ j. 持ち主の Pod を消す、k. `kubectl rollout restart ds/rproxy`、l. 持ち主のノードを drain、m. `docker stop <持ち主のノード>`、n. `docker pause <持ち主のノード>`（戻ったときに二重にならないこと）、o. control-plane のコンテナを `docker pause`（`hold` で通信が続くこと）。要約に MetalLB L2 との比較を出す。

## 8. 受け入れテスト（`acceptance.yml`）の変更

| 入力 | 既定 | 中身 | 入れる PR |
|---|---|---|---|
| `managed_replicas` | `2` | 今と同じ（`managed.replicas`） | — |
| `install` | `helm` | `kustomize` で `config/default` に overlay を当てて入れる | B の後 |
| `ui` | `false` | UI の chart も入れ、4.2 の確認をする | C |
| `strict` | `false` | E の確認で失敗のリクエストが 1 つでもあれば落とす | E |
| `mode` | `managed` | `fleet-vip` で 7.8 のシナリオ j〜o | F |

途切れの上限（`GAP_LIMIT`）で落とすのは、rproxy-gateway が決める Pod の削除・drain・rollout・parameters の変更を、l2-local・l2-cluster・nodeport-lb で測ったときだけ。`bgp` の途切れ（経路の取り下げ・BFD）とノードの喪失は CNI・ロードバランサ・ネットワークの側の時間なので記録だけ（`info (network-dependent)`）。

足すシナリオ（A）。今の a〜g の後に回す。`RproxyGatewayParameters` の CRD がない（公開した古い chart）ときは SKIP。

- h. Gateway の parameters を変える：namespace の admin の権限で `RproxyGatewayParameters` を書き、Gateway の `infrastructure.parametersRef` で指す（replicas を 1 つ増やす、resources）。通信を流したまま入れ替わり、`Accepted` が True のまま、途切れが `GAP_LIMIT` 以下。
- i. 誤った parameters：同じ権限で tolerations を書く → Gateway が `InvalidParameters`・`Programmed: True`、Deployment は変わらず通信が続く（途切れが `GAP_LIMIT` 以下）。直すと `Accepted: True` に戻る。

## 9. 進め方

1. この文書（docs の PR）。
2. A：`RproxyGatewayParameters`（v0.4.2）。
3. B：chart の ConfigMap、`config/`、リリースの `install.yaml`（v0.4.2）。A と B は chart を両方変えるので、後から入るほうが `scripts/render-config.sh` で `config/` を描き直す。
4. E：rproxy-api のリリースの後、`RPROXY_SHUTDOWN_*` をつなぐ（6.）。
5. C：UI の chart の後、発見の Secret と UI のトークン。
6. F：fleet の VIP。大きいので、ほかと別のパッチにする。
7. 受け入れ（8.）は各 PR のブランチで手で回し、v0.4.1 から下がっていないことを確かめる。

## 10. 決めたこと

| # | 決めること | 決めたこと |
|---|---|---|
| Q1 | A の CRD を 1 つにするか、クラス用の cluster-scoped の種類を分けるか | **1 つ**（namespaced、クラスの参照はコントローラの namespace だけ、`policy` はクラスの参照だけ） |
| Q2 | 参照が誤りになったとき、前に動いていた Gateway をどうするか | **前の形のまま残す**（`Accepted: False`・`Programmed: True`）。Deployment のある Gateway の rproxy を、誤った参照のために消さない |
| Q3 | rproxy のバイナリの既定の `drain` | rproxy-api の設計で決める（バイナリの既定は 0）。managed の既定は `5s`・`25s`（6.） |
| Q4 | 既定を変えるもの：replicas 2 以上の PDB、NodePort の `externalTrafficPolicy: Local`、readiness の `/readyz`、猶予 | PDB は v0.4.1 で済んだ。NodePort は `Cluster` のまま（v0.4.1 の測定）。`/readyz` は E のときに受け入れテストで決める（6.3）。猶予は E のときに 6.2 の形 |
| Q5 | テナントが既定で決められる項目 | 2.5 の表のとおり。`service.type`・`loadBalancerClass`・nodeSelector・tolerations・affinity・priorityClass はクラスの `policy` で開ける。`image`・`extraEnv` はテナントには開けない |
| Q6 | chart のコントローラの設定を args から ConfigMap（`envFrom`）に移すか | **移す**（chart の値は変わらない） |
| Q7 | UI の chart を gateway の chart の subchart にするか | **しない**（別の chart、版も別）。gateway の chart には `ui.namespace` などの値だけ |
| Q8 | UI が Kubernetes の rproxy を見る方法 | **コントローラが読むだけのトークンと発見の Secret を UI の namespace に書く**（管理者の `ui.namespace` と Gateway の `ui.visible`） |
| Q9 | Kubernetes のルールを UI の利用者に見せるか | **管理者だけ**。namespace と Keycloak のグループの対応は後で |
| Q17 | F の方式 | **案 1b：fleet + 自前の `rproxy-gateway vip`（Lease、gratuitous ARP / NA）** |
| Q18 | API サーバに届かないときの VIP | **`hold`**（ほかの MAC が同じ VIP を告げたらすぐ手放す） |
| Q19 | VIP ごとに待ち受けを分けるか | **分けない**（`0.0.0.0` のまま）。要るなら rproxy-api に `IP_FREEBIND` の issue |
| Q20 | VIP を Gateway が新しく求められるか | **管理者の一覧だけ**（Gateway は一覧から選ぶ） |
| Q21 | Lease の既定 | **期限 3 秒・更新 1 秒・やり直し 0.5 秒** |
| Q22 | F をいつ出すか | v0.4 の系列の別のパッチ（A〜E を待たせない） |
| — | 版 | **v0.5.0 は作らない**。v0.4.2 から順にパッチで出す |
| — | chart のクラスの既定の parameters | **`managed.parameters` が空でないときだけ描く**（`helm upgrade` は新しい CRD を入れないため。2.7） |

Q10〜Q16（UI の migration・MariaDB、rproxy-api の #240・#241、利用量）は UI・rproxy-api の設計にある。
