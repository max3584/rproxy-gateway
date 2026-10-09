English: [en/DESIGN.md](en/DESIGN.md)

# rproxy-gateway の設計

rproxy-api の docs/DESIGN-v0.4.md 3.（#28）を形にしたもの。ここにはコントローラの側の決めごとを書く。

## 全体

```
Gateway API / CRD ──watch──▶ rproxy-gateway ──PUT /rulesets/k8s/<ns>/<name>──▶ rproxy（制御 API、HTTPS + トークン）
                                   │                                              ▲
                                   └──証明書の Secret（<id>-certs）──kubelet がボリュームに──┘（rproxy の Pod は API を使わない）
```

- rproxy と話すのは制御 API だけ（`GET /capabilities`、`GET /readyz`、`GET` / `PUT` / `DELETE /rulesets/{name}`）。rproxy は Kubernetes の API を知らない。
- Gateway 1 つが rproxy のルールセット 1 つ（`k8s/<Gateway の namespace>/<Gateway の名前>`）。(プロトコル, アドレス, ポート) 1 つがルール 1 つで、同じポートのリスナー（ホスト名が違う）は 1 つのルールにまとめる。
- ひとつのループが、変更のたび（少し待ってまとめる）と `--resync-secs`（既定 30 秒）ごとに、担当するすべての Gateway を描き直す。描いた内容が前回と同じで、Pod のセットの etag も前回 PUT したときのままなら PUT しない。

## 冗長化（リーダー選出）

- コントローラは複数動かせる（chart の既定は 2 レプリカ、`controller.replicas`）。どのレプリカも watch を続けるが、反映（PUT）・状態の書き込み・rproxy の配置をするのは、コントローラの namespace の Lease（`coordination.k8s.io/v1`、名前 `rproxy-gateway`、`--leader-lease`）を持つ 1 つだけ。
- リーダーは 5 秒ごとに Lease を更新する。Lease は最後の更新から 15 秒有効。持ち主がいないか 15 秒更新のない Lease は、ほかのレプリカが取る（書き込みは `resourceVersion` つきなので、2 つが同時に取ることはない）。
- 10 秒更新できなかったリーダーは自分から降りる（ほかが取れるようになる 15 秒より前に止まる）。降りたときは途中の反映もそこでやめる。止めるとき（SIGTERM）は Lease を手放すので、すぐにほかのレプリカが引き継ぐ。
- 引き継いだレプリカは rproxy の今のルールセットを読み、etag が違えば PUT し直す（`If-Match`）。同時に 2 つが書いても、rproxy の `If-Match` と `generation` が古いほうを断る。
- レプリカ 1 つで動かすなら `--leader-elect=false`（chart の `controller.leaderElection: false`）。

## rproxy の置き方（`--mode`）

| mode | rproxy | アドレス（Gateway の `status.addresses`） |
|---|---|---|
| `managed`（既定） | コントローラが Gateway の namespace に、Gateway ごとの Deployment・Service・ServiceAccount（`rproxy-<id>`、Service の型は `--service-type`、既定 `LoadBalancer`）と Secret（証明書 `rproxy-<id>-certs`、制御 API `rproxy-<id>-api`）を作る。どれも Gateway が持ち主（`ownerReferences`）で、Gateway が消えれば消える | `spec.addresses` があればそれ、なければ Service のロードバランサのアドレス（`ClusterIP` 型なら ClusterIP） |
| `fleet`（1 つの信頼の範囲のため。[SECURITY.md](SECURITY.md)） | 先に置いた rproxy の Pod（chart の `hostNetwork: true` の DaemonSet など、`--fleet-selector`）がすべての Gateway を受け持つ。コントローラはすべての Pod に同じセットを PUT する | `--fleet-address`、なければ Pod のノードの IP |

- `<id>` は `<namespace>-<name>`（40 文字まで）とハッシュ 6 桁。
- managed で作るものには、Gateway の `spec.infrastructure` の `labels`・`annotations`（Pod にも）と、`gateway.networking.k8s.io/gateway-name` のラベルを付ける（コントローラのラベルが優先。Pod を選ぶのに使う）。Deployment・Service・PDB・Pod の形は、GatewayClass の `parametersRef` と Gateway の `spec.infrastructure.parametersRef` が指す `RproxyGatewayParameters`（下の表）で Gateway ごとに変えられる（どちらもなければフラグのまま。決めごとは [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 2.）。ほかの種類を指す・見つからない・許されていない項目を書いた Gateway は `Accepted: False`（`InvalidParameters`）。ただし rproxy の Deployment がもうある Gateway は、参照が誤りになっても最後に正しかった形のまま動かし続ける（Deployment・Service を変えず、ルールセットの反映は続ける。`Programmed: True`）。
- `spec.addresses`：`IPAddress` だけ（ほかの型は `Accepted: False`、`UnsupportedAddress`）。managed では Service の `externalIPs` にする（既定では使えない：`--address-cidr` の範囲だけで、ほかの Service の IP は取れない。[SECURITY.md](SECURITY.md)）（kube-proxy がその IP への通信を rproxy に送る）。値のない項目は Service のアドレスのまま。unspecified・loopback・link-local・multicast の IP や、クラスタが Service に付けられない IP は `Programmed: False`（`AddressNotUsable`）。fleet では fleet のアドレス（`--fleet-address` かノードの IP）のどれかでなければ `AddressNotUsable`。
- managed の Pod は非 root（65532）で、1024 未満のポートは `net.ipv4.ip_unprivileged_port_start=0`（namespace ごとの安全な sysctl）で受ける。
- fleet で同じポートを 2 つの Gateway が使うと、後から来たほうのルールは rproxy が `409 already_exists` で断り、リスナーの `Programmed` が `False` になる。

## rproxy の可用性（managed）

rproxy の Pod が入れ替わる（削除、`kubectl rollout restart`、ノードの drain、イメージ・証明書の更新）ときに通信が途切れないようにする。

- **readiness gate**（`rproxy.max3584.net/ruleset-applied`）：rproxy の Pod は、コントローラがその Pod にルールセットを反映して（`PUT` が通って）からはじめて Ready になり、Service の endpoint に入る。`/healthz` だけでは、ルールのない rproxy に通信が来る。コントローラは Pod の `status.conditions` にこの条件を書く（`pods/status` の patch）。rproxy のコンテナが再起動したら（ルールセットはメモリにある）`False` に戻し、反映し直したら `True` にする（書いたときの再起動の回数を条件の `message` に持つ）。一度 `True` になった Pod は、その後の更新を待つ間（証明書のファイルが揃うまでなど）も `True` のまま。rproxy が断ったセット（`Rejected`）も「待つものがない」として `True`。fleet の DaemonSet にも同じ gate を付ける（すべての Gateway のセットが載ってから Ready。DaemonSet の rolling update はそれを待つ）。
- **rolling update**：`maxUnavailable: 0`。新しい Pod が（gate を含めて）Ready になってから古い Pod を止める。
- **SIGTERM での終わり方**（rproxy v0.4.1 の `features.graceful_shutdown`）：削除が決まった Pod は、すぐに EndpointSlice で `ready: false`（`terminating: true`）になり、kubelet が rproxy に SIGTERM を送る。rproxy は `RPROXY_SHUTDOWN_DELAY`（`managed.shutdown.delay` / `--shutdown-delay`、既定 15 秒）の間、受け付けを続けながら `/readyz` を `draining`（503）にする。readiness（`/readyz`）が落ちて endpoint の `serving` も `false` になり、MetalLB は告知をほかのノードへ移し、kube-proxy は新しい接続をほかの Pod へ送り、クラウドのロードバランサは `healthCheckNodePort`（そのノードの Pod が終了中だけになると失敗）でノードを外す。その後 `RPROXY_SHUTDOWN_DRAIN`（`managed.shutdown.drain` / `--shutdown-drain`、既定 25 秒）の間、待ち受けを閉じて今の接続の終わりを待つ（終われば早く止まる）。`terminationGracePeriodSeconds` は delay + drain + 5 秒（既定 45）。Gateway ごとに parameters の `rproxy.shutdown` で変えられる。preStop は付けない（イメージにシェルが要らず、Kubernetes の版によらない）。certsync は SIGTERM ですぐ止まる（コントローラは削除中の Pod に話しかけない）。
  - 受け入れテスト（l2-local・l2-cluster、replicas 2）で 6 通りを比べて決めた（[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 6.）。v0.4.1 の preStop 15 秒の形のまま drain を足すと、待ち受けを閉じてから Pod が消えるまで（接続の終わりを待つ数秒）endpoint が `serving` のまま残り、MetalLB L2 + `Local` の告知しているノードの Pod を消したときに 4〜6 秒途切れた。`/readyz` と delay の組み合わせだけが、待ち受けを閉じる前に endpoint を外せる。
  - `features.graceful_shutdown` のない rproxy（parameters の `rproxy.image` で古いイメージを使う）は SIGTERM ですぐ止まるので、v0.4.1 の形（**preStop** 15 秒、`terminationGracePeriodSeconds` は preStop + 15 秒、readiness は `/healthz`、`RPROXY_SHUTDOWN_*` は渡さない）にする。preStop は Kubernetes 1.30 以降（kubelet の `sleep` アクションが既定で使える）は `lifecycle.preStop.sleep`、それより前は rproxy のコンテナで `sleep`（コントローラが起動時に API サーバの版を見て選ぶ）。コントローラが出荷する rproxy のイメージ（`--rproxy-image` の既定）は持っていると分かっている。ほかのイメージは、それを動かす Pod の `/capabilities` が `graceful_shutdown` を返してから新しい形にする（Pod がもう一度入れ替わる。分かったイメージはコントローラが動いている間覚えている）。
  - **preStop**（`managed.preStopSeconds` / `--pre-stop-secs`）：0.4.2 から既定は未設定（上の 2 つの形のとおり、新しい rproxy にはなし、古い rproxy には 15 秒）。値を設定した入れ方（0.4.1 で `managed.preStopSeconds` を書いていたもの）はその値を守り、どの Pod にもその preStop を付ける（新しい rproxy では preStop の後に delay と drain、猶予は preStop + delay + drain + 5 秒）。0 は preStop なし。
- **プローブ**（`managed.readinessProbe` / `--readiness-probe`、`managed.livenessProbe` / `--liveness-probe`）：readiness は既定で 2 秒ごと、2 回続けて失敗で外す（固まった rproxy が 4 秒ほどで endpoint から外れる）。liveness は 5 秒ごと・3 回で再起動（ゆっくりのまま）。形は `periodSeconds=2,timeoutSeconds=1,failureThreshold=2,successThreshold=1,initialDelaySeconds=0`（どれかだけでもよい）。readiness のパスは `/readyz`（`managed.readinessProbe.path` / `--readiness-path`。起動の復元中と終わりかけも not ready）か `/healthz`。liveness はいつも `/healthz`（`draining` は死んでいない）。
- **replicas が 2 以上**（`managed.replicas`）：Gateway ごとに PodDisruptionBudget（`rproxy-<id>`、`maxUnavailable: 1`、`unhealthyPodEvictionPolicy: AlwaysAllow`。Gateway が持ち主。replicas を 1 に戻すと消す）と、`kubernetes.io/hostname` の topologySpreadConstraints（`ScheduleAnyway`）。drain で 2 つの Pod が同時に止まらず、ふだんは別のノードに置く。
- **externalTrafficPolicy**（`managed.externalTrafficPolicy` / `--external-traffic-policy`）：
  - `Local`（`LoadBalancer` の既定）：クライアントの IP が rproxy に届く。ready な rproxy の Pod のあるノードだけが通信を受ける。ロードバランサは Service の `healthCheckNodePort`（kube-proxy が答える。ノードの Pod が終了中だけになると失敗を返す）でノードを選ぶ。MetalLB の L2 はそのノードの 1 つから告知する。
  - `Cluster`：どのノードも、どの ready な Pod にも送る（kube-proxy が SNAT するので、rproxy に見えるのはノードの IP）。Pod が入れ替わってもロードバランサの側は変わらない。
  - 空（既定）：`LoadBalancer` は `Local`、`NodePort` は `Cluster`（v0.4.0 と同じ）。`NodePort` の前に自前の L4 のロードバランサを置くなら `Local` にして、ノードのポートへのヘルスチェックで選ばせる。
  - `healthCheckNodePort` は Service ごとに Kubernetes が割り当てる（Gateway ごとに違うので、値にはしない）。
- **allocateLoadBalancerNodePorts**（`managed.allocateLoadBalancerNodePorts`、既定 true）：MetalLB のようにノードのポートを使わないロードバランサなら false にできる。
- コントローラは 1 つの Gateway の Pod へ、4 つずつ並べて PUT する。
- 選ばなかった形：止まる Pod の gate を先に `False` にして endpoint の `serving` も落とす（rproxy がまだ preStop で答えている間）。MetalLB は早く告知を移すが、`externalTrafficPolicy: Local` の kube-proxy はそのノードに来た通信を（ほかのノードの Pod に送れないので）落とすので、告知が移るまでの間がかえって途切れた（v0.4.1）。rproxy v0.4.1 の `/readyz` の `draining` も `serving` を落とすが、受け入れテストではこちらのほうが途切れが短かった（0.2〜1.2 秒。上の「SIGTERM での終わり方」）。

受け入れテスト（`.github/workflows/acceptance.yml`、`scripts/acceptance.sh` の `TOPOLOGY`）で、ロードバランサの形ごとに途切れを測る。結果と勧めは [README](../README.md) の「可用性」。

## 制御 API の接続

コントローラは最初に起動したとき、自分の namespace に次の Secret を作る（あれば読むだけ）。fleet の rproxy はこれを使う。

| Secret | 中身 | 読む人 |
|---|---|---|
| `rproxy-gateway-ca` | CA の証明書と鍵 | コントローラ（rproxy の Pod は読めない） |
| `rproxy-gateway-api-tls` | rproxy の制御 API の証明書（CA が `rproxy-api.rproxy-gateway.internal` に出す。Pod へは IP でつなぐので名前は固定） | rproxy（ボリューム） |
| `rproxy-gateway-token` | コントローラのトークン（`token`）と、rproxy が読むトークンファイル（`tokens.yaml`、SHA-256 だけ。スコープは `rules:read`・`rules:write`・`acme:write`） | `token` はコントローラ、`tokens.yaml` は rproxy（ボリュームでその項目だけ） |

managed の rproxy は、それぞれ Gateway の namespace の `rproxy-<id>-api` を使う：CA が `<id>.rproxy-api.rproxy-gateway.internal` に出した制御 API の証明書と、マスタートークンから導いたトークン（HMAC-SHA256、鍵がマスタートークンで中身が Gateway の id）のトークンファイル（`tokens.yaml`、SHA-256 だけ）。コントローラは Pod ごとにその名前とトークンでつなぐ。ある Gateway の namespace の Secret を読めても、ほかの Gateway の rproxy にもコントローラにもつなげない（マスタートークンと CA の鍵はコントローラの namespace だけ）。

`ui.namespace`（`--ui-namespace`）のときは、UI に見せる Gateway（parameters の `ui.visible`）のトークンファイルに UI 用の読むだけのトークン（`rules:read`・`metrics:read`）を足し、UI の namespace に Secret `rproxy-ui-discovery`（Pod の一覧・CA の証明書・UI 用のトークン）を書く（[SECURITY.md](SECURITY.md) の「UI に見せる」、[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 4.）。rproxy v0.4.2（`features.tokens_reload`）は変わったトークンファイルを読み直すので、UI 用のトークンは Pod のテンプレートの注釈のハッシュに入れず、入れる・外しても Pod は入れ替わらない（Pod が UI 用のトークンを受け付けるのを確かめてから発見の Secret に載せる）。それより前の rproxy（parameters の `rproxy.image` で古いイメージ）はトークンファイルを起動時と SIGHUP でしか読み直さないので、ハッシュに入れて Pod を 1 回入れ替える。

rproxy には `RPROXY_FILES_TRUSTED_DIRS`（Secret のボリュームのディレクトリ。kubelet が root のファイルにするため。[SECURITY.md](SECURITY.md)）も渡す。

rproxy は `RPROXY_API_ADDR=0.0.0.0`、`RPROXY_API_PORT=9443`、`RPROXY_TOKEN_FILE`、`RPROXY_TLS_CERT` / `RPROXY_TLS_KEY` で動く（制御 API を loopback 以外で開くときに rproxy が求める 3 つ）。

## 証明書（Secret のボリューム、certsync）

- `certificateRefs` の Secret は、rproxy のホストのファイルにして `cert_file` / `key_file` で指す（鍵を制御 API に流さない。rproxy-api の設計 3.3）。
- ファイル名は中身のハッシュ（`<sha256 の先頭 16 桁>.crt` / `.key`）。中身が変わればパスが変わり、ルールの変更として rproxy に届く。
- コントローラは Gateway ごとの証明書を 1 つの Secret（`rproxy-<id>-certs`）にまとめ、kubelet がそれを rproxy の Pod に Secret のボリューム（`/var/run/rproxy-gateway/certs`、読むだけ、モード 0440）としてつなぐ。fleet では、すべての Gateway の証明書を 1 つの Secret（`rproxy-fleet-certs`）にまとめ、DaemonSet の Pod につなぐ（Secret は 1 MiB まで）。
- rproxy の Pod は Kubernetes の API を使わない：ServiceAccount のトークンをつながず（`automountServiceAccountToken: false`）、RBAC もない。参照された証明書だけが、コントローラの書いた Secret を通して Pod に届く。同じ namespace のほかの Secret（CA の鍵、コントローラのトークン、ほかの Gateway の証明書）は読めない。
- Secret を書き換えたら、コントローラは Pod に注釈（`rproxy.max3584.net/certs`、中身のハッシュ）を付ける。kubelet は Pod の更新を受けてボリュームをすぐに更新する（注釈がなくても、kubelet の定期の同期（1 分ほど）で更新される）。
- 同じ Pod の `certsync`（このイメージの `rproxy-gateway certsync`）は、コントローラが送った名前（中身のハッシュ）のうちそのディレクトリにあるものを `POST /files` で返すだけ（一覧は返さない。API は使わない。Pod の IP で待ち受ける）。コントローラは PUT の前にファイルが揃ったかを確かめる（揃うまで `Programmed: False`、理由 `Pending`）。
- どのルールも使わなくなったファイルは、5 分 Secret に残してから外す（古いルールが読み直しても困らないように）。

### 選ばなかった形

| 形 | 選ばなかった理由 |
|---|---|
| certsync が Secret を watch する（前の形） | rproxy の Pod に namespace のすべての Secret を読む権限が要る。`resourceNames` では watch・list を名前で絞れない |
| 1 つの namespace にすべての Gateway の rproxy を置く（前の形） | rproxy の Pod の近くに CA の鍵とマスタートークンがあり、すべての Gateway の制御 API を同じ証明書・トークンで開く。Gateway API の `infrastructure`（Gateway の namespace に作るもの）にも合わない |
| 鍵を制御 API で送る | rproxy-api の設計 3.3（鍵をネットワークに流さない）に反する |

## 反映（ルールセット）

1. Pod の `GET /capabilities`（`features.rulesets` がなければ「rproxy v0.4.0 以降が要る」として `Programmed: False`）。`features.labels` がなければ `labels` を外して送る。
2. `GET /readyz` が ready になるまで待つ（`features.readyz` がない rproxy は待たない）。
3. `GET /rulesets/{name}`。前回 PUT したものと同じ内容・同じ etag なら何もしない。違えば（中身が変わった、rproxy が再起動してセットがない、ほかの誰かが変えた）`PUT /rulesets/{name}`。`generation` は Gateway の `metadata.generation`（rproxy にあるほうが大きければそちら。作り直した Gateway が `stale_generation` にならないように）、`If-Match` は今の etag。
4. rproxy が 1 つのルールを断ったら（`400`、`rules[i]: ...`）、そのルールを外して残りを PUT し直す（RproxyRule や移行したルートの 1 つの誤りでセット全体を止めない）。断られたルールは `Accepted: False`（`Invalid`）として状態に書く。
5. PUT のあとの `GET /rulesets/{name}` のルールの `conditions` から状態を書く。`failed` のルールがあれば、少し後にもう一度 PUT する（kubelet が書いたばかりのファイルなど）。

## 変換（Gateway API → rproxy）

| Gateway API | rproxy |
|---|---|
| リスナー `HTTP` | tcp のルール、`http`（平文） |
| リスナー `HTTPS`（`tls.mode: Terminate`） | tcp のルール、`tls.mode: terminate`（同じポートのリスナーの証明書をまとめる。rproxy が SNI で選ぶ）、`http` |
| HTTPRoute の `matches` | `match` の式：`Host`（`*.example.com` は `**.example.com`）、`Path`（Exact）、`Path(p) \|\| PathPrefix(p/)`（PathPrefix、区切りの `/` で合わせる）、`PathRegexp(^(?:re)$)`、`Method`、`Header` / `HeaderRegexp`、`Query` / `QueryRegexp` |
| ルールの優先 | Gateway API の順（ホスト名の具体さ → Exact → 長い PathPrefix → method → ヘッダの数 → クエリの数 → 古いルート → namespace/name → 書いた順）を `priority` に直す |
| 同じポートの、より具体的なホスト名のリスナー | ワイルドカード・ホスト名なしのリスナーのルートに `!Host(...)` を足して、そのホスト名を取らない（リスナーの分離） |
| `backendRefs` | Service の EndpointSlice の ready な Pod の IP（ClusterIP ではない）を `servers` に。`weight` は Pod の数で割って配る。ExternalName は名前のまま（既定では使えない：`--allow-external-name-services`） |
| 使える backend がない | `respond`（500） |
| 一部の backendRef が無効 | その backendRef の重みの分だけ rproxy が 500 で答える（`servers[]` の `status: 500`） |
| backend の Service のポートの `appProtocol: kubernetes.io/h2c` | サービスの `protocol: h2c`（転送先と前置きからの HTTP/2） |
| `RequestHeaderModifier` / `ResponseHeaderModifier` | `headers`（`set`・`add`・`remove`。`add` は既にある値の後ろに `,` で足す） |
| `RequestRedirect` | `redirect_regex`（`status` に 301・302・303・307・308。ポートは Gateway API の決まり：scheme を変えたらそのスキームの既定、変えなければリスナーのポート） |
| `URLRewrite` | hostname は `replace_host`、path は `replace_path`（ReplaceFullPath）・`replace_path_regex`（ReplacePrefixMatch） |
| `CORS` | `cors`（`allow_origins`・`allow_methods`・`allow_headers`・`expose_headers`・`allow_credentials`・`max_age`、`maxAge` の既定は 5） |
| `RequestMirror` | `mirror`（ミラー先の Pod の IP のサービスを作る。`percent` / `fraction`）。ミラー先が見つからなければ `ResolvedRefs: False` でミラーだけ外す |
| backendRef の `filters` | その backend の `servers[].middlewares`（`RequestHeaderModifier`・`ResponseHeaderModifier`・`URLRewrite`、rproxy v0.4.3 の `server_middleware_kinds` があれば `CORS`・`RequestRedirect`・`RequestMirror` も。URLRewrite・RequestRedirect の ReplacePrefixMatch は規則の path の接頭辞が 1 つのときだけ。ミラーはその backend に送るリクエストだけを写す） |
| `retry` | `retry`（`attempts` は Gateway API の回数 + 1、`codes` は `status`、`backoff` は `initial_interval`）。ミドルウェアの最後 |
| `ExternalAuth`（experimental） | `forward_auth`（rproxy v0.4.3 の `features.forward_auth`）。backendRef の Service の Pod の IP のサービス（`<規則>/f<n>/extauth`、BackendTLSPolicy があれば `https://` と `tls`）を `service` で指す。HTTP：`client_request`（クライアントのメソッド、`http.path` の後ろにパス、Host、Content-Length）、`allow_status: ["200"]`、`request_headers` は `authorization` と `allowedHeaders`、`response_headers` は `allowedResponseHeaders`（空なら `["*"]`）。gRPC：`protocol: grpc`（サービスは h2c、TLS なら h2）、`request_headers` は `allowedHeaders`（空ならすべて）。`forwardBody.maxSize` は `forward_body`。backend が見つからない・ready な Pod がない・使える CA がないときは、確かめずに通さないよう規則は 500（`ResolvedRefs: False`）。backendRef の `ExternalAuth` はその backend の `servers[].middlewares` |
| `ExtensionRef`（`RproxyMiddleware`） | そのミドルウェア（`spec` をそのまま） |
| `timeouts.request` / `timeouts.backendRequest` | ルートの `timeouts.request` / `timeouts.backend_request` |
| GRPCRoute | 同じポートの HTTPRoute と同じ `http` のルールに。メソッドの一致はパスの一致にする（`service` と `method`：`Path(/<service>/<method>)`、`service` だけ：`PathPrefix(/<service>/)`、`method` だけ・`RegularExpression`：`PathRegexp`）。ヘッダの一致・フィルタ（`RequestHeaderModifier`・`ResponseHeaderModifier`・`RequestMirror`・`ExtensionRef`）・backendRef のフィルタは HTTPRoute と同じ。転送先とは h2c（サービスの `protocol: h2c`）。rproxy の名前は `grpc:` で始める |
| BackendTLSPolicy | 対象の Service（`sectionName` でそのポート）に送る `servers` を `https://` にし、サービスの `tls`（`server_name` は `hostname`、`ca_file` は `caCertificateRefs` の ConfigMap の `ca.crt`、`wellKnownCACertificates: System` は rproxy の既定のルート、`subject_alt_names`）。同じ対象のポリシーは古いものが勝ち、ほかは `Accepted: False`（`Conflicted`）。ポートのものが Service 全体のものに勝つ。使える CA がなければ `Accepted: False`（`NoValidCACertificate`、`ResolvedRefs` は `InvalidKind` / `InvalidCACertificateRef`）で、その backend の分は 500。状態は `status.ancestors[]`（その Service に送るルートのある Gateway） |
| Gateway の `spec.tls.backend.clientCertificateRef` | BackendTLSPolicy のサービスの `tls` の `cert_file` / `key_file`。Gateway の `ResolvedRefs`（`InvalidClientCertificateRef`・`RefNotPermitted`） |
| リスナー `TLS`（`tls.mode: Passthrough`） | tcp のルール、`tls.mode: sni`、`unmatched: reject`。TLSRoute のホスト名ごとに `tls.routes` |
| 同じポートの `HTTPS` と `TLS`（Passthrough） | `http` のルールの `tls.routes`（`passthrough: true`）。そのホスト名だけ復号しない |
| 同じポートの複数の `HTTPS` のリスナー（HTTP/2 の接続の使い回し） | `tls.misdirected`：リスナーごとに 1 つのグループ（ホスト名、ないものは `*`）。SNI で選んだリスナーとは別のリスナーのホスト名のリクエストは 421（`GatewayHTTPSListenerDetectMisdirectedRequests`、rproxy v0.4.3 の `http_options` の `misdirected`。ない rproxy では付けない） |
| リスナー `TLS`（`tls.mode: Terminate`） | tcp のルール、`tls.mode: terminate`、TLSRoute のホスト名ごとに `tls.routes`（同じポートの Passthrough のリスナーの分は `passthrough: true`） |
| TLSRoute の宛先 | `tls.routes[].targets`：すべての backend の Pod の IP（weight を Pod の数で配る） |
| リスナー `TCP` / `UDP` | tcp / udp のルール、`targets`（TCPRoute / UDPRoute のすべての backend の Pod の IP、weight を Pod の数で配る） |
| 何もつながっていない `TLS` / `TCP` / `UDP` のリスナー | ルールを作らない（リスナーは `Programmed: True`。Service のポートはある） |
| 使える backend のない TLSRoute | その名前は `127.0.0.1:1` へ（つないでから閉じる。Gateway API は接続の拒否ではなくリセットを求める） |
| 1 つの TCP / UDP のリスナーに複数のルート | どれも `Accepted` だが、通信は最も古いルートへ |
| ListenerSet | Gateway の `allowedListeners`（既定は `None`）が許す ListenerSet のリスナーを、Gateway のリスナーの後ろに足す（古いもの → namespace/name の順。同じポートで食い違えば前のものが勝つ）。証明書の参照・`allowedRoutes` の `Same` は ListenerSet の namespace から（ほかの namespace の Secret には from `ListenerSet` の ReferenceGrant）。ルートは parentRef が `kind: ListenerSet` のときだけそのリスナーにつながる。ListenerSet の状態は `Accepted`（`NotAllowed`、有効なリスナーがなければ `ListenersNotValid`、Gateway が受け付けられていなければ `ParentNotAccepted`）、`Programmed`、`listeners`。Gateway の `status.attachedListenerSets` は受け付けた数 |
| Gateway の `spec.tls.frontend`（クライアント証明書の検証） | そのポートのルールの `tls.client_auth`（`mode: required`、`ca_file` は `caCertificateRefs` の ConfigMap の `ca.crt` をまとめたファイル）。`perPort` があればそのポート、なければ `default`。ConfigMap 以外は `InvalidCACertificateKind`、見つからない・`ca.crt` のないものは `InvalidCACertificateRef`、ほかの namespace は ReferenceGrant（to `ConfigMap`）が要る（`RefNotPermitted`）。使えるものが 1 つもなければリスナーは `Accepted: False`（`NoValidCACertificate`）。`AllowInsecureFallback` は `mode: optional_no_verify`（証明書を求めて確かめるが、なくても通らなくてもつなぐ。結果は `X-Client-Verify: SUCCESS / FAILED / NONE` と `X-Forwarded-Client-Cert` で backend に渡る）で、Gateway に `InsecureFrontendValidationMode: True`。そのモードのない rproxy（`features.client_auth_modes`）では証明書を求めない |
| 証明書の使えない `HTTPS` のリスナー | ルートはつながる（`attachedRoutes` に数える）が、ルールは作らない（`ResolvedRefs: False`、`Programmed: False`） |

### rproxy の CRD（`rproxy.max3584.net/v1beta1`、`v1alpha1` も同じ形で非推奨のまま使える。[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 11.4）

| CRD | 使い方 |
|---|---|
| `RproxyMiddleware` | `spec` は rproxy の `http.middlewares.<名前>` の形（例 `{rate_limit: {average: 10}}`）。HTTPRoute の `ExtensionRef` フィルタで使う。見つからなければそのルールは 500 で、`ResolvedRefs: False` |
| `RproxyPolicy` | `spec.targetRefs`（同じ namespace の Gateway、`sectionName` でそのリスナー、Service）に、ルールの `limits`・`bandwidth`・`geoip`・`outlierDetection`・`allowFrom`・`crowdsec`、転送先への接続の時間 `connectTimeout`・応答の時間 `responseTimeout`（v0.4.5）を足す（GEP-713）。Service を指すと、その Service に送る L4 のルール（`outlierDetection` は `http` のサービスにも）。同じ項目を複数のポリシーが決めたら古いほうが勝つ。状態は `status.ancestors[]`（見つからないリスナーは `Accepted: False`、`TargetNotFound`） |
| `RproxyGatewayParameters` | managed の Gateway の rproxy の形（replicas、PDB、Pod のラベル・注釈・resources・topologySpread・nodeSelector・tolerations・affinity・priorityClass、Service の型・externalTrafficPolicy・loadBalancerClass・sourceRanges・ipFamilyPolicy・ラベル・注釈、rproxy のイメージ・logLevel・performance・追加の環境変数）。GatewayClass の `parametersRef`（コントローラの namespace、`policy` で Gateway に許す項目を決める）と Gateway の `infrastructure.parametersRef`（同じ namespace）から。合わせ方・検証・誰が何を決められるかは [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 2. |
| `RproxyRule` | `spec.rule` はルールそのもの（`POST /rules` の本文）。`spec.parentRef` の Gateway のルールセットに足す。ほかの namespace の Gateway には、その namespace の ReferenceGrant（from `rproxy.max3584.net/RproxyRule`、to `Gateway`）が要る。同じキーのルールが既にあれば `Accepted: False`（`Conflicted`）。状態は rproxy のルールの `conditions` を写す |

できないもの（ルートは `Accepted: False`、理由 `UnsupportedValue`）：backendRef の `ExtensionRef` フィルタ。rproxy に機能がないもの（下の「rproxy の機能」）。

### rproxy の機能（`features`）

上の新しい設定（`headers` の `add`、リダイレクトの `status`、ルートの `timeouts`、`replace_host`、`servers[].middlewares`、`cors`、`retry` の `status`、`mirror`、サービスの `protocol`・`tls`、`tls.routes[].targets`、`servers[].status`）は rproxy v0.4.0 の機能（rproxy-api docs/API.md の「Gateway API 向けの L7・TLS」）。コントローラは Gateway の rproxy の Pod の `GET /capabilities` の `features` を読み、すべての Pod にある設定だけを使う。

- 足りない設定を使うルートは `Accepted: False`（`UnsupportedValue`、足りない `features` の名前を書く）。ほかのルートはそのまま動く。
- 前の形で同じことができるものは前の形にする：`timeouts.backendRequest` はサービスの `timeouts.response`、TLSRoute の宛先は Service の ClusterIP（weight の最も大きい backendRef）、一部が無効な backendRefs は有効なものだけ。
- まだ聞いていない Pod（作ったばかり）は v0.4.0 のすべてがあるものとして描き、聞いたあとの次の反映で直す。

## 状態

| 書くところ | 中身 |
|---|---|
| GatewayClass | `Accepted`（`parametersRef` が誤りなら `InvalidParameters`）、`SupportedVersion`、`supportedFeatures` |
| Gateway | `addresses`、`Accepted`（`UnsupportedAddress`、`InvalidParameters`、`ListenersNotValid`）、`Programmed`（アドレスがあり、1 つ以上の Pod に反映できた。`AddressNotUsable`）、`ResolvedRefs`（`tls.backend` があるとき）、`InsecureFrontendValidationMode`、`attachedListenerSets`。`observedGeneration` は Gateway の generation |
| ListenerSet | `Accepted`、`Programmed`、`listeners`（Gateway のリスナーと同じ） |
| BackendTLSPolicy・RproxyPolicy | `status.ancestors[]`（Gateway ごと、ほかのコントローラの項目は残す） |
| Ingress（移行） | `status.loadBalancer.ingress`（移行先の Gateway のアドレス） |
| リスナー | `Accepted`（`UnsupportedProtocol`、`ProtocolConflict`、`HostnameConflict`、証明書がない）、`ResolvedRefs`（`InvalidCertificateRef`、`RefNotPermitted`、`InvalidRouteKinds`）、`Conflicted`、`Programmed`（rproxy のルールの `Programmed`）、`supportedKinds`、`attachedRoutes` |
| ルートの `status.parents[]` | `Accepted`（`NotAllowedByListeners`、`NoMatchingListenerHostname`、`NoMatchingParent`、`UnsupportedValue`）、`ResolvedRefs`（`BackendNotFound`、`RefNotPermitted`、`InvalidKind`）。rproxy のルールの `Accepted` / `ResolvedRefs` が `False` ならそれも書く。ほかのコントローラの項目は残す |

`lastTransitionTime` は状態が変わったときだけ新しくする。中身が変わらなければ書き込まない。
