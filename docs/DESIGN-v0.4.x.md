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
| C. UI を Kubernetes で | UI 用の読むだけのトークンと発見の Secret（UI の chart は UI のリポジトリ） | v0.4.3 |
| E. SIGTERM での終わり方 | rproxy の `RPROXY_SHUTDOWN_*` を渡す、猶予の秒数 | v0.4.2（rproxy v0.4.1） |
| F. Pod が直接持つ VIP | fleet の `vip` サイドカー・Lease・状態 | v0.4.4 |

### v0.4.1 で済んだもの

v0.4.1（#32）で managed の Pod・Service の可用性を入れた。A・E はこれを前提にし、作り直さない。

| v0.4.1 にあるもの | A・E での扱い |
|---|---|
| readiness gate `rproxy.max3584.net/ruleset-applied` | そのまま |
| preStop（`--pre-stop-secs`、既定 15 秒）、`terminationGracePeriodSeconds` = preStop + 15 | `features.graceful_shutdown` のない rproxy だけ。ある rproxy は 6. の形（preStop なし、delay + drain + 5） |
| replicas が 2 以上で PDB（`maxUnavailable: 1`）と `kubernetes.io/hostname` の topologySpread | 既定のまま。A の `podDisruptionBudget`・`pod.topologySpreadConstraints` で変えられる |
| プローブの秒数（`--readiness-probe`・`--liveness-probe`） | そのまま。readiness のパスは `features.graceful_shutdown` のある rproxy では `/readyz`（6.3） |
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
    shutdown: {delay: 15s, drain: 25s}  # E（6.）
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
| `rproxy.shutdown.delay`・`.drain` | `RPROXY_SHUTDOWN_DELAY`・`RPROXY_SHUTDOWN_DRAIN` と猶予（6.） | `--shutdown-delay`（15 秒）・`--shutdown-drain`（25 秒） | `0s`〜`10m`。`features.graceful_shutdown` のない rproxy には写さない（preStop のまま） |
| `rproxy.extraEnv` | rproxy のコンテナ | なし | `RPROXY_*` だけ。コントローラが決める名前（`RPROXY_API_*`・`RPROXY_TOKEN_FILE`・`RPROXY_TLS_*`・`RPROXY_FILES_*`・`RPROXY_CONFIG`・`RPROXY_DATABASE_URL`・`RPROXY_UPDATE*`・`RPROXY_HANDOFF*`・`RPROXY_STATIC_RULES`・`RPROXY_SHUTDOWN_*`、上の項目で渡す名前）は誤り |
| `ui.visible` | C の発見の Secret に載せるか | クラスの値、なければ `true` | Gateway はクラスが `false` のとき `true` にできない（v0.4.3 から。`ui.namespace` のときだけ効く） |

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

1. 管理者が gateway の chart で `ui.namespace: rproxy-ui`（`--ui-namespace`）と `ui.podSelector`（既定 `app.kubernetes.io/name: rproxy-ui`・`app.kubernetes.io/component: ui`）を決める。決めなければ何も作らない（今と同じ）。
2. 各 Gateway の `tokens.yaml` に 2 つ目のトークン `rproxy-ui` を足す：導き方は `HMAC(master, "rproxy-gateway-ui/<id>")`（コントローラのトークンと別の値）、スコープは **`rules:read`・`metrics:read` だけ**。書き込みは rproxy が `403` で断るので、UI の作りに頼らない。（実装で分かったこと：rproxy v0.4.1 はトークンファイルを起動時と SIGHUP でしか読み直さないので、Pod が 1 回入れ替わる。4.2）
3. コントローラは UI の namespace に Secret `rproxy-ui-discovery` を書く：`nodes.yaml`（Gateway ごとのグループ `k8s:<ns>/<name>` と Pod ごとのノード、`url: https://<Pod の IP>:9443`、`tls_server_name: <id>.rproxy-api.rproxy-gateway.internal`、`readonly: true`）、`ca.crt`（CA の証明書だけ。鍵は入れない）、`token-<id>`。parameters の `ui.visible: false` の Gateway は載せない。fleet ではすべての fleet の Pod（トークンは `rproxy-gateway-token` に足す）。載せるのは UI のトークンを受け付ける Pod だけ：Ready（readiness gate の `ruleset-applied` も True）、終わりかけでない、今のトークンファイルの Pod のテンプレート（`rproxy.max3584.net/api` のハッシュ）から作られた Pod（v0.4.3 で決めた。古い Pod に 401 を受け続けると rproxy が UI の送信元を締め出すため。終わる Pod の最後の 1 間隔の利用量は取らない、Q16）。
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

### 4.2 v0.4.3 で入れた形

4.1 のとおり。細かいところ：

- `nodes.yaml` のノードは `k8s:<ns>/<gateway>/<Pod の名前>`（fleet は `k8s:fleet/<Pod の名前>`）、各ノードに `url`・`tls_server_name`・`tls_ca: ca.crt`・`token_file: token-<id>`・`readonly: true`。グループも `readonly: true`。グループは名前の順、Pod は名前の順で、中身が変わったときだけ書く。Pod のない Gateway はグループを作らない。
- 見せる Gateway がなくなったら Secret を消す（UI の chart は Secret のボリュームを `optional: true` でマウントする）。`ui.namespace` を外したときは、前の namespace の Secret を手で消す。
- 見せるのは parameters が正しい Gateway だけ（`InvalidParameters` で前の形のまま残した Gateway は載せず、UI のトークンも外す）。
- fleet の Pod は、受け持つ Gateway がすべて見せるときだけ載せる（fleet の Pod はすべての Gateway のルールを持つため）。
- UI のトークンはトークンファイルにあり、トークンファイルは Pod のテンプレートのハッシュ（`rproxy.max3584.net/api`）に入る。rproxy v0.4.1 はトークンファイルを起動時と SIGHUP でしか読み直さない（rproxy-api docs/API.md）ので、`ui.namespace` を決めたとき・Gateway を見せる／隠すときに、その Gateway の Pod が 1 回入れ替わる（rollout は `maxUnavailable: 0`・readiness gate・graceful shutdown）。`ui.namespace` が空なら Secret も Deployment も今までと同じ。fleet の DaemonSet は chart のものなので、UI のトークンを足した後は手で `rollout restart` する。入れ替えをなくすには rproxy がトークンファイルの変化を見て読み直す（証明書の `RPROXY_CERT_CHECK_SECS` と同じ）必要がある（max3584/rproxy-api#253。入ったら、その機能を `features` で答える rproxy では入れ替えない。コードの TODO）。replicas が 2 以上なら入れ替えの途切れは 1 秒に満たない（受け入れテストの d. rollout restart と同じ形）。
- v0.4.4（rproxy v0.4.2、max3584/rproxy-api#253）：rproxy が変わったトークンファイルを読み直す（`features.tokens_reload`、`RPROXY_TOKENS_CHECK_SECS` 既定 10 秒）。その rproxy（出荷するイメージ、または Pod がそう答えたイメージ）では UI のトークンをハッシュに入れず、Pod を入れ替えない。fleet も `rollout restart` が要らない。代わりにコントローラが UI のトークンで `GET /rules` を聞き（受け付けるまで Pod ごとに 15 秒おき。rproxy は 1 分に 20 回断られた送り元を締め出す）、受け付けた Pod だけを発見の Secret に載せる。

### 4.3 試験（gateway の側）

- 単体：UI のトークンの導き方とスコープ、発見の Secret の中身（鍵がない、`ui.visible: false` が載らない）、NetworkPolicy。
- 受け入れ：入力 `ui: true` で UI の chart も入れ、発見の Secret の Pod が UI に出る、UI のトークンで `PUT /rulesets` が `403`。

## 5. rproxy-api #240・#241（rproxy-gateway は使わない）

- #240 証明書の API（`PUT /certs/{name}`）：鍵が制御 API を流れる。rproxy-gateway は今どおり Secret のボリュームと certsync を使う（rproxy-api の設計 3.3、[DESIGN.md](DESIGN.md) の「選ばなかった形」）。managed の Pod のルートは読むだけで、書ける場所もない。
- #241 組の保存：正は etcd にあり、rproxy の再起動後はコントローラが `/readyz` を待って PUT し直す。保存すると、rproxy が止まっている間に消した Gateway のルールが戻ってくる。コントローラのトークンは `persist` を持たないので、何もしなくても保存されない。

## 6. E. SIGTERM での終わり方（gateway の側）

rproxy-api が `--shutdown-delay` / `RPROXY_SHUTDOWN_DELAY`（SIGTERM の後、`/readyz` を `draining` にしたまま受け付け続ける）と `--shutdown-drain` / `RPROXY_SHUTDOWN_DRAIN`（待ち受けを閉じて今の接続の終わりを待つ）を足す（バイナリの既定はどちらも 0、`features.graceful_shutdown`）。

### 6.1 つなぐ時期

- rproxy-api v0.4.1 で入った。chart の rproxy のイメージ（`rproxy.image.tag`、`--rproxy-image` の既定、release.yml の `RPROXY_VERSION`）を 0.4.1 に上げた。
- 古い rproxy のイメージを `rproxy.image` で使うと `RPROXY_SHUTDOWN_*` は効かず、SIGTERM ですぐ止まる。そのため新しい形（preStop なし、`/readyz`）にするのは、Pod の rproxy が `features.graceful_shutdown` を持つと分かってから：コントローラが出荷するイメージ（`--rproxy-image` の既定）はそのまま、ほかのイメージはそれを動かす Pod の `/capabilities` が答えてから（Pod がもう一度入れ替わる。分かったイメージはコントローラが動いている間覚える）。分からない間は v0.4.1 の形（preStop、`/healthz`、`RPROXY_SHUTDOWN_*` なし）。

### 6.2 形

- managed（`features.graceful_shutdown` のある rproxy）：`RPROXY_SHUTDOWN_DELAY`・`RPROXY_SHUTDOWN_DRAIN` を A の `rproxy.shutdown`、なければコントローラの `--shutdown-delay`（既定 15 秒）・`--shutdown-drain`（既定 25 秒）（chart の `managed.shutdown`）から渡す。preStop は付けず、`terminationGracePeriodSeconds` は delay + drain + 5（既定 45）。readiness は `/readyz`（6.3）。
- `managed.preStopSeconds` / `--pre-stop-secs` は既定を未設定にした（新しい rproxy には preStop なし、古い rproxy には 15 秒）。設定してある入れ方（0.4.1 で値を書いたもの）は値を守り、どの Pod にもその preStop を付ける（猶予は preStop + delay + drain + 5）。
- fleet：chart の `fleet.shutdown`（既定 delay 5 秒・drain 25 秒、猶予は delay + drain + 5）。hostNetwork で Service の endpoint がないので、外の LB・VIP のヘルスチェックを `https://<ノード>:9443/readyz` に向け、それが外すまでの時間（間隔 × 回数）より delay を長くする（README と values に書いた）。
- コントローラは終わりかけ（`deletionTimestamp` あり）の Pod に PUT しない（今と同じ。rproxy も delay・drain の間は変更を `503 shutting_down` で断る）。

#### 受け入れテストでの比較

rproxy v0.4.1、replicas 2、l2-local（MetalLB L2 + `Local`）と l2-cluster（MetalLB L2 + `Cluster`）で 6 通りを 1 回ずつ回した。値はシナリオの「200 のない最長の時間」（秒）。b1・b2 は Pod の削除（告知していないノード・しているノード）、c は drain、d は rollout restart、h は parameters の変更、f はコントローラと rproxy を一緒に再起動（上限なし）。失敗は b1〜h の失敗したリクエストの合計。

| 形 | l2-local b1/b2/c/d/h | 失敗 | f | l2-cluster b1/b2/c/d/h | 失敗 |
|---|---|---|---|---|---|
| A. preStop 15・delay 0・drain 0・`/healthz`（v0.4.1） | 0.3/1.2/1.2/0.1/1.2 | 9 | 1.3 | 0.1/0.1/0.1/0.2/0.2 | 0 |
| B. preStop 15・delay 0・drain 25・`/healthz` | 0.2/**4.7**/0.2/0.2/0.2 | 122 | 6.1 | 0.2/0.2/0.2/0.2/0.2 | 0 |
| C. preStop 0・delay 15・drain 25・`/healthz` | 0.1/**6.1**/1.2/0.2/1.2 | 140 | 5.8 | 0.2/0.2/0.3/1.2/0.2 | 1 |
| **D. preStop 0・delay 15・drain 25・`/readyz`（採った形）** | 1.2/0.2/1.2/1.2/1.2 | 12 | 0.2 | 0.1/0.2/0.2/0.2/0.2 | 0 |
| E. preStop 15・delay 0・drain 25・`/readyz` | 0.2/**4.0**/0.2/1.2/0.2 | 104 | 2.6 | 0.2/0.2/0.2/0.2/0.2 | 0 |
| F. preStop 10・delay 5・drain 25・`/healthz` | 0.1/**6.0**/1.2/1.2/0.2 | 139 | 4.6 | 0.1/0.1/0.2/0.1/1.2 | 1 |

- drain を足し、待ち受けを閉じる前に endpoint を外さない形（B・C・E・F）は、l2-local の b2（告知しているノードの Pod の削除）で 4〜6 秒途切れて `GAP_LIMIT`（3 秒）を超えた。待ち受けを閉じてから Pod が消えるまで（接続の終わりを待つ数秒）endpoint が `serving` のまま残り、MetalLB は Pod が消えるまで告知を移さない。E は `/readyz` でも delay 0 なので、readiness が落ちる（最大 4 秒）前に待ち受けが閉じる。
- D は SIGTERM で `/readyz` が `draining` になり、delay の間（rproxy はまだ受け付ける）に endpoint が外れて MetalLB が告知を移す。途切れは v0.4.1（A）と同じ程度（l2-local で 1 リクエストの 1.2 秒まで）で、そのうえ今の接続は drain で終われる。l2-cluster ではどの形も差がない。
- delay を 5 秒（設計の案）にするのは測っていない。readiness が落ちるまで最大 4 秒（2 秒 × 2 回）かかるので、既定は v0.4.1 の preStop と同じ 15 秒にした。

### 6.3 readiness を `/readyz` にするか（10. Q4）

**`features.graceful_shutdown` のある rproxy では `/readyz`**（`managed.readinessProbe.path` / `--readiness-path` で `/healthz` にもできる）。古い rproxy は `/healthz` のまま。

- 設計の時点では、`/readyz` の `draining` で endpoint の `serving` が先に落ちるのは v0.4.1 で選ばなかった形（止まる Pod の gate を先に `False` にする）と同じで、MetalLB L2 + `Local` で途切れが長くなると考えていた。測ると、delay の間 rproxy が受け付け続けるので、D（`/readyz`）が A と同じ程度で、`/healthz` で drain を足した形（B・C）より短かった（6.2 の表）。
- liveness は `/healthz` のまま（`draining` の rproxy を再起動させない）。
- 起動のときは readiness gate（ルールセットを反映したか）が `/readyz` より強い条件なので、起動の早さは変わらない。
- bgp・nodeport-lb は比べていない（bgp は記録だけ）。必要になれば `managed.readinessProbe.path` で `/healthz` に戻せる。

### 6.4 試験

- 単体：環境変数と猶予の秒数（フラグ・parameters・丸め）、`features.graceful_shutdown` のない rproxy では preStop と `/healthz` が残り `RPROXY_SHUTDOWN_*` を渡さないこと、イメージの分かり方（出荷するイメージ、Pod の `/capabilities`、覚えていること）。
- e2e：`rproxy:e2e`（rproxy-api の master）は出荷するイメージではないので、Pod が `graceful_shutdown` を答えた後に新しい形（preStop なし、`/readyz`、`RPROXY_SHUTDOWN_*`、猶予 45 秒）に入れ替わること。
- 受け入れ：失敗したリクエストは前から要約に出ている。入力 `strict`（既定 false）で、`GAP_LIMIT` を当てるシナリオ（b・c・d・h・i）に失敗したリクエストが 1 つでもあれば落とす（bgp は記録だけのまま）。`source=checkout` で chart の rproxy のイメージがまだ出ていなければ、rproxy-api の同じ版のリリースのバイナリから作る。

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

### 7.9 v0.4.4 で入れた形

7.2〜7.8 のとおり。設計から変えたところと細かいところ：

- 値：`fleet.vip.addresses` の 1 つは、アドレス（正規の形。`2001:0db8::10` ではなく `2001:db8::10`）か `{address, interface, nodeSelector}`。`interface` が空なら `fleet.vip.interface`、それも空なら VIP を含むサブネットのアドレスを持つインタフェース。`nodeSelector` はその VIP を持てるノードのラベル。`fleet.vip.garp`・`onApiUnreachable`・`metricsPort`（既定 9445）・`resources`。chart は `fleet.enabled`・`hostNetwork`・`managed.addressCIDRs`（空なら VIP は使えない）、IPv6 の VIP に `::` の待ち受けがなければ誤りにする。コントローラには `RPROXY_GATEWAY_FLEET_VIPS` と `RPROXY_GATEWAY_ADDRESS_CIDR` を渡す（VIP を使うときだけ。使わない fleet は今までと同じ）。
- 持つ条件の「反映の知らせ」は、新しい口を作らず readiness gate `rproxy.max3584.net/ruleset-applied` を使う：今の rproxy の再起動の回数で `True` なら反映済み（コントローラが rproxy の再起動で `False` に戻すのと同じ判断）。`vip` は自分の Pod を watch する。
- 手放す順：VIP を外してから Lease の holder を空ける（設計の逆。二重に持つ時間をなくす。差は数ミリ秒）。
- drain：`kubectl drain` は DaemonSet の Pod を追い出さないので、ノードが cordon されたら VIP を手放す（理由 `cordoned`）。cordon のノードの Pod は、ほかの Pod が取らないまま Lease の期限の 2 倍が過ぎたときだけ取り、取ったものは cordon では手放さない（すべてのノードが cordon でも VIP がなくならない）。`vip` は自分のノードを watch する（ラベル・cordon）。
- 期限の数え方：Lease の `renewTime`（持ち主の時計）ではなく、自分が Lease の変化を最後に見てからの時間で数える（client-go の leader election と同じ。ノードの時計を比べない）。
- API サーバが戻った直後：持ち主でない Pod は、API サーバに届かなかったときから期限の分は期限切れの Lease を取らない（持ち主が先に更新できる）。watch のやり直しは kube の既定（最大 30 秒）ではなく 1 秒おき。
- ほかの MAC：ARP（送り主のアドレスが VIP）と NS（送り元が VIP）・NA（対象が VIP）を聞く。持ち主が期限内に Lease を更新できていればほかの MAC は古いので告げ直し、できていなければ（`hold` で API サーバに届かない）手放して期限の分は取らない。ほかのノードが unicast で答える ARP・NA は聞こえない（broadcast・multicast だけ）。
- 権限：ServiceAccount のトークンの投影は Pod の ServiceAccount のものしか作れないので、VIP を使うときは fleet の Pod の ServiceAccount を `rproxy-gateway-vip` にし（`automountServiceAccountToken: false` のまま）、`projected` のトークンを `vip` のコンテナにだけつなぐ。Role：`leases` の get・list・watch・update（`resourceNames` で VIP の Lease だけ。list・watch は `metadata.name` の field selector）、`pods` の get・list・watch（自分の readiness gate）。ClusterRole：`nodes` の get・list・watch。Lease は chart が作り、`vip` に create は与えない。
- `vip` は rproxy の `/readyz` を Pod の IP の 9443 で 0.5 秒（`retryInterval`）おきに読む（`rproxy-gateway-api-tls` の `ca.crt` だけをマウントして検証）。
- 状態：VIP を使う fleet の Gateway の `status.addresses` は VIP（`spec.addresses` で選んだもの、なければ使える VIP すべて）。持ち主のいない時間が 10 秒を超えた VIP を選んだ Gateway は `Programmed: False`（`AddressNotUsable`）。選んでいない Gateway は、使える VIP のどれも持たれていないときだけ。
- Kustomize：`config/samples/fleet-vip`（例の VIP 192.0.2.10。Lease の名前と Role の `resourceNames` がアドレスのハッシュなので、ほかの VIP は chart を描いて使う）。
- 受け入れ：`mode=fleet-vip`。HTTP・HTTPS・TCP を 0.1 秒おきに流し続け、UDP（`UDPRoute`、agnhost の netexec）は VIP から答えが返ることを最初と各シナリオの後に確かめる（流し続けはしない）。m は設計の `docker stop` ではなく `docker kill`（下の値）。シナリオは j・k・l・o・n・m の順（`docker kill` したノードは別のアドレスで戻ることがあるので最後）。j・k・l・o は `GAP_LIMIT` で落とし、m・n は記録だけ（n は戻ったときに VIP が 1 つのノードだけになることを確かめる）。MetalLB L2 との比較は managed の受け入れ（l2-local・l2-cluster、6.2）の値と並べる。

#### 受け入れテストでの値

`mode=fleet-vip`、kind（ワーカー 3 台）、VIP 1 つ、既定の値（期限 3 秒・更新 1 秒・やり直し 0.5 秒、`hold`）で回した（run 37805425584）。「最長の途切れ」はどれかのプローブが 200 を返さなかった最長の時間、「Lease」は Lease の持ち主が変わった時刻（シナリオの始まりから）。

| シナリオ | 失敗（HTTP/HTTPS/TCP） | 最長の途切れ | Lease | 結果 |
|---|---|---|---|---|
| j. 持ち主の Pod の削除 | 1/0/0（883 のうち） | 0.2 秒 | +0.09 秒 | PASS |
| k. `rollout restart ds/rproxy` | 0/0/0（1090） | 0.1 秒 | +7.35 秒（持ち主の Pod の番が来たとき） | PASS |
| l. 持ち主のノードの drain（cordon） | 0/1/0（345） | 1.2 秒 | +0.47 秒 | PASS |
| o. control plane の `docker pause` 20 秒（`hold`） | 0/0/0（971） | 0.2 秒 | 移らない | PASS |
| n. 持ち主のノードの `docker pause`（記録だけ） | 26/27/26（316） | 6.4 秒 | +2.36 秒 | 戻して 1.2 秒で VIP は 1 つのノードだけ |
| m. 持ち主のノードの `docker kill`（記録だけ） | 27/27/14（416） | 5.7 秒 | +3.15 秒 | — |

- 予定の移動（j・k・l）は Lease が 0.1〜0.5 秒で移り、途切れは 1 リクエストまで（MetalLB L2 + `Local` の managed（6.2 の D）の b2・c・d は 0.2〜1.2 秒）。
- n・m は Lease の期限（3 秒）で移った。失敗の多くは、止めたノードにあるバックエンド（echo）の Pod に rproxy が送ったもの（ノードが NotReady になって EndpointSlice から外れるまで。VIP とは関係なく、managed の g と同じ）。
- 最初の回（run 37803895238）では m を `docker stop` にした：kind のノードが止まるときに Pod が SIGTERM で止まるので、VIP は drain と同じく +0.06 秒で移り、失敗は 0。ノードの喪失を測るため `docker kill` にした。
- UDP は最初と各シナリオの後に VIP から答えが返った（`IP_PKTINFO` で VIP から返す）。
- 要約の `vip` の行が空になるところは、ランナーから `docker exec` でノードを読めなかったとき（プローブは成功している）。

## 8. 受け入れテスト（`acceptance.yml`）の変更

| 入力 | 既定 | 中身 | 入れる PR |
|---|---|---|---|
| `managed_replicas` | `2` | 今と同じ（`managed.replicas`） | — |
| `install` | `helm` | `kustomize` で `config/default` に overlay を当てて入れる | B の後 |
| `ui` | `false` | UI の chart も入れ、4.3 の確認をする | C |
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
4. E：rproxy-api v0.4.1 の `RPROXY_SHUTDOWN_*` をつなぐ（6.）。
5. C：UI の chart の後、発見の Secret と UI のトークン。
6. F：fleet の VIP。大きいので、ほかと別のパッチにする。
7. 受け入れ（8.）は各 PR のブランチで手で回し、v0.4.1 から下がっていないことを確かめる。

## 10. 決めたこと

| # | 決めること | 決めたこと |
|---|---|---|
| Q1 | A の CRD を 1 つにするか、クラス用の cluster-scoped の種類を分けるか | **1 つ**（namespaced、クラスの参照はコントローラの namespace だけ、`policy` はクラスの参照だけ） |
| Q2 | 参照が誤りになったとき、前に動いていた Gateway をどうするか | **前の形のまま残す**（`Accepted: False`・`Programmed: True`）。Deployment のある Gateway の rproxy を、誤った参照のために消さない |
| Q3 | rproxy のバイナリの既定の `drain` | rproxy-api の設計で決める（バイナリの既定は 0）。managed の既定は delay `15s`・drain `25s`（受け入れテストで決めた。6.） |
| Q4 | 既定を変えるもの：replicas 2 以上の PDB、NodePort の `externalTrafficPolicy: Local`、readiness の `/readyz`、猶予 | PDB は v0.4.1 で済んだ。NodePort は `Cluster` のまま（v0.4.1 の測定）。readiness は `/readyz`（E の受け入れテストで決めた。6.3）。猶予は delay + drain + 5（6.2） |
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

## 11. ノードの喪失とバックエンド（v0.4.5）

ノードが止まると、そのノードのバックエンドの Pod は、ノードが NotReady になって EndpointSlice の `ready` が false になるまで（node-monitor-grace-period、40〜60 秒）宛先に残り、rproxy はそこへ送り続ける。止まったノードの Pod は SYN に答えないので、接続は rproxy の接続の時間（L7 は 5 秒、L4 は宛先が複数なら 5 秒、1 つなら OS の再送で約 2 分）まで待ってから失敗する。ノードの喪失を見つけること自体（CNI・ロードバランサ・ノードの監視の時間）は扱わない（記録だけ。8.）。rproxy-gateway で決められるのは次の 3 つ。

| 項目 | 形 | 既定 |
|---|---|---|
| a. EndpointSlice の条件 | `ready`（なしは true）の endpoint だけを使う。`terminating` のものは使わず、ready な endpoint が 1 つもないときだけ `serving` で終了中のものを使う（KEP-1669、kube-proxy と同じ：全部の Pod が止まりかけでも drain の間は答える）。`ready: true` でも `terminating: true` なら ready とみなさない | 常に（今までは `ready` だけを見て、終了中で serving のものも捨てていた） |
| b. 受け身のヘルスチェック | rproxy の `outlier_detection` を、コントローラが描くすべてのバックエンドに付ける。HTTP（HTTPRoute・GRPCRoute のサービス）：`consecutive_gateway_failures: 3`（502・503・504・接続できない・応答の時間切れ）、`consecutive_5xx: 0`（アプリの 5xx では外さない）、`ejection_time: 10s`、`max_ejection_time: 1m`、`max_ejected_percent: 50`。L4（TCP・TLS・UDP のルール）：`consecutive_failures: 1`、`ejection_time: 10s`、`max_ejection_time: 1m`（rproxy の既定の 1 回・10 秒に、続けて外れたら倍にする上限を足したもの） | 有効（chart の `backends.outlierDetection`。`null` で外す） |
| c. 接続の時間 | HTTP のサービスの `timeouts.connect: 2s`（失敗は 502 なので長め）、L4 の tcp のルールの `connect_timeout: 1s`（rproxy v0.4.3。過ぎたら次の宛先へ。宛先が 1 つならクライアントの接続を閉じる） | 有効（chart の `backends.connectTimeout`。`""` で rproxy の既定） |

- 上書き：RproxyPolicy の `outlierDetection`（Gateway・リスナー・Service）があればそれを使い、既定は足さない。外すには `outlierDetection: {max_ejected_percent: 0}`。接続の時間はコントローラ全体の値だけ（RproxyPolicy に項目を足すのは CRD の変更なので、要るなら別に決める）。
- コントローラの引数：`--backend-outlier-http`・`--backend-outlier-l4`（`key=value,...`、空で外す）、`--backend-connect-timeout-http`・`--backend-connect-timeout-l4`（空で rproxy の既定）。`RPROXY_GATEWAY_BACKEND_*`。
- rproxy の版：`connect_timeout` は `features.connect_timeout` のある rproxy（v0.4.3）にだけ送る。`outlier_detection` と `timeouts.connect` は v0.4.0 からある。
- 既定を変えたこと：今までの Gateway のルールにも付く（オーナーの依頼。ルールはその場で変わり、接続は切れない）。
- 受け入れ：シナリオ p（記録だけ。managed・fleet-vip のどちらでも）：rproxy が通らないノードのうち echo の Pod があるものを `docker pause` し、失敗が続いた時間（最後の失敗）、EndpointSlice で NotReady になった時刻、10 秒失敗のない状態に戻るまでを測る。入力 `rproxy_ref` で rproxy を rproxy-api のブランチから作る。

#### 受け入れテストでの値

（測定の後に書く）
