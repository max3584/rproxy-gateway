English: [en/SECURITY.md](en/SECURITY.md)

# rproxy-gateway のセキュリティ

コントローラが何を信頼し、テナント（Gateway やルートを書ける人）に何をさせないかをまとめる。設計の全体は [DESIGN.md](DESIGN.md)。

## 信頼の境界

| 誰 | できること | できないこと |
|---|---|---|
| クラスタの管理者 | chart の値・コントローラのフラグを決める | — |
| Gateway の持ち主（namespace の編集者） | 自分の namespace に Gateway・ルート・RproxyRule・RproxyMiddleware を書く | ほかの namespace の Service・Secret を、そちらの ReferenceGrant なしに使う。クラスタの IP を自分のものにする。ほかの Gateway の鍵や rproxy を使う |
| managed の rproxy の Pod | 自分の Gateway の証明書（マウントした Secret）を読む | Kubernetes の API を使う（トークンなし・RBAC なし）。ほかの Gateway の資格を使う |

## managed と fleet

- **managed（既定）**：Gateway ごとに、Gateway の namespace に rproxy を置く。制御 API の証明書とトークンは Gateway ごと（CA が `<id>.rproxy-api.rproxy-gateway.internal` に出したもの、マスタートークンから HMAC で導いたもの）。テナントを分けるのはこちら。
- **fleet**：すべての Gateway が同じ rproxy の Pod（hostNetwork の DaemonSet）を使う。**1 つの信頼の範囲（1 人の管理者）のためのもの**で、テナントを分けない。すべての Gateway の証明書を 1 つの Secret（`rproxy-fleet-certs`）にまとめて全 Pod に渡し、ノードのポートは先に取った Gateway のもの（後から来たものは `Programmed: False`）。fleet では RproxyRule を既定で読まない（`fleet.rproxyRules` / `--fleet-rproxy-rules`）。

## テナントの入力への制限

| 項目 | 既定 | 変え方 |
|---|---|---|
| `spec.addresses`（managed では Service の `externalIPs`） | **使えない**（`Programmed: False`、`AddressNotUsable`）。許した範囲でも、ほかの Service の ClusterIP・externalIPs・LB の IP は取れない（CVE-2020-8554 の形を防ぐ） | `managed.addressCIDRs` / `--address-cidr`。Service や Pod の範囲を入れない |
| `spec.infrastructure.annotations` の Service への伝わり方 | アドレス・LB を決める注釈（`metallb.universe.tf/`、`lbipam.cilium.io/`、`service.beta.kubernetes.io/` など）は Service に付けない（Pod・ServiceAccount には付ける） | `managed.serviceAnnotationPrefixes` / `--service-annotation-prefix` |
| ExternalName の Service を backend に | **使えない**（`ResolvedRefs: False`）。API サーバ・ほかの namespace・メタデータの口など、何でも名指しできるため | `controller.allowExternalNameServices` / `--allow-external-name-services` |
| RproxyRule・RproxyMiddleware が名前を書くファイル（`*_file`・`file`・`*_path`） | その Gateway の証明書のファイル（証明書ディレクトリの中の、その Gateway の分）だけ。ほかは `Accepted: False` / `UnsupportedValue` | — |
| 移行（Ingress・Traefik）のほかの namespace への参照（Service・Middleware・TLSOption・errors の service） | 参照先の namespace の ReferenceGrant（from `traefik.io` の `IngressRoute*`・`Middleware`）がなければ変換しない（Traefik の `allowCrossNamespace=false` と同じ） | `migration.allowCrossNamespace` / `--migration-allow-cross-namespace` |
| Ingress の path | バッククォート・引用符・制御文字・`/` で始まらないものは変換しない（`match` の式に埋め込むため） | — |
| Ingress の `defaultBackend` | 移行先の Gateway の namespace のものだけ | — |
| Gateway の `RproxyGatewayParameters`（`infrastructure.parametersRef`、同じ namespace のものだけ） | replicas（`policy.maxReplicas`、既定 10 まで）・PDB・resources・Pod と Service のラベル／注釈・topologySpread・externalTrafficPolicy・sourceRanges・ipFamilyPolicy・logLevel・performance だけ。LB のアドレスを決める注釈は上の許可リストを通ったものだけ（ほかは `InvalidParameters`）。`service.type`・`loadBalancerClass`・nodeSelector・tolerations・affinity・priorityClassName は既定で使えない。rproxy のイメージ・追加の環境変数・`policy` は使えない。コントローラの接頭辞（`rproxy.max3584.net/` など）のラベル・注釈も使えない | GatewayClass の `RproxyGatewayParameters`（chart の `managed.parameters`）の `policy`（`gatewayOverrides`・`maxReplicas`・`allowedPriorityClasses`・`allowedLoadBalancerClasses`）。イメージと環境変数は開けない（[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 2.5） |

## ほかの namespace の秘密鍵（M7）

`certificateRefs`（と `spec.tls.backend.clientCertificateRef`）がほかの namespace の Secret を ReferenceGrant 付きで指すと、managed ではその鍵を Gateway の namespace の Secret（`rproxy-<id>-certs`）に写す（rproxy の Pod がマウントできるのは同じ namespace の Secret だけ）。そのため **Gateway の namespace で Secret を読める人は、その鍵を読める**。ReferenceGrant は「参照の許可」だが、ここでは結果として「読む許可」にもなる。共有のワイルドカード証明書などで困るなら：

- `controller.crossNamespaceSecrets: false`（`--cross-namespace-secrets=false`）でほかの namespace の鍵を使わない（`ResolvedRefs: False`、`RefNotPermitted`）。Gateway API の conformance のうちほかの namespace の証明書の試験は通らなくなる
- または鍵を Gateway の namespace に置く

## コントローラの権限（M10）

- 既定ではすべての namespace の Gateway API の型・Secret・Service・Deployment・ServiceAccount・NetworkPolicy・PodDisruptionBudget・Pod（patch。rproxy の Pod の readiness gate のため `pods/status` の patch も）を扱う ClusterRole を持つ（Gateway の namespace に rproxy を置き、どこの証明書でも参照できるため）。コントローラが乗っ取られると、クラスタ全体の Secret が読める。readiness gate の条件は、その gate を持つ Pod にだけ書く。
- chart は ClusterRole `rproxy-gateway-parameters-edit`（`rproxygatewayparameters` の読み書き）を namespace の admin に集約する（`rbac.authorization.k8s.io/aggregate-to-admin`。edit には渡さない。`rbac.aggregateToAdmin: false` で切れる）。書ける範囲はクラスの `policy` で抑える。GatewayClass の `parametersRef` はコントローラの namespace のものしか使わないので、テナントはクラスの既定を変えられない。
- `controller.watchNamespaces`（`--watch-namespaces`）を決めると、そこ（とコントローラの namespace）だけを watch し、chart はその namespace ごとの Role を作る。ClusterRole に残るのは GatewayClass と namespace（allowedRoutes の selector）だけ。

## ネットワーク

- managed：Gateway ごとに NetworkPolicy を作る（`managed.networkPolicy`、既定 true）。rproxy の制御 API（9443）と certsync（9444）はコントローラの Pod からだけ、リスナーのポートは誰からでも。CNI が NetworkPolicy を扱わなければ効かない。
- certsync はファイルの一覧を返さない：コントローラが名前（中身のハッシュ）を送り、あるかどうかだけを返す。Pod の IP で待ち受ける。
- fleet（hostNetwork）では NetworkPolicy が効かない。制御 API と certsync は Pod の IP（＝ノードの IP）で待ち受けるので、ノードへの外からの通信はファイアウォールで絞る。

## 制御 API の資格と入れ替え

- CA（`rproxy-gateway-ca`）は pathLen 0、名前の制約 `rproxy-gateway.internal`、10 年。制御 API の証明書は 1 年で、終わる 30 日前に出し直し、rproxy の Pod を入れ替える（Pod のテンプレートの注釈）。
- コントローラは rproxy の答えの本文を 8 MiB、certsync の答えを 1 MiB までしか読まない。managed の Pod は、その Gateway の Deployment の ReplicaSet（`rproxy-<id>-...`）のものだけを相手にする。
- 入れ替え：
  - CA：`kubectl -n rproxy-gateway-system delete secret rproxy-gateway-ca rproxy-gateway-api-tls` → コントローラを再起動。新しい CA で制御 API の証明書がすべて出し直され、rproxy の Pod が入れ替わる
  - マスタートークン：`kubectl -n rproxy-gateway-system delete secret rproxy-gateway-token` → コントローラを再起動。Gateway ごとのトークンも変わり、rproxy の Pod が入れ替わる（fleet は DaemonSet を再起動）

## rproxy のルールセットの持ち主

rproxy はルールセットを、作ったトークンの名前のものにする（ほかの admin でないトークンは変えられない）。コントローラのトークンは値が変わっても名前はいつも `rproxy-gateway` なので、トークンを入れ替えても（rproxy の再起動のあとの PUT し直しも）同じ持ち主のまま。トークンの `allow_rulesets` は、fleet のものが `k8s/`、managed の Gateway ごとのものがその Gateway のルールセット（`k8s/<namespace>/<name>`）だけ。

## rproxy のファイルの所有者の確認

rproxy は、ルールや設定が指す証明書・鍵のファイルが rproxy のユーザーのものかを確かめる（`global.files.owner_check`）。kubelet は Secret のボリュームのファイルを root の持ち物（グループは fsGroup、モード 0440）にするので、コントローラは rproxy に `RPROXY_FILES_TRUSTED_DIRS` を渡し、そのディレクトリ（managed：`/var/run/rproxy-gateway/certs`・`/etc/rproxy-gateway/api`、fleet：それに `/etc/rproxy-gateway/api-tls`・`/etc/rproxy-gateway/token`）では root のファイルも使えるようにする。モードの確認（グループ・ほかの人が書けない、鍵はほかの人が読めない）はそのまま。

## イメージ

`controller.image.digest`・`rproxy.image.digest`（`sha256:...`）でダイジェストに固定できる（タグより優先）。
