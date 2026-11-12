English: [en/GUIDE.md](en/GUIDE.md)

# rproxy-gateway 設定説明書

この説明書は、rproxy-gateway を使って Kubernetes に Gateway を作り、外からの通信をアプリへ流し、それを運用していくための手引きです。Kubernetes の基本（Deployment・Service・namespace・Helm）は知っているけれど、rproxy は初めて、という PaaS の運用者を読み手に想定しています。やりたいことごとに、そのまま貼って使える YAML と、うまくいったかを確かめるコマンドを載せています。

この説明書では、どの実装でも同じ意味になる**標準の Gateway API** と、rproxy-gateway でしか使えない **rproxy だけの設定**を分けて書いています。どこが標準でどこが rproxy 固有かの地図は、[標準の Gateway API と rproxy だけの設定](#標準の-gateway-api-と-rproxy-だけの設定) にあります。

関連する文書は次のとおりです。

- なぜそう作ったかという設計の詳しいところは [DESIGN.md](DESIGN.md) と [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) にあります。
- 複数のチーム（テナント）で 1 つのクラスタを分け合うときの考え方は [SECURITY.md](SECURITY.md) にあります。
- Gateway に外から届くアドレスを用意する方法（MetalLB など）は [PLATFORM.md](PLATFORM.md) にあります。
- Ingress や Traefik から移るときの手順は [MIGRATION.md](MIGRATION.md) にあります。

例の中の名前（`app.example.com`、namespace `default`、Service `app`）とアドレス（`192.0.2.0/24`・`203.0.113.0/24`）は、文書のために予約された値です。実際に使うときは自分の環境のものに置き換えてください。

例はこの版（v0.4.22、同梱の rproxy も v0.4.22）で確かめています。YAML はすべて CRD のスキーマに照らして確かめ、`rproxy-gateway render`（[15.3](#153-クラスタなしで確かめる)）で rproxy のルールに変換できることも確かめています。

## 目次

[標準の Gateway API と rproxy だけの設定](#標準の-gateway-api-と-rproxy-だけの設定)（この説明書の地図）

**第 1 部 はじめに**

0. [Gateway API のあらまし](#0-gateway-api-のあらまし)
1. [入れる](#1-入れる)

**第 2 部 標準の Gateway API で書く**（ほかの実装でも同じ）

2. [はじめての Gateway と HTTPRoute](#2-はじめての-gateway-と-httproute)
3. [HTTPS](#3-https)
4. [HTTP の振り分け](#4-http-の振り分け)
5. [L4 の転送](#5-l4-の転送)
6. [ほかの namespace](#6-ほかの-namespace)
7. [Gateway API のそのほかの機能](#7-gateway-api-のそのほかの機能)

**第 3 部 rproxy だけの設定**（この実装の拡張）

8. [rproxy の CRD](#8-rproxy-の-crd)
9. [managed と fleet](#9-managed-と-fleet)
10. [動いているときに変えると](#10-動いているときに変えると)
11. [rproxy との話し方](#11-rproxy-との話し方)
12. [セキュリティの基本](#12-セキュリティの基本)

**第 4 部 運用と困ったとき**

13. [運用](#13-運用)
14. [レシピ](#14-レシピ)
15. [困ったとき](#15-困ったとき)

v0.4.22 までの版のこの説明書から章の番号が変わりました。もとの 7.5〜7.8（リスナーのフィルタ・BackendTLSPolicy・ClusterTrustBundle・アドレスの到達範囲）は 7.1〜7.4 に、もとの 7.1〜7.4（rproxy の CRD）は 8.1〜8.4 に、もとの 8.〜14. は 9.〜15. になっています。

## 標準の Gateway API と rproxy だけの設定

この説明書には、2 種類の設定が出てきます。どの Gateway API の実装でも同じ意味を持つ**標準の Gateway API** と、rproxy-gateway でしか使えない **rproxy だけの設定**です。別の実装へ移るときにそのまま持っていけるのは前者だけなので、この説明書では二つをはっきり分けています。

本文では、次の印で区別します。

- **印なし**：標準の Gateway API（standard channel の機能）です。同じ YAML がほかの実装でも同じ意味になります。ただし、どの機能に対応しているかは実装ごとに違うので、移る先の実装の対応表（conformance の結果）は確かめてください。
- **Gateway API experimental**：標準の Gateway API の一部ですが、まだ experimental channel にある（GA になっていない）機能です。experimental の CRD が要り、これからの版で形が変わることがあります。
- **rproxy 固有**：rproxy-gateway の拡張や、rproxy ならではの動きです。ほかの実装には持っていけません。rproxy の CRD、chart の値、managed と fleet、rproxy の変更の当て方（`in_place`・`recreate`）、readiness gate、Gateway ごとの NetworkPolicy などがこれにあたります。Gateway API が「実装ごと（Implementation-specific）」としている書き方のうち rproxy が受け付けるもの（ClusterTrustBundle を CA に指すなど）は、「**rproxy 固有（実装ごと）**」と書きます。

**標準の Gateway API でできること（ほかの実装と同じ）**

| やりたいこと | 節 |
|---|---|
| Gateway とルートを作り、状態を確かめる | [2.2](#22-gateway-と-httproute)・[2.4](#24-確かめる) |
| HTTPS（証明書の Secret、HTTP から HTTPS へのリダイレクト） | [3.1](#31-tls-の-secret-を-certificaterefs-で)・[3.3](#33-http-から-https-へリダイレクト) |
| ホスト名・パス・ヘッダでの振り分け、重み、フィルタ、タイムアウト | [4.1](#41-ホスト名)〜[4.5](#45-retry-とタイムアウト) |
| gRPC の振り分け | [4.6](#46-grpcroute) |
| TCP・UDP・TLS のまま転送する | [5.](#5-l4-の転送) |
| namespace をまたいで使う（allowedRoutes・ReferenceGrant・ListenerSet） | [6.](#6-ほかの-namespace) |
| 転送先への TLS、クライアント証明書の確かめ（CA を ClusterTrustBundle で指すのは **rproxy 固有（実装ごと）**。Core は ConfigMap だけ） | [7.1](#71-backendtlspolicy)・[7.2](#72-clustertrustbundle-とクライアント証明書の確かめ) |
| HTTPRoute の `retry`・`ExternalAuth`、リスナーのフィルタ、アドレスの到達範囲（**Gateway API experimental**） | [4.5](#45-retry-とタイムアウト)・[7.3](#73-リスナーのフィルタ)・[7.4](#74-アドレスの到達範囲) |

**rproxy だけの設定（この実装の拡張）**

| 何か | なぜあるか（標準で書けないこと） | 節 |
|---|---|---|
| chart の値 | rproxy-gateway の入れ方と、すべての Gateway の既定 | [1.2](#12-helm-install)・[1.3](#13-主な-chart-の値) |
| managed が作るもの | Gateway ごとの rproxy の Deployment・Service など | [2.3](#23-何ができるか) |
| `RproxyPolicy` | 接続元のアドレスの制限、接続数・帯域の上限、L4 の時間、受け身のヘルスチェック。標準の Gateway API には項目がありません | [8.1](#81-rproxypolicy) |
| `RproxyMiddleware` | 流量の制限、アドレスでの許可、セキュリティのヘッダ、`forward_auth` など、標準のフィルタにない処理。標準の拡張点 `ExtensionRef` から使います | [8.2](#82-rproxymiddleware) |
| `RproxyRule` | クラスタの外への転送など、Gateway API では書けない rproxy のルール | [8.3](#83-rproxyrule) |
| `RproxyGatewayParameters` | rproxy の Pod と Service の形。標準の `parametersRef` が指す中身です | [8.4](#84-rproxygatewayparameters) |
| managed と fleet | rproxy の Pod の置き方 | [9.](#9-managed-と-fleet) |
| 変更の当て方（`in_place`・`recreate`）、readiness gate、止まり方 | 変更や Pod の入れ替えで、今の接続がどうなるか | [10.](#10-動いているときに変えると) |
| 制御 API、トークン、NetworkPolicy | コントローラと rproxy のつながり方と守り方 | [11.](#11-rproxy-との話し方)・[12.](#12-セキュリティの基本) |

第 4 部（運用・レシピ・困ったとき）は両方にまたがります。レシピでは、rproxy 固有の部分に印を付けています。

## 第 1 部 はじめに

第 1 部では、Gateway API の全体像（0.）と、rproxy-gateway の入れ方（1.）を説明します。1.1 の Gateway API の CRD は標準のものですが、1.2・1.3 の Helm の chart とその値は rproxy-gateway のものです。

## 0. Gateway API のあらまし

この章では、rproxy-gateway が読み取る Kubernetes の Gateway API の全体像を説明します。Gateway API をすでに使ったことがあれば、最後の段落だけ読めば足ります。

Gateway API は、「クラスタの外から来た通信を、どの Service へ送るか」を書くための Kubernetes の標準 API です。Ingress の後継にあたり、書く人の役割ごとにリソースが分かれているのが特徴です。

| リソース | 書く人 | 何を書くか |
|---|---|---|
| `GatewayClass` | クラスタの管理者（rproxy-gateway では chart が `rproxy` という名前で作ります） | この種類の Gateway をどのコントローラが受け持つか |
| `Gateway` | Gateway の持ち主（インフラの担当者） | 待ち受け（リスナー）の一覧。リスナーごとにポート・プロトコル・ホスト名・証明書を書きます |
| `HTTPRoute`・`GRPCRoute`・`TLSRoute`・`TCPRoute`・`UDPRoute` | アプリの持ち主 | どのリクエストや接続を、どの Service へ送るか |
| `ReferenceGrant` | 参照される側の namespace の持ち主 | ほかの namespace から、自分の namespace の Service や Secret を使ってよいという許可 |

通信の流れは次のようになります。

```
クライアント ──▶ Gateway のリスナー（:80 / :443）──▶ ルート（ホスト名・パス）──▶ Service の Pod
                  └─ rproxy の Pod（Gateway ごと）が待ち受ける
```

rproxy-gateway は、これらのリソースを読んで rproxy（実際に通信を中継するプロキシ）のルールに変換し、Gateway ごとの rproxy に送るコントローラです。rproxy 自身は Kubernetes のことを何も知らず、rproxy-gateway から受け取ったルールに従って動くだけです（詳しくは [11.](#11-rproxy-との話し方)）。

## 1. 入れる

この章では、rproxy-gateway をクラスタに入れる手順と、入れるときに決めておく chart の値を説明します。Gateway API の CRD、rproxy-gateway の chart の順に入れます。

### 1.1 Gateway API の CRD

Gateway API のリソース（Gateway や HTTPRoute など）の定義である CRD は、rproxy-gateway の chart には入っていません。クラスタに 1 回だけ、先に入れておきます。

Gateway API の CRD には、配られ方の違う 2 つの channel があります。

| channel | 入るもの（v1.6.3） |
|---|---|
| standard | GatewayClass・Gateway・HTTPRoute・GRPCRoute・TLSRoute・TCPRoute・UDPRoute・ListenerSet・ReferenceGrant・BackendTLSPolicy |
| experimental | standard のすべてに加えて、まだ実験中の項目 |

ふつうは standard で足ります。次の機能を使うときだけ experimental の CRD が要ります。

| 機能 | 要る CRD |
|---|---|
| HTTPRoute の `retry`（[4.5](#45-retry-とタイムアウト)） | v1.6 では experimental。v1.7 からは standard に入っています |
| HTTPRoute の `ExternalAuth` フィルタ | experimental |
| リスナーのフィルタ（`filters.requests`、[7.3](#73-リスナーのフィルタ)） | v1.7 の experimental |
| `spec.addresses[].routability`（[7.4](#74-アドレスの到達範囲)） | v1.7 の experimental |

どちらか一方を入れます。

```bash
# standard（ふつうはこれで足ります）
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml
# experimental（上の表の機能を使うとき。v1.7 の機能を使うなら v1.7.0 の experimental-install.yaml）
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/experimental-install.yaml
```

Gateway API v1.6 を使うには Kubernetes 1.32 以上が要ります。Kubernetes と Gateway API の版の組み合わせは、[README](../README.md) の「対応する版」に一覧があります。

入ったことは次のコマンドで確かめられます。`gateway.networking.k8s.io` の CRD が並べば入っています。

```bash
kubectl get crd | grep gateway.networking.k8s.io
```

### 1.2 helm install

> **rproxy 固有**：ここからは rproxy-gateway 自身の入れ方です。

次に rproxy-gateway 本体を Helm で入れます。

```bash
helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway \
  -n rproxy-gateway-system --create-namespace -f values.yaml
```

`-f values.yaml` は省いてもかまいません。既定の値のままでも動きます（値を変えたいときは [1.3](#13-主な-chart-の値) を見てください）。chart が入れるのは、rproxy-gateway 自身の CRD（`rproxy.max3584.net`）、RBAC、コントローラ（2 レプリカ）、そして GatewayClass `rproxy` です。

入ったら、コントローラの Pod と GatewayClass を確かめます。

```bash
kubectl -n rproxy-gateway-system get pods
# NAME                              READY   STATUS    RESTARTS   AGE
# rproxy-gateway-6c9d8f7b5d-4xk2p   1/1     Running   0          1m
# rproxy-gateway-6c9d8f7b5d-9qm7r   1/1     Running   0          1m
kubectl get gatewayclass rproxy
# NAME     CONTROLLER                              ACCEPTED   AGE
# rproxy   rproxy.max3584.net/gateway-controller   True       1m
```

GatewayClass の `ACCEPTED` が `True` なら、コントローラがこの GatewayClass を受け持っています。コントローラが対応していると名乗っている Gateway API の機能の一覧は、`kubectl get gatewayclass rproxy -o jsonpath='{.status.supportedFeatures[*].name}'` で見られます。

Helm を使わずに入れたいとき（`install.yaml` や Kustomize）は、[README](../README.md) の「Helm を使わずに入れる」を見てください。

### 1.3 主な chart の値

> **rproxy 固有**：chart の値は rproxy-gateway のものです。標準の Gateway API には、実装の入れ方や既定を決める項目はありません。Gateway ごとに変えたいものは、標準の入り口 `spec.infrastructure.parametersRef` から RproxyGatewayParameters（[8.4](#84-rproxygatewayparameters)）で変えられます。

ここでは、入れるときに決めておくことの多い chart の値を、役割ごとにまとめて説明します。どの値も既定のままで動くので、変えたいものだけを `values.yaml` に書けば足ります。ここに載せていない値も含めた全部は、[values.yaml](../charts/rproxy-gateway/values.yaml) にコメント付きで載っています。

説明の前に、2 つの言葉を押さえておきます。

- **managed**（既定の動かし方）：Gateway を 1 つ作るたびに、コントローラがその Gateway 専用の rproxy の Deployment と Service を、Gateway と同じ namespace に作ります。`managed.*` の値は、この Gateway ごとの rproxy の形を決めます。
- **fleet**：chart が入れた 1 組の rproxy（DaemonSet）が、すべての Gateway をまとめて受け持つ動かし方です。`fleet.enabled: true` で切り替えます。違いと選び方は [9.](#9-managed-と-fleet) で説明します。

**コントローラ**

| 値 | 既定 | 何を決めるか |
|---|---|---|
| `controller.replicas` | `2` | コントローラの Pod の数 |
| `controller.watchNamespaces` | `[]`（すべての namespace） | コントローラが見る namespace |
| `controller.crossNamespaceSecrets` | `true` | ほかの namespace の証明書の Secret を使えるか |

- `controller.replicas`：コントローラは何個動かしても、Lease（Kubernetes のロックのしくみ）で選ばれた 1 つ（リーダー）だけが rproxy に変更を送ります。ほかは控えとして待ち、リーダーが止まると引き継ぎます。2 つあれば、コントローラの Pod が入れ替わるときも、控えがすぐに引き継げます。ふつうは既定のままにします（[13.3](#133-冗長化とスケール)）。
- `controller.watchNamespaces`：空のときは、すべての namespace の Gateway やルートを見ます。namespace を並べると、その namespace（とリリースの namespace）だけを見て、namespace に属するオブジェクト（Service・Secret・ルートなど）への権限も、クラスタ全体の ClusterRole ではなく、その namespace の Role に絞られます。namespace に属さないオブジェクト（GatewayClass など）のための ClusterRole は残ります。rproxy-gateway を使う namespace が決まっていて、コントローラの権限を小さくしたいときに使います（[12.](#12-セキュリティの基本)）。
- `controller.crossNamespaceSecrets`：`true` のとき、Gateway の `certificateRefs` から、ReferenceGrant（[6.2](#62-referencegrantほかの-namespace-の-servicesecret-を使う)）で許されたほかの namespace の Secret を使えます。managed では、その秘密鍵が Gateway の namespace の Secret に写されます。秘密鍵がほかの namespace に写るのを避けたいときは `false` にします（[SECURITY.md](SECURITY.md)）。

**Gateway ごとの rproxy（managed）**

| 値 | 既定 | 何を決めるか |
|---|---|---|
| `managed.replicas` | `1` | Gateway ごとの rproxy の Pod の数 |
| `managed.serviceType` | `LoadBalancer` | Gateway ごとの Service の型 |
| `managed.externalTrafficPolicy` | `""`（型に合わせて決める） | Service がクライアントの通信をどのノードの rproxy へ渡すか |
| `managed.allocateLoadBalancerNodePorts` | `true` | `LoadBalancer` の Service にノードのポートを割り当てるか |
| `managed.addressCIDRs` | `[]`（使えない） | Gateway の `spec.addresses` に書いてよいアドレスの範囲 |
| `managed.networkPolicy` | `true` | Gateway ごとに NetworkPolicy を作るか |
| `managed.shutdown` | `{delay: 15s, drain: 25s}` | rproxy の Pod が止まるときの段取り |
| `managed.parameters` | `{}` | すべての Gateway に共通の既定と、テナントに許す範囲 |

- `managed.replicas`：既定の 1 つでも動きますが、その Pod が入れ替わる間（更新やノードの drain）は通信が途切れます。本番では 2 以上にしてください。2 以上にすると、コントローラが PDB（PodDisruptionBudget。drain などで同時に止めてよい Pod の数を Kubernetes に伝えるもの）を `maxUnavailable: 1` で作り、Pod をなるべく別々のノードに置きます。Gateway ごとに変えたいときは [8.4](#84-rproxygatewayparameters) の `replicas` を使います。
- `managed.serviceType`：Gateway のアドレスをどう用意するかで選びます。
  - `LoadBalancer`（既定）：クラスタのロードバランサの実装（MetalLB やクラウドの LB）が Service にアドレスを配り、それが Gateway のアドレスになります。そうした実装のないクラスタでは、アドレスが付かず Gateway が `Programmed: False` のままになります（[2.4](#24-確かめる)）。
  - `NodePort`：各ノードのポートで受けます。前段に自分でロードバランサを置く形です（[PLATFORM.md](PLATFORM.md)）。
  - `ClusterIP`：クラスタの中からだけ届きます。Gateway のアドレスは Service の ClusterIP になります。
- `managed.externalTrafficPolicy`：Kubernetes の Service の `externalTrafficPolicy` です。外から来た通信を、どのノードの rproxy の Pod に渡すかを決めます。
  - `Local`：通信は、そのノードで動いている rproxy の Pod にだけ渡ります。rproxy にクライアントの本当のアドレスが届くので、RproxyPolicy の `allowFrom`（[8.1](#81-rproxypolicy)）やログでクライアントを見分けられます。ロードバランサは Service の `healthCheckNodePort` を見て、Ready な rproxy の Pod があるノードにだけ通信を送ります。
  - `Cluster`：どのノードに届いた通信も、ほかのノードの rproxy の Pod へ回せます。ノードが落ちたときや、アドレスを広報しているノードが替わったときの切り替わりはなめらかですが、rproxy に見えるのはノードのアドレスになり、クライアントのアドレスは失われます。
  - 空（既定）のときは、`LoadBalancer` なら `Local`、`NodePort` なら Kubernetes の既定の `Cluster` になります。`ClusterIP` には付きません。クライアントのアドレスで許可や制限をするなら `Local` のまま、それより切り替わりのなめらかさを取るなら `Cluster` にします。Gateway ごとに変えるなら [8.4](#84-rproxygatewayparameters) の `service.externalTrafficPolicy` を使います。考え方は [DESIGN.md](DESIGN.md) の「rproxy の可用性（managed）」にあります。
- `managed.allocateLoadBalancerNodePorts`：`LoadBalancer` の Service にも、Kubernetes はふつうノードのポート（NodePort）を割り当てます。MetalLB のように、ノードのポートを使わずにアドレスを直接ノードへ届けるロードバランサでは要らないので、`false` にすると無駄なポートを開けずに済みます。`LoadBalancer` 以外の型には効きません。
- `managed.addressCIDRs`：Gateway の `spec.addresses` に決まったアドレスを書くと、managed ではそれが Service の `externalIPs` になります。ここに書いた範囲の内側のアドレスだけが使え、空（既定）なら決まったアドレスは使えません。テナントが勝手なアドレスで待ち受けるのを防ぐためです。範囲には、Service や Pod のアドレスの範囲を絶対に含めないでください。含めると、Gateway がほかの Service 宛ての通信を横取りできてしまいます（[7.4](#74-アドレスの到達範囲)・[PLATFORM.md](PLATFORM.md)）。
- `managed.networkPolicy`：`true` のとき、Gateway ごとに NetworkPolicy を作り、rproxy の制御用のポートにはコントローラからしか届かないようにします。リスナーのポートには誰からでも届きます。CNI が NetworkPolicy に対応していないと効きません（[12.](#12-セキュリティの基本)）。
- `managed.shutdown`：rproxy の Pod が止まるとき（更新やノードの drain）の段取りです。SIGTERM を受けた rproxy は、`delay` の間は新しい接続も受け付けながら Ready をやめ、その間に Service やロードバランサが通信を別の Pod へ移します。その後、待ち受けを閉じ、今ある接続が終わるのを最大 `drain` まで待ちます。`drain` を過ぎても続く接続は切れます。長い WebSocket や DB の接続を待ちたいときは `drain` を、ロードバランサが通信を移すのに時間がかかるときは `delay` を延ばします。Gateway ごとに変えるなら [8.4](#84-rproxygatewayparameters) の `rproxy.shutdown` を使います（[10.3](#103-pod-の入れ替えと-readiness-gate)）。
- `managed.parameters`：すべての managed の Gateway に共通の既定（Pod の数・resources など）と、テナントが Gateway ごとに変えてよい項目の範囲（`policy`）を書きます。ここに書いた内容は、chart が作る `RproxyGatewayParameters` になります（[8.4](#84-rproxygatewayparameters)）。

**fleet**

| 値 | 既定 | 何を決めるか |
|---|---|---|
| `fleet.enabled` | `false` | managed ではなく fleet で動かすか |

`true` にすると、Gateway ごとの rproxy を作らず、chart の DaemonSet がすべての Gateway を受け持ちます。fleet の細かい値（`fleet.addresses` など）と切り替えの注意は [9.](#9-managed-と-fleet) にあります。

**転送先（バックエンド）**

この 3 つは、ルートの転送先の Pod に rproxy がどうつなぎ、どう見張るかの既定です。

| 値 | 既定 | 何を決めるか |
|---|---|---|
| `backends.connectTimeout` | `{http: 1s, l4: 1s}` | 転送先の Pod へつなぐのを待つ時間 |
| `backends.responseTimeout` | `30s` | HTTPRoute の転送先の応答ヘッダを待つ時間 |
| `backends.outlierDetection` | （下で説明） | 失敗が続く Pod を一時的に外すしくみ（受け身のヘルスチェック） |

- `backends.connectTimeout`：`http` は HTTP 系のルート、`l4` は TCP・TLS・UDP のルートの値です。HTTP でつながらなければ 502 を返し、L4 では次の Pod を試します。クラスタの外やネットワークの遠い転送先で、1 秒では接続が間に合わないときに延ばします。空文字にすると rproxy 自身の既定（5 秒。L4 で Pod が 1 つなら OS の既定で約 2 分）になります。
- `backends.responseTimeout`：リクエストを送ってから応答ヘッダが返るまで待つ時間です。過ぎると 504 を返します。ルートに `timeouts`（[4.5](#45-retry-とタイムアウト)）を書いたルールと GRPCRoute には付きません。応答の本文（大きなダウンロード・SSE）や、確立した後の WebSocket は切りません。応答を返すまで 30 秒より長く考え込むアプリがあるなら、そのルートだけ `timeouts` か RproxyPolicy の `responseTimeout`（[8.1](#81-rproxypolicy)）で延ばすのが基本です。空文字にすると rproxy 自身の既定（60 秒）になります。
- `backends.outlierDetection`：転送先の Pod が落ちても、Kubernetes がそれを EndpointSlice から外すまでには時間がかかります（ノードごと落ちたときは 40〜60 秒）。その間、rproxy は自分で失敗を数えて、壊れた Pod を一時的に外します。既定では、HTTP は 502・503・504・接続できない・応答の時間切れが 3 回続いた Pod を外し（アプリ自身が返した 5xx は数えません）、L4 は接続に 1 回失敗した Pod を外します。外す時間は 10 秒から始まり、繰り返すたびに倍になって 1 分までです。HTTP では、外すのは Pod 全体の半分までです。`{}` にすると rproxy 自身の既定（L4 は 1 回の失敗で 10 秒外す、HTTP は外さない）になります。特定の Gateway や Service だけ変えたいときは RproxyPolicy の `outlierDetection`（[8.1](#81-rproxypolicy)）を使います。

**そのほか**

| 値 | 既定 | 何を決めるか |
|---|---|---|
| `ui.namespace` | `""`（使わない） | 管理 UI に Gateway の rproxy を見せるか |
| `migration.migrateTo` | `""`（使わない） | Ingress・Traefik の設定を取り込む先の Gateway |

- `ui.namespace`：管理 UI（別の chart で入れます）の namespace を書くと、コントローラがその namespace に、UI が Gateway の rproxy を見つけるための Secret（`rproxy-ui-discovery`）と、読むだけのトークンを書きます。UI に見せる Gateway（RproxyGatewayParameters の `ui.visible` が `false` でないもの）の NetworkPolicy も、UI の Pod から制御 API へ届くように開けます。UI を使わないなら空のままにします。
- `migration.migrateTo`：Ingress や Traefik の設定を rproxy に取り込むときに、取り込み先の Gateway を `namespace/名前` で書きます。手順は [MIGRATION.md](MIGRATION.md) にあります。

**例**：rproxy を Gateway ごとに 2 つずつ置き、MetalLB でアドレスを配るクラスタに入れる `values.yaml` です。

```yaml
# values.yaml
managed:
  replicas: 2
  allocateLoadBalancerNodePorts: false
```

`replicas: 2` で、Pod の入れ替えの間も通信が途切れないようにしています。`allocateLoadBalancerNodePorts: false` は、MetalLB がノードのポートを使わないので、要らないポートを割り当てないための設定です。

## 第 2 部 標準の Gateway API で書く

第 2 部（2.〜7.）は、標準の Gateway API の書き方です。ここの YAML は、ほかの Gateway API の実装でも同じ意味になります。ただし、rproxy がそれをどう動かすか（既定の時間や、変更したときの動きなど）に触れるところがあり、そこには「**rproxy 固有**」の印を付けています。experimental channel の機能には「**Gateway API experimental**」の印を付けています。

## 2. はじめての Gateway と HTTPRoute

この章では、いちばん小さい構成（HTTP のリスナー 1 つと、それにつながるルート 1 つ）を作り、外からアプリに届くところまでを確かめます。以降の章の例は、ここで作るものを土台にしています。

### 2.1 転送先のアプリ

まず、転送先にするアプリを用意します。Gateway API の試験でも使われる echo サーバで、受けたリクエストの中身（どの Pod が受けたかも含みます）を JSON で返します。

```yaml
apiVersion: apps/v1
kind: Deployment
metadata: {name: app, namespace: default}
spec:
  replicas: 2
  selector: {matchLabels: {app: app}}
  template:
    metadata: {labels: {app: app}}
    spec:
      containers:
        - name: echo
          image: registry.k8s.io/gateway-api/echo-basic:v1.5.1
          env:
            - {name: POD_NAME, valueFrom: {fieldRef: {fieldPath: metadata.name}}}
            - {name: NAMESPACE, valueFrom: {fieldRef: {fieldPath: metadata.namespace}}}
          ports: [{containerPort: 3000}]
---
apiVersion: v1
kind: Service
metadata: {name: app, namespace: default}
spec:
  selector: {app: app}
  ports: [{name: http, port: 8080, targetPort: 3000}]
```

Service `app` はポート 8080 で受け、Pod の 3000 番へ渡します。以降の例では、ルートからこの Service の 8080 を指します。

### 2.2 Gateway と HTTPRoute

次に、ポート 80 で HTTP を待ち受ける Gateway `web` と、`app.example.com` 宛てのリクエストを Service `app` へ送る HTTPRoute を作ります。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs:
    - name: web
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - name: app
          port: 8080
```

大事な行は次のとおりです。

- `gatewayClassName: rproxy` で、この Gateway を rproxy-gateway に受け持たせます。
- `parentRefs` で、ルートをどの Gateway につなぐかを指します。
- `hostnames` は、リクエストの Host ヘッダで選ぶホスト名です。省くと、すべてのホスト名のリクエストを受けます。
- `backendRefs` は送り先の Service とそのポートです。
- **rproxy 固有**：rproxy は Service の ClusterIP を通さず、Service の EndpointSlice に載っている Ready な Pod の IP へ直接送ります。そのため、Pod の間の振り分けとヘルスチェックは rproxy 自身が行います。

### 2.3 何ができるか

> **rproxy 固有**：Gateway からどんな Pod や Service が作られるかは、実装ごとに違います。この節は rproxy-gateway の managed の場合です。

managed（既定）では、Gateway を作ると、コントローラが Gateway と同じ namespace に、その Gateway 専用の rproxy 一式を作ります。ここでは何が作られるかを説明します。

名前の `<id>` は `<namespace>-<name>` の後ろにハッシュ 6 桁を付けたものです（例：`default-web-1a2b3c`）。どれも Gateway を持ち主（ownerReference）としているので、Gateway を消せば一緒に消えます。自分で作ったり消したりする必要はありません。

| 作られるもの | 名前 | 役割 |
|---|---|---|
| rproxy の Deployment・ServiceAccount | `rproxy-<id>` | 通信を中継する rproxy の Pod |
| Service（`LoadBalancer`） | `rproxy-<id>` | Gateway のアドレス。リスナーのポートをすべて持ちます |
| 証明書の Secret | `rproxy-<id>-certs` | rproxy が使う証明書と鍵（[11.](#11-rproxy-との話し方)） |
| 制御 API の Secret（証明書・トークンファイル） | `rproxy-<id>-api` | コントローラと rproxy が話すための資格 |
| NetworkPolicy | `rproxy-<id>` | 制御用のポートをコントローラだけに開ける（[12.](#12-セキュリティの基本)） |

Gateway のラベルで、まとめて見られます。

```bash
kubectl -n default get deploy,svc,pod -l gateway.networking.k8s.io/gateway-name=web
```

### 2.4 確かめる

Gateway とルートが受け付けられ、通信が届くかを確かめます。

```bash
kubectl -n default get gateway web
# NAME   CLASS    ADDRESS        PROGRAMMED   AGE
# web    rproxy   192.0.2.100    True         40s
kubectl -n default get httproute app
# NAME   HOSTNAMES             AGE
# app    ["app.example.com"]   40s
```

- `ADDRESS` が Gateway のアドレスです。`LoadBalancer` の Service に付いたアドレス（`ClusterIP` 型なら ClusterIP）が出ます。
- `PROGRAMMED` が `True` なら、rproxy の Pod の 1 つ以上にルールが載り、通信を受けられる状態です。

アドレスを取り出して、実際にリクエストを送ってみます。DNS をまだ向けていなくても、Host ヘッダを付ければ試せます。

```bash
ADDR=$(kubectl -n default get gateway web -o jsonpath='{.status.addresses[0].value}')
curl -H 'Host: app.example.com' http://$ADDR/
```

もっと詳しい状態は `kubectl describe` で読めます。

```bash
kubectl -n default describe gateway web
```

```
Status:
  Addresses:
    Type:   IPAddress
    Value:  192.0.2.100
  Conditions:
    Reason:  Accepted      Status: True   Type: Accepted
    Reason:  Programmed    Status: True   Type: Programmed
  Listeners:
    Attached Routes:  1
    Conditions:
      Reason:  Accepted       Status: True   Type: Accepted
      Reason:  ResolvedRefs   Status: True   Type: ResolvedRefs
      Reason:  NoConflicts    Status: False  Type: Conflicted
      Reason:  Programmed     Status: True   Type: Programmed
    Name:  http
```

（表示は一部を省いています。）Gateway 全体とリスナーごとに、条件（condition）が並びます。`Accepted` は書いた内容が受け付けられたこと、`ResolvedRefs` は証明書などの参照が見つかったこと、`Programmed` は rproxy に載ったことを表します。`Conflicted` だけは `False` が正常です。`Attached Routes` は、そのリスナーにつながったルートの数です。

ルートの状態は `kubectl -n default describe httproute app` の `Status.Parents` に、つないだ Gateway ごとに出ます。`Accepted` と `ResolvedRefs` が `True` なら正常です。

`ADDRESS` が空のままで、`PROGRAMMED` が `False`（reason `AddressNotAssigned`、message「waiting for an address (the rproxy Service)」）なら、クラスタに `LoadBalancer` の Service へアドレスを配るもの（MetalLB やクラウドのロードバランサ）がありません。[PLATFORM.md](PLATFORM.md) を見て用意するか、chart の `managed.serviceType`（**rproxy 固有**）を `NodePort` か `ClusterIP` にしてください（[1.3](#13-主な-chart-の値)）。

## 3. HTTPS

この章では、Gateway で HTTPS を受ける方法を説明します。証明書を自分で Secret にする方法（3.1）、cert-manager に取らせる方法（3.2）、HTTP で来たものを HTTPS へ送り直す方法（3.3）の順です。

### 3.1 TLS の Secret を certificateRefs で

手元に証明書と鍵のファイルがあるときの方法です。まず、それを `kubernetes.io/tls` 型の Secret にします。

```bash
kubectl -n default create secret tls app-tls --cert=app.crt --key=app.key
```

次に、Gateway に HTTPS のリスナーを足して、その Secret を `certificateRefs` で指します。ルートは HTTPS のリスナーにつなぎます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        mode: Terminate
        certificateRefs:
          - name: app-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs:
    - name: web
      sectionName: https
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - name: app
          port: 8080
```

大事な行は次のとおりです。

- `protocol: HTTPS` のリスナーでは、rproxy が TLS を終え（復号し）、中身を HTTP として振り分けます。`mode: Terminate` はこの動きを表す値で、省いても同じです。
- 同じポートに `HTTPS` のリスナーを複数（ホスト名ごとに別の証明書で）書けます。rproxy は、クライアントが TLS の最初に送るホスト名（SNI）を見て証明書を選びます。
- ルートの `sectionName` は、つなぐリスナーの名前です。省くと、そのルートを受けられるリスナーすべてにつながります。ここでは `https` だけにつなぎ、HTTP では受けないようにしています。

rproxy での証明書の扱いについて、知っておくとよいことが 2 つあります（**rproxy 固有**）。

- 秘密鍵は、コントローラと rproxy の間の制御 API を通りません。コントローラが鍵を `rproxy-<id>-certs` という Secret に写し、kubelet がそれを rproxy の Pod にファイルとして渡します（[11.](#11-rproxy-との話し方)）。
- 証明書の Secret を書き換える（更新する）と、今の接続を切らずに、新しい接続から新しい証明書に替わります（[10.](#10-動いているときに変えると)）。

うまくいったかは、リスナーの `ResolvedRefs` で確かめます。証明書が見つかれば reason も `ResolvedRefs` になります。

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{.conditions[?(@.type=="ResolvedRefs")].reason}{"\n"}{end}'
# http ResolvedRefs
# https ResolvedRefs
curl --resolve app.example.com:443:$ADDR https://app.example.com/
```

Secret がない、または中身が壊れているときは、リスナーが `ResolvedRefs: False`（reason `InvalidCertificateRef`）と `Programmed: False` になり、そのポートのルールは作られません。

### 3.2 cert-manager

証明書を Let's Encrypt などから自動で取って更新したいときは、[cert-manager](https://cert-manager.io/) を使います。cert-manager が作った Secret を、3.1 と同じように `certificateRefs` で指すだけです。rproxy-gateway は Secret を読むだけで、cert-manager との間に特別なつなぎはありません。

書き方は 2 通りあります。

**Certificate を自分で書く形**：受け入れテストで、証明書の更新まで確かめている形です。ClusterIssuer（どこからどう証明書を取るか）と Certificate（どの名前の証明書をどの Secret に置くか）を書きます。

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: {name: letsencrypt}
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@example.com
    privateKeySecretRef: {name: letsencrypt-account}
    solvers:
      - http01:
          gatewayHTTPRoute:
            parentRefs:
              - {name: web, namespace: default, kind: Gateway, sectionName: http}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: app-tls, namespace: default}
spec:
  secretName: app-tls
  dnsNames: [app.example.com]
  issuerRef: {name: letsencrypt, kind: ClusterIssuer, group: cert-manager.io}
```

- Gateway は 3.1 と同じです（`certificateRefs: [{name: app-tls}]`）。Secret ができるまでリスナーは `ResolvedRefs: False` ですが、できれば自動で `True` になります。何もし直す必要はありません。
- `gatewayHTTPRoute` は、Let's Encrypt の HTTP-01 の確認（チャレンジ）に答えるための HTTPRoute を、cert-manager が Gateway の `http` リスナーに作る形です。これを使うには、cert-manager の Gateway API への対応を有効にしておきます（cert-manager の chart の `config.enableGatewayAPI: true` など。cert-manager の文書で確かめてください）。DNS-01 で確認するなら、Gateway は関係しません。
- 3.3 のリダイレクトのルートを同じホスト名に置いても問題ありません。チャレンジのパス（`/.well-known/acme-challenge/...`）のほうが長く一致するので、そちらが先に選ばれます。

**Gateway に注釈を書く形**：Gateway API の標準でも rproxy のものでもなく、cert-manager の機能です。Gateway に注釈を 1 つ書くと、cert-manager がリスナーの `hostname` と `certificateRefs` から Certificate を自動で作ります。Certificate を自分で書かずに済みます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata:
  name: web
  namespace: default
  annotations:
    cert-manager.io/cluster-issuer: letsencrypt
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
```

`cert-manager.io/cluster-issuer` の注釈で、どの ClusterIssuer を使うかを指します。

どちらの形でも、証明書ができたかは次のように確かめます。

```bash
kubectl -n default get certificate app-tls     # READY が True
kubectl -n default get secret app-tls
```

**rproxy の ACME は使いません**（**rproxy 固有**）：rproxy 自身にも ACME（Let's Encrypt などから自分で証明書を取る機能）がありますが、Gateway API からは使いません。証明書は `certificateRefs` の Secret から渡します。理由は、rproxy の ACME が Pod ごとに証明書を取りに行くためです。Pod が複数あると、HTTP-01 のチャレンジの問い合わせがほかの Pod に届いて失敗することがあります。証明書は cert-manager で取ってください。例外は、Traefik の `certResolver` を移行するときだけです（[MIGRATION.md](MIGRATION.md) の「変換」）。

DNS-01 で使う DNS の事業者が cert-manager に組み込まれていなくても（OCI など）、cert-manager の DNS-01 の webhook で対応できることが多いです。ただし webhook は事業者ごとに別のもので、このプロジェクトでは試していません。

### 3.3 HTTP から HTTPS へリダイレクト

HTTPS を用意したら、HTTP（80）で来たリクエストを HTTPS へ送り直すのがふつうです。HTTP のリスナーに、リダイレクトだけをするルートをつなぎます。

```yaml
# HTTP（80）に来たものはすべて HTTPS へ
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: to-https, namespace: default}
spec:
  parentRefs:
    - name: web
      sectionName: http
  hostnames: [app.example.com]
  rules:
    - filters:
        - type: RequestRedirect
          requestRedirect:
            scheme: https
            statusCode: 301
```

- `sectionName: http` で、このルートを HTTP のリスナーだけにつなぎます。アプリのルート（3.1 の `app`）は `sectionName: https` だけにつないだままにします。
- `scheme: https` でスキームを変えると、ポートはそのスキームの既定（443）になります。ポートを書く必要はありません。

`Location` ヘッダに HTTPS の URL が入った 301 が返れば成功です。

```bash
curl -sI -H 'Host: app.example.com' http://$ADDR/path
# HTTP/1.1 301 Moved Permanently
# location: https://app.example.com/path
```

## 4. HTTP の振り分け

この章では、HTTPRoute で HTTP のリクエストを振り分ける方法を説明します。ホスト名・パス・ヘッダでの振り分け、重みでの分割、ヘッダの書き換えなどのフィルタ、retry とタイムアウト、gRPC の順です。この章の HTTPRoute は、どれも 2.2 の Gateway `web` につなぎます。

### 4.1 ホスト名

1 つの Gateway で複数のホスト名のサイトを受けるときの書き方です。リスナーの `hostname` とルートの `hostnames` の両方に合うリクエストだけが、そのルートに届きます。次の例では、ワイルドカード証明書の HTTPS のリスナー 1 つに、`app.example.com` と `api.example.com` の 2 つのルートをつなぎます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: "*.example.com"
      tls:
        certificateRefs:
          - name: wildcard-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: api, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [api.example.com]
  rules:
    - backendRefs: [{name: api, port: 8080}]
```

ホスト名の合わせ方の決まりは次のとおりです。

- `*.example.com` は、`example.com` の下のすべての名前（`a.b.example.com` のような深い名前も）に合います。`example.com` そのものには合いません。
- リスナーとルートのホスト名に重なりがないと、ルートは受け付けられず、`Accepted: False`（reason `NoMatchingListenerHostname`）になります。
- どのルートにも合わないリクエストには 404 を返します。
- 同じポートに、もっと具体的なホスト名のリスナー（たとえば `app.example.com`）があると、その名前のリクエストは具体的なリスナーのルートにだけ届きます。ワイルドカードのリスナーのルートには届きません。これは Gateway API の決まり（リスナーの分離）です。

### 4.2 パス・ヘッダ・クエリ・メソッド

同じホスト名の中で、パスやヘッダによって送り先を変える書き方です。次の例は、1 つの HTTPRoute に、よく使う一致の種類を並べたものです。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    # /api/ で始まるもの（/api 自身も）
    - matches:
        - path: {type: PathPrefix, value: /api}
      backendRefs: [{name: api, port: 8080}]
    # /healthz ちょうど
    - matches:
        - path: {type: Exact, value: /healthz}
      backendRefs: [{name: app, port: 8080}]
    # ヘッダ X-Debug: 1 で、POST のもの
    - matches:
        - headers:
            - {name: X-Debug, value: "1"}
          method: POST
      backendRefs: [{name: app-v2, port: 8080}]
    # クエリ ?beta=true（matches の中の 1 つが合えばよい：OR）
    - matches:
        - queryParams:
            - {name: beta, value: "true"}
        - path: {type: RegularExpression, value: "/v[0-9]+/beta/.*"}
      backendRefs: [{name: app-v2, port: 8080}]
    # どれにも合わないもの
    - backendRefs: [{name: app, port: 8080}]
```

読み方の決まりは次のとおりです。

- `matches` の 1 つの項の中に書いた条件は、すべてに合う必要があります（AND）。`matches` に項を複数並べると、どれか 1 つに合えばよくなります（OR）。3 つ目のルールはヘッダと POST の両方が要り、4 つ目のルールはクエリか正規表現のパスのどちらかで足ります。
- `PathPrefix: /api` は `/api` と `/api/...` に合い、`/apis` には合いません。パスの区切りの `/` 単位で比べるためです。
- ルールが選ばれる順番は、書いた順ではありません。Gateway API の決まった優先順（Exact → 長い PathPrefix → method → ヘッダの数 → クエリの数 → 古いルート…）で選ばれます。詳しくは [DESIGN.md](DESIGN.md) の「変換」の「ルールの優先」にあります。
- ヘッダとクエリにも `type: RegularExpression`（正規表現）が使えます。

### 4.3 重みとカナリア

新しい版のアプリに、リクエストの一部だけを送って様子を見たい（カナリア）ときの書き方です。1 つのルールの `backendRefs` に複数の Service を並べ、`weight` で割合を決めます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080, weight: 90}
        - {name: app-v2, port: 8080, weight: 10}
```

- この例では、リクエストのおよそ 1 割が `app-v2` に行きます。
- **rproxy 固有**：コントローラは Service ごとの重みをその Pod の数で割って、Pod ごとの重みにします。そのため、2 つの Service の Pod の数が違っても、Service の間の比は保たれます。
- `weight: 0` の Service には何も送りません。
- 重みを変えても、今の接続は切れません（[10.](#10-動いているときに変えると)）。重みを少しずつ上げていく手順は [14.5](#145-重みでカナリア) にあります。

### 4.4 フィルタ

フィルタは、リクエストを送る前（や応答を返す前）に、ヘッダやパスを書き換えたり、リダイレクトを返したりするしくみです。次の例は、よく使う 3 種類のフィルタをルールごとに使い分けたものです。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    # ヘッダを足す・消す
    - matches: [{path: {type: PathPrefix, value: /}}]
      filters:
        - type: RequestHeaderModifier
          requestHeaderModifier:
            set: [{name: X-Env, value: prod}]
            add: [{name: X-Via, value: rproxy}]
            remove: [X-Internal-Token]
        - type: ResponseHeaderModifier
          responseHeaderModifier:
            set: [{name: Strict-Transport-Security, value: max-age=31536000}]
      backendRefs: [{name: app, port: 8080}]
    # /old/... を /new/... に書き換えて送る（クライアントには見えない）
    - matches: [{path: {type: PathPrefix, value: /old}}]
      filters:
        - type: URLRewrite
          urlRewrite:
            hostname: internal.example.com
            path: {type: ReplacePrefixMatch, replacePrefixMatch: /new}
      backendRefs: [{name: app, port: 8080}]
    # /docs をほかのサイトへリダイレクト（クライアントに 302 を返す）
    - matches: [{path: {type: PathPrefix, value: /docs}}]
      filters:
        - type: RequestRedirect
          requestRedirect:
            hostname: docs.example.com
            path: {type: ReplaceFullPath, replaceFullPath: /}
            statusCode: 302
```

- `RequestHeaderModifier`・`ResponseHeaderModifier` の `set` は値を置き換え、`add` はすでにある値の後ろに `,` で区切って足し、`remove` はヘッダを消します。
- `URLRewrite` は、転送先へ送るときだけパスやホスト名を書き換えます。クライアントには書き換えたことは見えません。`ReplacePrefixMatch` は、ルールのパスの一致が 1 つだけ（`PathPrefix`）のときにしか使えません。
- `RequestRedirect` は転送先へ送らず、クライアントにリダイレクトを返します。

ほかに、`CORS`、`RequestMirror`（リクエストの写しを別の Service へ送る）、backendRef ごとの `filters` も使えます。**rproxy 固有**：流量の制限や認証など rproxy 独自の機能は、標準の拡張点 `ExtensionRef` から RproxyMiddleware を指して使います（[8.2](#82-rproxymiddleware)）。

### 4.5 retry とタイムアウト

転送先が遅いときや一時的に失敗したときの扱いを、ルールごとに決める書き方です。`timeouts` で待つ時間の上限を、`retry` で送り直す条件を書きます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
      timeouts:
        request: 30s          # クライアントのリクエスト全体
        backendRequest: 10s   # 転送先への 1 回の送信
      retry:                  # Gateway API v1.6 は experimental の CRD、v1.7 から standard
        attempts: 2           # 最初の 1 回に足す回数
        codes: [502, 503, 504]
        backoff: 100ms
```

- `timeouts.request` は、クライアントのリクエスト全体にかけてよい時間です。`timeouts.backendRequest` は、転送先へ 1 回送ってから応答が返るまでの時間です。どちらかを過ぎると 504 を返します。
- **rproxy 固有**：`timeouts` を書かないルールには、転送先の応答ヘッダを 30 秒まで待つ既定が付きます（chart の `backends.responseTimeout`、[1.3](#13-主な-chart-の値)）。応答の本文（大きなダウンロードや SSE）と WebSocket は、この既定では切りません。応答を返すまでに長く時間のかかるアプリには、`backendRequest` を書くか、RproxyPolicy の `responseTimeout`（[8.1](#81-rproxypolicy)）を使います。
- `retry` の `attempts` は、最初の 1 回に足して送り直す回数です。
- **rproxy 固有**：rproxy が送り直すのは、つながらなかったとき、応答がなかったとき、`codes` に書いた状態コードが返ったときです。ただし、何度送っても結果が同じになるメソッド（GET・HEAD・OPTIONS・PUT・DELETE・TRACE）で、本文のないリクエストだけを送り直します。POST などは、二重に処理されるおそれがあるので送り直しません。
- **Gateway API experimental**：`retry` は、Gateway API v1.6 では experimental の CRD が要ります（[1.1](#11-gateway-api-の-crd)）。v1.7 からは standard に入っています。

### 4.6 GRPCRoute

gRPC のサービスを公開するときは、HTTPRoute の代わりに GRPCRoute を使います。パスではなく、gRPC のサービス名とメソッド名で振り分けられます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: GRPCRoute
metadata: {name: echo, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [grpc.example.com]
  rules:
    - matches:
        - method: {service: echo.Echo, method: Ping}
      backendRefs: [{name: grpc-echo, port: 9000}]
    - matches:
        - method: {service: echo.Echo}
          headers:
            - {name: x-canary, value: "true"}
      backendRefs: [{name: grpc-echo, port: 9000}]
```

- 1 つ目のルールは `echo.Echo/Ping` の呼び出しだけに、2 つ目のルールは `echo.Echo` のすべてのメソッドのうち、ヘッダ `x-canary: true` を付けた呼び出しに合います。
- GRPCRoute は `HTTP`・`HTTPS` のリスナーにつなぎます。HTTPRoute と同じポートに置けます。
- **rproxy 固有**：rproxy は転送先と h2c（暗号化しない HTTP/2）で話します。転送先が TLS で受けるなら、BackendTLSPolicy（[7.1](#71-backendtlspolicy)）を使います。
- **rproxy 固有**：GRPCRoute には、HTTPRoute のような 30 秒の応答の既定は付きません。gRPC のストリームは応答ヘッダを長く返さないことがあるためです。時間の上限が要るなら RproxyPolicy の `responseTimeout` を使います。

HTTPRoute の転送先が h2c で受けるアプリなら、Service のポートに `appProtocol: kubernetes.io/h2c` を書いてください。

## 5. L4 の転送

この章では、HTTP ではない通信（データベース、DNS、メールなど）を、TCP・UDP・TLS のまま転送する方法を説明します。L4 とは、中身を HTTP として読まず、接続やパケットの単位で転送することを指します。

### 5.1 TCPRoute と UDPRoute

TCP や UDP のポートを、そのまま Service へ転送する書き方です。次の例では、PostgreSQL（TCP の 5432）と DNS（UDP の 53）を 1 つの Gateway で受けます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: l4, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: postgres
      port: 5432
      protocol: TCP
      allowedRoutes:
        kinds: [{kind: TCPRoute}]
    - name: dns
      port: 53
      protocol: UDP
      allowedRoutes:
        kinds: [{kind: UDPRoute}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: postgres, namespace: default}
spec:
  parentRefs:
    - {name: l4, sectionName: postgres}
  rules:
    - backendRefs:
        - {name: postgres, port: 5432}
---
apiVersion: gateway.networking.k8s.io/v1
kind: UDPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs:
    - {name: l4, sectionName: dns}
  rules:
    - backendRefs:
        - {name: dns, port: 53}
```

- `allowedRoutes.kinds` は、そのリスナーが受けるルートの種類です。省いてもかまいません。省くと、`TCP` のリスナーは TCPRoute を、`UDP` のリスナーは UDPRoute を受けます。
- L4 のルートにはホスト名がないので、ルートは `sectionName` でリスナーを名指してつなぎます。
- **rproxy 固有**：1 つのリスナーに TCPRoute（や UDPRoute）を複数つなぐと、どれも `Accepted` になりますが、通信はいちばん古いルートにだけ行きます。1 つのリスナーには 1 つのルートをつないでください。
- **rproxy 固有**：ルートが 1 つもつながっていない `TCP`・`UDP` のリスナーには、rproxy のルールを作りません（Service のポートはあります）。
- Gateway API の CRD が古い（v1.6 より前）と、TCPRoute・UDPRoute は `v1alpha2` です。API サーバが出している版で書いてください。

うまくいったかは、ルートの状態と、実際の接続で確かめます。

```bash
kubectl -n default get tcproute,udproute
kubectl -n default get tcproute postgres -o jsonpath='{.status.parents[0].conditions[*].reason}'
# Accepted ResolvedRefs
psql -h $ADDR -p 5432 -U app
dig @$ADDR example.com
```

### 5.2 TLSRoute（passthrough と terminate）

TLS の通信を、ホスト名（SNI）で振り分けて転送する書き方です。rproxy で復号するかどうかで、2 つの形があります。

- **Passthrough**（復号しない）：証明書は転送先の Pod が持ち、rproxy は暗号のまま中継します。エンドツーエンドで暗号化したいときや、証明書を Gateway に預けたくないときに使います。
- **Terminate**（rproxy が復号する）：rproxy が証明書で TLS を終え、中身を平文の TCP として転送先へ送ります。TLS に対応していないアプリの前で、TLS だけを受け持たせたいときに使います。

次の例は、両方の形のリスナーを 1 つの Gateway に並べたものです。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: tls, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    # 復号しない（証明書は転送先の Pod が持つ）
    - name: passthrough
      port: 443
      protocol: TLS
      hostname: "*.example.com"
      tls:
        mode: Passthrough
    # rproxy が復号して、平文の TCP で転送先へ
    - name: terminate
      port: 8443
      protocol: TLS
      hostname: mail.example.com
      tls:
        mode: Terminate
        certificateRefs:
          - name: mail-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: secure-app, namespace: default}
spec:
  parentRefs:
    - {name: tls, sectionName: passthrough}
  hostnames: [secure.example.com]
  rules:
    - backendRefs:
        - {name: secure-app, port: 8443}
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: mail, namespace: default}
spec:
  parentRefs:
    - {name: tls, sectionName: terminate}
  hostnames: [mail.example.com]
  rules:
    - backendRefs:
        - {name: mail, port: 443}
```

- **Passthrough** では、rproxy は TLS の最初のメッセージ（ClientHello）にある SNI だけを見て、暗号のまま転送先へ送ります。HTTP の中身は見えないので、ヘッダの書き換えなどはできません。どのルートのホスト名にも合わない接続は断ります。
- **Terminate** では、rproxy が証明書で TLS を終え、中身を TCP として送ります。中身を HTTP として振り分けることはしません。HTTP として振り分けたいなら、`HTTPS` のリスナーと HTTPRoute を使ってください（[3.1](#31-tls-の-secret-を-certificaterefs-で)）。
- 同じポートに `HTTPS` と `TLS`（Passthrough）のリスナーを並べられます。ホスト名によって、復号する名前と復号しない名前を分けられます（[14.4](#144-443-で-sni-の-passthrough)）。

### 5.3 ルートごとのリスナー

どのルートを、どんなリスナーにつなぐかの早見表です。リスナーの `protocol` と `tls.mode` によって、受けられるルートの種類と、証明書が要るかが決まります。

| ルート | リスナーの `protocol` | `tls.mode` | 証明書（`certificateRefs`） |
|---|---|---|---|
| HTTPRoute・GRPCRoute | `HTTP` | — | — |
| HTTPRoute・GRPCRoute | `HTTPS` | `Terminate`（省略可） | 要る |
| TLSRoute | `TLS` | `Passthrough` | 要らない（転送先が持つ） |
| TLSRoute | `TLS` | `Terminate` | 要る |
| TCPRoute | `TCP` | — | — |
| UDPRoute | `UDP` | — | — |

rproxy でのポートについて、知っておくとよいことが 2 つあります（**rproxy 固有**）。

- managed の Service は、Gateway のリスナーのポートをすべて持ちます。TCP と UDP の同じポート（DNS の 53 など）を 1 つの `LoadBalancer` の Service に載せるには、ロードバランサの実装が TCP と UDP の混在に対応している必要があります。
- 1024 未満のポートも、そのまま使えます。rproxy の Pod は root ではなく、安全な sysctl で低いポートを開けられるようにしています。

## 6. ほかの namespace

この章では、namespace をまたいで Gateway を使う方法を説明します。インフラの担当者が共有の Gateway を 1 つ持ち、各チームが自分の namespace からルートやリスナーをつなぐ、という使い方のための機能です。

### 6.1 allowedRoutes：Gateway がどの namespace のルートを受けるか

既定では、Gateway は自分と同じ namespace のルートしか受けません（`from: Same`）。ほかの namespace のチームにも Gateway を使わせたいときは、リスナーの `allowedRoutes` で受ける namespace を広げます。次の例では、`gateway-access: "true"` のラベルが付いた namespace のルートだけを受けます。

```yaml
# infra が持つ共有の Gateway。ラベル gateway-access: "true" の namespace のルートを受ける
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: shared, namespace: infra}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: "*.example.com"
      tls:
        certificateRefs:
          - name: wildcard-tls
      allowedRoutes:
        namespaces:
          from: Selector
          selector:
            matchLabels:
              gateway-access: "true"
```

- `from` には、`Same`（既定。同じ namespace だけ）、`All`（すべての namespace）、`Selector`（ラベルで選んだ namespace）のどれかを書きます。
- 受けない namespace のルートは、`Accepted: False`（reason `NotAllowedByListeners`）になります。
- `Selector` を使うなら、namespace のラベルをテナントが自分で書き換えられないようにしておいてください。書き換えられると、許していない namespace からもルートをつなげてしまいます。

### 6.2 ReferenceGrant：ほかの namespace の Service・Secret を使う

ルートが指す Service は、ルートと同じ namespace のものが基本です。ほかの namespace の Service を指すには、**指される側**の namespace に ReferenceGrant を置きます。ReferenceGrant は「この namespace のこれを、あの namespace のあの種類のリソースから使ってよい」という、参照される側の持ち主が出す許可です。勝手にほかのチームの Service へ通信を流せないようにするためのしくみです。

次の例では、`team-a` の HTTPRoute が、`default` の Service `api` を使います。

```yaml
# team-a のルート：ほかの namespace の Gateway を namespace つきで指す
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: team-a}
spec:
  parentRefs:
    - {name: shared, namespace: infra}
  hostnames: [app.example.com]
  rules:
    - matches: [{path: {type: PathPrefix, value: /api}}]
      backendRefs:
        # ほかの namespace の Service（下の ReferenceGrant が要る）
        - {name: api, namespace: default, port: 8080}
    - backendRefs:
        - {name: app, port: 8080}
---
# default の持ち主が「team-a の HTTPRoute から Service api を使ってよい」と許す
apiVersion: gateway.networking.k8s.io/v1beta1
kind: ReferenceGrant
metadata: {name: team-a-to-api, namespace: default}
spec:
  from:
    - {group: gateway.networking.k8s.io, kind: HTTPRoute, namespace: team-a}
  to:
    - {group: "", kind: Service, name: api}
```

- `parentRefs` の `namespace: infra` は、6.1 の共有の Gateway を指しています。Gateway を指すのに ReferenceGrant は要りません（Gateway の側の `allowedRoutes` で許します）。
- ReferenceGrant の `from` は「誰から」（`team-a` の HTTPRoute）、`to` は「何を」（Service `api`）です。
- ReferenceGrant がないと、ルートは `ResolvedRefs: False`（reason `RefNotPermitted`、message「Service default/api: no ReferenceGrant allows the reference」）になり、その backend に向かうはずのリクエストには 500 を返します。
- Gateway の `certificateRefs` で、ほかの namespace の Secret を使うときも同じです（`from` は `kind: Gateway`、`to` は `kind: Secret`）。**rproxy 固有**：managed では、その秘密鍵が Gateway の namespace の Secret に写されます（[SECURITY.md](SECURITY.md) の「ほかの namespace の秘密鍵」）。
- ReferenceGrant は、Gateway API v1.6 では `v1` と `v1beta1` のどちらでも書けます。

### 6.3 ListenerSet：ほかの namespace からリスナーを足す

共有の Gateway に、テナントが自分のホスト名と証明書のリスナーを足すしくみです。6.1 のワイルドカード証明書のように Gateway の持ち主がまとめて証明書を用意するのではなく、各チームが自分の証明書を自分の namespace に置きたいときに使います。

Gateway の持ち主は、`allowedListeners` でどの namespace からリスナーを足してよいかを決めます（既定は `None` で、誰も足せません）。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: shared, namespace: infra}
spec:
  gatewayClassName: rproxy
  allowedListeners:
    namespaces:
      from: Selector
      selector:
        matchLabels:
          gateway-access: "true"
  listeners:
    - name: http
      port: 80
      protocol: HTTP
---
# team-a が自分のホスト名と証明書のリスナーを足す
apiVersion: gateway.networking.k8s.io/v1
kind: ListenerSet
metadata: {name: team-a, namespace: team-a}
spec:
  parentRef:
    name: shared
    namespace: infra
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls      # team-a の Secret
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: team-a}
spec:
  parentRefs:
    - group: gateway.networking.k8s.io
      kind: ListenerSet
      name: team-a
      sectionName: https
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080}
```

- ListenerSet の `parentRef` で、リスナーを足す先の Gateway を指します。
- ListenerSet のリスナーの証明書は、ListenerSet の namespace（ここでは `team-a`）から探します。`allowedRoutes` の `Same` も、ListenerSet の namespace を指します。
- ルートは、`parentRefs` に `kind: ListenerSet` を書いたときだけ、ListenerSet のリスナーにつながります。Gateway を指したルートは、ListenerSet のリスナーにはつながりません。
- 同じポートで設定が食い違うと、古い ListenerSet が勝ちます。

足したリスナーが受け付けられたかは、次のように確かめます。

```bash
kubectl -n team-a get listenerset team-a
# NAME     ACCEPTED   PROGRAMMED   AGE
# team-a   True       True         30s
kubectl -n infra get gateway shared -o jsonpath='{.status.attachedListenerSets}'
```

## 7. Gateway API のそのほかの機能

この章では、第 2 部のここまでの章で扱わなかった標準の Gateway API の機能を説明します。転送先への TLS（7.1）とクライアント証明書の確かめ（7.2）は standard channel の機能です。リスナーのフィルタ（7.3）とアドレスの到達範囲（7.4）は、Gateway API v1.7 の experimental channel の機能です。

### 7.1 BackendTLSPolicy

BackendTLSPolicy は、rproxy から転送先の Pod へ TLS（HTTPS）で送るための Gateway API の設定です。転送先のアプリが HTTPS でしか受けないときや、クラスタの中の通信も暗号化したいときに使います。どの名前の証明書を、どの CA で確かめるかを書きます。

```yaml
# Service secure-app の 8443 へは HTTPS で送り、証明書を ConfigMap の CA で確かめる
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {name: secure-app, namespace: default}
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: secure-app
  validation:
    hostname: secure-app.default.svc
    caCertificateRefs:
      - group: ""
        kind: ConfigMap
        name: secure-app-ca     # キー ca.crt に PEM
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: secure, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [secure.example.com]
  rules:
    - backendRefs: [{name: secure-app, port: 8443}]
```

- `targetRefs` で、TLS で送る Service を指します。HTTPRoute の側には何も足しません。
- `validation.hostname` は、TLS の SNI として送り、転送先の証明書を確かめるのに使う名前です。確かめる名前を別にしたいときは `subjectAltNames` を使います。
- `caCertificateRefs` は、転送先の証明書を確かめる CA です。ConfigMap のキー `ca.crt` に PEM で置きます。公開の CA の証明書なら、代わりに `wellKnownCACertificates: System` と書けます。
- `targetRefs` に `sectionName` を書くと、Service の特定のポートだけに効かせられます。ポートを指したポリシーは、Service 全体を指したポリシーに勝ちます。
- 使える CA がないと `Accepted: False`（reason `NoValidCACertificate`）になり、その backend へのリクエストには 500 を返します。
- 転送先がクライアント証明書を求めるなら、Gateway の `spec.tls.backend.clientCertificateRef` に、出す証明書の Secret を書きます。

効いたかは、`kubectl -n default describe backendtlspolicy secure-app` の `Status.Ancestors`（Gateway ごとの状態）で確かめます。

### 7.2 ClusterTrustBundle とクライアント証明書の確かめ

この節では、CA の証明書をクラスタ全体で共有する ClusterTrustBundle の使い方と、クライアントに証明書を求める（mTLS）設定を説明します。

ClusterTrustBundle は、クラスタ全体で使う CA の証明書の束を置く Kubernetes のリソースです（Kubernetes 1.37 の `certificates.k8s.io/v1`。それより前はベータの API を有効にしたときに使えます）。`caCertificateRefs` から名前で指せ、namespace に属さないので ReferenceGrant は要りません。

> **rproxy 固有（実装ごと）**：Gateway API で `caCertificateRefs` が必ず指せる（Core）のは ConfigMap だけで、ClusterTrustBundle を指せるかは実装ごとに違います（Implementation-specific）。ほかの実装に持っていくなら、CA は ConfigMap に置いてください。`frontend.validation` と BackendTLSPolicy そのものは標準です。社内の CA を一か所に置いて、多くの Gateway や BackendTLSPolicy から使いたいときに便利です。

次の例では、社内の CA を ClusterTrustBundle にし、それを転送先の証明書の確かめ（BackendTLSPolicy）と、クライアント証明書の確かめ（Gateway）の両方に使います。

```yaml
apiVersion: certificates.k8s.io/v1
kind: ClusterTrustBundle
metadata: {name: internal-ca}
spec:
  trustBundle: |
    -----BEGIN CERTIFICATE-----
    ...（社内の CA の PEM）...
    -----END CERTIFICATE-----
---
apiVersion: gateway.networking.k8s.io/v1
kind: BackendTLSPolicy
metadata: {name: secure-app, namespace: default}
spec:
  targetRefs:
    - {group: "", kind: Service, name: secure-app}
  validation:
    hostname: secure-app.default.svc
    caCertificateRefs:
      - group: certificates.k8s.io
        kind: ClusterTrustBundle
        name: internal-ca
---
# クライアント証明書を求める（mTLS）。CA は ConfigMap か ClusterTrustBundle
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  tls:
    frontend:
      default:
        validation:
          caCertificateRefs:
            - {group: certificates.k8s.io, kind: ClusterTrustBundle, name: internal-ca}
  listeners:
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
```

- Gateway の `spec.tls.frontend.default.validation` で、HTTPS のリスナーに来るクライアントに証明書を求め、その CA で確かめます。
- `spec.tls.frontend` の `perPort` で、ポートごとに違う設定にできます。
- `mode: AllowInsecureFallback` にすると、クライアントが証明書を出さなくても、確かめに通らなくても接続を受け、結果を `X-Client-Verify` ヘッダで転送先に伝えます（このヘッダは **rproxy 固有**）。アプリの側で扱いを決めたいときに使います。
- API サーバが ClusterTrustBundle を出していないと、参照は reason `InvalidKind`（Gateway では `InvalidCACertificateKind`）になります。

### 7.3 リスナーのフィルタ

> **Gateway API experimental**：Gateway API v1.7 の experimental の CRD が要ります。

リスナーのフィルタは、そのリスナーに来たすべてのリクエストに、ルートを選ぶ**前に**働く処理です。ホスト名やパスにかかわらず、ポート全体に認証や流量の制限をかけたいときに使います。Gateway API v1.7 の experimental の CRD で使える機能（GEP-5224）で、リスナーに `filters.requests` を書きます。フィルタは書いた順に働き、rproxy v0.4.22 の `http.pre_routing` になります。

次の例では、ポート 80 のすべてのリクエストを、まず認可サーバに問い合わせ、次に流量の制限にかけます。

```yaml
# Gateway API v1.7 の experimental の CRD が要る
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
      filters:
        requests:
          # 1. どのルートより先に、認可サーバ（Service ext-authz）に聞く
          - type: ExternalAuth
            externalAuth:
              protocol: HTTP
              backendRef: {name: ext-authz, port: 9001}
              http:
                path: /check
                allowedResponseHeaders: [X-User]
          # 2. 次に、すべてのリクエストに流量の制限
          - type: ExtensionRef
            extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: rate-limit}
---
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: rate-limit, namespace: default}
spec:
  rate_limit: {average: 10, period: 1s, burst: 20}
```

- `ExternalAuth` は、Service `ext-authz` の 9001 番の `/check` に問い合わせ、許されたときだけ先へ進めます。`allowedResponseHeaders` は、認可サーバの応答から転送先へ渡すヘッダです。
- **rproxy 固有**：2 つ目のフィルタは、8.2 と同じ RproxyMiddleware を使っています。ほかの実装では、`ExtensionRef` で指せるものはその実装ごとに違います。

rproxy で使えるものと決まりは次のとおりです（**rproxy 固有**）。

- 使えるフィルタは `ExternalAuth` と `ExtensionRef`（RproxyMiddleware）だけです。リスナーは `HTTP`・`HTTPS` だけです。
- RproxyMiddleware のうち、次のものは使えません。`retry`・`mirror`・`replace_host` は、rproxy の `pre_routing` が受け付けません。サービスを名指す設定（`forward_auth` の `service` など）は、名指せる先がルートのルールだけで、リスナーにはありません（認証は `ExternalAuth` か `forward_auth` の `address` を使います）。Host を変える `headers` は、変えた後でリスナーを選び直すことはしないので断ります。
- rproxy では同じポートのリスナーが 1 つのルールになるので、フィルタはそのポートのすべてのリスナーで同じにしてください。そろっていないと、フィルタを持つリスナーが `Accepted: False`（reason `UnsupportedValue`）になります。
- 参照が解決しない（Service や RproxyMiddleware がない）ときは、リスナーが `ResolvedRefs: False`（reason `InvalidListenerFilterRef`）になり、そのフィルタのところで 500 を返します。認証を飛ばして通すことはしません。
- Gateway の rproxy の Pod がすべて `pre_routing` に対応していると答えるまで、リスナーは `Accepted: False`（reason `UnsupportedValue`、message「not known (yet)」）です。同梱の rproxy v0.4.22 は対応しているので、ふつうは Pod が起動すれば受け付けられます。
- 詳しくは [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 17. にあります。

### 7.4 アドレスの到達範囲

> **Gateway API experimental**：Gateway API v1.7 の experimental の CRD が要ります。

Gateway に、外向きのアドレスに加えて、クラスタの中からだけ届くアドレスを持たせる機能です。同じ Gateway を、外のクライアントとクラスタの中のアプリの両方から使い、中からの通信は外を回らせたくないときに使います。Gateway API v1.7 の experimental の CRD で、`spec.addresses[].routability` を書けます（GEP-5093）。`routability: Cluster` の項目ごとに、コントローラが Gateway の namespace に、クラスタの中だけで使う ClusterIP の Service を作ります。

```yaml
# Gateway API v1.7 の experimental の CRD が要る
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  addresses:
    - type: IPAddress                       # 既定：Service の外向きのアドレス（LoadBalancer）
    - type: IPAddress
      routability: Cluster                  # 加えて、クラスタの中だけの ClusterIP の Service
  listeners:
    - name: http
      port: 80
      protocol: HTTP
```

- 1 つ目の項目は、値も `routability` もないので、ふつうどおり Service の外向きのアドレス（`LoadBalancer`）になります。
- 2 つ目の項目が、クラスタの中だけの ClusterIP の Service になります。

rproxy での決まりは次のとおりです（**rproxy 固有**）。

- 項目がすべて `Cluster` なら、Gateway の Service 自体も `ClusterIP` になり、外には出しません。
- `Cluster` の項目に値（決まった ClusterIP）を書くことは、既定ではできません（`AddressesAssigned: False` になります）。管理者がクラスの `policy.clusterIPs: true` で開ければ使えます。
- 項目の順番を変えると Service が作り直され、ClusterIP が変わります。項目を足すときは末尾に足してください。
- fleet では `Cluster` のアドレスは割り当てません。
- 値のある `type: IPAddress`（決まった外向きのアドレス）は、managed では Service の `externalIPs` になり、chart の `managed.addressCIDRs`（[1.3](#13-主な-chart-の値)）の範囲の内側だけ使えます。詳しくは [PLATFORM.md](PLATFORM.md) にあります。

## 第 3 部 rproxy だけの設定

第 3 部（8.〜12.）は、rproxy-gateway だけの設定と動きです。ここに書いたものは、ほかの Gateway API の実装には持っていけません。それぞれの節の初めに、なぜその設定があるのか（標準の Gateway API で書けないことは何か）と、標準の代わりがあればそれを書いています。

## 8. rproxy の CRD

> **rproxy 固有**：この章の 4 つの CRD は rproxy-gateway の拡張で、ほかの実装には持っていけません。

Gateway API は、どの実装でも同じ意味になることを優先しているので、接続元のアドレスでの制限や流量の制限のように、実装によってできることが違う機能は標準に入っていません。その代わり、実装が独自の機能を足すための決まった入り口を用意しています。ルートのフィルタの `ExtensionRef`、Gateway の `spec.infrastructure.parametersRef`、そしてポリシーを Gateway や Service に結びつける形（Policy Attachment。`targetRefs` で対象を指し、状態を `status.ancestors` に出す形）です。rproxy-gateway の CRD は、これらの入り口を使って rproxy の機能を Kubernetes から使えるようにするものです。

CRD の API グループは `rproxy.max3584.net/v1beta1` です（`v1alpha1` も同じ形で使えますが、非推奨です）。`kubectl get` では短い名前も使えます。

| CRD | 短い名前 | 何に使うか | 標準の入り口 |
|---|---|---|---|
| `RproxyPolicy` | `rppol` | Gateway・リスナー・Service に、接続元の制限・接続数の制限・時間などを足す（[8.1](#81-rproxypolicy)） | Policy Attachment の形 |
| `RproxyMiddleware` | `rpmw` | HTTPRoute・GRPCRoute から、rproxy のミドルウェア（流量の制限・認証など）を使う（[8.2](#82-rproxymiddleware)） | フィルタの `ExtensionRef` |
| `RproxyRule` | `rprule` | Gateway API で書けない rproxy のルールを、そのまま Gateway に足す（[8.3](#83-rproxyrule)） | なし（rproxy-gateway だけのもの） |
| `RproxyGatewayParameters` | `rpgwp` | managed の rproxy の Pod・Service の形（Pod の数・resources・Service の設定など）を変える（[8.4](#84-rproxygatewayparameters)） | Gateway・GatewayClass の `parametersRef` |

### 8.1 RproxyPolicy

RproxyPolicy は、Gateway・リスナー・Service に対して、rproxy の制限や時間の設定を足すためのものです。たとえば「DB のポートには社内からしかつながせない」「つながったまま黙っている DB の接続を長く許す」「この Gateway の転送先の応答は 2 分まで待つ」といったことを書けます。

**なぜ rproxy 固有か**：標準の Gateway API には、接続元のアドレスでの許可、接続数や帯域の上限、L4 の接続の時間、受け身のヘルスチェックを書く項目がありません。標準で書けるのは、HTTPRoute の `timeouts`（[4.5](#45-retry-とタイムアウト)）による HTTP の時間だけです。HTTP の応答を待つ時間を決めたいだけなら、ほかの実装にも持っていける `timeouts` を先に考えてください。

効かせる先は `spec.targetRefs` に書きます。書けるのは、**ポリシーと同じ namespace の** Gateway か Service です。Gateway を書くとそのすべてのリスナーに、`sectionName` も書くとそのリスナーだけに効きます。Service を書くと、その Service へ送るルールに効きます。

書ける項目は次のとおりです。「効くところ」の列は、その項目が rproxy のどのルールに付くかを表します（「ルール」はリスナーのポートごとの rproxy のルール、「HTTP のサービス」は HTTPRoute の転送先ごとの設定です）。

| 項目 | 何を決めるか | 効くところ |
|---|---|---|
| `allowFrom` | 接続を受け付ける接続元のアドレスの範囲（CIDR の一覧）。ほかからの接続は断ります | ルール |
| `limits` | 同時の接続の数や、新しい接続の速さの上限（rproxy の `limits` の形） | ルール |
| `bandwidth` | 帯域の上限（rproxy の `bandwidth` の形） | ルール |
| `geoip` | 国や ASN で接続を許すか決める（rproxy の `geoip` の形。rproxy に GeoIP のデータベースが要ります） | ルール |
| `crowdsec` | CrowdSec の判定で接続を断る（rproxy に `global.crowdsec` の設定が要ります） | ルール |
| `outlierDetection` | 受け身のヘルスチェック。失敗が続く Pod を一時的に外す条件（rproxy の `outlier_detection` の形） | L4 のルール・HTTP のサービス |
| `connectTimeout` | 転送先へつなぐのを待つ時間（100ms〜10m） | L4 の tcp のルール・HTTP のサービス |
| `responseTimeout` | 転送先の応答ヘッダを待つ時間（1h まで） | HTTP のサービス |
| `idleTimeout` | 何も運ばない L4 の接続を閉じるまでの時間（`0s` で閉じない。rproxy の既定は 1h） | L4 の tcp のルール |
| `halfCloseTimeout` | 片側が送り終えた後、もう片側も閉じるまで待つ時間（`0s` で待ち続ける。既定は 1m） | L4 の tcp のルール |

`limits`・`bandwidth`・`geoip`・`outlierDetection` は、rproxy のルールの形（snake_case）をそのまま書きます。形の詳しい説明は、rproxy-api の docs/API.md の「L4 の制限と帯域」「GeoIP」「受け身のヘルスチェック」にあります。

**例 1**：L4 の Gateway `l4`（[5.1](#51-tcproute-と-udproute)）の PostgreSQL のリスナーを、社内のアドレスからだけ受け、接続の数も絞ります。あわせて、Service `postgres` へ送る接続について、黙ったままの接続を 8 時間まで許します。

```yaml
# Gateway の 1 つのリスナー（postgres）：社内の範囲からだけ、接続の数も絞る
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: postgres-listener, namespace: default}
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: l4
      sectionName: postgres
  allowFrom: [10.0.0.0/8, 192.168.0.0/16]
  limits:
    max_connections: 500
    per_source:
      max_connections: 20
      new_connections: {average: 5, period: 1s, burst: 10}
---
# Service postgres に送るルール：つながったまま黙る接続を 8 時間まで許す
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: postgres-service, namespace: default}
spec:
  targetRefs:
    - group: ""
      kind: Service
      name: postgres
  connectTimeout: 2s
  idleTimeout: 8h
  halfCloseTimeout: 1m
```

- 1 つ目は `sectionName: postgres` で、そのリスナーだけに効かせています。`limits` は、リスナー全体で同時 500 接続まで、接続元 1 つあたり同時 20 接続まで、新しい接続は接続元ごとに毎秒平均 5 件（一度に 10 件まで）という意味です。
- 2 つ目は Service を指しているので、どのリスナーから来たかにかかわらず、Service `postgres` へ送る接続に効きます。コネクションプールのように長く黙る接続が、既定の 1 時間で切られないようにしています。

**例 2**：HTTP の Gateway `web` のすべてのリスナーで、転送先の応答を 2 分まで待ち、受け身のヘルスチェックを緩めます。

```yaml
# Gateway web のすべてのリスナー：転送先の応答を 2 分まで待つ、受け身のヘルスチェックを変える
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: web-backends, namespace: default}
spec:
  targetRefs:
    - group: gateway.networking.k8s.io
      kind: Gateway
      name: web
  responseTimeout: 2m
  outlierDetection:
    consecutive_gateway_failures: 5
    consecutive_5xx: 0
    ejection_time: 30s
```

`outlierDetection` は、ゲートウェイの失敗（502・503・504・接続できないなど）が 5 回続いた Pod を、まず 30 秒外す、という設定です（chart の既定は 3 回・10 秒）。`consecutive_5xx: 0` で、アプリ自身が返した 5xx は数えないようにしています（chart の既定と同じ）。これを書かないと、rproxy の既定の `consecutive_5xx: 5` が効き、アプリの 5xx が 5 回続いただけで Pod が外れます。

効き方の決まりは次のとおりです。

- Service を指すと、その Service へ送る L4 のルールに効きます。`outlierDetection`・`connectTimeout`・`responseTimeout` は、その Service を転送先にする HTTP のサービスにも効きます。
- 同じ項目を 2 つのポリシーが決めていたら、古いポリシーが勝ちます。
- `outlierDetection` を書くと、chart の `backends.outlierDetection`（[1.3](#13-主な-chart-の値)）の既定は丸ごと使われなくなります。書かなかった項目は、chart の既定ではなく rproxy 自身の既定になります（HTTP では `consecutive_5xx: 5`（アプリの 5xx も数えます）、`ejection_time: 30s`、`max_ejection_time: 5m` など。rproxy-api の docs/API.md の「受け身のヘルスチェック」）。chart の既定と同じ動きにしたい項目は、自分で書いてください。外すしくみ自体を止めたいときは `outlierDetection: {max_ejected_percent: 0}` と書きます。
- `idleTimeout`・`halfCloseTimeout` は、HTTP・HTTPS のルールと UDP には付きません。HTTP の接続の時間は [8.4](#84-rproxygatewayparameters) の `rproxy.http` で決めます。
- HTTPRoute に `timeouts` を書いたルールには、`responseTimeout` は付きません（ルートに書いたほうが勝ちます）。

ポリシーが効いているかは、`status.ancestors[]` で確かめます。ポリシーが効いた Gateway ごとに、条件が並びます。

```bash
kubectl -n default get rproxypolicy postgres-service -o jsonpath='{range .status.ancestors[*]}{.ancestorRef.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# l4 Accepted=True(Accepted) rproxy.max3584.net/RelayTimeouts=True(Applied)
```

- 時間の書き方が誤っていると、`Accepted: False`（reason `Invalid`）になり、そのポリシーは丸ごと使われません。
- `sectionName` で指したリスナーがないと、`Accepted: False`（reason `TargetNotFound`）になります。

### 8.2 RproxyMiddleware

RproxyMiddleware は、rproxy の HTTP のミドルウェア（リクエストを転送する前後に挟む処理）を、HTTPRoute や GRPCRoute から使うためのものです。流量の制限、接続元のアドレスでの許可、セキュリティのヘッダ、認証など、Gateway API の標準のフィルタにない機能を使えます。

**なぜ rproxy 固有か**：標準のフィルタ（[4.4](#44-フィルタ)）でできるのは、ヘッダの書き換え・リダイレクト・URL の書き換え・写し・CORS と、experimental の `ExternalAuth` です。流量の制限やアドレスでの許可などは標準にないので、標準の拡張点 `ExtensionRef` から RproxyMiddleware を指して使います。認証だけなら、標準の代わりとして `ExternalAuth` フィルタ（**Gateway API experimental**）があります。

`spec` には、rproxy のミドルウェアを 1 つ、`{種類: {設定}}` の形で書きます。使える種類と設定は、rproxy-api の docs/API.md（「v0.3 の設定」）の `http.middlewares` と同じです。ルートからは、フィルタの `type: ExtensionRef` で名前を指して使います。

次の例では、3 つのミドルウェアを作り、`/admin` には社内の範囲だけの制限を、それ以外には流量の制限とセキュリティのヘッダをかけます。

```yaml
# 1 つのクライアントの IP あたり、毎秒平均 10 件・一度に 20 件まで（超えたら 429）
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: rate-limit, namespace: default}
spec:
  rate_limit: {average: 10, period: 1s, burst: 20}
---
# 社内の範囲だけ（ほかは 403）
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: office-only, namespace: default}
spec:
  ip_allow: {source_range: [10.0.0.0/8, 192.168.0.0/16]}
---
# 応答にセキュリティのヘッダ
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: secure-headers, namespace: default}
spec:
  headers: {frame_deny: true, content_type_nosniff: true, referrer_policy: same-origin}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - matches: [{path: {type: PathPrefix, value: /admin}}]
      filters:
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: office-only}
      backendRefs: [{name: admin, port: 8080}]
    - filters:
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: rate-limit}
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: secure-headers}
      backendRefs: [{name: app, port: 8080}]
```

- `extensionRef` の `group`・`kind`・`name` で、使う RproxyMiddleware を指します。1 つのルールに複数書けます。
- 指せるのは、ルートと同じ namespace の RproxyMiddleware だけです。見つからないと、そのルールは 500 を返し、ルートは `ResolvedRefs: False` になります。
- 形や値が rproxy の決まりに合わないと、ルートが `Accepted: False`（reason `UnsupportedValue`、message に理由）になります。そのときも、同じポートのほかのルートはそのまま動きます。
- backendRef ごとの `filters` には、`ExtensionRef` を書けません（`UnsupportedValue` になります）。

**数え直しに注意**：`rate_limit`・`in_flight`・`circuit_breaker` は、rproxy の中でリクエストを数えて判断します。この数は、そのポートの rproxy のルールの `http` の部分が変わると、最初から数え直しになります。転送先の Pod の IP もルールの `http` の中（`services` の `servers`）にあるので、同じポートのどれかのルートを変えたときだけでなく、転送先の Pod が増えたり減ったり入れ替わったり（スケールや rollout）するたびにも、数え直しになります。

**ファイルを指す設定は使いにくい**：ファイルの名前で指す項目（`basic_auth` の `users_file` など）は、その Gateway の証明書のファイルしか指せません（[SECURITY.md](SECURITY.md)）。利用者の一覧のファイルは rproxy に渡せないので、認証には `forward_auth` か Gateway API の `ExternalAuth` を使ってください。

#### 認証（`forward_auth`）

`forward_auth` は、リクエストのたびに認証サーバへ問い合わせ、認証サーバが 2xx を返したときだけ転送先へ通すミドルウェアです。oauth2-proxy や Authelia のようなサインインのしくみをアプリの前に置けます。

認証サーバをクラスタの中で動かすなら、`service` で名指します。名指せるのは、そのミドルウェアを使う HTTPRoute 自身のルール（ルールの `name`、名前がなければ `r<番号>`）だけです。次の例では、oauth2-proxy を同じ HTTPRoute のルール `oauth2` として置き、アプリのルールの前で `login` に問い合わせさせます。

```yaml
# oauth2-proxy に毎回聞き、2xx なら通す。service は同じ HTTPRoute のルールの name
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: login, namespace: default}
spec:
  forward_auth:
    service: oauth2
    path: /oauth2/auth
    response_headers: [X-Auth-Request-User, X-Auth-Request-Email]
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    # oauth2-proxy 自身（サインインの画面とコールバック）
    - name: oauth2
      matches: [{path: {type: PathPrefix, value: /oauth2}}]
      backendRefs: [{name: oauth2-proxy, port: 4180}]
    # アプリ：先に login で確かめる
    - name: app
      filters:
        - type: ExtensionRef
          extensionRef: {group: rproxy.max3584.net, kind: RproxyMiddleware, name: login}
      backendRefs: [{name: app, port: 8080}]
```

- `path` は、認証サーバのどのパスに問い合わせるかです。
- `response_headers` は、認証サーバの応答から転送先へ渡すヘッダです。アプリはこれでサインインした人を知れます。
- 使い方の手順は [14.6](#146-認証のミドルウェアextensionref) にあります。

**クラスタの外の認証サーバ**：クラスタの外の URL に問い合わせる設定（`forward_auth.address`・`oidc.issuer`）は、既定では使えません。テナントが Gateway の Pod から好きな URL へ通信させられないようにするためです。使うには、管理者が GatewayClass の parameters（chart の `managed.parameters`）で、問い合わせてよいホストを許します。

```yaml
# values.yaml
managed:
  parameters:
    policy:
      middlewares:
        outboundHosts: [auth.example.com]       # *、ホスト名、*.example.com、IP の範囲
        # allowedKinds: [headers, rate_limit, forward_auth]   # 使える種類を限るなら
```

- `outboundHosts` には、`*`（どこでも）、ホスト名、`*.example.com` のようなワイルドカード、IP の範囲を書けます。
- `allowedKinds` を書くと、テナントが使えるミドルウェアの種類を限れます（既定はすべて）。

許したホストに問い合わせる RproxyMiddleware は、次のように書きます。

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: authelia, namespace: default}
spec:
  forward_auth:
    address: https://auth.example.com/api/verify
    response_headers: [Remote-User, Remote-Groups]
```

許していないホストを書くと、ルートは `Accepted: False`（reason `UnsupportedValue`、message「auth.example.com is not allowed by the GatewayClass (policy.middlewares.outboundHosts; by default no URL out)」）になります。

### 8.3 RproxyRule

RproxyRule は、rproxy のルールそのもの（制御 API の `POST /rules` の本文と同じ形）を、Gateway のルールセットに足すためのものです。Gateway API では書けないこと、たとえばクラスタの外のサーバへの転送や、rproxy の細かい設定を使いたいときの逃げ道です。

**なぜ rproxy 固有か**：標準の Gateway API のルートで、どの実装でも使える転送先は Kubernetes の Service です。rproxy のルールの細かい項目（PROXY protocol で送信元を渡す `source_ip` など）も書けません。RproxyRule はそれを補う rproxy-gateway だけの逃げ道で、標準の代わりはありません。

次の例では、Gateway `web` の rproxy に、クラスタの外の SSH サーバへの TCP の転送を 1 つ足します。

```yaml
# Gateway web の rproxy に、クラスタの外の SSH サーバへの TCP の転送を 1 つ足す
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyRule
metadata: {name: bastion-ssh, namespace: default}
spec:
  parentRef: {name: web}
  rule:
    protocol: tcp
    listen_addr: 0.0.0.0
    listen_port: 2222
    remote_addr: 192.0.2.50
    remote_port: 22
    allow_from: [203.0.113.0/24]
```

- `parentRef` で、ルールを足す Gateway を指します。`rule` の中は rproxy のルールの形（snake_case）です。この例は、2222 番で受けた接続を `192.0.2.50:22` へ転送し、`203.0.113.0/24` からの接続だけを受けます。
- ルールの待ち受けのポートは、managed の Service にも自動で足されます（ポートの範囲は 100 ポートまで）。
- ほかの namespace の Gateway に足すには、その Gateway の namespace に ReferenceGrant（`from` は `group: rproxy.max3584.net, kind: RproxyRule`、`to` は `kind: Gateway`）が要ります。
- 同じキー（プロトコル・アドレス・ポート）のルールがすでに Gateway にあると、`Accepted: False`（reason `Conflicted`）になります。rproxy がルールを断ると `Accepted: False`（reason `Invalid`）になり、そのときもほかのルールは動きます。

**信頼についての注意**：RproxyRule は、テナントの制限（[SECURITY.md](SECURITY.md)）の外にあります。どんな宛先へのルールでも書けてしまいます。RproxyRule を書ける人を、Gateway の持ち主と同じくらい信頼できないなら、GatewayClass の parameters の `policy.rproxyRules: false` で使えなくしてください。fleet では、既定で RproxyRule を読みません（chart の `fleet.rproxyRules`）。

足したルールが動いているかは、`PROGRAMMED` の列で確かめます。

```bash
kubectl -n default get rproxyrule
# NAME          GATEWAY   PROGRAMMED
# bastion-ssh   web       True
```

### 8.4 RproxyGatewayParameters

RproxyGatewayParameters は、managed の rproxy の Pod と Service の形（Pod の数、resources、Service の設定、rproxy の動き方など）を変えるためのものです。

**なぜ rproxy 固有か**：Gateway API が決めているのは、Gateway・GatewayClass から設定を指す入り口（`parametersRef`）と、`spec.infrastructure` のラベル・注釈までです。指す先の中身は実装ごとに決めることになっていて、RproxyGatewayParameters はその rproxy-gateway 版です。Pod や Service にラベル・注釈を付けたいだけなら、標準の `spec.infrastructure.labels`・`annotations` でも付けられます（ただし、どの注釈を Service に載せるかは rproxy-gateway が絞っています。[12.](#12-セキュリティの基本)）。

使い方は 2 つあります。

- **Gateway ごとに変える**（Gateway の参照）：Gateway と同じ namespace に書き、Gateway の `spec.infrastructure.parametersRef` で指します。その Gateway だけに効きます。
- **すべての Gateway の既定を決める**（クラスの参照）：chart の `managed.parameters` に書きます。テナントが Gateway の参照で何を変えてよいか（`policy`）も、ここで決めます。

まず Gateway ごとに変える例です。Gateway `web` の rproxy を 3 つにし、ゾーンに分散させ、Service の受け付ける接続元を絞り、止まるときに長めに接続を待ちます。

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyGatewayParameters
metadata: {name: web, namespace: default}
spec:
  replicas: 3
  podDisruptionBudget: {maxUnavailable: 1}
  pod:
    resources:
      rproxy: {requests: {cpu: 500m, memory: 128Mi}, limits: {memory: 512Mi}}
    topologySpreadConstraints:
      - {maxSkew: 1, topologyKey: topology.kubernetes.io/zone, whenUnsatisfiable: ScheduleAnyway}
  service:
    externalTrafficPolicy: Local
    loadBalancerSourceRanges: [203.0.113.0/24]
  rproxy:
    logLevel: info
    shutdown: {delay: 15s, drain: 60s}
    http:
      http2MaxStreams: 1000
      timeouts: {writeStall: 2m, upgradedIdle: 0s}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  infrastructure:
    parametersRef:
      group: rproxy.max3584.net
      kind: RproxyGatewayParameters
      name: web
  listeners:
    - name: http
      port: 80
      protocol: HTTP
```

- Gateway の `infrastructure.parametersRef` で、同じ namespace の RproxyGatewayParameters `web` を指しています。ほかの namespace のものは指せません（ReferenceGrant でも開けられません）。
- `rproxy.shutdown.drain: 60s` で、Pod が止まるときに今の接続を 60 秒まで待ちます（既定は 25 秒、[10.3](#103-pod-の入れ替えと-readiness-gate)）。
- `rproxy.http` は、クライアントとの HTTP の接続の上限です。この例では HTTP/2 の同時のストリームを 1000 までにし、応答を 2 分のあいだまったく受け取らないクライアントの接続を閉じ、WebSocket などのアップグレードした接続は黙っていても閉じない（`0s`）ようにしています。

主な項目は次のとおりです（全部は [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 2.2）。右の列は、テナントが Gateway の参照で既定のまま書けるかどうかです。「書けない」項目も、管理者がクラスの参照の `policy.gatewayOverrides` で開ければ書けるようになります（「クラスの参照だけ」と書いたものは開けられません）。

**Pod の数と配置**

| 項目 | 何を決めるか | テナントが書けるか |
|---|---|---|
| `replicas` | rproxy の Pod の数。1 から `policy.maxReplicas`（既定 10）まで | 書ける |
| `podDisruptionBudget` | PDB の `minAvailable` か `maxUnavailable`（片方だけ）。書かなければ、Pod が 2 つ以上のとき `maxUnavailable: 1` | 書ける |
| `pod.resources.rproxy`・`.certsync` | rproxy と certsync（証明書のファイルを用意する脇のコンテナ）の resources | 書ける |
| `pod.labels`・`pod.annotations`・`pod.topologySpreadConstraints` | Pod のラベル・注釈・分散のしかた。分散を書かなければ、Pod が 2 つ以上のときノードに分散します | 書ける |
| `pod.nodeSelector`・`tolerations`・`affinity`・`priorityClassName` | Pod を置くノードと優先度 | 書けない（クラスで開ける） |

`nodeSelector` などが既定で書けないのは、テナントが専用のノードやコントロールプレーンに Pod を載せたり、優先度でほかの Pod を追い出したりできないようにするためです。

**Service**

| 項目 | 何を決めるか | テナントが書けるか |
|---|---|---|
| `service.type` | Service の型（`LoadBalancer`・`NodePort`・`ClusterIP`） | 書けない（クラスで開ける） |
| `service.externalTrafficPolicy` | `Local` か `Cluster`（意味は [1.3](#13-主な-chart-の値) の `managed.externalTrafficPolicy`） | 書ける |
| `service.loadBalancerSourceRanges` | ロードバランサで受け付ける接続元の範囲（`LoadBalancer` のときだけ付きます） | 書ける |
| `service.ipFamilyPolicy`・`labels`・`annotations` | IPv4・IPv6 の扱い、Service のラベル・注釈 | 書ける（アドレスを決める注釈は許可リストにあるものだけ） |
| `service.loadBalancerClass` | どのロードバランサの実装に任せるか。Service を作った後は変えられません | 書けない（クラスで開ける） |

`service.type` と `loadBalancerClass` が既定で書けないのは、外向きのアドレスを取れてしまうためです。`service.type` を変えると Service が変わり、ロードバランサのアドレスも変わります。

**rproxy の動き**

| 項目 | 何を決めるか | テナントが書けるか |
|---|---|---|
| `rproxy.logLevel` | rproxy のログの細かさ（`error`〜`trace`） | 書ける |
| `rproxy.performance` | rproxy の性能の設定（ワーカーの数など） | 書ける |
| `rproxy.shutdown` | 止まるときの `delay`・`drain`（意味は [1.3](#13-主な-chart-の値) の `managed.shutdown`） | 書ける |
| `rproxy.http` | クライアントとの HTTP の接続の上限（`http2MaxStreams`・`timeouts`） | 書ける |
| `rproxy.image`・`rproxy.extraEnv` | rproxy のイメージと、追加の環境変数（`RPROXY_*`） | 書けない（クラスの参照だけ） |
| `proxyProtocol` | 前段のロードバランサが付ける PROXY protocol のヘッダを読む（[PLATFORM.md](PLATFORM.md)） | 書けない（クラスで開ける） |

`rproxy.image` と `extraEnv` は、Gateway の制御 API の資格を持つ Pod で好きなイメージや設定を動かせてしまうので、テナントには開けられません。`proxyProtocol` は、ヘッダでどのクライアントにでも成りすませる接続元を決める設定なので、既定では管理者だけが書けます。

**そのほか**

| 項目 | 何を決めるか | テナントが書けるか |
|---|---|---|
| `ui.visible` | 管理 UI にこの Gateway を見せるか | 書ける（クラスが `false` なら `true` にはできない） |
| `policy` | テナントに何を許すか | 書けない（クラスの参照だけ） |

**すべての Gateway の既定（クラスの参照）**：chart の `managed.parameters` に書くと、chart が `RproxyGatewayParameters` `rproxy-default`（リリースの namespace）を作り、GatewayClass の `parametersRef` から指します。テナントに許すもの（`policy`）は、ここにだけ書けます。次の例では、すべての Gateway の rproxy を 2 つずつにし、テナントが変えてよい項目を絞ります。

```yaml
# values.yaml
managed:
  parameters:
    replicas: 2
    pod:
      resources: {rproxy: {requests: {cpu: 100m, memory: 64Mi}}}
    rproxy:
      extraEnv:
        - {name: RPROXY_MAX_RULES, value: "20000"}
    policy:
      maxReplicas: 5
      # 書くと既定のリストと置き換わる（DESIGN-v0.4.x.md の 2.5）
      gatewayOverrides: [replicas, podDisruptionBudget, pod.resources, pod.tolerations, rproxy.logLevel]
```

- `extraEnv` で rproxy のルールの数の上限（`RPROXY_MAX_RULES`）を上げています。値は文字列なので引用符で囲みます。
- `policy.maxReplicas: 5` で、テナントが `replicas` に書ける上限を 5 にしています。
- `policy.gatewayOverrides` を書くと、書いた項目だけをテナントが書けるようになります（既定の「書ける」のリストと丸ごと置き換わります）。この例では `pod.tolerations` を開け、リストにない既定の項目は閉じています。閉じた項目のうち、上の Gateway `web` の参照が使っているのは `pod.topologySpreadConstraints`・`service.*`・`rproxy.shutdown`・`rproxy.http` です。そのため、このクラスでは、上の Gateway `web` の参照は `InvalidParameters` になります。

値の合わせ方の決まりは次のとおりです。

- 値は、コントローラのフラグ（chart の `managed.*`）→ クラスの参照 → Gateway の参照の順に重ね、後ろのものが勝ちます。
- リストとオブジェクト（`tolerations`・`resources.rproxy` など）は丸ごと置き換わります。ラベルなどのマップは、キーごとに合わせます。
- `v0.4.1` から上げたクラスタでは、`managed.parameters` を書く前に rproxy の CRD を当ててください。`helm upgrade` は新しい CRD を入れません（[13.1](#131-更新)）。

効いたかは、RproxyGatewayParameters と Deployment で確かめます。

```bash
kubectl -n default get rpgwp
# NAME   REPLICAS
# web    3
kubectl -n default get deploy -l gateway.networking.k8s.io/gateway-name=web
```

誤ったときの動きは次のとおりです。

- 許されていない項目や誤った値を書くと、Gateway が `Accepted: False`（reason `InvalidParameters`）になります。message に項目の名前が出ます（例：「spec.service.type: not allowed by the GatewayClass (policy.gatewayOverrides)」）。
- **もう動いている Gateway は止めません**：rproxy の Deployment がすでにある Gateway は、最後に正しかった形のまま動き続けます（`Programmed: True`）。書き間違い 1 つで通信が止まらないようにするためです。直せば、次の反映で新しい形になります。
- `rproxy.http` が効いたかは、Gateway の条件 `rproxy.max3584.net/HTTPSettings` で分かります（reason は `Applied`・`Kept`・`UnsupportedRproxy`）。

**fleet では使えません**：fleet では Gateway の参照は使えません。`parametersRef` を書いた Gateway は `Accepted: False`（reason `InvalidParameters`、message「not used in fleet mode」）になり、受け付けられない Gateway のルールは fleet の Pod に送られません。上の「前の形のまま動かす」は managed だけの動きなので、fleet では**その Gateway の通信が止まります**（変更も届かず、再起動した Pod や新しいノードの Pod には何も載りません）。fleet では `parametersRef` を外してください。

## 9. managed と fleet

> **rproxy 固有**：Gateway API は、Gateway をどう動かすか（プロキシの Pod をどこに置くか）を実装に任せています。managed と fleet は rproxy-gateway の置き方で、標準の代わりはありません。

rproxy-gateway には、rproxy の Pod の置き方が 2 つあります。この章では、その違いと選び方、切り替えるときの注意を説明します。入れる前に一度決めておくのがおすすめです。

- **managed**（既定）：Gateway を 1 つ作るたびに、コントローラがその Gateway 専用の rproxy の Deployment と Service を、Gateway の namespace に作ります。Gateway ごとに Pod も鍵も分かれるので、複数のチームでクラスタを分け合う PaaS に向いています。
- **fleet**：chart が入れる 1 つの DaemonSet（`hostNetwork: true` で、ノードのネットワークを直接使う rproxy）が、すべての Gateway をまとめて受け持ちます。Gateway が増えても Pod は増えません。1 人の管理者が使うクラスタのための形です。

| | managed（既定） | fleet |
|---|---|---|
| rproxy | Gateway ごとに、Deployment と Service を Gateway の namespace に作る | chart の DaemonSet（`hostNetwork: true`）が、すべての Gateway を受け持つ |
| Gateway のアドレス | Service のロードバランサのアドレス（MetalLB・クラウドなど） | ノードの IP か `fleet.addresses`（VIP はプラットフォームが用意する） |
| 鍵・トークン | Gateway ごとに別 | すべての Gateway で共有 |
| テナントを分けるか | 分ける | 分けない（1 人の管理者のため） |
| `RproxyGatewayParameters` | 使える | Gateway の参照は使えない。書くと Gateway が `InvalidParameters` になり、ルールが送られず通信が止まる（DaemonSet の形は chart の `fleet.*` で決める） |
| RproxyRule | 使える | 既定では読まない（`fleet.rproxyRules`） |
| 同じポートを 2 つの Gateway で使う | 使える（Service が別なので） | 古い Gateway が持つ。後の Gateway のリスナーは `Accepted: False`（`PortUnavailable`）。`fleet.listen: addresses` にすれば、アドレスが違えば使える |
| NetworkPolicy | Gateway ごとに作る | 効かない（ノードのファイアウォールで守る） |

**選び方**：ふつうは managed を選んでください。fleet が向いているのは、ロードバランサの実装がない、ノードのアドレスで直接受けたい、1 人で使うクラスタで Pod の数を減らしたい、といったときです。

fleet にする `values.yaml` の例です。

```yaml
# values.yaml
fleet:
  enabled: true
  addresses: [192.0.2.10]    # Gateway の status に書くアドレス（VIP や前段の LB）
  # listen: addresses        # Gateway の spec.addresses ごとに待ち受ける（managed.addressCIDRs の内）
  # nodeSelector: {node-role.example.com/edge: "true"}
```

- `addresses` は、Gateway の `status` に書くアドレスです。プラットフォームが用意した VIP や、前段のロードバランサのアドレスを書きます。省くとノードの IP を書きます。
- `listen: addresses` にすると、Gateway の `spec.addresses` に書いたアドレスだけで待ち受けます（`managed.addressCIDRs` の範囲の内側）。アドレスが違えば、複数の Gateway が同じポートを使えます。
- `nodeSelector` で、rproxy を置くノードを選べます。

前段に置くロードバランサや VIP のヘルスチェックは、`https://<ノードの IP>:9443/readyz` に向けてください。rproxy が止まり始めると 503 を返すので、止まる前に通信を別のノードへ移せます。書き方は [PLATFORM.md](PLATFORM.md) の「fleet」にあります。

**切り替え**：`helm upgrade` で `fleet.enabled` を変えると、置き方が切り替わります。どちらの向きでも通信が途切れ、Gateway のアドレスが変わるので、メンテナンスの時間に、DNS とファイアウォールの変更と合わせて行ってください。

- **managed → fleet**：コントローラが managed の Deployment と Service を消し、fleet の Pod にルールを送ります。Gateway のアドレスは、ロードバランサのアドレスから、ノードの IP（か `fleet.addresses`）に変わります。
  - `parametersRef` を書いた Gateway は `InvalidParameters` になり、ルールが送られず通信が止まります（managed のように前の形のまま動かすことはしません）。**切り替える前に、すべての Gateway の `spec.infrastructure.parametersRef` を外してください**。Pod の形は chart の `fleet.*` で決めます。
  - RproxyRule は、`fleet.rproxyRules: true` にしないと使われません。
- **fleet → managed**：helm が DaemonSet を消し、コントローラが Gateway ごとの Deployment と Service を作ります。アドレスは新しいロードバランサのアドレスになります。

```bash
helm upgrade rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway -n rproxy-gateway-system -f values.yaml
kubectl get gateway -A        # ADDRESS が変わったことを確かめる
```

## 10. 動いているときに変えると

> **rproxy 固有**：変更したときに今の接続がどうなるかは、Gateway API では決まっておらず、実装ごとに違います。この章は rproxy-gateway と rproxy の動きです。状態（condition）の形は標準ですが、reason や message には rproxy 固有のものがあります。

この章では、通信が流れている Gateway やルートを変えたときに、今の接続がどうなるかを説明します。変更の前に「この変更で接続は切れるか」を確かめるのに使ってください。

コントローラは、変更があるたびに Gateway のルールセット全体を描き直して rproxy に送ります（[11.](#11-rproxy-との話し方)）。rproxy は、前のルールセットとの差だけを当てます。rproxy がルール（ポートごとの待ち受け）の差を当てるやり方は 3 つあり、どれになるかで今の接続への影響が決まります（rproxy-api の docs/API.md の「ルールの組」）。

| rproxy の当て方 | 何をするか | 今の接続 |
|---|---|---|
| `in_place` | 待ち受けはそのままに、中身だけをその場で差し替える | 切れない。新しい接続やリクエストから新しい設定が効く |
| `recreate` | 待ち受けを閉じて作り直す | そのポートの接続が切れる |
| 削除 | ルールセットから外れたルールの待ち受けを閉じる | そのポートの接続が切れる |

ほとんどの変更は `in_place` になります。接続が切れるのは、ポートやプロトコルを変えたときと、リスナーや Gateway を消したときです。

### 10.1 よくある変更

よくある変更ごとに、どうなるかをまとめます。

**接続が切れない変更（`in_place`）**

| 変更 | どうなるか |
|---|---|
| ルートを足す・変える・消す（パス・ヘッダ・重み・フィルタ） | 接続は切れません。ただし、そのポートの `rate_limit`・`in_flight`・`circuit_breaker` の数は最初から数え直しになります（[8.2](#82-rproxymiddleware)） |
| 転送先の Pod が増える・減る（スケール・rollout） | 終わる Pod には新しいリクエストを送りません。HTTP のポートでは Pod の IP がルールの `http` の中にあるので、ここでもそのポートの `rate_limit`・`in_flight`・`circuit_breaker` は数え直しになります |
| 証明書の Secret の更新（cert-manager の更新など） | 証明書のファイルの名前が中身のハッシュなので、新しい接続から新しい証明書を使います |
| RproxyPolicy（`allowFrom`・`limits`・時間など） | 次の接続から効きます |
| 同じポートにリスナー（ホスト名）を足す・消す | そのポートのルールの `in_place` なので、ほかのホスト名の接続は切れません |

**接続が切れる変更**

| 変更 | どうなるか |
|---|---|
| リスナーのポートを変える | 古いポートのルールが消え、その接続は切れます。新しいポートのルールができ、managed の Service のポートも変わります |
| リスナー（そのポートの最後の 1 つ）を消す | そのポートのルールが消え、接続が切れます |
| 同じポートのプロトコルを変える（`HTTP` ↔ `TCP` など） | `recreate` になり、そのポートの接続が切れます |
| Gateway を消す | managed では、Gateway が持ち主の Deployment と Service が消え、すべての接続が切れ、アドレスもなくなります。fleet では、その Gateway の rproxy のルールセットが消え、その Gateway の接続が切れます |

**Pod や Service が入れ替わる変更**

| 変更 | どうなるか |
|---|---|
| `RproxyGatewayParameters`（replicas 以外の Pod の形）、`spec.infrastructure` のラベル・注釈 | Pod の rolling update になります（10.3 のしかけで通信は止まりません）。replicas の変更は Pod を足す・減らすだけです |
| `rproxy.http` の値 | Pod は入れ替わりません。rproxy が設定ファイルを読み直します（kubelet がファイルを更新するまで 1 分ほどかかります）。初めて付けたときと、全部外したときだけ rolling update になります |
| `service.type` の変更 | Service が変わり、ロードバランサのアドレスが変わります |
| コントローラの更新（rproxy や certsync のイメージが変わる） | managed のすべての Gateway で rolling update になります |

### 10.2 誤りは状態に出る

rproxy は、受け付けられないルールやルートがあっても、それだけを外して残りを動かします。1 つの誤りで Gateway 全体を止めることはしません。外したものは、次のように Kubernetes のオブジェクトの状態（condition）に出ます。

| 起きたこと | 出るところ |
|---|---|
| rproxy がルートを断った | そのルートが `Accepted: False`（reason `UnsupportedValue`、message に rproxy の答え）。同じポートのほかのルートは動きます |
| rproxy が RproxyRule を断った | その RproxyRule が `Accepted: False`（reason `Invalid`） |
| rproxy がルールセット全体を断った（数の上限など） | Gateway とリスナーが `Programmed: False`（reason `Invalid`、message「rproxy refused the rule set: ...」） |
| 証明書のファイルがまだ Pod に届いていない | `Programmed: False`（reason `Pending`）。届けば `True` になります |
| 待ち受けを開けない（ポートが使われている） | リスナーが `Programmed: False` |
| 参照の誤り（Service や Secret がない） | ルートが `ResolvedRefs: False`（reason `BackendNotFound`・`RefNotPermitted`）、リスナーが `ResolvedRefs: False`（reason `InvalidCertificateRef`） |

断られたものは、中身が変わるか 5 分たつまで送り直しません。直したら、次の反映ですぐに送られます。

### 10.3 Pod の入れ替えと readiness gate

managed の rproxy の Pod は、更新やノードの drain で入れ替わるときも、次のしかけで通信を止めないようにしています（[DESIGN.md](DESIGN.md) の「rproxy の可用性」）。

- **readiness gate**（`rproxy.max3584.net/ruleset-applied`）：readiness gate は、Pod が Ready になるための条件を Kubernetes に足すしくみです。rproxy の Pod は、プロセスが起動しただけでは Ready にならず、コントローラがルールセットを送り終えてこの条件を `True` にしてから Ready になり、Service に入ります。そのため、ルールを持たない起動直後の rproxy に通信が届くことはありません。
- **rolling update**：Deployment の `maxUnavailable: 0` で、新しい Pod が Ready になってから古い Pod を止めます。
- **止まり方**：SIGTERM を受けた rproxy は、`delay`（既定 15 秒）の間は受け付けを続けながら `/readyz` を 503 にし、Service から外れるのを待ちます。その後、待ち受けを閉じ、`drain`（既定 25 秒）まで今の接続が終わるのを待ちます。`drain` を過ぎても続く接続（長い WebSocket や DB の接続）は切れます。長い接続を待ちたいときは、`rproxy.shutdown.drain`（[8.4](#84-rproxygatewayparameters)）か chart の `managed.shutdown` を延ばしてください。
- **PDB と分散**：`replicas` が 2 以上なら、PDB（`maxUnavailable: 1`）とノードへの分散が付き、ノードの drain でも同時に止まる Pod は 1 つまでになります。`replicas` が 1 だと、入れ替えの間に、ただ 1 つの Pod が止まる時間があります。

readiness gate が満たされているかは、Pod の一覧で確かめられます。

```bash
kubectl -n default get pod -l gateway.networking.k8s.io/gateway-name=web -o wide
# READINESS GATES の列が 1/1 なら、ルールセットが載っている
```

### 10.4 再起動したとき

コントローラや rproxy が再起動したときの動きです。

| 再起動するもの | どうなるか |
|---|---|
| コントローラ | rproxy は今のルールのまま動き続けます（通信は止まりません）。新しいリーダーが rproxy のルールセットを読み、違いがあれば送り直します |
| rproxy のコンテナ | ルールセットは rproxy のメモリにだけあるので、消えます。コントローラが readiness gate を `False` に戻して Service から外し、rproxy の `/readyz` を待ってルールセットを送り直し、`True` に戻します |
| rproxy の Pod（削除・drain） | 10.3 の rolling update と同じです |

コントローラが止まっている間は、変更が rproxy に届きません。rproxy は止まる前の形のまま動き続けます。

### 10.5 誰が・どうやって切り替えるか

この節では、ルートの変更や Pod の増減が、どのような流れで rproxy に届くかを説明します。

rproxy に直接指示を出す人はいません。Kubernetes のオブジェクトを変えることが、そのまま指示になります。コントローラは次の種類のオブジェクトを watch していて（`src/controller/cache.rs`）、どれかが変わるたびに、Gateway のルールセットを描き直して送ります。

- Gateway API：GatewayClass・Gateway・HTTPRoute・GRPCRoute・TLSRoute・TCPRoute・UDPRoute・ReferenceGrant・ListenerSet・BackendTLSPolicy
- rproxy の CRD：RproxyMiddleware・RproxyPolicy・RproxyRule・RproxyGatewayParameters
- Kubernetes：Pod（rproxy の Pod の状態と readiness gate）・Service・EndpointSlice・Secret・ConfigMap（中身は参照されたものだけを読みます）・Namespace（`allowedRoutes` の selector のため）・ClusterTrustBundle。移行のときは Ingress と Traefik の CRD も

オブジェクトが変わるきっかけは、たとえば次のとおりです。

| きっかけ | 何が変わるか |
|---|---|
| 人が `kubectl apply` する | Gateway・ルート・rproxy の CRD |
| GitOps（Argo CD・Flux）が Git の中身を当てる | 同じです（コントローラは `kubectl apply` と区別しません） |
| Pod が増える・減る・落ちる・入れ替わる | EndpointSlice が変わり、rproxy の転送先（`servers`）が自動で変わります。人は何もしません |
| cert-manager が証明書を更新する | Secret が変わり、証明書のファイルが自動で替わります |
| 段階的なリリースの道具（Argo Rollouts・Flagger）が重みを変える | HTTPRoute の `backendRefs[].weight` |

段階的なリリースの道具は、Gateway API のプラグインで HTTPRoute を書き換えるだけなので、しくみの上では rproxy-gateway でも動くはずです。ただし、このプロジェクトでは試していません。

道具が重みを 90 / 10 にするときに書き換える内容は、自分で書くと次のようになります。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: web}]
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080, weight: 90}      # 今の版
        - {name: app-v2, port: 8080, weight: 10}   # 新しい版
```

**誰が何を変えられるか**は、Kubernetes の RBAC で決めます。たとえば次のように分けます。

- Gateway（ポート・証明書・アドレス）：インフラの管理者だけが書けるようにします（たとえば `infra` の namespace に置きます）。
- ルート：チームごとの namespace で、そのチームが書けるようにします。どの namespace のルートを受けるかは、管理者が Gateway の `allowedRoutes`（[6.1](#61-allowedroutesgateway-がどの-namespace-のルートを受けるか)）で決めます。
- rproxy の制御 API に書き込めるトークンを持つのは、コントローラだけです（chart の `ui.namespace` を決めたときは、管理 UI が読むだけのトークンを持ちます）。利用者や CI、GitOps の道具は rproxy の API を直接呼ばず、Kubernetes のオブジェクトだけを書きます。

## 11. rproxy との話し方

> **rproxy 固有**：コントローラとプロキシの間のやりとりは、実装ごとに違います。

この章では、コントローラが rproxy にルールをどう渡しているかを説明します。ふだんの運用で意識する必要はありませんが、rproxy のログを読むときや、[15.3](#153-クラスタなしで確かめる) の `render` の出力を読むときの助けになります。

コントローラは、Gateway 1 つを rproxy のルールセット 1 つに変換し、rproxy-api の制御 API で送ります。この API は、VM で動かす rproxy に `curl` で使う API と同じもので、ルールの JSON も同じ形です。

```
PUT https://<rproxy の Pod の IP>:9443/rulesets/k8s/<Gateway の namespace>/<Gateway の名前>
Authorization: Bearer <その Gateway のトークン>
If-Match: <今の etag>
{"generation": <Gateway の metadata.generation>, "rules": [ ... ]}
```

- ルールセットの名前は `k8s/<namespace>/<name>` で、Gateway ごとのトークンはこの名前のルールセットだけを書けます。
- rproxy のルールは、(プロトコル, アドレス, ポート) の組 1 つにつき 1 つです。同じポートのリスナーとルートは、1 つのルールにまとまります。
- 中身が前と同じなら送りません。変更がなくても、`--resync-secs`（既定 30 秒）ごとに描き直して確かめます。

たとえば 2.2 の Gateway `web` と HTTPRoute `app` は、次のルールセットになります（`rproxy-gateway render -f <ファイル>` で、クラスタなしに描けます）。

```json
{
  "rules": [
    {
      "protocol": "tcp",
      "listen_addr": "0.0.0.0",
      "listen_port": 80,
      "http": {
        "routes": [
          {
            "match": "Host(`app.example.com`)",
            "name": "default/app/r0/m0/h0",
            "priority": 1,
            "service": "default/app/r0"
          }
        ],
        "services": {
          "default/app/r0": {
            "servers": [
              {"url": "http://10.244.1.10:3000"},
              {"url": "http://10.244.2.11:3000"}
            ]
          }
        }
      },
      "labels": {
        "app.kubernetes.io/managed-by": "rproxy-gateway",
        "gateway.networking.k8s.io/gateway-name": "web",
        "gateway.networking.k8s.io/gateway-namespace": "default"
      }
    }
  ]
}
```

- ポート 80 のリスナーが、`listen_port: 80` のルール 1 つになっています。HTTPRoute のルールは `http.routes` に、転送先は `http.services` に入ります。名前の `default/app/r0` は「namespace `default` の HTTPRoute `app` の 0 番目のルール」という意味です。
- `servers` は、Service の EndpointSlice に載っている Pod の IP と、Pod のポート（Service の `targetPort`）です。2.1 の Service は 8080 → 3000 なので `:3000` になっています。
- 実際のコントローラは、これに chart の `backends` の既定（サービスの `outlier_detection`、`timeouts.connect: 1s`、`timeouts.response: 30s`）を足して送ります。

**制御 API を通らないもの**：秘密鍵を API に流さないため、また rproxy の Pod が Kubernetes の API を使わずに済むように、次のものはファイルや環境変数で渡します。

| もの | 渡し方 |
|---|---|
| 証明書・鍵（`certificateRefs`、BackendTLSPolicy の CA など） | コントローラが Secret `rproxy-<id>-certs` にまとめ、kubelet が Pod の `/var/run/rproxy-gateway/certs` にファイルとしてつなぎます。ルールは `cert_file` などで、ファイルの名前（中身のハッシュ）を指します。同じ Pod の `certsync` コンテナが、ファイルが揃ったかをコントローラに答えます |
| トークンファイル（`tokens.yaml`。トークンのハッシュだけが入ります）と制御 API の証明書 | Secret `rproxy-<id>-api` をボリュームとしてつなぎます |
| rproxy の設定ファイル（`config.json`。`rproxy.http` を書いたとき） | 同じ `rproxy-<id>-api` の項目と、環境変数 `RPROXY_CONFIG` で渡します |
| ログの段階・性能・止まり方・`extraEnv` | Pod の環境変数で渡します |

fleet では、証明書は Secret `rproxy-fleet-certs`、トークンファイルは Secret `rproxy-gateway-token`、`fleet.http` の設定は ConfigMap `rproxy-fleet-config` で渡します。

## 12. セキュリティの基本

> **rproxy 固有**：この章は rproxy-gateway の守り方です。標準の Gateway API で関係するのは、namespace をまたぐ参照の許可（`allowedRoutes`・ReferenceGrant・`allowedListeners`、[6.](#6-ほかの-namespace)）です。

この章では、rproxy-gateway が Gateway どうし、テナントどうしをどう分けているか、そして運用者が気をつけることを短くまとめます。テナントの分け方の詳しい説明は [SECURITY.md](SECURITY.md) にあります。

- **トークンは Gateway ごと**：managed の rproxy は、Gateway ごとの制御 API の証明書とトークン（コントローラのマスタートークンから導いたもの）で動きます。トークンは、自分の Gateway のルールセット（`k8s/<namespace>/<name>`）しか書けません。そのため、ある Gateway の namespace の Secret を読めても、ほかの Gateway の rproxy には何もできません。マスタートークンと CA の鍵は、コントローラの namespace（`rproxy-gateway-system`）にだけあります。
- **rproxy の Pod は Kubernetes の API を使わない**：rproxy の Pod には ServiceAccount のトークンをつながず、RBAC の権限もありません。rproxy が乗っ取られても、Kubernetes の API には触れません。
- **制御 API の Service はない**：Gateway ごとの Service が持つのは、リスナーのポート（と RproxyRule の待ち受けのポート）だけです。制御 API（9443）と certsync（9444）の Service は作りません。コントローラは Pod ごとに `https://<Pod の IP>:9443` へ直接 PUT します。Pod はそれぞれ自分のルールセットを持つので、Service を通すと 1 つの Pod にしか届かず、正しく動かないためです。利用者が 9443・9444 のための Service や NetworkPolicy を作る必要はありません。
- **NetworkPolicy（managed）**：Gateway ごとに自動で作ります。制御 API（9443）と certsync（9444）にはコントローラの Pod からだけ届き（chart の `ui.namespace` を決めたときは、UI に見せる Gateway の 9443 には UI の Pod からも届きます）、リスナーのポートには誰からでも届きます。CNI が NetworkPolicy に対応していないと効きません。
- **fleet と hostNetwork**：fleet では NetworkPolicy が効きません。制御 API（9443）と certsync（9444）はノードの IP で待ち受けるので、ノードのファイアウォールで、ノードの 9443・9444 への通信を絞ってください。外からだけでなく、クラスタの中の Pod からも hostNetwork の 9443・9444 に届き、NetworkPolicy では止められません。
  - 9443 は、コントローラと、`/readyz` を見るロードバランサのヘルスチェックだけに開けます。9444（certsync。`0.0.0.0` で待ち受けます）は、コントローラだけに開けます。
  - コントローラからの通信は、Pod の IP から来ることが多いです（Pod からノードへの通信の送信元のアドレスは CNI によります）。コントローラの Pod の範囲（かコントローラのノード）を許し、ほかを断ってください。
  - fleet は 1 つの信頼の範囲のためのもので、テナントを分けません。
- **既定で閉じているもの**：次のものは、既定では使えないようにしてあります。開けるときは、信頼できるテナントのクラスだけにしてください。
  - Gateway の `spec.addresses`（chart の `managed.addressCIDRs`）
  - ExternalName の Service（どこでも指せるため）
  - 外の URL に問い合わせるミドルウェア（`policy.middlewares.outboundHosts`）
  - アドレスを決めるロードバランサの注釈（`serviceAnnotationPrefixes`）
- **ほかの namespace の秘密鍵**：ReferenceGrant でほかの namespace の証明書を使うと、managed ではその秘密鍵が Gateway の namespace に写されます。困るなら chart の `controller.crossNamespaceSecrets: false` にしてください。
- **コントローラの権限を絞る**：chart の `controller.watchNamespaces` で見る namespace を限ると、namespace に属するオブジェクトへの権限が、その namespace の Role に絞られます（GatewayClass など namespace に属さないもののための ClusterRole は残ります）。さらに `rbac.admissionPolicy.enabled: true`（Kubernetes 1.30 以上）にすると、ValidatingAdmissionPolicy で、コントローラが書き換えられるものを自分の作ったオブジェクトに限れます。

## 第 4 部 運用と困ったとき

第 4 部（13.〜15.）は、入れた後の運用、よくある構成のレシピ、困ったときの手がかりです。標準の Gateway API と rproxy 固有の話の両方にまたがります。

## 13. 運用

この章では、入れた後の日々の運用（更新、監視、冗長化、バックアップ、ログ）を説明します。

### 13.1 更新

rproxy-gateway を新しい版に上げる手順です。Helm は chart を上げても CRD を更新しないので、rproxy-gateway の CRD を先に自分で当ててから chart を上げます。

```bash
# 1. rproxy の CRD を先に当てる（helm upgrade は CRD を更新しない）
kubectl apply --server-side -f https://github.com/max3584/rproxy-gateway/releases/download/v0.4.22/rproxy.max3584.net.yaml
# 2. chart を上げる
helm upgrade rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway --version 0.4.22 \
  -n rproxy-gateway-system -f values.yaml
# 3. 確かめる
kubectl -n rproxy-gateway-system rollout status deploy/rproxy-gateway
kubectl get gateway -A
```

更新で起きることと、気をつけることは次のとおりです。

- chart の版ごとに、同梱の rproxy の版が決まっています（chart の `rproxy.image.tag`。chart 0.4.22 なら rproxy 0.4.22）。rproxy のイメージ（とコントローラのイメージに入っている certsync）が変わると、managed のすべての Gateway の Pod が rolling update で入れ替わります（10.3）。replicas が 2 以上なら、途切れはふつう 1 秒ほどまでです（[README](../README.md) の「可用性」）。fleet では DaemonSet が入れ替わります。
- 版ごとの変更は、GitHub の Releases のリリースノートで確かめてください。v0.4.x の間は、今の値や Gateway を壊す変更はしません（[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 1.）。
- Kustomize や `install.yaml` で入れたときは、新しい版の `install.yaml` を `kubectl apply --server-side` します（CRD も含まれています）。
- Gateway API の CRD を上げるときは、新しい版の `standard-install.yaml`（か experimental）を `kubectl apply --server-side` します。コントローラは新しい種類を 30 秒ごとに探すので、コントローラの再起動は要りません。Kubernetes の版との組み合わせは [README](../README.md) の「対応する版」で確かめてください。

### 13.2 監視

この節では、何を見ていれば Gateway の異常に気づけるかを説明します。

**状態（conditions）**：まず見るべきものです。次の条件が `True` でないときにアラートを出すようにすると、設定の誤りや反映の失敗に気づけます。

| 対象 | 見る条件 |
|---|---|
| Gateway | `Accepted`、`Programmed` |
| リスナー（`status.listeners[]`） | `Accepted`、`ResolvedRefs`、`Programmed` |
| ルート（`status.parents[]`） | `Accepted`、`ResolvedRefs` |
| RproxyPolicy・BackendTLSPolicy（`status.ancestors[]`） | `Accepted` |
| RproxyRule | `Accepted`、`Programmed` |
| Gateway の rproxy 独自の条件 | `rproxy.max3584.net/HTTPSettings`・`rproxy.max3584.net/ProxyProtocol`（こちらは `False` のときに知らせる） |

`True` でない条件を一覧にするコマンドです。

```bash
# Gateway とリスナーで True でない条件
kubectl get gateways -A -o json | jq -r '.items[] | "\(.metadata.namespace)/\(.metadata.name)" as $n
  | (.status.conditions[]?, (.status.listeners[]? | .conditions[]?))
  | select(.status != "True" and .type != "Conflicted") | "\($n) \(.type)=\(.status) \(.reason): \(.message)"'
# ルートで True でない条件
kubectl get httproutes,grpcroutes,tlsroutes,tcproutes,udproutes -A -o json | jq -r '.items[]
  | "\(.kind) \(.metadata.namespace)/\(.metadata.name)" as $n | .status.parents[]? | .conditions[]?
  | select(.status != "True") | "\($n) \(.type)=\(.status) \(.reason): \(.message)"'
```

リスナーの `Conflicted` だけは `False` が正常なので、1 つ目のコマンドでは除いています。Prometheus で見るなら、kube-state-metrics の Custom Resource State の機能で、これらの条件を指標にできます。

**Pod**：コントローラの Deployment（`rproxy-gateway`）と rproxy の Pod が Ready か、再起動の回数が増えていないかを見ます。rproxy の Pod の READINESS GATES が `0/1` のままなら、ルールセットが載っていません（10.3）。

**ログ**：コントローラのログは 13.5 と [15.2](#152-コントローラのログ) を見てください。rproxy のログは `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy` で読めます（`ruleset.apply`、`conn.*`、`http.access` など。意味は rproxy-api の docs/API.md にあります）。

**rproxy の `/metrics`**：rproxy は制御 API（9443）の `GET /metrics` に、Prometheus の形の指標を出します（`metrics:read` のトークンが要ります）。ただし rproxy-gateway がそのトークンを渡すのは管理 UI（chart の `ui.namespace`）だけで、NetworkPolicy も UI の Pod からだけを通します。Prometheus が直接読む設定は今の版にはないので、指標は管理 UI で見てください。

### 13.3 冗長化とスケール

この節では、コントローラと rproxy を何個動かすか、それぞれどこで決めるかをまとめます。

| もの | 決める値 | どうなるか |
|---|---|---|
| コントローラ | `controller.replicas`（既定 2） | Lease `rproxy-gateway`（コントローラの namespace）で 1 つがリーダーになり、それだけが反映します。リーダーが止まると、15 秒ほどでほかが引き継ぎます（SIGTERM で止まったときはすぐ）。1 つだけで動かすなら `controller.leaderElection: false` にします |
| rproxy（すべての Gateway の既定） | `managed.replicas` か `managed.parameters.replicas` | 2 以上で、PDB（`maxUnavailable: 1`）と、`kubernetes.io/hostname` での分散（topologySpread、`ScheduleAnyway`）が付きます |
| rproxy（Gateway ごと） | `RproxyGatewayParameters` の `replicas`・`podDisruptionBudget`・`pod.topologySpreadConstraints`・`pod.resources` | テナントが書ける `replicas` は `policy.maxReplicas`（既定 10）までです |
| fleet | DaemonSet（ノードごとに 1 つ） | `fleet.nodeSelector`・`tolerations` で、置くノードを選びます |

今のリーダーと、Gateway の PDB は次のように確かめます。

```bash
kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}'   # 今のリーダー
kubectl -n default get pdb -l gateway.networking.k8s.io/gateway-name=web
```

- 本番では rproxy を 2 つ以上にしてください。1 つでは、Pod の入れ替えやノードの drain の間に通信が途切れます。
- 置き方ごとの途切れの実測値は、[README](../README.md) の「可用性」と [PLATFORM.md](PLATFORM.md) にあります。
- fleet の rproxy は、ルールセットを 1000 個（`RPROXY_MAX_RULESETS`）、ルールを 10000 個（`RPROXY_MAX_RULES`）まで持てます。fleet ではすべての Gateway が同じ rproxy に載るので、Gateway がもっと多いときは `fleet.extraEnv` で上限を上げてください（値は `"5000"` のように引用符で囲みます）。

### 13.4 バックアップ

rproxy のルールは rproxy のメモリにだけあり、元になる正しいデータは Kubernetes のオブジェクトです。そのため、バックアップするのは Kubernetes のオブジェクトと chart の値です。何を取ればよいかは次のとおりです。

| もの | 要るか |
|---|---|
| Gateway API のオブジェクト（Gateway・各ルート・ReferenceGrant・ListenerSet・BackendTLSPolicy） | 要る |
| rproxy の CRD のオブジェクト（RproxyPolicy・RproxyMiddleware・RproxyRule・RproxyGatewayParameters） | 要る |
| 証明書の Secret（`certificateRefs` が指すもの）、CA の ConfigMap | 要る（cert-manager が作るものは取り直せます） |
| chart の values（`values.yaml`） | 要る |
| コントローラの Secret（`rproxy-gateway-ca`・`rproxy-gateway-api-tls`・`rproxy-gateway-token`） | なくてもよい。ないとコントローラが作り直し、rproxy の Pod が 1 回入れ替わります。管理 UI の発見の Secret も書き直されます |
| Gateway ごとの `rproxy-<id>-certs`・`rproxy-<id>-api`、rproxy の Deployment・Service | 要らない（コントローラが作ります） |

kubectl で取るなら次のとおりです。

```bash
kubectl get gateways,httproutes,grpcroutes,tlsroutes,tcproutes,udproutes,referencegrants,listenersets,backendtlspolicies -A -o yaml > gateway-api.yaml
kubectl get rproxypolicies,rproxymiddlewares,rproxyrules,rproxygatewayparameters -A -o yaml > rproxy-crs.yaml
```

戻すときは、先に Gateway API の CRD と chart を入れてから、取ったものを当ててください。Velero などで namespace ごとに取るなら、上の種類が含まれるようにします。新しいクラスタに戻すと、Service のロードバランサのアドレスが変わることがあります。アドレスを固定したいなら、ロードバランサの注釈か `spec.addresses` で決めてください（[PLATFORM.md](PLATFORM.md)）。

### 13.5 ログの段階

ログの細かさは、コントローラと rproxy で別々に決めます。

- **コントローラ**：形式は chart の `controller.logFormat`（既定 `json`）で決めます。段階は環境変数 `RPROXY_GATEWAY_LOG`（既定 `info`。`debug` なども使えます）です。調べものの間だけ上げるなら、次のようにします。

  ```bash
  kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG=debug
  ```

  `RPROXY_GATEWAY_LOG` は chart にない値なので、`helm upgrade` をしても消えません（Helm は自分が書いた項目しか直さないためです）。終わったら、`kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG-` で消して戻してください。
- **rproxy**：`RproxyGatewayParameters` の `rproxy.logLevel`（`error`〜`trace`）で決めます（[8.4](#84-rproxygatewayparameters)）。

## 14. レシピ

この章は、よくある構成をそのまま使える形にまとめたものです。各レシピは前の章の説明を組み合わせているので、細かい意味は参照先の節を見てください。

### 14.1 HTTPS の Web アプリ（cert-manager）

2.1 のアプリを、Let's Encrypt の証明書付きの HTTPS で公開し、HTTP で来たものは HTTPS へリダイレクトする構成です。標準の Gateway API と cert-manager だけで書けます。cert-manager の Gateway API への対応を有効にしておいてください（3.2）。

```yaml
apiVersion: cert-manager.io/v1
kind: ClusterIssuer
metadata: {name: letsencrypt}
spec:
  acme:
    server: https://acme-v02.api.letsencrypt.org/directory
    email: admin@example.com
    privateKeySecretRef: {name: letsencrypt-account}
    solvers:
      - http01:
          gatewayHTTPRoute:
            parentRefs:
              - {name: web, namespace: default, kind: Gateway, sectionName: http}
---
apiVersion: cert-manager.io/v1
kind: Certificate
metadata: {name: app-tls, namespace: default}
spec:
  secretName: app-tls
  dnsNames: [app.example.com]
  issuerRef: {name: letsencrypt, kind: ClusterIssuer, group: cert-manager.io}
---
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: web, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: http
      port: 80
      protocol: HTTP
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: to-https, namespace: default}
spec:
  parentRefs:
    - {name: web, sectionName: http}
  hostnames: [app.example.com]
  rules:
    - filters:
        - type: RequestRedirect
          requestRedirect: {scheme: https, statusCode: 301}
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs:
    - {name: web, sectionName: https}
  hostnames: [app.example.com]
  rules:
    - backendRefs:
        - {name: app, port: 8080}
```

中身は、3.2 の ClusterIssuer と Certificate、3.1 の Gateway、3.3 のリダイレクトのルート、そしてアプリのルートです。手順は次のとおりです。

1. 上の YAML を当てます。`kubectl -n default get gateway web` で `ADDRESS` を見て、DNS の `app.example.com` をそのアドレスに向けます。
2. `kubectl -n default get certificate app-tls` が `READY True` になるまで待ちます（その間、`https` のリスナーは `ResolvedRefs: False` です）。
3. `curl -I http://app.example.com/` で 301 が、`curl https://app.example.com/` で 200 が返れば完成です。

### 14.2 DB への TCP（長い idleTimeout）

PostgreSQL のような DB を TCP で公開する構成です。rproxy は、何も運ばない L4 の接続を既定で 1 時間で閉じます。コネクションプールのように、つながったまま長く黙る接続がある DB では、RproxyPolicy の `idleTimeout` を延ばします。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: db, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: postgres
      port: 5432
      protocol: TCP
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: postgres, namespace: default}
spec:
  parentRefs:
    - {name: db, sectionName: postgres}
  rules:
    - backendRefs:
        - {name: postgres, port: 5432}
---
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyPolicy
metadata: {name: db, namespace: default}
spec:
  targetRefs:
    - {group: gateway.networking.k8s.io, kind: Gateway, name: db, sectionName: postgres}
  allowFrom: [10.0.0.0/8]
  idleTimeout: 8h
  halfCloseTimeout: 0s
```

- **rproxy 固有**：RproxyPolicy で、`10.0.0.0/8` からの接続だけを受け、黙った接続を 8 時間まで許し、片側が閉じた接続も時間で閉じないようにしています（[8.1](#81-rproxypolicy)）。
- `kubectl -n default get rproxypolicy db -o yaml` の `status.ancestors[].conditions` に `rproxy.max3584.net/RelayTimeouts: True`（reason `Applied`）が出ていれば効いています。
- DB をクラスタの外に公開するなら、`allowFrom` や `service.loadBalancerSourceRanges`（[8.4](#84-rproxygatewayparameters)）で接続元を絞ってください。
- rproxy の Pod が入れ替わるときは、`drain`（既定 25 秒）を過ぎた接続が切れます。アプリは再接続できるようにしておいてください。

### 14.3 UDP

DNS のように、UDP と TCP で同じポートを使うサービスを公開する構成です。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: dns, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    - name: dns-udp
      port: 53
      protocol: UDP
    - name: dns-tcp
      port: 53
      protocol: TCP
---
apiVersion: gateway.networking.k8s.io/v1
kind: UDPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs:
    - {name: dns, sectionName: dns-udp}
  rules:
    - backendRefs:
        - {name: dns, port: 53}
---
apiVersion: gateway.networking.k8s.io/v1
kind: TCPRoute
metadata: {name: dns, namespace: default}
spec:
  parentRefs:
    - {name: dns, sectionName: dns-tcp}
  rules:
    - backendRefs:
        - {name: dns, port: 53}
```

- 同じ 53 番に UDP と TCP のリスナーを 1 つずつ置き、それぞれに UDPRoute と TCPRoute をつないでいます。
- 転送先の Service `dns` には、TCP と UDP の 53 の両方のポートを書いてください。
- ロードバランサの実装が、1 つの Service での TCP と UDP の混在に対応している必要があります。

**転送先にクライアントのアドレスを渡したいとき**（**rproxy 固有**）：`externalTrafficPolicy: Local`（`LoadBalancer` の既定）で、クライアントのアドレスが届くのは rproxy までです。rproxy はそれを `allowFrom`・`geoip`・ログに使います。しかし、Gateway のルートから作る TCP・UDP のルールは、転送先へ送信元のアドレスを渡しません（`source_ip` を付けません）。そのため、転送先に見えるのは rproxy の Pod の IP です。転送先にクライアントのアドレスを渡すには、RproxyRule（[8.3](#83-rproxyrule)）で rproxy のルールそのものを書き、`source_ip: proxy_v2`（UDP でも使えます）か `proxy_v1`（TCP だけ）を付けます。転送先が PROXY protocol を読める必要があります（rproxy-api の docs/API.md の「ルール」）。RproxyPolicy には、送信元を渡す項目はありません。

### 14.4 443 で SNI の passthrough

同じ 443 番で、ある名前は rproxy が復号して HTTP で振り分け、別の名前は復号せずに転送先へそのまま送る構成です。証明書を Gateway に預けたくないサービスと、ふつうの Web アプリを、同じアドレス・同じポートで公開できます。

```yaml
apiVersion: gateway.networking.k8s.io/v1
kind: Gateway
metadata: {name: edge, namespace: default}
spec:
  gatewayClassName: rproxy
  listeners:
    # app.example.com は rproxy が復号して HTTP で振り分ける
    - name: https
      port: 443
      protocol: HTTPS
      hostname: app.example.com
      tls:
        certificateRefs:
          - name: app-tls
    # secure.example.com は復号せずにそのまま転送先へ
    - name: passthrough
      port: 443
      protocol: TLS
      hostname: secure.example.com
      tls:
        mode: Passthrough
---
apiVersion: gateway.networking.k8s.io/v1
kind: HTTPRoute
metadata: {name: app, namespace: default}
spec:
  parentRefs: [{name: edge, sectionName: https}]
  hostnames: [app.example.com]
  rules:
    - backendRefs: [{name: app, port: 8080}]
---
apiVersion: gateway.networking.k8s.io/v1
kind: TLSRoute
metadata: {name: secure-app, namespace: default}
spec:
  parentRefs: [{name: edge, sectionName: passthrough}]
  hostnames: [secure.example.com]
  rules:
    - backendRefs: [{name: secure-app, port: 8443}]
```

- rproxy は 443 番を 1 つのルールで受け、SNI が `secure.example.com` の接続だけを復号せずに送ります（[5.2](#52-tlsroutepassthrough-と-terminate)）。
- `secure.example.com` の証明書は、`secure-app` の Pod が持ちます。
- 確かめるには、`openssl s_client -connect $ADDR:443 -servername secure.example.com` で `secure-app` の証明書が、`-servername app.example.com` で `app-tls` の証明書が見えることを見ます。

### 14.5 重みでカナリア

新しい版のアプリへ、少しずつリクエストを移していく手順です（重みの書き方は [4.3](#43-重みとカナリア)）。

1. 新しい版 `app-v2` の Deployment と Service を作ります。
2. 4.3 の HTTPRoute で、`app: 90`・`app-v2: 10` を当てます。
3. 状態とエラーを見ながら、重みを書き換えて当て直します（`90/10` → `50/50` → `0/100`）。どの段階でも接続は切れません。
4. 問題があって戻すときは、`app-v2` の重みを 0 にします。

重みだけを書き換えるコマンドと、実際の割合を見るコマンドです。

```bash
# 重みだけ変える（rules[0] の 2 つの backendRefs）
kubectl -n default patch httproute app --type=json -p '[
  {"op": "replace", "path": "/spec/rules/0/backendRefs/0/weight", "value": 50},
  {"op": "replace", "path": "/spec/rules/0/backendRefs/1/weight", "value": 50}]'
# 割合を見る（echo は Pod の名前を返す）
for i in $(seq 100); do curl -s -H 'Host: app.example.com' http://$ADDR/ | jq -r .pod; done | sort | uniq -c
```

試す人だけを新しい版へ送るカナリア（ヘッダで選ぶ形）は、4.2 の `headers` の一致で書けます。

### 14.6 認証のミドルウェア（ExtensionRef）

> **rproxy 固有**：RproxyMiddleware を使います。標準の Gateway API で書くなら、`ExternalAuth` フィルタ（**Gateway API experimental**）を使います。

oauth2-proxy で、アプリの前にサインインを置く構成です。8.2 の RproxyMiddleware `login`（`forward_auth`）と HTTPRoute をそのまま使います。

1. oauth2-proxy を Service `oauth2-proxy`（ポート 4180）で動かします。リダイレクトの URL は `https://app.example.com/oauth2/callback` にし、`--reverse-proxy` と `--set-xauthrequest` を付けます（oauth2-proxy の文書で確かめてください）。
2. 8.2 の RproxyMiddleware `login` と HTTPRoute `app` を当てます（Gateway は 14.1 の `web`）。
3. サインインしていないと、oauth2-proxy の `/oauth2/auth` が 401 を返し、それがそのままクライアントに返ります（rproxy は、認証サーバの 2xx 以外の答えをそのまま返します）。サインインは `https://app.example.com/oauth2/start` から始めます。ブラウザを自動でサインインの画面へ送る形は、oauth2-proxy の文書（forward auth での使い方）で確かめてください。

補足です。

- `forward_auth` の `service` には、同じ HTTPRoute のルールの `name`（ここでは `oauth2`）を書きます。ほかのルートのルールは名指せません。
- クラスタの外の認証サーバなら、`service` の代わりに `address` を使い、GatewayClass の `policy.middlewares.outboundHosts` でそのホストを許します（8.2）。
- Gateway API の `ExternalAuth` フィルタ（experimental の CRD）でも同じことができます（rproxy では `forward_auth` になります）。

### 14.7 リスナーのフィルタ

> **Gateway API experimental**（リスナーのフィルタ）と **rproxy 固有**（流量の制限の RproxyMiddleware）を組み合わせます。

ホスト名やパスにかかわらず、そのポートのすべてのリクエストに認証と流量の制限をかける構成です。7.3 の YAML をそのまま使います（Gateway API v1.7 の experimental の CRD が要ります）。

当てたら、リスナーが受け付けられたかと、ルートのないパスでも認証が先に働くかを確かめます。

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# http Accepted=True(Accepted) ResolvedRefs=True(ResolvedRefs) Conflicted=False(NoConflicts) Programmed=True(Programmed)
curl -i -H 'Host: anything.example.com' http://$ADDR/no-such-path
# 認可サーバが断れば、ルートがなくても 403 などが先に返る
```

- ルートの `ExtensionRef` とリスナーに同じ RproxyMiddleware を書くと、別々に 2 回働きます。
- 認可サーバの Service を消すと、リスナーが `ResolvedRefs: False`（reason `InvalidListenerFilterRef`）になり、500 を返します。認証なしで通すことはしません。

## 15. 困ったとき

この章では、うまく動かないときに見るところと、よく出る状態の意味、直し方をまとめます。まず 15.1 の表で状態の reason を探し、それでも分からなければ 15.2 のコントローラのログを見てください。

### 15.1 よく見る状態

`kubectl describe` などで見える条件（condition）と reason ごとに、意味とすることをまとめます。

**GatewayClass と Gateway**

| どこ | 条件（reason） | 意味 | すること |
|---|---|---|---|
| GatewayClass | `Accepted: False`（`InvalidParameters`） | クラスの parameters（chart の `managed.parameters`）が誤っている | message に出る項目を直します。クラスの parameters は、コントローラの namespace のものしか指せません |
| Gateway | `Programmed: False`（`AddressNotAssigned`） | Service にアドレスが付かない | ロードバランサの実装を入れる（[PLATFORM.md](PLATFORM.md)）か、`managed.serviceType` を変えます。このとき `kubectl get svc` の `EXTERNAL-IP` は `<pending>` です |
| Gateway | `Accepted: False`（`InvalidParameters`） | `RproxyGatewayParameters` が誤っているか、許されていない項目を書いた | message に出る項目を直します。すでに動いていた Gateway は前の形のまま動きます（`Programmed: True`） |
| Gateway | `Accepted: False`（`UnsupportedAddress`） | `spec.addresses` の型が `IPAddress` でない | `IPAddress` にします |
| Gateway | `Programmed: False`（`AddressNotUsable`） | `spec.addresses` が `managed.addressCIDRs` の外か、使えない IP | 管理者に範囲を開けてもらうか、アドレスを直します |
| Gateway・リスナー | `Programmed: False`（`Invalid`、「rproxy refused the rule set: 409 limit ...」） | rproxy の数の上限に当たった | `RPROXY_MAX_RULESETS`・`RPROXY_MAX_RULES` を上げます（[DESIGN.md](DESIGN.md) の「rproxy の置き方」） |
| Gateway・リスナー | `Programmed: False`（`Pending`） | 反映を待っている（証明書のファイル、Pod の起動） | 1〜2 分待ちます。続くならコントローラのログを見ます |
| Gateway・リスナー | `Programmed: False`（「rproxy ... has no rule sets (features.rulesets; rproxy v0.4.0 or later)」） | rproxy の版が古い（`rproxy.image` を変えた） | イメージを chart の版のものに戻します |

**リスナー**

| 条件（reason） | 意味 | すること |
|---|---|---|
| `ResolvedRefs: False`（`InvalidCertificateRef`） | 証明書の Secret がない、または `tls.crt`・`tls.key` がない | Secret を作ります（cert-manager なら Certificate の状態を見ます） |
| `ResolvedRefs: False`（`RefNotPermitted`） | ほかの namespace の Secret を使っているのに ReferenceGrant がない | Secret の namespace に ReferenceGrant を置きます |
| `ResolvedRefs: False`（`InvalidRouteKinds`） | `allowedRoutes.kinds` に、そのプロトコルで受けられない種類を書いた | プロトコルに合う種類にします（[5.3](#53-ルートごとのリスナー)） |
| `Accepted: False`（`PortUnavailable`） | fleet で、古い Gateway が同じポートを持っている | ポートを変えるか、`fleet.listen: addresses` にします |
| `Accepted: False`（`UnsupportedProtocol`） | 知らないプロトコルを書いた | `HTTP`・`HTTPS`・`TLS`・`TCP`・`UDP` のどれかにします |
| `Accepted: False`（`UnsupportedValue`） | リスナーのフィルタが使えないか、同じポートでそろっていない | message を見ます（[7.3](#73-リスナーのフィルタ)） |

**ルート**

| 条件（reason） | 意味 | すること |
|---|---|---|
| `Accepted: False`（`NotAllowedByListeners`） | リスナーの `allowedRoutes` が、ルートの namespace か種類を許していない | Gateway の `allowedRoutes` と、ルートの `sectionName` を見直します |
| `Accepted: False`（`NoMatchingListenerHostname`） | ルートとリスナーのホスト名に重なりがない | ホスト名を直します |
| `Accepted: False`（`NoMatchingParent`） | `parentRefs` の Gateway やリスナーがない | 名前・namespace・`sectionName` を直します |
| `Accepted: False`（`UnsupportedValue`） | rproxy が受け付けない値、RproxyMiddleware の誤り、許されていない外への URL | message に出る項目を直します |
| `ResolvedRefs: False`（`BackendNotFound`） | Service がない | Service の名前とポートを直します。直るまで、その backend の分は 500 になります |
| `ResolvedRefs: False`（`RefNotPermitted`） | ほかの namespace の Service を使っているのに ReferenceGrant がない | Service の namespace に ReferenceGrant を置きます（[6.2](#62-referencegrantほかの-namespace-の-servicesecret-を使う)） |

**ポリシーと RproxyRule**

| どこ | 条件（reason） | 意味 | すること |
|---|---|---|---|
| RproxyPolicy | `Accepted: False`（`Invalid`・`TargetNotFound`） | 時間の書き方の誤り、または指したリスナーがない | 値か `sectionName` を直します |
| RproxyRule | `Accepted: False`（`Conflicted`・`Invalid`・`NotAllowed`） | 同じキーのルールがある、rproxy が断った、クラスが RproxyRule を切っている | ポートを変える、message を見る、`policy.rproxyRules` を見ます |
| BackendTLSPolicy | `Accepted: False`（`NoValidCACertificate`・`Conflicted`） | 使える CA がない、または同じ対象により古いポリシーがある | CA の ConfigMap（`ca.crt`）を直すか、ポリシーを 1 つにします |

**状態は正常なのに、通信がおかしいとき**

- **404 が返る**：ホスト名（`Host` ヘッダ）がどのルートにも合っていません。`curl -H 'Host: ...'` でホスト名を変えて試してください。
- **500 が返る**：送れる backend がありません。Service の Pod が Ready でないか、ルートが `BackendNotFound`・`RefNotPermitted` です。
- **502・504 が返る**：転送先につながらないか、応答が遅すぎます。`backends.connectTimeout`・`responseTimeout`（[1.3](#13-主な-chart-の値)）を見てください。
- **つながらない（タイムアウトする）**：Service の `EXTERNAL-IP`、ファイアウォール、`loadBalancerSourceRanges`、RproxyPolicy の `allowFrom` を見直してください。rproxy の Pod が Ready か（READINESS GATES）も見ます。

### 15.2 コントローラのログ

状態だけでは原因が分からないときは、コントローラのログを読みます。コントローラは 2 つ動いていますが、反映しているのはリーダーだけなので、リーダーのログを読みます。

```bash
# リーダーのログ（2 つのうちリーダーだけが反映する）
LEADER=$(kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}')
kubectl -n rproxy-gateway-system logs $LEADER --since=1h
# info 以外だけを読みやすく
kubectl -n rproxy-gateway-system logs $LEADER --since=1h \
  | jq -r 'select(.level != "INFO") | "\(.timestamp) \(.level) \(.fields.message) \(.fields | del(.message) | tostring)"'
```

ログは 1 行に 1 つの JSON です（`timestamp`・`level`・`fields.message` と、ほかの項目）。よく見る message と、その意味は次のとおりです。

| message | 意味 |
|---|---|
| `became the leader`・`lost the Lease to another replica` | リーダーが替わった（コントローラの入れ替えではふつうのこと） |
| `could not renew the Lease in time; stepping down` | API サーバに届かず、リーダーを降りた（API サーバや etcd の遅れ） |
| `rproxy refused a rule; applying the others` | rproxy がルールを 1 つ断った。状態にも出ます |
| `rproxy refused routes; applying the others` | rproxy がルートを断り、それを外して残りを当てた |
| `rproxy refused the rule set` | ルールセット全体を断られた（数の上限など） |
| `listener port taken by another Gateway` | fleet で、ほかの Gateway とポートを取り合っている |
| `rule not running` | rproxy でルールが動いていない（ポートを開けない、名前を解決できないなど） |
| `cannot deploy rproxy` | Deployment や Service を作れない（RBAC、admission、ResourceQuota） |
| `reconcile failed` | 反映が 1 回失敗した。続けてやり直します |
| `not served by the API server (CRD not installed); looked for again later` | その種類の CRD がない。入れれば 30 秒ほどで見つけます |
| `migration: not converted` | 移行で変換できなかったものがある（[MIGRATION.md](MIGRATION.md)） |

もっと詳しく見たいときは、13.5 の手順で `RPROXY_GATEWAY_LOG=debug` にします。rproxy の側のログは `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy` で、証明書のファイルの受け渡しのログは `-c certsync` で読めます。

### 15.3 クラスタなしで確かめる

`rproxy-gateway render` を使うと、マニフェストのファイルから、rproxy のルールセットと状態を描けます。クラスタも rproxy も要らないので、当てる前に YAML を確かめたいときや、変換の結果を見たいときに使います。GatewayClass・Service・EndpointSlice も同じファイルに入れてください。

```bash
docker run --rm -v $PWD:/w ghcr.io/max3584/rproxy-gateway:0.4.22 render -f /w/manifests.yaml
```

出力の `listeners`・`routes` の `conditions` には、クラスタで出るのと同じ reason が出ます。ただし、rproxy の Pod に聞いて決める項目（たとえばリスナーのフィルタ）は、「not known (yet)」になります。
