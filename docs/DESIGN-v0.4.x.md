English: [en/DESIGN-v0.4.x.md](en/DESIGN-v0.4.x.md)

# v0.4.x の設計：Kubernetes での運用

v0.4.0・v0.4.1 の受け入れテストで分かった、Kubernetes で運用するときの穴を埋める。rproxy-api・UI と一緒に決めた設計（オーナーの承認済み）のうち、rproxy-gateway の分（A・B・C の gateway の側・E の gateway の側・F）をここに書く。rproxy の証明書の API（rproxy-api #240）と組の保存（#241）は rproxy-api の設計にあり、rproxy-gateway は使わない（5.）。

今の決めごとは [DESIGN.md](DESIGN.md)、テナントの線は [SECURITY.md](SECURITY.md)。

## 1. 方針

| 項目 | 決めたこと |
|---|---|
| 目的 | managed の rproxy を Gateway ごとに変えられない、Helm 以外で入れにくい、UI を Kubernetes に置けない、rproxy が SIGTERM ですぐ終わる、Service を通さない VIP がない（F はやめた。7.） |
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
| F. Pod が直接持つ VIP | fleet の `vip` サイドカー・Lease・状態 | v0.4.4 で入れ、v0.4.5 でやめた（7.） |

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

CRD `RproxyGatewayParameters`（`rproxy.max3584.net/v1alpha1`（v0.4.5 から `v1beta1`、11.4）、namespaced、shortname `rpgwp`）。GatewayClass の `spec.parametersRef`（クラスの既定）と、Gateway の `spec.infrastructure.parametersRef`（その Gateway の上書き）の両方から指す。

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

## 7. F. rproxy の Pod が直接持つ VIP（やめた）

v0.4.4 で fleet の `vip` サイドカー（`rproxy-gateway vip`、chart の `fleet.vip`。VIP ごとの Lease で持ち主を決め、ノードのインタフェースに VIP を足して gratuitous ARP / unsolicited NA を出す）を入れたが、v0.4.5 で外した。

- 理由：アドレス（VIP）を用意して移すのはプラットフォーム（MetalLB・kube-vip・Cilium の LB IPAM・クラウドのロードバランサ・ノードの keepalived）の仕事で、どれにも使われてきた実装がある。rproxy-gateway が持つと、NET_ADMIN・NET_RAW のコンテナ、Lease の RBAC、ARP / NDP の扱い、ネットワークの障害の試験までを抱える。rproxy は複数のアドレスで待ち受けられる（`--listen-addr`、Gateway の `spec.addresses`）ので、rproxy-gateway は正しいアドレスで待ち受けることだけをし、アドレスの用意の仕方は [PLATFORM.md](PLATFORM.md) に書く。
- `fleet.vip` は v0.4.4 の 1 日だけで、既定で切っていた（opt-in）ので、パッチで外した。`fleet.vip` を値に書いたままの `helm upgrade` は失敗する（chart の `fail`）。残る Lease・RBAC・ノードのアドレスの片付けは [PLATFORM.md](PLATFORM.md) の「v0.4.4 の `fleet.vip` から移る」。
- v0.4.4 の受け入れテスト（kind、VIP 1 つ、Lease の期限 3 秒）の値：予定の移動（持ち主の Pod の削除・rollout restart・drain）の途切れは 0.1〜1.2 秒、ノードの喪失（`docker pause`・`docker kill`）は 5.7〜6.4 秒。同じ形は kube-vip（ARP、Lease）・keepalived（VRRP）でも作れ、値はそれぞれの設定で決まる。

## 8. 受け入れテスト（`acceptance.yml`）の変更

| 入力 | 既定 | 中身 | 入れる PR |
|---|---|---|---|
| `managed_replicas` | `2` | 今と同じ（`managed.replicas`） | — |
| `install` | `helm` | `kustomize` で `config/default` に overlay を当てて入れる | B の後 |
| `ui` | `false` | UI の chart も入れ、4.3 の確認をする | C |
| `strict` | `false` | E の確認で失敗のリクエストが 1 つでもあれば落とす | E |

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
6. F：fleet の VIP。大きいので、ほかと別のパッチにした（v0.4.4）。v0.4.5 でやめた（7.）。
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
| Q17 | F の方式 | v0.4.4 は案 1b（fleet + 自前の `rproxy-gateway vip`。Lease、gratuitous ARP / NA）。**v0.4.5 でやめ、アドレスはプラットフォームに任せる**（7.、[PLATFORM.md](PLATFORM.md)） |
| Q19 | VIP ごとに待ち受けを分けるか | v0.4.4 は**分けない**（`0.0.0.0` のまま）。v0.4.5 で `fleet.listen: addresses` を足した（Gateway の `spec.addresses` で待ち受ける。rproxy-api v0.4.3 の `listen_freebind`。既定は `wildcard` のまま。12.） |
| — | 版 | **v0.5.0 は作らない**。v0.4.2 から順にパッチで出す |
| — | chart のクラスの既定の parameters | **`managed.parameters` が空でないときだけ描く**（`helm upgrade` は新しい CRD を入れないため。2.7） |

Q10〜Q16（UI の migration・MariaDB、rproxy-api の #240・#241、利用量）は UI・rproxy-api の設計にある。

## 11. Gateway API の残り（v0.4.5、rproxy v0.4.3）

docs/CONFORMANCE.md の「名乗っていないもの」を埋める（オーナーの依頼、2026-10-08）。rproxy に要る口は rproxy-api の docs/DESIGN-v0.4.x.md の 7.。どれも rproxy の `features` で見分け、ない rproxy では前のとおり（名乗る機能のルートは `UnsupportedValue`、421 は付けない）。

### 11.1 421 Misdirected Request（`GatewayHTTPSListenerDetectMisdirectedRequests`）

- 同じポートの `HTTPS` のリスナーが 2 つ以上なら、ルールの `tls.misdirected.groups` にリスナーごとのグループを書く（ホスト名は rproxy の形（`*.example.com` → `**.example.com`）、ホスト名のないリスナーは `*`）。rproxy は SNI と Host が違うグループなら 421 を返す。Host がどのリスナーにも当たらなければ（`*` のリスナーがないとき）ルートで選ばれて 404。
- 1 つしかなければ付けない（分けるものがない）。`TLS`（Terminate）のリスナーは HTTP を話さないので入れない。
- rproxy v0.4.3 の `http_options` の `misdirected` がない rproxy には付けない（今までどおり、ほかのリスナーのルートに届く）。

### 11.2 backendRef の `CORS`・`RequestRedirect`・`RequestMirror` のフィルタ

- その backend の `servers[].middlewares` に、規則のフィルタと同じ形のミドルウェア（`cors`、`redirect_regex`、`mirror` とミラー先のサービス）を書く。rproxy v0.4.3 の `server_middleware_kinds` に `cors`・`redirect_regex`・`mirror` があるときだけ（ない rproxy ではルートが `UnsupportedValue`）。
- `RequestRedirect` の ReplacePrefixMatch は `URLRewrite` と同じく、規則の path の接頭辞が 1 つのときだけ（転送先は規則のすべての match に使われる）。
- ミラーはその backend に当たったリクエストだけを写す（rproxy が 1 つのリクエストで 1 回だけ写す）。ミラー先が見つからなければ規則のフィルタと同じく `ResolvedRefs: False` でミラーだけ外す。
- backendRef の `ExtensionRef` は今までどおり断る（`RproxyMiddleware` の種類を転送先ごとに確かめる口がないため）。

### 11.3 `ExternalAuth`（HTTP・gRPC）

- `ExternalAuth` のフィルタを rproxy の `forward_auth`（v0.4.3 で足した `service`・`client_request`・`allow_status`・`response_headers: ["*"]`・`forward_body`・`protocol: grpc`）にする。宛先は backendRef の Service の Pod の IP のサービス（ほかの backend と同じく EndpointSlice から。BackendTLSPolicy も同じく効く）。
- HTTP：Gateway API が必ず送るとする Host・メソッド・パス・Content-Length は `client_request` が送る。`Authorization` は `request_headers` に必ず入れる（Gateway API の「`allowedHeaders` が空なら決まったものだけ」。rproxy の `request_headers` は空だとすべてを送るため）。`allowedResponseHeaders` が空なら `["*"]`（応答そのものを表すヘッダは写さない）。200 だけが通す（`allow_status`）。
- gRPC：`allowedHeaders` が空ならすべて（Gateway API と rproxy で同じ意味）。サービスは h2c（BackendTLSPolicy があれば h2 と `tls`）。
- `forwardBody.maxSize`（0 は送らない）。大きい本文は 413（型の説明の「切って送る」ではなく、フィルタの説明と Envoy の既定の「断る」。rproxy-api の設計 7.3）。
- 宛先が使えない（見つからない・ReferenceGrant がない・ready な Pod がない・BackendTLSPolicy に使える CA がない）ときは、確かめずに通すことのないよう、その規則は 500（`ResolvedRefs: False`）。backendRef の `ExternalAuth` なら、その backend の分だけ 500（`servers[].status`。ない rproxy ではその backend を外す）。
- 名乗る機能：`HTTPRouteExternalAuth`・`HTTPRouteExternalAuthHTTP`・`HTTPRouteExternalAuthGRPC`・`HTTPRouteExternalAuthForwardBody`（GatewayClass の `supportedFeatures`）。Gateway API v1.6.3 の conformance にはこの機能の名前も試験もない（`pkg/features` にない）ので、試験は `tests/rproxy.rs`（本物の rproxy）・単体と rproxy-api の `tests/ext_authz.rs`。

### 11.4 rproxy の CRD を `v1beta1` に

- 4 つの CRD（`RproxyRule`・`RproxyMiddleware`・`RproxyPolicy`・`RproxyGatewayParameters`）は `v1beta1`（保存する版）と `v1alpha1`（`deprecated: true`、`deprecationWarning`）の両方を出す。形は同じなので、変換の webhook はなく `conversion.strategy: None`（API サーバが `apiVersion` を書き換えるだけ）。`rproxy-gateway crds` が `v1beta1` の型から `v1alpha1` の項を作る（単体の試験が形の一致を確かめる）。
- コントローラは CRD を discovery の優先の版で watch する（今までどおり）。新しい CRD なら `v1beta1`、`helm upgrade` で CRD が古いまま（`v1alpha1` だけ）でも `v1alpha1` で動く。状態（`RproxyRule` の `status`）も同じ版で書く。
- chart がクラスの既定の `RproxyGatewayParameters` を描くときは、クラスタが `v1beta1` を出していればそれ、なければ `v1alpha1`（Helm の `.Capabilities`。`helm install` は `crds/` を入れた後に調べる）。`config/`・例・e2e は `v1beta1`。e2e は `RproxyRule` を 1 つ `v1alpha1` で書き、`v1beta1` で読めること・保存の版が `v1beta1` であることを確かめる。受け入れテストは CRD の保存の版で書く（公開した古い chart でも動く）。
- **v0.4.4 からの更新**：
  1. `kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.5/rproxy.max3584.net.yaml`（`helm upgrade` は CRD を更新しない。Kustomize の `config/crd` は一緒に当たる）。
  2. コントローラを更新する。今の `v1alpha1` のオブジェクトはそのまま `v1beta1` としても読める（書き直しは要らない）。CRD を当てる前にコントローラだけ更新しても `v1alpha1` で動く。
  3. 手元のマニフェストの `apiVersion` は、時間のあるときに `rproxy.max3584.net/v1beta1` に変える（`v1alpha1` は警告が出るだけ）。
- **保存の版**：etcd の中の今のオブジェクトは、次に書かれるまで `v1alpha1` のまま（CRD の `status.storedVersions` は `["v1alpha1", "v1beta1"]`）。形が同じなので動きは変わらない。将来 `v1alpha1` を出すのをやめる版（マイナー）の前には、すべてのオブジェクトを書き直して（`kubectl get rproxyrules,rproxymiddlewares,rproxypolicies,rproxygatewayparameters -A -o json | kubectl replace -f -`、または kube-storage-version-migrator）から `status.storedVersions` を `["v1beta1"]` にする（`kubectl patch crd <名前> --subresource=status --type=merge -p '{"status":{"storedVersions":["v1beta1"]}}'`）。v0.4.x のうちは `v1alpha1` を出し続ける（パッチで壊さない）。

### 11.5 Mesh（GAMMA）の見立て（作らない）

Gateway API の Mesh（GAMMA、conformance の `MESH-HTTP`・`MESH-GRPC`、機能 `Mesh` と `MeshClusterIPMatching`・`MeshConsumerRoute` など）は、HTTPRoute・GRPCRoute の `parentRefs` に **Service** を書き、クラスタの中の Pod から Service への通信（east-west）にルートを当てる。オーナーの依頼で、作らずに見立てだけ書く（2026-10-08）。

**要るもの**

| 部分 | 中身 | 今の rproxy-gateway / rproxy |
|---|---|---|
| 通信を取る | 各 Pod の外向きの通信を rproxy に向ける：Pod ごとのサイドカーを注入する（mutating webhook と、iptables / nftables を書く init コンテナか CNI のプラグイン）か、ノードごとのプロキシ（Istio ambient の ztunnel・Cilium の形。eBPF か TPROXY でノードの全 Pod を取る） | ない。fleet の hostNetwork の DaemonSet はあるが、Pod の通信を横取りしない |
| 元の宛先で振り分ける | 取った接続の元の宛先（`SO_ORIGINAL_DST` か TPROXY の宛先）を読み、Service の ClusterIP:port ごとに「仮想の待ち受け」として L7 のルートを選ぶ（`MeshClusterIPMatching`）。ルートのない Service はそのまま kube-proxy の動きを真似る（Pod の IP に分ける） | rproxy のルールは待ち受けの (アドレス, ポート) がキー。元の宛先で選ぶ口は rproxy-api の大きな変更（新しい待ち受けの形、ルールの数がクラスタの Service の数になる） |
| 全 Service の設定 | ルートのない Service も含め、クラスタのすべての Service・EndpointSlice をすべてのプロキシに配る（数千の Service、変化の多い EndpointSlice）。今の「Gateway 1 つ = ルールセット 1 つを PUT」は、毎回すべてを置き換えるので重い | ルールセットの PUT は全体の置き換え（etag 付き）。差分の配り方（xDS の増分のようなもの）がない |
| 送り手のルート | `MeshConsumerRoute`：ルートの namespace の Pod が送るときだけ効くルート。送り手（どの Pod か）を知る必要がある | 接続元の IP から Pod・namespace を引く表が要る |
| 識別と mTLS | メッシュは普通、ワークロードの証明書（SPIFFE）で相互 TLS をする（Gateway API の conformance は求めないが、メッシュとして使うなら要る）。CA・証明書の発行と回転・Pod ごとの鍵 | rproxy の TLS は終端・転送先への TLS・クライアント証明書の確認はあるが、Pod ごとの証明書の発行はない |
| conformance の環境 | Mesh の試験（v1.6.3 で 22 本）は、`gateway-conformance-mesh` の namespace の echo の Pod に入って Service へ curl する。namespace のラベル（`--namespace-labels`）で注入を有効にする | 注入の仕組みがないと 1 本も動かない |

**見積もり**：rproxy-api に「元の宛先で選ぶ待ち受け」と大きなルールセットの差分の反映、rproxy-gateway に注入の webhook と iptables の init（か CNI）、Service の全体の描画、送り手の表、（使うなら）mTLS の証明書。Gateway の機能の今までの追加（この 11. の全部）の数倍の量で、Pod の通信の道に入るため、障害の影響がクラスタ全体に広がる（今は Gateway の通信だけ）。受け入れテスト・セキュリティの線（「rproxy の Pod は Kubernetes の API を使わない」「テナントは自分の namespace だけ」）も作り直しになる。

**勧め：作らない**。rproxy-gateway は north-south（Gateway）に絞る。クラスタの中の通信にポリシー・mTLS が要るなら、Istio（ambient）・Linkerd・Cilium のメッシュと並べて使う（rproxy-gateway は入口のまま、メッシュは Pod の間。Gateway の rproxy の Pod をメッシュに入れるかはメッシュの側の設定）。需要がはっきりしたら、v0.4.x のパッチではなく次のマイナー（形が大きく変わる）で、ノードごとのプロキシの形（ambient に近い。fleet の DaemonSet を使える）から考える。docs/CONFORMANCE.md の「名乗っていないもの」には Mesh を残す。

## 12. fleet で Gateway ごとのアドレスで待ち受ける（v0.4.5）

fleet のルールは `0.0.0.0`（`--listen-addr`）で待ち受けるので、2 つ目の Gateway が同じポートを使うと rproxy が `409` で断る（アドレスが違っても）。rproxy-api v0.4.3 の `listen_freebind`（まだノードにないアドレスで待ち受ける、`IP_FREEBIND`）を使い、Gateway ごとに自分のアドレスで待ち受けられるようにした。アドレスをノードに置く（告げる・移す）のはプラットフォームの役目（MetalLB・kube-vip・Cilium の LB IPAM・クラウドの LB・ホストの keepalived など）で、rproxy-gateway は待ち受けるだけ（VIP のサイドカーには頼らない。オーナーの決定）。10. Q19 の見直し。

- 値：chart の `fleet.listen: wildcard | addresses`（既定 `wildcard` で今までどおり）。`addresses` でコントローラに `RPROXY_GATEWAY_FLEET_LISTEN=addresses` と `RPROXY_GATEWAY_ADDRESS_CIDR`（`managed.addressCIDRs`）。
- 待ち受けるアドレス：Gateway の `spec.addresses`（すべてが `--address-cidr` の内のとき。IPv4 が `listen_addr`、残りは `extra_listen_addrs`）。どのノードがそのアドレスを持つかでは変えない（`listen_freebind` なので、すべての fleet の Pod が同じルールで待ち受け、アドレスが来たノードで届く。アドレスが動いてもルールは変わらない）。ルールのキーは `tcp/<アドレス>:443`。`spec.addresses` のない Gateway は今までどおりワイルドカード。Gateway の `status.addresses` は `spec.addresses`。
- rproxy の版：fleet の Pod の rproxy がすべて `features.listen_freebind` を持つときだけ（答えた Pod のどれかが持たなければワイルドカードに戻してログ。まだ答えていない新しい Pod では戻さない）。fleet の Pod を作り直す必要はない（ルールごとの印）。
- 取り合い：fleet ではすべての Gateway のルールが同じ rproxy に入る。同じアドレス（か、それを含むワイルドカード）の同じポートを 2 つの Gateway が使うと、**古い Gateway**（作られた時刻、次に namespace・名前）が持ち、後の Gateway のそのルールは組から外してリスナーを `Accepted: False`（`PortUnavailable`、「tcp/192.0.2.10:443 is used by Gateway ns/name」）にする。`wildcard` でも同じ（今までは先に反映された方が勝ち、後の方には rproxy の `409` の文が `Programmed: False` に出ていた）。rproxy の `409` は残る（判定のずれの守り）。
- UDP：特定のアドレスで待ち受けるので、返信はそのアドレスから出る（`IP_PKTINFO` は要らない）。
- 守り：あるアドレスに来たものはそのアドレスの Gateway にだけ届く。アドレスは管理者の `managed.addressCIDRs` の内だけ（今までの `spec.addresses` の守りと同じ、[SECURITY.md](SECURITY.md)）。
- 受け入れ：`mode=fleet`（VIP なし、`fleet.listen=addresses`）と入力 `rproxy_ref`（rproxy を rproxy-api のブランチ・タグから作る）。シナリオ q：kind の docker ネットワークのアドレス 2 つを、プラットフォームの代わりに 1 つのワーカーに `ip addr add` で置き（ネットワークの側は記録だけ）、同じポート 8080・8443・UDP 9002 の Gateway を 2 つ（アドレスごと）作る。両方が Programmed（アドレスを置く前から）、アドレスごとにそれぞれのバックエンドに届くこと、1 つ目のアドレスの 8080 を求めた 3 つ目が `PortUnavailable` になり、ワイルドカードの `acc`（80・443・9000）と 1 つ目が乱れないこと（`GAP_LIMIT`）。

## 13. ノードの喪失とバックエンド（v0.4.5）

ノードが止まると、そのノードのバックエンドの Pod は、ノードが NotReady になって EndpointSlice の `ready` が false になるまで（node-monitor-grace-period、40〜60 秒）宛先に残り、rproxy はそこへ送り続ける。止まったノードの Pod は SYN に答えないので、接続は rproxy の接続の時間（L7 は 5 秒、L4 は宛先が複数なら 5 秒、1 つなら OS の再送で約 2 分）まで待ってから失敗する。ノードの喪失を見つけること自体（CNI・ロードバランサ・ノードの監視の時間）は扱わない（記録だけ。8.）。rproxy-gateway で決められるのは次の 3 つ。

| 項目 | 形 | 既定 |
|---|---|---|
| a. EndpointSlice の条件 | `ready`（なしは true）の endpoint だけを使う。`terminating` のものは使わず、ready な endpoint が 1 つもないときだけ `serving` で終了中のものを使う（KEP-1669、kube-proxy と同じ：全部の Pod が止まりかけでも drain の間は答える）。`ready: true` でも `terminating: true` なら ready とみなさない | 常に（今までは `ready` だけを見て、終了中で serving のものも捨てていた） |
| b. 受け身のヘルスチェック | rproxy の `outlier_detection` を、コントローラが描くすべてのバックエンドに付ける。HTTP（HTTPRoute・GRPCRoute のサービス）：`consecutive_gateway_failures: 3`（502・503・504・接続できない・応答の時間切れ）、`consecutive_5xx: 0`（アプリの 5xx では外さない）、`ejection_time: 10s`、`max_ejection_time: 1m`、`max_ejected_percent: 50`。L4（TCP・TLS・UDP のルール）：`consecutive_failures: 1`、`ejection_time: 10s`、`max_ejection_time: 1m`（rproxy の既定の 1 回・10 秒に、続けて外れたら倍にする上限を足したもの） | 有効（chart の `backends.outlierDetection`。`null` で外す） |
| c. 接続の時間 | HTTP のサービスの `timeouts.connect: 1s`、L4 の tcp のルールの `connect_timeout: 1s`（rproxy v0.4.3。過ぎたら次の宛先へ。宛先が 1 つならクライアントの接続を閉じる） | 有効（chart の `backends.connectTimeout`。`""` で rproxy の既定） |

- 上書き：RproxyPolicy の `outlierDetection`（Gateway・リスナー・Service）があればそれを使い、既定は足さない。外すには `outlierDetection: {max_ejected_percent: 0}`。接続の時間はコントローラ全体の値だけ（RproxyPolicy に項目を足すのは CRD の変更なので、要るなら別に決める）。
- コントローラの引数：`--backend-outlier-http`・`--backend-outlier-l4`（`key=value,...`、空で外す）、`--backend-connect-timeout-http`・`--backend-connect-timeout-l4`（空で rproxy の既定）。`RPROXY_GATEWAY_BACKEND_*`。
- rproxy の版：`connect_timeout` は `features.connect_timeout` のある rproxy（v0.4.3）にだけ送る。`outlier_detection` と `timeouts.connect` は v0.4.0 からある。
- 既定を変えたこと：今までの Gateway のルールにも付く（オーナーの依頼。ルールはその場で変わり、接続は切れない）。
- 受け入れ：シナリオ p（記録だけ。managed）：rproxy が通らないノードのうち echo の Pod があるものを `docker kill`（`P_HOW=pause` で `docker pause`）し、失敗が続いた時間（最後の失敗）、EndpointSlice で NotReady になった時刻、10 秒失敗のない状態に戻るまでを測る。入力 `rproxy_ref` で rproxy を rproxy-api のブランチから作る。

#### 受け入れテストでの値

測ったときは v0.4.4 の `mode=fleet-vip`（VIP のサイドカーは v0.4.5 で外した。VIP はバックエンドの失敗と関係しない）、シナリオ p、echo（3 つ、ワーカーごとに 1 つ）のあるノードのうち VIP の持ち主でもコントローラのリーダーでもないものを止めた。プローブは HTTP・HTTPS・TCP を 0.1 秒おきに新しい接続で（`--max-time 2`）。記録だけ。

| 止め方 | 版 | 失敗（HTTP/HTTPS/TCP） | 最長の途切れ | 失敗が続いた時間（最後の失敗） | NotReady |
|---|---|---|---|---|---|
| `docker kill` | 前（chart 0.4.4・rproxy 0.4.2、run 37881266745） | 29/30/4（781） | 2.3 秒 | 47.1 秒（NotReady まで続く） | 47.0 秒 |
| `docker kill` | 後（この版・rproxy v0.4.3、HTTP の接続 1 秒、run 37882953598） | 9/9/0（1146） | 1.2 秒 | 37.5 秒（0〜1 秒・11〜14 秒・35〜37 秒の 3 回だけ） | 44.9 秒 |
| `docker kill` | 後（HTTP の接続 2 秒、run 37881273245） | 10/18/0（1055） | 2.2 秒 | 44.9 秒 | 44.9 秒 |
| `docker pause` | 前（run 37878937573） | 30/30/30 | 2.3 秒 | 68.4 秒 | 49.2 秒 |
| `docker pause` | 後（HTTP の接続 2 秒、run 37880239529） | 21/21/21（411） | 2.2 秒 | 46.9 秒 | 46.5 秒 |

- `docker kill`（電源が落ちたノード：何も答えない）：前は死んだ Pod に送った接続が rproxy の接続の時間（5 秒）より先にクライアント（2 秒）に諦められるので、rproxy は失敗として数えず外さない。NotReady まで約 4 回に 1 回が落ち続けた。後は 1 秒で接続を諦めて外し（L7 は 3 回、L4 は 1 回）、外す時間（10 秒 → 20 秒）が過ぎて試し直すときだけ数回落ちる。TCP は次の宛先に移るので失敗 0。
- HTTP の接続を 2 秒にした回は、HTTPS（TLS の分だけクライアントの残り時間が短い）が rproxy の 2 秒より先に諦められて外れなかった。既定を 1 秒にしたのはこのため（クラスタの中の Pod は数ミリ秒で答える）。
- `docker pause`（カーネルは動くので、止めた Pod への TCP の接続は成り立ち、応答だけ来ない：止まりかけのノード・固まったアプリ）：接続の時間は効かず、応答の時間切れ（`timeouts.response`）がないと gateway の失敗にならないので、どちらも NotReady まで落ち続ける。応答の時間はアプリごとに違うので既定にはしない（HTTPRoute の `timeouts.backendRequest` で決める）。
