# Mesh（GAMMA）の試作の設計

この文書は影のブランチ `exp/mesh` だけにある。オーナーの決定（2026-10-15）：「Meshは、別のブランチでベンチマークみたいに影で作りましょう」。

- `main` にはマージしない。リリースしない。リリースノートと conformance の提出（docs/conformance/submission/）には書かない。PR も開かない。
- 利用者向けの文書ではないので、英語版は作らない（`main` に入れることになったら作る）。
- 前の見立ては [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 11.5（作らない、と勧めた）。この文書はその続きで、作るならどう作るかを書く。
- 調べた版：Gateway API v1.6.3（タグ）、rproxy-gateway の `main`（97021d2）、rproxy-api の `master`（f50f309、v0.4.10）。

## 1. 目的と目的でないもの

**目的**

- Gateway API の Mesh の conformance（`MESH-HTTP` と `MESH-GRPC`）の **core** を、kind の上で通す。
- 既定ではすべて切っておく（chart の値と、コントローラのフラグ）。入れても、Service にルートを付けるまで通信は何も変わらない。
- rproxy か通信を取る部品が止まったら、通信はふつうの kube-proxy の道に戻る。Mesh の部品が壊れても、クラスタの通信は止めない。
- 段階ごとに小さく作り、それぞれを CI で確かめる。

**目的でないもの**

- サイドカーの注入（Pod ごとの rproxy）。
- mTLS とワークロードの識別（SPIFFE）。conformance は求めない。
- 認可（AuthorizationPolicy のようなもの）。
- TCPRoute・TLSRoute・UDPRoute を Service に付けること。
- IPv6（段階の後のほう）。
- 複数のクラスタ（ServiceImport）。
- Cilium の kube-proxy の代わり（eBPF の socket LB）と Calico の eBPF のデータプレーン（5.8）。
- リリース・リリースノート・conformance の提出。

## 2. Gateway API v1.6.3 の Mesh が求めるもの

### 2.1 API の決まり（GEP-1294、Standard）

| 項目 | 決まり |
|---|---|
| 親 | HTTPRoute・GRPCRoute の `parentRefs` に `group: ""`、`kind: Service` を書く。Service の「フロントエンド」（ClusterIP）に来た通信にルートを当てる |
| どの通信か | 宛先の IP が親の Service の ClusterIP の通信。Pod の IP に直接送った通信（Service の「バックエンド」）には当てない |
| ポート | `parentRef.port` があればそのポートだけ。なければ Service のすべてのポート |
| `hostnames` | 実装は意味を持たせない（SHOULD NOT）。Host で分けたいなら Host ヘッダの一致を使う |
| ルートがある Service | ルートを付けると、今の暗黙の動き（kube-proxy の振り分け）を**置き換える**。どの規則にも合わない要求は 404 になる |
| producer のルート | ルートと親の Service が同じ namespace。どの namespace のクライアントにも当てる（SHOULD） |
| consumer のルート | ルートと親の Service の namespace が違う。ルートの namespace のクライアントにだけ当てる。その namespace のクライアントには producer のルートを使わない |
| 型の取り合い | 同じ Service とポートに違う型のルート：GRPCRoute > HTTPRoute > TLSRoute > TCPRoute。負けたほうは `Accepted: False`（`Conflicted`） |
| 状態 | ルートが当たったかをルートの状態に書く（MUST） |
| ReferenceGrant | backendRef がほかの namespace でも要らない |
| 使える Service | ClusterIP（selector の有無は問わない）。headless と ExternalName は親にしない（SHOULD NOT） |

### 2.2 conformance の仕組み

- 機能：core は `Mesh`。extended は `MeshClusterIPMatching`・`MeshConsumerRoute`・`MeshHTTPRouteRewritePath`・`MeshHTTPRouteSchemeRedirect`・`MeshHTTPRouteRedirectPort`・`MeshHTTPRouteRedirectPath`・`MeshHTTPRouteBackendRequestHeaderModification`・`MeshHTTPRouteQueryParamMatching`・`MeshHTTPRouteNamedRouteRule`（`pkg/features/mesh.go`）。
- プロファイル（`conformance/utils/suite/profiles.go`）：`MESH-HTTP` の core は `Mesh` と `HTTPRoute`、`MESH-GRPC` の core は `Mesh` と `GRPCRoute`。どちらも extended は上の Mesh の extended の機能。
- 試験の場（`conformance/mesh/manifests.yaml`）：namespace `gateway-conformance-mesh` と `gateway-conformance-mesh-consumer`。Deployment `echo-v1`・`echo-v2`（`registry.k8s.io/gateway-api/echo-advanced:v1.5.1`。HTTP 8080、gRPC 7070、TCP 9090、TLS 8443）。Service `echo`（両方の Pod）・`echo-v1`・`echo-v2`。ポートは `http` 80→8080（`appProtocol: http`）、`http-alt` 8080（`appProtocol: http`）、`https` 443→8443、`tcp` 9090、`grpc` 7070（`echo` だけ `appProtocol: kubernetes.io/h2c`）。
- 要求の送り方（`conformance/utils/echo/pod.go`）：suite は `echo-v1` の Pod に `kubectl exec` で入り、Pod の中の `client` コマンドで `http://echo/...`（gRPC は `grpc://echo:7070`）を呼ぶ。名前は Pod の中で CoreDNS が ClusterIP に引く。つまり **Pod から ClusterIP への通信を取れば試験が動く**。応答の Pod 名（`Hostname`）・状態コード・ヘッダで確かめる。
- Mesh の試験は**ルートの状態を確かめない**（GEP-1294 は状態を MUST にしているので、書きはする）。
- 名乗る機能：`--supported-features` で渡すか、`--mesh-name` で XMesh（`gateway.networking.x-k8s.io`、experimental の CRD に入っている）の `status.supportedFeatures` から読む。GatewayClass の `supportedFeatures` に Mesh の機能を書くと suite は警告して捨てる。
- `--namespace-labels` で試験の namespace にラベルを付けられる（サイドカーの注入を有効にするための口）。
- 試験は機能をすべて名乗ったときだけ走り、レポートでは試験の機能がすべてプロファイルの core にあれば core、そうでなければ extended に数える。プロファイルの外の機能を要る試験は、走ってもそのプロファイルに数えない。

### 2.3 v1.6.3 の Mesh の試験（`conformance/tests/mesh/`、22 本）

| 試験 | 機能（`Mesh` のほか） | 数えるところ |
|---|---|---|
| `MeshBasic` | なし | MESH-HTTP と MESH-GRPC の core |
| `MeshHTTPRouteSimpleSameNamespace` | HTTPRoute | MESH-HTTP の core |
| `MeshHTTPRouteMatching` | HTTPRoute | MESH-HTTP の core |
| `MeshHTTPRouteRequestHeaderModifier` | HTTPRoute | MESH-HTTP の core |
| `MeshHTTPRouteRedirectHostAndStatus` | HTTPRoute | MESH-HTTP の core |
| `MeshHTTPRouteWeight` | HTTPRoute | MESH-HTTP の core |
| `MeshTrafficSplit` | HTTPRoute | MESH-HTTP の core |
| `MeshGRPCRouteWeight` | GRPCRoute | MESH-GRPC の core |
| `MeshHTTPRouteNamedRule`・`MeshHTTPRouteQueryParamMatching`・`MeshHTTPRouteRedirectPath`・`MeshHTTPRouteRedirectPort`・`MeshHTTPRouteSchemeRedirect`・`MeshHTTPRouteBackendRequestHeaderModifier`・`MeshHTTPRouteRewritePath` | HTTPRoute と Mesh の extended の機能 1 つ | MESH-HTTP の extended（7 本） |
| `MeshFrontend` | HTTPRoute、`HTTPRouteResponseHeaderModification` | どのプロファイルにも数えない |
| `MeshFrontendHostname` | 同上と `MeshClusterIPMatching` | 同上 |
| `MeshPorts` | 同上と `HTTPRouteParentRefPort` | 同上 |
| `MeshConsumerRoute` | 同上と `MeshConsumerRoute` | 同上 |
| `MeshHTTPRoute303Redirect`・`307`・`308` | HTTPRoute と `HTTPRoute30xRedirectStatusCode` | 同上 |

core は MESH-HTTP が 7 本、MESH-GRPC が 2 本。

### 2.4 core を通すのに具体的に要ること

1. `echo-v1` の Pod から Service の ClusterIP:port（80、7070）への接続を、rproxy に取り込む。
2. 取り込んだ接続の**元の宛先**（ClusterIP:port）で、どの Service のどのポートかを決める。
3. その Service に付いた HTTPRoute・GRPCRoute で振り分ける：path・header の一致、`RequestHeaderModifier`、`RequestRedirect`（ホスト名と 301）、重み（70 / 30）、gRPC の h2c。どれも Gateway の側では rproxy-gateway が今できる（docs/CONFORMANCE.md の GATEWAY-HTTP・GATEWAY-GRPC の core はすべて通る）。
4. ルートのない Service と、ルートのないポート（`MeshBasic` の `echo:80` はルートなし）は、今までどおり届く。
5. Pod の IP に直接送った通信は取らない（`MeshFrontend` の「Send to pod IP」）。

## 3. 今あるものと足りないもの

| もの | 今 | Mesh で |
|---|---|---|
| HTTPRoute・GRPCRoute の変換（`src/render/http.rs`） | Gateway のリスナーごとに `http` のルール。一致・優先の順・フィルタ・重み・h2c | そのまま使う。「リスナー」の代わりに「Service の ClusterIP:port」を渡す |
| backend（`src/render/backends.rs`） | EndpointSlice の ready な Pod の IP を `servers` に | そのまま。rproxy は ClusterIP ではなく Pod の IP に送るので、もう一度取り込まれることはない |
| 親の判定（`src/render/mod.rs`） | `kind` が `Gateway`（と ListenerSet）だけ。Service の親は無視している | Service の親を Mesh の側で拾う。Gateway の側は今のまま |
| fleet（`charts/rproxy-gateway/templates/fleet.yaml`） | hostNetwork の DaemonSet の rproxy。すべての Pod に同じルールセットを PUT。readiness gate | 形を真似て、Mesh 用の DaemonSet を別に作る（fleet と managed のどちらとも並べる） |
| rproxy の `http` のルール | `(protocol, listen_addr, listen_port)` がキー。`match` に `ClientIP` がある | Service の ClusterIP:port ごとに 1 つの `http` のルール。consumer のルートは `ClientIP` で分ける |
| ルールセット（`PUT /rulesets/{name}`） | 宣言的。1 つの組に 10,000 ルールまで | Mesh の組を 1 つ（`k8s/mesh`）。ルートの付いた Service のポートだけなので数は少ない |
| `listen_freebind`（rproxy v0.4.3） | ホストにないアドレスで bind できる（`IP_FREEBIND`） | ClusterIP で bind するのに要る。でも TPROXY で受けるには足りない（下） |
| `source_ip: transparent` | **転送先への**接続をクライアントのアドレスから出す（`IP_TRANSPARENT`。docs/TRANSPARENT.md） | 使わない。待ち受けの側の `IP_TRANSPARENT` ではない |
| **待ち受けの `IP_TRANSPARENT`** | **ない**（`src/net/listen.rs` は `IP_FREEBIND` だけ） | 要る。TPROXY は `IP_TRANSPARENT` の付いたソケットにしか渡さない。rproxy-api に足す（5.4） |
| `GET /readyz` | 復元が終わるまでと終わりかけは 503 | 取り込みの部品が rproxy の生死を見るのに使う |

## 4. ほかのメッシュの取り方（概要）

| 実装 | 取り方 | 元の宛先 |
|---|---|---|
| Istio（サイドカー） | init コンテナか istio-cni が Pod の netns に iptables を書き、外向きを `REDIRECT` でサイドカーの 15001 へ。TPROXY の形もある（`-m socket --transparent` で印、`-j TPROXY`、印で `local` の経路） | `SO_ORIGINAL_DST`（TPROXY ではソケットの自分のアドレス） |
| Istio ambient（ztunnel） | ノードごとの ztunnel。今の形（in-pod）は istio-cni が Pod の netns に入って規則を書き、ztunnel が Pod の netns の中に待ち受けのソケットを開く。前の形（ホストの netns で取る）は CNI との相性で捨てた | Pod の netns の中のソケットで受けるので、ふつうの元の宛先 |
| Cilium | eBPF。socket LB（`connect()` で ClusterIP を Pod の IP に書き換える）と tc。L7 はノードごとの Envoy に eBPF の TPROXY で渡す | eBPF の表 |
| Linkerd | サイドカー。init コンテナか linkerd-cni が Pod の netns に iptables（外向きは 4140 へ `REDIRECT`） | `SO_ORIGINAL_DST` |

Istio ambient が「ホストの netns で取る」形をやめたことは大事な前例で、この設計の一番の危うさ（5.8、7.）にあたる。

## 5. 形（提案）

### 5.1 全体

```
Pod（クライアント）──veth──▶ ノードの netns の PREROUTING
                               │ 宛先が「ルートの付いた ClusterIP:port」で、rproxy の待ち受けがある
                               ├─ yes ─▶ TPROXY ─▶ ノードの rproxy（hostNetwork、ClusterIP:port の http のルール）─▶ backend の Pod の IP
                               └─ no ──▶ kube-proxy（今までどおり DNAT）─▶ Pod

rproxy-gateway（コントローラ、--mesh）──PUT /rulesets/k8s/mesh──▶ すべてのノードの Mesh の rproxy
mesh-agent（同じ Pod）──GET /rulesets/k8s/mesh（127.0.0.1）──▶ nftables の表 inet rproxy_mesh を書く
```

### 5.2 取り方の比較

| 案 | 中身 | 良いところ | 悪いところ |
|---|---|---|---|
| **A. ホストの nftables で TPROXY**（選ぶ） | ノードの netns の PREROUTING（conntrack より前）で、ルートの付いた ClusterIP:port への新しい接続を、ノードの rproxy の透過の待ち受けへ TPROXY。`notrack` で kube-proxy の DNAT を飛ばす | Pod の netns に入らない。CNI の連鎖も webhook も要らない。fleet の DaemonSet の形をそのまま使える。待ち受けがなければ規則が当たらず、kube-proxy の道に戻る（5.9）。CI（kind）で動かせる | rproxy に待ち受けの `IP_TRANSPARENT` が要る。kube-proxy・CNI の netfilter の規則と同じ所を通るので相性が出る（Istio ambient が前に当たった問題）。eBPF で ClusterIP を書き換える CNI では宛先が見えない |
| B. eBPF（cgroup の `connect4` で宛先を書き換える） | Pod の `connect()` で宛先を rproxy に変え、元の宛先を BPF の表に残す | netfilter と関わらない。kube-proxy の代わりの CNI とも並べられる可能性 | rproxy が BPF の表から元の宛先を読む口が要る（大きい変更）。BPF のプログラムの読み込み・版の違い・権限（`CAP_BPF`）。rproxy が止まると接続が拒まれるので、戻すには表に生死を持たせる工夫が要る。Cilium も同じフックを使う。rproxy-api のカーネルでの転送の方針（実機なしで CI の正しさで採る、既定はオフ）とも合わせる必要がある |
| C. Pod の netns の中で取る（Istio ambient の in-pod） | ノードの部品が各 Pod の netns に規則を書き、rproxy が Pod の netns の中で待ち受ける | CNI の相性が一番よい（Istio の今の答え） | rproxy が Pod ごとの netns にソケットを開く（`setns`）口が要る。Pod ごとの待ち受けの管理。rproxy-api の大きな変更 |
| D. サイドカー | Pod ごとに rproxy を注入（webhook と init の iptables） | 実績のある形。宛先は `SO_ORIGINAL_DST` | Pod ごとの資源。webhook が止まると Pod が作れない。Pod の入れ替えが要る。目的でないものにした |
| E. rproxy を変えない（`local` の経路と `listen_freebind`） | 印を付けた ClusterIP 宛てをノードで受けるようにし、`listen_freebind` で ClusterIP:port に bind した待ち受けで受ける | rproxy の変更なし | 応答の送信元（ClusterIP）がノードのアドレスでないので、`IP_TRANSPARENT` か `FLOWI_FLAG_ANYSRC` なしでは応答の経路が引けない見込み。ClusterIP をノードの local の表に足す形はホストの通信を壊しうる。段階 1 で確かめるだけにする |

**A を選ぶ理由**

- core に要るのは「Pod から ClusterIP」を取ることだけで、A はノードの netns の規則と rproxy の小さな追加だけで足りる。
- 規則は「透過の待ち受けがあるときだけ当たる」形にでき、rproxy が止まれば何もしなくても kube-proxy の道に戻る（5.9）。目的の「壊れても止めない」に一番近い。
- rproxy に要る追加（`listen_transparent`）は `listen_freebind` と同じ大きさ（ソケットの選択肢 1 つ）。B・C は rproxy の形を変える。
- 確かめられる：kind の既定（kindnet + kube-proxy の iptables モード）と `kubeProxyMode: nftables` の両方で CI に載せられる。
- 弱いところ（CNI の相性）は、対応する組み合わせを決めて表にし（5.8）、外れるクラスタでは入れても動かない（通信は今までどおり）ようにする。

### 5.3 nftables の規則

mesh-agent が書く。表は `inet rproxy_mesh` の 1 つだけで、ほかの表には触らない。下は形の見本で、正確な書き方と、ここに書いた動きの前提（★）は段階 1 で確かめる。

```
table inet rproxy_mesh {
  set svc4 { type ipv4_addr . inet_service; }        # ルートの付いた ClusterIP:port（rproxy のルールから）
  chain pre {
    type filter hook prerouting priority -310;       # conntrack（-200）と iptables の raw（-300）より前
    fib saddr type local return                      # ノード自身の通信は取らない（OUTPUT は通らないが念のため）
    # 1. 取り込んだ接続の続き：rproxy の透過のソケットがあれば、印を付けて conntrack を飛ばす
    meta l4proto tcp socket transparent 1 meta mark set 0x520 notrack accept
    # 2. 新しい接続（SYN だけ）：透過の待ち受けがあれば TPROXY。なければこの規則は当たらない（★）
    tcp flags & (syn | ack) == syn ip daddr . tcp dport @svc4 tproxy ip to ip daddr : tcp dport meta mark set 0x520 notrack accept
  }
  chain out {
    type filter hook output priority -310;
    ip saddr . tcp sport @svc4 notrack               # rproxy の応答（送信元が ClusterIP）も conntrack に入れない
  }
}
# 印の付いたものはノードで受ける（TPROXY のいつもの形）
ip rule add fwmark 0x520/0xfff lookup 520
ip route add local 0.0.0.0/0 dev lo table 520
```

決めごと：

- **取るのはルートの付いた ClusterIP:port だけ**。ルートのない Service・ポートは規則に当たらず、kube-proxy のまま（GEP-1294 の「ルートがなければ今までどおり」）。影響の範囲を Service のポートの単位に絞る。
- **新しい接続は SYN だけ**を TPROXY に回す。rproxy が止まっている間に kube-proxy の道でつながった接続は、rproxy が戻っても SYN を送らないので規則 2 に当たらず、規則 1 にも当たらない（rproxy のソケットがない）ので、kube-proxy の conntrack のまま続く。SYN 以外も TPROXY に回すと、rproxy の待ち受けがその接続の途中のパケットを受けて RST を返してしまう（★）。
- **`notrack`**：kube-proxy の DNAT は conntrack の上で動くので、conntrack に入れなければ DNAT されない。nftables の `accept` はほかの表の後の chain を止めないので、`accept` だけでは飛ばせない。
- **TPROXY の宛先**：元の宛先のアドレスとポートのまま（`ip daddr : tcp dport`）。rproxy は ClusterIP:port ごとのルールで、そのアドレスで待ち受けている。アドレスを省くと TPROXY は入ってきたインタフェースのアドレスで待ち受けを探す（★ nft で宛先を式で書けるか。書けなければ要素ごとの規則にする）。
- **印**：`0x520/0xfff`（kube-proxy の `0x4000`・`0x8000`、Calico の `0xffff0000` と重ならない下位のビット）。chart の値で変えられるようにする。Cilium など下位のビットを使う CNI とは重なりうる（7.）。
- **ノード自身の通信**（hostNetwork の Pod、kubelet）は OUTPUT を通るので取らない。rproxy から backend への接続も OUTPUT で、宛先は Pod の IP なので取らない（ループしない）。
- **UDP は取らない**。HTTP/3 も取らない。
- IPv6 は同じ形で `svc6` を足す（段階の後のほう）。

### 5.4 rproxy に要るもの（rproxy-api）

**`listen_transparent`**（新しい。名前は仮）：ルールの待ち受けのソケットに `IP_TRANSPARENT` / `IPV6_TRANSPARENT` を付ける。

- 要る理由：TPROXY は `IP_TRANSPARENT` の付いたソケットにしかパケットを渡さない（付いていなければ規則が当たらない）。今の rproxy の `IP_TRANSPARENT` は転送先への接続の側（`source_ip: transparent`、`src/net/source.rs`）だけで、待ち受けは `IP_FREEBIND` だけ（`src/net/listen.rs`）。
- 形：`listen_freebind` と同じ（ルールごとの bool。PATCH では変えない。`features.listen_transparent`。Linux だけ）。`CAP_NET_ADMIN` が要る（`GET /capabilities` の `transparent` と同じ確かめ方）。`IP_TRANSPARENT` のソケットはホストにないアドレスにも bind できるので、`listen_freebind` と一緒に要るかは段階 2 で確かめる。
- 受けた接続の自分のアドレスが元の宛先（ClusterIP:port）になる。ルールが ClusterIP:port ごとなので、rproxy は今の「待ち受けのキーでルールを選ぶ」動きのままでよい。元の宛先で選ぶ新しい仕組み（11.5 で大きいと書いたもの）は要らない。
- 確かめ方：rproxy-api の `scripts/test-freebind.sh` と同じ名前空間の試験。
- 扱い：rproxy-api を直すのはこの段階の外（オーナーの了解が要る）。rproxy-api にも影のブランチを作るか、issue にするかは未決（7.）。それまでの段階 1 は rproxy を使わずに確かめる。

**ほかの機能は今の rproxy のまま**：`http` のルール、`ClientIP` の一致、ルールセット、`labels`、`outlier_detection`、`/readyz`、`graceful_shutdown`。

**選ばなかった形**：1 つのワイルドカードの透過の待ち受け（例 `0.0.0.0:15001`）で受けて、元の宛先で仮想のルールを選ぶ形（Envoy の `original_dst` のリスナー）。ソケットは 1 つで済むが、rproxy の「待ち受けのキー = ルール」の形を変える大きな変更になる。ルートの付いた Service のポートの数は小さいので、ルールごとの待ち受けで足りる。

### 5.5 Service に付いたルートを rproxy のルールにする

| Gateway API | rproxy（ルールセット `k8s/mesh`） |
|---|---|
| ルートの付いた Service の ClusterIP:port（`spec.clusterIPs` の IPv4） | `http` のルール 1 つ。`listen_addr` は ClusterIP、`listen_port` は Service の `port`、`listen_transparent: true` |
| `parentRef.port` なし | Service の HTTP のポートすべて：`appProtocol` が `http`・`kubernetes.io/h2c`・`kubernetes.io/ws`・`kubernetes.io/wss` でないポートは取らない（TLS や TCP のクライアントを HTTP として受けると壊すため）。`appProtocol` のないポートを取るかは未決（7.）。conformance の `echo` の 80・8080 は `http`、7070 は `kubernetes.io/h2c` |
| `parentRef.port` あり | そのポートだけ（`appProtocol` によらず。書いた人が HTTP と言っている） |
| `hostnames` | 使わない（GEP-1294）。規則に Host の一致を足さない |
| HTTPRoute の規則 | 今の `render/http.rs` と同じ（一致、優先の順、フィルタ、重み、ミラー、retry、タイムアウト）。リダイレクトのポートの既定は Service の `port` |
| GRPCRoute | 今と同じ（メソッドの一致をパスに、転送先は h2c）。同じ Service とポートに HTTPRoute もあれば GRPCRoute が勝ち、HTTPRoute は `Accepted: False`（`Conflicted`）。Gateway の側は同じルールに混ぜているので、ここだけ違う |
| どの規則にも合わない要求 | 404（Gateway の側と同じ） |
| backendRef | EndpointSlice の Pod の IP（今と同じ）。ReferenceGrant は見ない |
| headless・ExternalName・ClusterIP のない Service を親に | ルールを作らない。ルートの状態は `Accepted: False`（理由は未決） |
| ルートの状態 | `status.parents[]` に Service の親の項目。`controllerName` は `rproxy.max3584.net/mesh-controller`（Gateway の側と分ける）。rproxy のルールの `conditions` から `Accepted`・`ResolvedRefs` |
| rproxy の外の Service | rproxy は Kubernetes を知らないまま。ClusterIP は数字で渡す |

ルールセットはすべての Mesh の rproxy に同じものを PUT する（fleet と同じ）。Service が消えたら、ルールも次の反映で消す。

### 5.6 producer と consumer

- **producer**（ルートと Service が同じ namespace）：5.5 のとおり、どのクライアントにも当てる。
- **consumer**（違う namespace。extended の `MeshConsumerRoute`）：段階 6。rproxy の `match` の `ClientIP` を使う。TPROXY で受けた接続の相手のアドレスはクライアントの Pod の IP のまま（kube-proxy の SNAT を通らない）。
  - consumer の namespace ごとに、その namespace の Pod の IP をまとめた `ClientIP(...)` を規則に足し、producer のルートより上の優先の帯に置く。
  - その帯の最後に、同じ `ClientIP(...)` で 404 を返す規則を置く（その namespace のクライアントには producer のルートを使わない）。
  - Mesh の rproxy には `global.trusted_proxies` を設定しない（`ClientIP` が `X-Forwarded-For` を読まないように）。
  - Pod の IP は変わりやすいので、PUT が増える。ノードの rproxy に来るのはそのノードの Pod だけなので、ノードごとに組を分けて小さくする形も考える（7.）。
- Service の「フロントエンドとバックエンド」：取るのは ClusterIP だけなので、Pod の IP に直接送った通信にはルートが当たらない（`MeshFrontend`）。rproxy から backend への接続（Pod の IP 宛て）にも当たらないので、ルートは 1 段だけ効く（GEP-1294 の「最初のルートだけ」）。

### 5.7 部品と権限

| 部品 | 中身 |
|---|---|
| コントローラ（`--mesh`、既定オフ） | Service を親にした HTTPRoute・GRPCRoute と Service・EndpointSlice を見て `k8s/mesh` を描き、Mesh の rproxy の Pod すべてに PUT する。ルートの状態を書く。Gateway の側（managed・fleet）とは別の流れにし、Mesh を切っていれば今の動きは 1 つも変わらない |
| DaemonSet `rproxy-mesh`（chart の `mesh.enabled`、既定 `false`） | hostNetwork。コンテナは 2 つ：rproxy（`CAP_NET_ADMIN`：`IP_TRANSPARENT`）と mesh-agent（`CAP_NET_ADMIN`：nftables と `ip rule`）。readiness gate は fleet と同じ（`k8s/mesh` を反映してから Ready） |
| mesh-agent（`rproxy-gateway mesh-agent`、新しいサブコマンド） | 同じ Pod の rproxy の制御 API（127.0.0.1、読むだけのトークン）から `k8s/mesh` を読み、`listen_transparent` のルールのうち `Programmed: True` のものの (アドレス, ポート) を `svc4` にする。1 秒ごとに `/readyz` を見る。nftables は `nft -f` で表ごと置き換える（イメージに `nftables` を入れる）。Kubernetes の API は使わない（「rproxy の Pod は Kubernetes の API を使わない」の線を守る） |
| ルールセットの名前 | `k8s/mesh`（コントローラのトークンの `allow_rulesets: [k8s/]` の内。Gateway の組は `k8s/<ns>/<name>` なのでぶつからない） |
| トークン | コントローラは今のマスタートークンから導いたもの。mesh-agent には `rules:read` だけのトークン |
| XMesh | 段階 6 で `XMesh` `rproxy` を作り、`status.supportedFeatures` に名乗る機能を書く（suite の `--mesh-name` で読める）。それまでは `--supported-features` で渡す |

### 5.8 kube-proxy・CNI と並ぶ

取るのは conntrack より前（priority -310）なので、kube-proxy の DNAT にも、iptables の raw（-300）にも先んじる。どの組み合わせで動くかは、CI で確かめたものだけを書く。

| 組み合わせ | 見込み | 確かめ方 |
|---|---|---|
| kindnet + kube-proxy（iptables） | 動く見込み。kube-proxy の filter の INPUT の「endpoint のない Service を拒む」規則は `ctstate NEW` を見るので、`notrack`（UNTRACKED）のパケットには当たらない見込み（★） | CI（段階 1・5） |
| kindnet + kube-proxy（nftables） | 動く見込み。kube-proxy の `filter-prerouting`（-110）の `service-endpoints-check`・`cluster-ips-check` は、endpoint のない Service と知らないポートだけを拒む（★） | CI（kind の `kubeProxyMode: nftables`） |
| kube-proxy（IPVS） | 未確認。IPVS は ClusterIP を `kube-ipvs0` に置き、LOCAL_IN で振り分ける。TPROXY で割り当てたソケットとどちらが勝つか分からない | 対象外にして始める |
| Calico（iptables のデータプレーン） | 未確認。Calico の raw の規則と印のビット | 後で |
| Cilium の kube-proxy の代わり（socket LB）・Calico の eBPF | **動かない**。ClusterIP は Pod の中の `connect()` か veth の tc で Pod の IP に変わるので、ノードの netfilter には ClusterIP が見えない。規則に当たらず、通信は今までどおり（壊れはしないが、ルートが効かない） | 対象外。mesh-agent が「`svc4` の宛先が来ない」ことを見つけて知らせられるかは未決 |
| NetworkPolicy | **意味が変わる**。取り込んだ接続はノードで受けて（INPUT）、rproxy からノードのアドレスで backend に出る。backend の Pod の ingress の NetworkPolicy はクライアントの Pod ではなくノードからの接続を見る（多くの実装はノードからを通す）。クライアントの egress の NetworkPolicy も FORWARD を通らなくなる | 文書に書き、段階の後のほうで調べる |

### 5.9 障害のときの動き

前提：規則 1・2 は「rproxy の透過のソケットがあるときだけ」当たる。ソケットがなければ、規則は何もしない（印も `notrack` も付かない）ので kube-proxy の道に戻る（★ 段階 1 で確かめる一番大事な点）。

| 出来事 | 新しい接続 | 今の接続 |
|---|---|---|
| rproxy のプロセスが落ちる | 待ち受けが消えるので規則 2 が当たらず、kube-proxy の道（mesh-agent がいなくても） | rproxy の側の接続は切れる。クライアントがつなぎ直せば kube-proxy の道 |
| rproxy は動くが固まる・`/readyz` が 503 | mesh-agent が 1 秒ごとに見て、2 回続けて失敗したら表を消す（約 2〜3 秒で kube-proxy の道） | 固まった rproxy の接続は戻らない |
| rproxy が再起動して組がまだない | ルールがないので待ち受けもなく、kube-proxy の道。コントローラが PUT すると取り込みが始まる | kube-proxy の道の接続はそのまま（SYN だけを取るので） |
| mesh-agent が落ちる | rproxy が動いていれば今の規則のまま動く。新しいルートは規則に入らない（そのポートは kube-proxy の道） | 変わらない |
| Pod ごと消える（rollout・drain） | 待ち受けが消えるので kube-proxy の道。mesh-agent は SIGTERM で表と `ip rule` を消す。消せずに残っても、ソケットがないので当たらない | rproxy の `graceful_shutdown` の間は続き、その後切れる |
| コントローラが止まる | 最後に PUT した組のまま。EndpointSlice の変化が届かない（受け身のヘルスチェックで外れる分だけ助かる） | 変わらない |
| ClusterIP が別の Service に使い回される | 古いルールが消えるまで（次の反映まで）、新しい Service への通信を古いルートで振り分けうる | — |
| Mesh を切る（`mesh.enabled=false`） | DaemonSet が消え、表も消える。kube-proxy の道 | rproxy の接続は切れる |

## 6. CI での確かめ方

- 影のブランチの CI は、`exp/mesh` への push で動く別のワークフロー（`.github/workflows/mesh.yml`、`on: push: branches: [exp/mesh]`）に置く。`main` の e2e・conformance・必須のチェックは触らない。ワークフローのファイルも `main` には入れない。
- kind のノードは 1 つで始め、複数ノード（クライアントと backend が別のノード）を後で足す。kube-proxy は iptables と nftables の 2 通り（kind の `networking.kubeProxyMode`）。
- rproxy は rproxy-api の ref（`RPROXY_REF`）からビルドする（今の e2e と同じ）。`listen_transparent` が入るまでは段階 1 の試験だけ。
- Mesh の conformance：`scripts/conformance.sh` に Mesh の走り方を足す（Gateway の走り方とは別の実行）。

```bash
go test ./conformance -run TestConformance -count=1 -v -args \
  --conformance-profiles=MESH-HTTP,MESH-GRPC \
  --supported-features=Mesh,HTTPRoute,GRPCRoute \
  --report-output=mesh-report.yaml
```

  - `--supported-features` に `Gateway` を入れないので、suite は Gateway の準備をしない（GatewayClass も要らない）。
  - Mesh の機能は GatewayClass の `supportedFeatures` に書かない（suite が捨てる）。
  - CI は core が 1 つでも落ちたら失敗にする。extended は知らせるだけ（今の conformance のジョブと同じ）。
- conformance のほかに、mesh の e2e（`scripts/mesh-e2e.sh`）で次を確かめる：
  - rproxy を止める（`kill`）・Pod を消す・mesh-agent を止める間も、クライアントの Pod から Service への要求が 200（kube-proxy の道に戻る）。
  - rproxy が止まっている間につないだ長い接続が、rproxy が戻っても切れない。
  - ルートのない Service・ポートの通信が rproxy を通らない（rproxy のアクセスログに出ない）。
  - Mesh を切った（chart の既定の）入れ方で、nftables に `rproxy_mesh` の表がなく、`ip rule` も増えていない。

## 7. 危うい点と未決

**危うい点**

- **CNI との相性**：Istio ambient はホストの netns で取る形を CNI との相性で捨てた。この設計は同じ場所で取る。対応する組み合わせを CI で確かめたものに絞り、外れたクラスタでは「入れても効かない（壊さない）」に留める。
- **★の前提**（5.3・5.9）が外れると形が変わる：TPROXY を conntrack より前（priority -310）で使えるか、規則の中で TPROXY の後の文（印・`notrack`）が「ソケットがあったときだけ」動くか、`socket transparent` がその priority で使えるか、`tproxy to ip daddr : tcp dport` と書けるか、`notrack` の SYN を kube-proxy の規則が拒まないか。段階 1 で 1 つずつ確かめ、外れたら案 A の中で直す（要素ごとの規則、mangle の priority と conntrack の印など）か、案 B・C を考え直す。
- **NetworkPolicy の意味が変わる**（5.8）。メッシュとして使うなら避けられない。
- **障害の範囲**：今の rproxy-gateway は Gateway の通信だけに入る。Mesh はクラスタの中の通信の道に入る。ルートの付いた Service のポートだけに絞るが、そのポートでは rproxy の不具合がそのまま見える。
- **権限**：`CAP_NET_ADMIN` の hostNetwork の Pod がすべてのノードに要る（今の fleet は `NET_BIND_SERVICE` だけ）。docs/SECURITY.md の線を引き直す。
- ClusterIP の使い回し（5.9）と、コントローラが止まったときの古い Endpoint。

**未決**

1. rproxy-api の `listen_transparent` をどう進めるか：rproxy-api にも影のブランチ（`exp/mesh`）を作るか、issue にするか。オーナーに聞く（この段階では rproxy-api に触らない）。
2. `appProtocol` のない Service のポートを、`parentRef.port` なしのルートで取るか（取らない案で始める）。
3. クライアントの側をどこまで取るか：今の案は「すべての Pod」（producer のルートはどのクライアントにも当てる、の決まりどおり）。namespace のラベルで選ぶ形（suite の `--namespace-labels` で試験の namespace に付けられる）を足すなら、mesh-agent が Kubernetes の API を使わずに Pod の IP を知る方法（コントローラが組に入れる、など）が要る。
4. consumer のルートの `ClientIP` の大きさと、ノードごとに組を分けるか。
5. headless などを親にしたルートの状態の理由。
6. 印のビットの既定（`0x520/0xfff`）が、よく使う CNI とぶつからないか。
7. Cilium の socket LB のクラスタで「効いていない」ことをどう知らせるか。
8. Mesh のルートの `controllerName` と XMesh の名前。ほかのメッシュと同じクラスタに入れたときの取り合い（両方が Service の親のルートを拾う）。
9. backend に見える接続元はノードのアドレスになる。クライアントの IP を渡す（`X-Forwarded-For` は今も付く）以上のことが要るか。

## 8. 段階

どの段階も小さくし、終わりの条件を CI で確かめられるものにする。

| 段階 | 中身 | 終わりの条件 |
|---|---|---|
| 0 | この文書 | `exp/mesh` に入っている |
| 1 | 取り込みの試し（rproxy を使わない）。kind の上で、hostNetwork の Pod の `socat`（透過の待ち受け）と 5.3 の規則を手で当てるスクリプト（`scripts/mesh-spike.sh`）と `mesh.yml` | iptables と nftables の kube-proxy の両方で：ルートの付いた ClusterIP:port への Pod からの接続が `socat` に届き、相手のアドレスがクライアントの Pod の IP。`socat` を止めると kube-proxy の道で 200。止めている間の長い接続が `socat` を戻しても切れない。ほかの Service に影響がない。★の前提をすべて書き留める |
| 2 | rproxy-api の `listen_transparent`（未決 1 の答えのあとで） | rproxy-api の名前空間の試験。段階 1 の `socat` を rproxy の `http` のルールに置き換えて同じ確かめが通る |
| 3 | コントローラの描画：Service を親にした HTTPRoute・GRPCRoute（producer だけ）→ `k8s/mesh`。`rproxy-gateway render` で出す。ルートの状態 | 単体テスト（conformance の Mesh のマニフェストを fixtures にして、描いた組を確かめる）。本物の rproxy が組を受け付ける（`tests/rproxy.rs`）。Mesh を切ったときの描画が今と 1 バイトも変わらない |
| 4 | chart の `mesh.enabled`、DaemonSet `rproxy-mesh`、`mesh-agent`、コントローラの `--mesh` | mesh の e2e（6.）：`MeshBasic`・`MeshTrafficSplit` と同じ要求が通る。障害の確かめ（rproxy の kill、Pod の削除、agent の停止）がすべて 200。既定の入れ方で何も増えない |
| 5 | Mesh の conformance を `mesh.yml` に | MESH-HTTP の core 7 / 7、MESH-GRPC の core 2 / 2（iptables と nftables の kube-proxy の両方） |
| 6 | extended：リダイレクト・書き換え・名前つきの規則・クエリ・backend のヘッダ（今の描画で足りる見込み）、`MeshPorts`・`MeshFrontend`（数えないが当たり前の動き）、consumer のルート、XMesh | MESH-HTTP の extended を名乗った分だけ通る |
| 後で（決めていない） | 複数ノード、IPv6、クライアントの namespace の選択、NetworkPolicy との関わり、ほかの CNI | — |

段階 1 で★の前提が崩れたら、そこで止めてこの文書を直し、オーナーに報告する。
