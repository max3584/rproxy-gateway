English: [en/DESIGN.md](en/DESIGN.md)

# rproxy-gateway の設計

rproxy-api の docs/DESIGN-v0.4.md 3.（#28）を形にしたもの。ここにはコントローラの側の決めごとを書く。

## 全体

```
Gateway API / CRD ──watch──▶ rproxy-gateway ──PUT /rulesets/k8s/<ns>/<name>──▶ rproxy（制御 API、HTTPS + トークン）
                                   │                                              ▲
                                   └──証明書の Secret（<id>-certs）──▶ certsync ──ファイル──┘（同じ Pod の emptyDir）
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
| `managed`（既定） | コントローラが自分の namespace に Gateway ごとの Deployment と Service（`rproxy-<id>`、型は `--service-type`、既定 `LoadBalancer`）を作る。Gateway が消えたら消す | Service のロードバランサのアドレス（`ClusterIP` 型なら ClusterIP） |
| `fleet` | 先に置いた rproxy の Pod（chart の `hostNetwork: true` の DaemonSet など、`--fleet-selector`）がすべての Gateway を受け持つ。コントローラはすべての Pod に同じセットを PUT する | `--fleet-address`、なければ Pod のノードの IP |

- `<id>` は `<namespace>-<name>`（40 文字まで）とハッシュ 6 桁。
- managed の Pod は非 root（65532）で、1024 未満のポートは `net.ipv4.ip_unprivileged_port_start=0`（namespace ごとの安全な sysctl）で受ける。
- fleet で同じポートを 2 つの Gateway が使うと、後から来たほうのルールは rproxy が `409 already_exists` で断り、リスナーの `Programmed` が `False` になる。

## 制御 API の接続

コントローラは最初に起動したとき、自分の namespace に次の Secret を作る（あれば読むだけ）。

| Secret | 中身 | 読む人 |
|---|---|---|
| `rproxy-gateway-ca` | CA の証明書と鍵 | コントローラ |
| `rproxy-gateway-api-tls` | rproxy の制御 API の証明書（CA が `rproxy-api.rproxy-gateway.internal` に出す。Pod へは IP でつなぐので名前は固定） | rproxy |
| `rproxy-gateway-token` | コントローラのトークン（`token`）と、rproxy が読むトークンファイル（`tokens.yaml`、SHA-256 だけ。スコープは `rules:read`・`rules:write`・`acme:write`） | `token` はコントローラ、`tokens.yaml` は rproxy |

rproxy は `RPROXY_API_ADDR=0.0.0.0`、`RPROXY_API_PORT=9443`、`RPROXY_TOKEN_FILE`、`RPROXY_TLS_CERT` / `RPROXY_TLS_KEY` で動く（制御 API を loopback 以外で開くときに rproxy が求める 3 つ）。

## 証明書（certsync）

- `certificateRefs` の Secret は、rproxy のホストのファイルにして `cert_file` / `key_file` で指す（鍵を制御 API に流さない。rproxy-api の設計 3.3）。
- ファイル名は中身のハッシュ（`<sha256 の先頭 16 桁>.crt` / `.key`）。中身が変わればパスが変わり、ルールの変更として rproxy に届く。
- コントローラは Gateway ごとの証明書を 1 つの Secret（`rproxy-<id>-certs`、ラベル `rproxy.max3584.net/certs-for`）にまとめる。rproxy の Pod の中の `certsync`（このイメージの `rproxy-gateway certsync`）がそれを watch して、rproxy と共有する `emptyDir`（`/var/run/rproxy-gateway/certs`）に書く。
- コントローラは PUT の前に certsync の `GET /files` でファイルが揃ったかを確かめる（揃うまで `Programmed: False`、理由 `Pending`）。どの Secret にもなくなったファイルは 5 分残してから消す（古いルールが読み直しても困らないように）。

## 反映（ルールセット）

1. Pod の `GET /capabilities`（`features.rulesets` がなければ「rproxy v0.4.0 以降が要る」として `Programmed: False`）。`features.labels` がなければ `labels` を外して送る。
2. `GET /readyz` が ready になるまで待つ（`features.readyz` がない rproxy は待たない）。
3. `GET /rulesets/{name}`。前回 PUT したものと同じ内容・同じ etag なら何もしない。違えば（中身が変わった、rproxy が再起動してセットがない、ほかの誰かが変えた）`PUT /rulesets/{name}`。`generation` は Gateway の `metadata.generation`（rproxy にあるほうが大きければそちら。作り直した Gateway が `stale_generation` にならないように）、`If-Match` は今の etag。
4. rproxy が 1 つのルールを断ったら（`400`、`rules[i]: ...`）、そのルールを外して残りを PUT し直す（RproxyRule や移行したルートの 1 つの誤りでセット全体を止めない）。断られたルールは `Accepted: False`（`Invalid`）として状態に書く。
5. PUT のあとの `GET /rulesets/{name}` のルールの `conditions` から状態を書く。`failed` のルールがあれば、少し後にもう一度 PUT する（certsync が書いたばかりのファイルなど）。

## 変換（Gateway API → rproxy）

| Gateway API | rproxy |
|---|---|
| リスナー `HTTP` | tcp のルール、`http`（平文） |
| リスナー `HTTPS`（`tls.mode: Terminate`） | tcp のルール、`tls.mode: terminate`（同じポートのリスナーの証明書をまとめる。rproxy が SNI で選ぶ）、`http` |
| HTTPRoute の `matches` | `match` の式：`Host`（`*.example.com` は `**.example.com`）、`Path`（Exact）、`Path(p) \|\| PathPrefix(p/)`（PathPrefix、区切りの `/` で合わせる）、`PathRegexp(^(?:re)$)`、`Method`、`Header` / `HeaderRegexp`、`Query` / `QueryRegexp` |
| ルールの優先 | Gateway API の順（ホスト名の具体さ → Exact → 長い PathPrefix → method → ヘッダの数 → クエリの数 → 古いルート → namespace/name → 書いた順）を `priority` に直す |
| 同じポートの、より具体的なホスト名のリスナー | ワイルドカード・ホスト名なしのリスナーのルートに `!Host(...)` を足して、そのホスト名を取らない（リスナーの分離） |
| `backendRefs` | Service の EndpointSlice の ready な Pod の IP（ClusterIP ではない）を `servers` に。`weight` は Pod の数で割って配る。ExternalName は名前のまま |
| 使える backend がない | `respond`（500） |
| `RequestHeaderModifier` / `ResponseHeaderModifier` | `headers`（`set`・`remove`。`add` は `set` になる） |
| `RequestRedirect` | `redirect_regex`（301 / 302。ポートは Gateway API の決まり：scheme を変えたらそのスキームの既定、変えなければリスナーのポート） |
| `URLRewrite` の path | `replace_path`（ReplaceFullPath）、`replace_path_regex`（ReplacePrefixMatch） |
| `ExtensionRef`（`RproxyMiddleware`） | そのミドルウェア（`spec` をそのまま） |
| `timeouts.backendRequest`（なければ `request`） | サービスの `timeouts.response` |
| リスナー `TLS`（`tls.mode: Passthrough`） | tcp のルール、`tls.mode: sni`、`unmatched: reject`。TLSRoute のホスト名ごとに `tls.routes` |
| 同じポートの `HTTPS` と `TLS`（Passthrough） | `http` のルールの `tls.routes`（`passthrough: true`）。そのホスト名だけ復号しない |
| リスナー `TLS`（`tls.mode: Terminate`） | tcp のルール、`tls.mode: terminate`、TLSRoute のホスト名ごとに `tls.routes`（同じポートの Passthrough のリスナーの分は `passthrough: true`） |
| TLSRoute の宛先 | `tls.routes` の 1 つの項目は宛先が 1 つなので、Service の ClusterIP（kube-proxy が Pod に配る。headless なら最初の Pod）。backendRefs が複数なら weight の最も大きいもの |
| リスナー `TCP` / `UDP` | tcp / udp のルール、`targets`（TCPRoute / UDPRoute のすべての backend の Pod の IP、weight を Pod の数で配る） |
| 何もつながっていない `TLS` / `TCP` / `UDP` のリスナー | ルールを作らない（リスナーは `Programmed: True`。Service のポートはある） |
| 使える backend のない TLSRoute | その名前は `127.0.0.1:1` へ（つないでから閉じる。Gateway API は接続の拒否ではなくリセットを求める） |
| 1 つの TCP / UDP のリスナーに複数のルート | どれも `Accepted` だが、通信は最も古いルートへ |
| 証明書の使えない `HTTPS` のリスナー | ルートはつながる（`attachedRoutes` に数える）が、ルールは作らない（`ResolvedRefs: False`、`Programmed: False`） |

### rproxy の CRD（`rproxy.max3584.net/v1alpha1`）

| CRD | 使い方 |
|---|---|
| `RproxyMiddleware` | `spec` は rproxy の `http.middlewares.<名前>` の形（例 `{rate_limit: {average: 10}}`）。HTTPRoute の `ExtensionRef` フィルタで使う。見つからなければそのルールは 500 で、`ResolvedRefs: False` |
| `RproxyPolicy` | `spec.targetRefs`（同じ namespace の Gateway、`sectionName` でそのリスナー、Service）に、ルールの `limits`・`bandwidth`・`geoip`・`outlierDetection`・`allowFrom`・`crowdsec` を足す（GEP-713）。Service を指すと、その Service に送る L4 のルール（`outlierDetection` は `http` のサービスにも）。同じ項目を複数のポリシーが決めたら古いほうが勝つ。状態は `status.ancestors[]`（見つからないリスナーは `Accepted: False`、`TargetNotFound`） |
| `RproxyRule` | `spec.rule` はルールそのもの（`POST /rules` の本文）。`spec.parentRef` の Gateway のルールセットに足す。ほかの namespace の Gateway には、その namespace の ReferenceGrant（from `rproxy.max3584.net/RproxyRule`、to `Gateway`）が要る。同じキーのルールが既にあれば `Accepted: False`（`Conflicted`）。状態は rproxy のルールの `conditions` を写す |

できないもの（ルートは `Accepted: False`、理由 `UnsupportedValue`）：`URLRewrite` の hostname、`RequestMirror`、`CORS` フィルタ、backendRef ごとのフィルタ、リダイレクトの 303 / 307 / 308。

## 状態

| 書くところ | 中身 |
|---|---|
| GatewayClass | `Accepted`、`SupportedVersion`、`supportedFeatures` |
| Gateway | `addresses`、`Accepted`、`Programmed`（アドレスがあり、1 つ以上の Pod に反映できた）。`observedGeneration` は Gateway の generation |
| リスナー | `Accepted`（`UnsupportedProtocol`、`ProtocolConflict`、`HostnameConflict`、証明書がない）、`ResolvedRefs`（`InvalidCertificateRef`、`RefNotPermitted`、`InvalidRouteKinds`）、`Conflicted`、`Programmed`（rproxy のルールの `Programmed`）、`supportedKinds`、`attachedRoutes` |
| ルートの `status.parents[]` | `Accepted`（`NotAllowedByListeners`、`NoMatchingListenerHostname`、`NoMatchingParent`、`UnsupportedValue`）、`ResolvedRefs`（`BackendNotFound`、`RefNotPermitted`、`InvalidKind`）。rproxy のルールの `Accepted` / `ResolvedRefs` が `False` ならそれも書く。ほかのコントローラの項目は残す |

`lastTransitionTime` は状態が変わったときだけ新しくする。中身が変わらなければ書き込まない。
