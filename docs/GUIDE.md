English: [en/GUIDE.md](en/GUIDE.md)

# rproxy-gateway 設定説明書

rproxy-gateway で Gateway を作り、通信を流し、運用するための手引き。やりたいことごとに、そのまま貼れる YAML と、確かめるコマンドを並べる。

- 決めごとの詳しいところは [DESIGN.md](DESIGN.md)・[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md)、テナントの分け方は [SECURITY.md](SECURITY.md)、アドレスの用意（MetalLB など）は [PLATFORM.md](PLATFORM.md)、Ingress・Traefik からの移行は [MIGRATION.md](MIGRATION.md)。
- 例の名前（`app.example.com`、`default` の namespace、Service `app`）とアドレス（`192.0.2.0/24`・`203.0.113.0/24`）は文書用。自分のものに置き換える。
- 例はこの版（v0.4.22、同梱の rproxy v0.4.22）で確かめた。YAML はすべて CRD のスキーマで確かめ、`rproxy-gateway render` で変換が通ることを確かめた。

## 目次

0. [Gateway API のあらまし](#0-gateway-api-のあらまし)
1. [入れる](#1-入れる)
2. [はじめての Gateway と HTTPRoute](#2-はじめての-gateway-と-httproute)
3. [HTTPS](#3-https)
4. [HTTP の振り分け](#4-http-の振り分け)
5. [L4 の転送](#5-l4-の転送)
6. [ほかの namespace](#6-ほかの-namespace)
7. [rproxy の CRD とそのほかの設定](#7-rproxy-の-crd-とそのほかの設定)
8. [managed と fleet](#8-managed-と-fleet)
9. [動いているときに変えると](#9-動いているときに変えると)
10. [rproxy との話し方](#10-rproxy-との話し方)
11. [セキュリティの基本](#11-セキュリティの基本)
12. [運用](#12-運用)
13. [レシピ](#13-レシピ)
14. [困ったとき](#14-困ったとき)

## 0. Gateway API のあらまし

Gateway API は Kubernetes の「外からの通信をどこへ送るか」を書くための API。役割ごとにリソースが分かれる。

| リソース | 誰が書く | 中身 |
|---|---|---|
| `GatewayClass` | クラスタの管理者（chart が `rproxy` を作る） | どのコントローラが受け持つか |
| `Gateway` | Gateway の持ち主 | 待ち受け（リスナー）：ポート・プロトコル・ホスト名・証明書 |
| `HTTPRoute`・`GRPCRoute`・`TLSRoute`・`TCPRoute`・`UDPRoute` | アプリの持ち主 | どのリクエスト・接続をどの Service へ送るか |
| `ReferenceGrant` | 参照される側の namespace の持ち主 | ほかの namespace から自分の Service・Secret を使ってよいという許可 |

```
クライアント ──▶ Gateway のリスナー（:80 / :443）──▶ ルート（ホスト名・パス）──▶ Service の Pod
                  └─ rproxy の Pod（Gateway ごと）が待ち受ける
```

rproxy-gateway はこれらを読んで rproxy のルールにし、Gateway ごとの rproxy に送る。rproxy は Kubernetes を知らない（[10.](#10-rproxy-との話し方)）。

## 1. 入れる

### 1.1 Gateway API の CRD

Gateway API の CRD は chart に入っていない。先に入れる。channel は 2 つある。

| channel | 入るもの（v1.6.3） |
|---|---|
| standard | GatewayClass・Gateway・HTTPRoute・GRPCRoute・TLSRoute・TCPRoute・UDPRoute・ListenerSet・ReferenceGrant・BackendTLSPolicy |
| experimental | standard のすべてと、まだ実験中の項目 |

experimental の CRD が要るもの：

| 機能 | 要る CRD |
|---|---|
| HTTPRoute の `retry` | v1.6 は experimental。v1.7 から standard |
| HTTPRoute の `ExternalAuth` フィルタ | experimental |
| リスナーのフィルタ（`filters.requests`、[7.5](#75-リスナーのフィルタ)） | v1.7 の experimental |
| `spec.addresses[].routability`（[7.8](#78-アドレスの到達範囲)） | v1.7 の experimental |

```bash
# standard（ふつうはこれで足りる）
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/standard-install.yaml
# experimental（上の表の機能を使うなら。v1.7 の機能には v1.7.0 の experimental-install.yaml）
kubectl apply --server-side -f https://github.com/kubernetes-sigs/gateway-api/releases/download/v1.6.3/experimental-install.yaml
```

Kubernetes と Gateway API の版の組み合わせは [README](../README.md) の「対応する版」。Gateway API v1.6 には Kubernetes 1.32 以上が要る。

確かめる：

```bash
kubectl get crd | grep gateway.networking.k8s.io
```

### 1.2 helm install

```bash
helm install rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway \
  -n rproxy-gateway-system --create-namespace -f values.yaml
```

`-f values.yaml` は省いてよい（既定の値で動く）。chart は rproxy の CRD（`rproxy.max3584.net`）、RBAC、コントローラ（2 レプリカ）、GatewayClass `rproxy` を入れる。

確かめる：

```bash
kubectl -n rproxy-gateway-system get pods
# NAME                              READY   STATUS    RESTARTS   AGE
# rproxy-gateway-6c9d8f7b5d-4xk2p   1/1     Running   0          1m
# rproxy-gateway-6c9d8f7b5d-9qm7r   1/1     Running   0          1m
kubectl get gatewayclass rproxy
# NAME     CONTROLLER                              ACCEPTED   AGE
# rproxy   rproxy.max3584.net/gateway-controller   True       1m
```

`ACCEPTED` が `True` なら、コントローラが GatewayClass を受け持っている。名乗っている機能は `kubectl get gatewayclass rproxy -o jsonpath='{.status.supportedFeatures[*].name}'` で見られる。

Helm を使わずに入れるとき（`install.yaml`、Kustomize）は [README](../README.md) の「Helm を使わずに入れる」。

### 1.3 主な chart の値

全部は [values.yaml](../charts/rproxy-gateway/values.yaml) にコメントつきである。

| 値 | 既定 | 中身 |
|---|---|---|
| `controller.replicas` | `2` | コントローラの数。Lease で 1 つがリーダーになる |
| `managed.replicas` | `1` | Gateway ごとの rproxy の Pod の数。2 以上で PDB とノードへの分散が付く |
| `managed.serviceType` | `LoadBalancer` | Gateway ごとの Service の型（`LoadBalancer`・`NodePort`・`ClusterIP`） |
| `managed.externalTrafficPolicy` | `""` | 空なら `LoadBalancer` は `Local`、`NodePort` は `Cluster` |
| `managed.allocateLoadBalancerNodePorts` | `true` | MetalLB のようにノードのポートを使わない LB なら `false` |
| `managed.addressCIDRs` | `[]` | Gateway の `spec.addresses` に使ってよい範囲。空なら使えない |
| `managed.networkPolicy` | `true` | Gateway ごとの NetworkPolicy（[11.](#11-セキュリティの基本)） |
| `managed.shutdown` | `{delay: 15s, drain: 25s}` | rproxy の止まり方（[9.](#9-動いているときに変えると)） |
| `managed.parameters` | `{}` | すべての Gateway の既定の `RproxyGatewayParameters` とテナントへの許可（[7.4](#74-rproxygatewayparameters)） |
| `fleet.enabled` | `false` | fleet にする（[8.](#8-managed-と-fleet)） |
| `backends.connectTimeout` | `{http: 1s, l4: 1s}` | 転送先の Pod への接続の時間 |
| `backends.responseTimeout` | `30s` | HTTPRoute の転送先の応答ヘッダを待つ時間（ルートに `timeouts` がないとき） |
| `backends.outlierDetection` | （値あり） | 受け身のヘルスチェック。失敗した Pod を外す |
| `controller.watchNamespaces` | `[]` | 見る namespace を限る（権限も Role に限る） |
| `controller.crossNamespaceSecrets` | `true` | ほかの namespace の証明書の Secret を ReferenceGrant 付きで使う |
| `ui.namespace` | `""` | 管理 UI に Gateway の rproxy を見せる |
| `migration.migrateTo` | `""` | Ingress・Traefik からの移行（[MIGRATION.md](MIGRATION.md)） |

例：rproxy を 2 つずつ置き、MetalLB を使う。

```yaml
# values.yaml
managed:
  replicas: 2
  allocateLoadBalancerNodePorts: false
```

## 2. はじめての Gateway と HTTPRoute

### 2.1 転送先のアプリ

例に使うアプリ。Gateway API の試験に使われる echo（受けたリクエストを JSON で返す）。

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

### 2.2 Gateway と HTTPRoute

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

- `parentRefs` でつなぐ Gateway を指す。`hostnames` は Host ヘッダで選ぶ名前（省けばすべて）。
- `backendRefs` は Service とそのポート。rproxy は Service の ClusterIP ではなく、EndpointSlice の ready な Pod の IP に直接送り、振り分けとヘルスチェックをする。

### 2.3 何ができるか

managed（既定）では、コントローラが Gateway の namespace に次を作る。`<id>` は `<namespace>-<name>` とハッシュ 6 桁（例 `default-web-1a2b3c`）。どれも Gateway が持ち主で、Gateway を消せば消える。

| もの | 名前 |
|---|---|
| rproxy の Deployment・ServiceAccount | `rproxy-<id>` |
| Service（`LoadBalancer`） | `rproxy-<id>` |
| 証明書の Secret | `rproxy-<id>-certs` |
| 制御 API の Secret（証明書・トークンファイル） | `rproxy-<id>-api` |
| NetworkPolicy | `rproxy-<id>` |

```bash
kubectl -n default get deploy,svc,pod -l gateway.networking.k8s.io/gateway-name=web
```

### 2.4 確かめる

```bash
kubectl -n default get gateway web
# NAME   CLASS    ADDRESS        PROGRAMMED   AGE
# web    rproxy   192.0.2.100    True         40s
kubectl -n default get httproute app
# NAME   HOSTNAMES             AGE
# app    ["app.example.com"]   40s
```

- `ADDRESS` が Gateway のアドレス。`LoadBalancer` の Service に付いたもの（`ClusterIP` 型なら ClusterIP）。
- `PROGRAMMED` が `True` なら、rproxy の Pod の 1 つ以上にルールが載った。

アドレスだけを取り出して試す：

```bash
ADDR=$(kubectl -n default get gateway web -o jsonpath='{.status.addresses[0].value}')
curl -H 'Host: app.example.com' http://$ADDR/
```

`kubectl describe` で状態を読む：

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

（表示は短くしてある。）ルートの状態は `kubectl -n default describe httproute app` の `Status.Parents`（Gateway ごと）に `Accepted` と `ResolvedRefs` が出る。

`ADDRESS` が空で `PROGRAMMED` が `False`（`AddressNotAssigned`、「waiting for an address (the rproxy Service)」）なら、クラスタに `LoadBalancer` のアドレスを配るもの（MetalLB・クラウドの LB など）がない。[PLATFORM.md](PLATFORM.md) で用意するか、`managed.serviceType: NodePort` / `ClusterIP` にする。

## 3. HTTPS

### 3.1 TLS の Secret を certificateRefs で

証明書と鍵を `kubernetes.io/tls` の Secret にする。

```bash
kubectl -n default create secret tls app-tls --cert=app.crt --key=app.key
```

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

- `HTTPS` のリスナーは rproxy が TLS を終える（`mode: Terminate`、省いても同じ）。同じポートに複数の `HTTPS` のリスナー（ホスト名ごとに証明書）を書ける。rproxy が SNI で証明書を選ぶ。
- `sectionName` はリスナーの名前。省けば、そのルートを受けられるすべてのリスナーにつながる。
- 鍵は制御 API を通らない。コントローラが `rproxy-<id>-certs` に写し、kubelet が rproxy の Pod にファイルとして渡す（[10.](#10-rproxy-との話し方)）。
- 証明書の Secret を書き換えると（更新）、接続を切らずに新しい証明書に替わる（[9.](#9-動いているときに変えると)）。

確かめる：

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{.conditions[?(@.type=="ResolvedRefs")].reason}{"\n"}{end}'
# http ResolvedRefs
# https ResolvedRefs
curl --resolve app.example.com:443:$ADDR https://app.example.com/
```

Secret がない・壊れているとリスナーは `ResolvedRefs: False`（`InvalidCertificateRef`）・`Programmed: False` になり、そのポートのルールは作らない。

### 3.2 cert-manager

[cert-manager](https://cert-manager.io/) が作る Secret をそのまま `certificateRefs` で指せばよい。rproxy-gateway は Secret を読むだけで、cert-manager との特別なつなぎはない。

**Certificate を書く形**（受け入れテストで更新まで確かめている形）：

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

- Gateway は 3.1 と同じ（`certificateRefs: [{name: app-tls}]`）。Secret ができるまでリスナーは `ResolvedRefs: False` で、できれば自動で `True` になる。
- HTTP-01 の `gatewayHTTPRoute` は、cert-manager がチャレンジ用の HTTPRoute を Gateway の `http` リスナーに作る形。cert-manager の Gateway API の対応を有効にしておく（cert-manager の chart の `config.enableGatewayAPI: true` など。cert-manager の文書で確かめる）。DNS-01 なら Gateway は関係しない。
- 3.3 のリダイレクトのルートを同じホスト名に置いても、チャレンジのパス（`/.well-known/acme-challenge/...`）のほうが長い一致なので先に選ばれる。

**Gateway に注釈を書く形**（cert-manager の機能。cert-manager がリスナーの `hostname` と `certificateRefs` から Certificate を作る）：

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

確かめる：

```bash
kubectl -n default get certificate app-tls     # READY が True
kubectl -n default get secret app-tls
```

### 3.3 HTTP から HTTPS へリダイレクト

HTTP のリスナーには、リダイレクトだけのルートをつなぐ。

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

アプリのルート（3.1 の `app`）は `sectionName: https` だけにつなぐ。スキームを変えるとポートはそのスキームの既定（443）になる。

```bash
curl -sI -H 'Host: app.example.com' http://$ADDR/path
# HTTP/1.1 301 Moved Permanently
# location: https://app.example.com/path
```

## 4. HTTP の振り分け

この章の HTTPRoute は 2.2 の Gateway `web` につなぐ。

### 4.1 ホスト名

リスナーの `hostname` とルートの `hostnames` の両方に合うものだけが届く。

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

- `*.example.com` は `example.com` の下の名前すべて（`a.b.example.com` も）。`example.com` 自身は含まない。
- リスナーとルートのホスト名が交わらないと、ルートは `Accepted: False`（`NoMatchingListenerHostname`）。
- どのルートにも合わないリクエストは 404。
- 同じポートに、より具体的なホスト名のリスナーがあれば、その名前は具体的なリスナーのルートだけに届く（Gateway API のリスナーの分離）。

### 4.2 パス・ヘッダ・クエリ・メソッド

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

- 1 つの `matches` の項の中の条件はすべて合うこと（AND）。`matches` の項どうしは OR。
- `PathPrefix: /api` は `/api` と `/api/...` に合い、`/apis` には合わない（区切りの `/` で合わせる）。
- 順番は書いた順ではなく Gateway API の決まり（Exact → 長い PathPrefix → method → ヘッダの数 → クエリの数 → 古いルート…）。[DESIGN.md](DESIGN.md) の「変換」の「ルールの優先」。
- ヘッダ・クエリにも `type: RegularExpression` が使える。

### 4.3 重みとカナリア

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

- リクエストのおよそ 1 割が `app-v2` に行く。コントローラは重みを Pod の数で割って、Pod ごとの重みにする（Pod の数が違っても比は保たれる）。
- `weight: 0` の backend には送らない。
- 重みを変えても接続は切れない（[9.](#9-動いているときに変えると)）。進め方は [13.5](#135-重みでカナリア)。

### 4.4 フィルタ

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

- `set` は置き換え、`add` は既にある値の後ろに `,` で足す、`remove` は消す。
- `URLRewrite` の `ReplacePrefixMatch` は、ルールのパスの一致が 1 つ（`PathPrefix`）のときだけ使える。
- ほかに `CORS`、`RequestMirror`（写しを別の Service へ）、backendRef ごとの `filters` も使える。rproxy の機能（流量の制限、認証など）は `ExtensionRef` で [7.2](#72-rproxymiddleware)。

### 4.5 retry とタイムアウト

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

- `timeouts` のないルールには、転送先の応答ヘッダを 30 秒まで待つ既定が付く（chart の `backends.responseTimeout`）。応答の本文（大きなダウンロード・SSE）と WebSocket は切らない。長く待たせるアプリには `backendRequest` を書くか、RproxyPolicy の `responseTimeout`（[7.1](#71-rproxypolicy)）。
- rproxy が送り直すのは、つながらない・応答がないときと `codes` の状態コードで、冪等なメソッド（GET・HEAD・OPTIONS・PUT・DELETE・TRACE）の本文のないリクエストだけ。
- 時間切れは 504。

### 4.6 GRPCRoute

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

- `HTTP`・`HTTPS` のリスナーにつなぐ。HTTPRoute と同じポートに置ける。
- 転送先とは h2c（平文の HTTP/2）で話す。TLS の転送先なら BackendTLSPolicy（[7.6](#76-backendtlspolicy)）。
- GRPCRoute には 30 秒の応答の既定は付かない（ストリームのため）。要るなら RproxyPolicy の `responseTimeout`。

HTTPRoute の転送先が h2c なら、Service のポートに `appProtocol: kubernetes.io/h2c` を書く。

## 5. L4 の転送

### 5.1 TCPRoute と UDPRoute

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

- `allowedRoutes.kinds` は省いてよい（`TCP` のリスナーは TCPRoute、`UDP` は UDPRoute を受ける）。
- 1 つのリスナーに複数の TCPRoute・UDPRoute をつなぐと、どれも `Accepted` だが通信は最も古いルートへ行く。
- 何もつながっていない `TCP`・`UDP` のリスナーはルールを作らない（Service のポートはある）。
- 古い Gateway API の CRD（v1.6 より前）では TCPRoute・UDPRoute が `v1alpha2`。API サーバが出している版で書く。

確かめる：

```bash
kubectl -n default get tcproute,udproute
kubectl -n default get tcproute postgres -o jsonpath='{.status.parents[0].conditions[*].reason}'
# Accepted ResolvedRefs
psql -h $ADDR -p 5432 -U app
dig @$ADDR example.com
```

### 5.2 TLSRoute（passthrough と terminate）

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

- **Passthrough**：rproxy は TLS の最初のメッセージ（ClientHello）の SNI だけを見て、暗号のまま転送先へ送る。HTTP の中身は見えない（ヘッダの書き換えなどはできない）。どの名前にも合わない接続は断る。
- **Terminate**：rproxy が証明書で TLS を終え、中身を TCP として送る（HTTP として振り分けない）。HTTP として振り分けたいなら `HTTPS` のリスナーと HTTPRoute を使う。
- 同じポートに `HTTPS` と `TLS`（Passthrough）のリスナーを並べられる。ホスト名で、復号する名前と復号しない名前を分ける（[13.4](#134-443-で-sni-の-passthrough)）。

### 5.3 ルートごとのリスナー

| ルート | リスナーの `protocol` | `tls.mode` | 証明書（`certificateRefs`） |
|---|---|---|---|
| HTTPRoute・GRPCRoute | `HTTP` | — | — |
| HTTPRoute・GRPCRoute | `HTTPS` | `Terminate`（省略可） | 要る |
| TLSRoute | `TLS` | `Passthrough` | 要らない（転送先が持つ） |
| TLSRoute | `TLS` | `Terminate` | 要る |
| TCPRoute | `TCP` | — | — |
| UDPRoute | `UDP` | — | — |

- managed の Service は、Gateway のリスナーのポートをすべて持つ。TCP と UDP の同じポート（DNS の 53 など）を 1 つの `LoadBalancer` の Service に載せるには、LB の実装が TCP と UDP の混在に対応していること。
- 1024 未満のポートもそのまま使える（rproxy の Pod は非 root で、安全な sysctl で開ける）。

## 6. ほかの namespace

### 6.1 allowedRoutes：Gateway がどの namespace のルートを受けるか

既定では Gateway と同じ namespace のルートだけを受ける（`from: Same`）。共有の Gateway は `allowedRoutes` で開ける。

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

- `from` は `Same`（既定）・`All`・`Selector`。
- 受けない namespace のルートは `Accepted: False`（`NotAllowedByListeners`）。
- namespace のラベルはテナントが書けないようにしておく。

### 6.2 ReferenceGrant：ほかの namespace の Service・Secret を使う

ルートは自分の namespace の Service を指すのが基本。ほかの namespace の Service を指すには、**指される側**に ReferenceGrant を置く。

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

- ReferenceGrant がないと、ルートは `ResolvedRefs: False`（`RefNotPermitted`、「Service default/api: no ReferenceGrant allows the reference」）で、その backend の分は 500 になる。
- Gateway の `certificateRefs` でほかの namespace の Secret を使うときも同じ（`from` は `kind: Gateway`、`to` は `kind: Secret`）。managed ではその鍵が Gateway の namespace の Secret に写る（[SECURITY.md](SECURITY.md) の「ほかの namespace の秘密鍵」）。
- ReferenceGrant は Gateway API v1.6 では `v1`・`v1beta1` の両方で書ける。

### 6.3 ListenerSet：ほかの namespace からリスナーを足す

共有の Gateway に、テナントが自分のホスト名と証明書のリスナーを足す。Gateway の `allowedListeners`（既定は `None`）で許す。

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

- ListenerSet の証明書と `allowedRoutes` の `Same` は ListenerSet の namespace で決まる。
- ルートは `parentRefs` に `kind: ListenerSet` を書いたときだけ、そのリスナーにつながる。
- 同じポートで食い違えば、古い ListenerSet が勝つ。

確かめる：

```bash
kubectl -n team-a get listenerset team-a
# NAME     ACCEPTED   PROGRAMMED   AGE
# team-a   True       True         30s
kubectl -n infra get gateway shared -o jsonpath='{.status.attachedListenerSets}'
```

## 7. rproxy の CRD とそのほかの設定

rproxy-gateway の CRD は `rproxy.max3584.net/v1beta1`（`v1alpha1` も同じ形で使えるが非推奨）。

| CRD | 短い名前 | 使い方 |
|---|---|---|
| `RproxyPolicy` | `rppol` | Gateway・リスナー・Service に、L4 の制限や時間を足す |
| `RproxyMiddleware` | `rpmw` | HTTPRoute・GRPCRoute の `ExtensionRef` で rproxy のミドルウェアを使う |
| `RproxyRule` | `rprule` | rproxy のルールそのものを Gateway に足す |
| `RproxyGatewayParameters` | `rpgwp` | managed の rproxy の Pod・Service の形 |

### 7.1 RproxyPolicy

`spec.targetRefs` に、**同じ namespace の** Gateway（`sectionName` でそのリスナーだけ）か Service を書く。

| 項目 | 中身 | 効くところ |
|---|---|---|
| `allowFrom` | 受け付ける接続元の CIDR の一覧 | ルール |
| `limits` | 接続の数・速さ（rproxy の `limits` の形） | ルール |
| `bandwidth` | 帯域（rproxy の `bandwidth` の形） | ルール |
| `geoip` | 国・ASN での許可（rproxy の `geoip` の形。rproxy に GeoIP のデータベースが要る） | ルール |
| `crowdsec` | CrowdSec の判定で断る（rproxy に `global.crowdsec` が要る） | ルール |
| `outlierDetection` | 受け身のヘルスチェック（rproxy の `outlier_detection` の形） | L4 のルール・HTTP のサービス |
| `connectTimeout` | 転送先への接続の時間（100ms〜10m） | L4 の tcp のルール・HTTP のサービス |
| `responseTimeout` | 転送先の応答ヘッダを待つ時間（1h まで） | HTTP のサービス |
| `idleTimeout` | 何も運ばない L4 の接続を閉じるまで（`0s` でなし、rproxy の既定 1h） | L4 の tcp のルール |
| `halfCloseTimeout` | 片側が送り終えた後に閉じるまで（`0s` でなし、既定 1m） | L4 の tcp のルール |

`limits`・`bandwidth`・`geoip`・`outlierDetection` は rproxy のルールの形（snake_case）をそのまま書く。形は rproxy-api の docs/API.md（「L4 の制限と帯域」「GeoIP」「受け身のヘルスチェック」）。

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
    ejection_time: 30s
```

- Service を指すと、その Service に送る L4 のルール（`outlierDetection`・`connectTimeout`・`responseTimeout` は HTTP のサービスにも）に効く。
- 同じ項目を 2 つのポリシーが決めたら、古いポリシーが勝つ。
- `outlierDetection` を書くと、chart の `backends.outlierDetection` の既定は付かない。外すには `outlierDetection: {max_ejected_percent: 0}`。
- `idleTimeout`・`halfCloseTimeout` は HTTP・HTTPS のルールと UDP には付かない（HTTP の接続は [7.4](#74-rproxygatewayparameters) の `rproxy.http`）。
- HTTPRoute の `timeouts` があるルールには `responseTimeout` は付かない（ルートが勝つ）。

確かめる（状態は Gateway ごとに `status.ancestors[]`）：

```bash
kubectl -n default get rproxypolicy postgres-service -o jsonpath='{range .status.ancestors[*]}{.ancestorRef.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# l4 Accepted=True(Accepted) rproxy.max3584.net/RelayTimeouts=True(Applied)
```

- 時間の形が誤り：`Accepted: False`（`Invalid`）で、そのポリシーは使わない。
- 指したリスナーがない：`Accepted: False`（`TargetNotFound`）。

### 7.2 RproxyMiddleware

`spec` は rproxy のミドルウェア 1 つ（`{種類: {設定}}`）。HTTPRoute・GRPCRoute のフィルタの `ExtensionRef` で使う。種類と設定は rproxy-api の docs/API.md（「v0.3 の設定」）の `http.middlewares` と同じ。

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

- RproxyMiddleware はルートと同じ namespace のもの。見つからなければそのルールは 500 を返し、ルートは `ResolvedRefs: False`。
- 形や値が rproxy の決まりに合わないと、ルートは `Accepted: False`（`UnsupportedValue`、message に理由）になり、同じポートのほかのルートはそのまま動く。
- `rate_limit`・`in_flight` の数は、そのポートの rproxy のルールが変わる（同じポートのどれかのルートを変える）と最初からになる。
- backendRef の `filters` には `ExtensionRef` を書けない（`UnsupportedValue`）。
- 名前でファイルを指す項目（`basic_auth` の `users_file` など）は、その Gateway の証明書のファイルしか指せない（[SECURITY.md](SECURITY.md)）。利用者の一覧のファイルは渡せないので、認証は `forward_auth` か Gateway API の `ExternalAuth` を使う。

**認証（`forward_auth`）**：リクエストごとに認証サーバに聞き、2xx なら通す。名指せるサービスは、それを使う HTTPRoute 自身のルール（ルールの `name` か `r<番号>`）だけ。

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

外の URL に聞く（`forward_auth.address`・`oidc.issuer`）のは、既定では使えない（Gateway の Pod から好きな URL に要求させないため）。GatewayClass の parameters（chart の `managed.parameters`）で許す：

```yaml
# values.yaml
managed:
  parameters:
    policy:
      middlewares:
        outboundHosts: [auth.example.com]       # *、ホスト名、*.example.com、IP の範囲
        # allowedKinds: [headers, rate_limit, forward_auth]   # 使える種類を限るなら
```

```yaml
apiVersion: rproxy.max3584.net/v1beta1
kind: RproxyMiddleware
metadata: {name: authelia, namespace: default}
spec:
  forward_auth:
    address: https://auth.example.com/api/verify
    response_headers: [Remote-User, Remote-Groups]
```

許していないと、ルートは `Accepted: False`（`UnsupportedValue`、「auth.example.com is not allowed by the GatewayClass (policy.middlewares.outboundHosts; by default no URL out)」）。

### 7.3 RproxyRule

`spec.rule` は rproxy のルールそのもの（制御 API の `POST /rules` の本文）。Gateway API で書けないもの（クラスタの外への転送、rproxy の細かい設定）を Gateway のルールセットに足す。

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

- ルールの待ち受けのポートは managed の Service にも足される（範囲は 100 ポートまで）。
- ほかの namespace の Gateway に足すには、その namespace の ReferenceGrant（from `group: rproxy.max3584.net, kind: RproxyRule`、to `kind: Gateway`）が要る。
- 同じキー（プロトコル・アドレス・ポート）のルールが Gateway にあれば `Accepted: False`（`Conflicted`）。rproxy が断れば `Accepted: False`（`Invalid`）で、ほかのルールは動く。
- RproxyRule はテナントの制限（[SECURITY.md](SECURITY.md)）の外：どの宛先でも書ける。書ける人を Gateway の持ち主として信じられないなら、クラスの `policy.rproxyRules: false` で切る。fleet では既定で読まない（`fleet.rproxyRules`）。

確かめる：

```bash
kubectl -n default get rproxyrule
# NAME          GATEWAY   PROGRAMMED
# bastion-ssh   web       True
```

### 7.4 RproxyGatewayParameters

managed の rproxy の Pod・Service の形を Gateway ごとに変える。Gateway の namespace に書き、Gateway の `spec.infrastructure.parametersRef` で指す。

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

主な項目（全部は [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 2.2）：

| 項目 | 中身 | テナント（Gateway の参照）が既定で書けるか |
|---|---|---|
| `replicas` | Pod の数（1〜`policy.maxReplicas`、既定 10） | 書ける |
| `podDisruptionBudget` | `minAvailable` か `maxUnavailable` | 書ける |
| `pod.resources.rproxy`・`.certsync` | コンテナの resources | 書ける |
| `pod.labels`・`pod.annotations`・`pod.topologySpreadConstraints` | Pod | 書ける |
| `pod.nodeSelector`・`tolerations`・`affinity`・`priorityClassName` | Pod | 書けない（クラスで開ける） |
| `service.type` | `LoadBalancer`・`NodePort`・`ClusterIP` | 書けない（クラスで開ける） |
| `service.externalTrafficPolicy`・`loadBalancerSourceRanges`・`ipFamilyPolicy`・`labels`・`annotations` | Service | 書ける（アドレスを決める注釈は許可リストだけ） |
| `service.loadBalancerClass` | Service（作った後は変えられない） | 書けない（クラスで開ける） |
| `rproxy.logLevel`・`performance`・`shutdown` | rproxy の環境変数 | 書ける |
| `rproxy.http` | HTTP のクライアントの接続の上限（`http2MaxStreams`・`timeouts`） | 書ける |
| `rproxy.image`・`rproxy.extraEnv` | rproxy のイメージ・追加の環境変数（`RPROXY_*`） | 書けない（クラスの参照だけ） |
| `proxyProtocol` | 前段の PROXY protocol を読む（[PLATFORM.md](PLATFORM.md)） | 書けない（クラスで開ける） |
| `ui.visible` | 管理 UI に見せるか | 書ける（クラスが false なら true にできない） |
| `policy` | テナントに許すもの | 書けない（クラスの参照だけ） |

**すべての Gateway の既定（クラスの参照）**：chart の `managed.parameters` に書く。chart が `RproxyGatewayParameters` `rproxy-default`（リリースの namespace）を作り、GatewayClass の `parametersRef` で指す。`policy` はここだけに書ける。

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

- `gatewayOverrides` を書くと、書いた項目だけをテナントが書ける。この例のクラスでは、上の Gateway の参照（`pod.topologySpreadConstraints`・`service.*`・`rproxy.shutdown`・`rproxy.http`）は `InvalidParameters` になる。
- 合わせる順は、コントローラのフラグ → クラスの参照 → Gateway の参照（後ろが勝つ）。リストとオブジェクトは丸ごと置き換え、ラベルなどのマップはキーごと。
- `v0.4.1` から上げたクラスタでは、`managed.parameters` を書く前に rproxy の CRD を当てる（`helm upgrade` は新しい CRD を入れない。[12.1](#121-更新)）。

確かめる：

```bash
kubectl -n default get rpgwp
# NAME   REPLICAS
# web    3
kubectl -n default get deploy -l gateway.networking.k8s.io/gateway-name=web
```

- 許されていない項目・誤った値：Gateway が `Accepted: False`（`InvalidParameters`）。message に項目の名前（例「spec.service.type: not allowed by the GatewayClass (policy.gatewayOverrides)」）。
- **もう動いている Gateway は止めない**：rproxy の Deployment がある Gateway は、前の正しい形のまま動き続ける（`Programmed: True`）。直せば次の反映で新しい形になる。
- `rproxy.http` は条件 `rproxy.max3584.net/HTTPSettings`（`Applied`・`Kept`・`UnsupportedRproxy`）で分かる。
- fleet では Gateway の参照は使えない（`InvalidParameters`）。

### 7.5 リスナーのフィルタ

Gateway API v1.7 の experimental の CRD で、リスナーに `filters.requests` を書ける（GEP-5224）。ルートを選ぶ**前に**、そのリスナーのすべてのリクエストに書いた順で働く。rproxy v0.4.22 の `http.pre_routing` になる。

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

- 使えるのは `ExternalAuth` と `ExtensionRef`（RproxyMiddleware）だけ。`HTTP`・`HTTPS` のリスナーだけ。
- RproxyMiddleware の `retry`・`mirror`・`replace_host` と、サービスを名指す設定（`forward_auth` の `service` など）、Host を変える `headers` は使えない。
- 同じポートのリスナーは 1 つの rproxy のルールなので、フィルタはそのポートのすべてのリスナーで同じにする。そろっていなければフィルタを持つリスナーは `Accepted: False`（`UnsupportedValue`）。
- 参照が解決しない（Service や RproxyMiddleware がない）ときは、リスナーが `ResolvedRefs: False`（`InvalidListenerFilterRef`）で、そのフィルタの場所で 500 を返す（飛ばして通すことはしない）。
- Gateway の rproxy の Pod がすべて `pre_routing` を持つと答えるまで、リスナーは `Accepted: False`（`UnsupportedValue`、「not known (yet)」）。同梱の rproxy v0.4.22 は持つので、ふつうは Pod が起動すれば受け付けられる。
- 詳しくは [DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 17.

### 7.6 BackendTLSPolicy

rproxy から転送先へ TLS で送る。

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

- `hostname` は SNI と証明書の確かめに使う名前。`subjectAltNames` で確かめる名前を変えられる。公開の CA なら `caCertificateRefs` の代わりに `wellKnownCACertificates: System`。
- `targetRefs` の `sectionName` で Service のポートを選べる（ポートのものが Service 全体のものに勝つ）。
- 使える CA がないと `Accepted: False`（`NoValidCACertificate`）で、その backend の分は 500。
- 転送先にクライアント証明書を出すなら、Gateway の `spec.tls.backend.clientCertificateRef`（Secret）。

確かめる：`kubectl -n default describe backendtlspolicy secure-app` の `Status.Ancestors`（Gateway ごと）。

### 7.7 ClusterTrustBundle とクライアント証明書の確かめ

Kubernetes の ClusterTrustBundle（クラスタ全体の CA の束。Kubernetes 1.37 の `certificates.k8s.io/v1`、それより前はベータの API を有効にしたとき）を、`caCertificateRefs` から名前で指せる。ReferenceGrant は要らない。

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

- API サーバが ClusterTrustBundle を出していないと、参照は `InvalidKind`（Gateway は `InvalidCACertificateKind`）。
- `spec.tls.frontend` の `perPort` でポートごとに変えられる。`mode: AllowInsecureFallback` は証明書がなくても・通らなくてもつなぎ、結果を `X-Client-Verify` ヘッダで backend に渡す。

### 7.8 アドレスの到達範囲

Gateway API v1.7 の experimental の CRD で、`spec.addresses[].routability` を書ける（GEP-5093）。`Cluster` の項目ごとに、Gateway の namespace にクラスタの中だけの ClusterIP の Service を作る。

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

- 項目が全部 `Cluster` なら、Gateway の Service も `ClusterIP` になる（外に出さない）。
- 値つき（決まった ClusterIP）は既定で使えない（`AddressesAssigned: False`）。クラスの `policy.clusterIPs: true` で開ける。
- 項目の順を変えると Service が作り直され、ClusterIP が変わる。足すときは末尾に。
- fleet では `Cluster` は割り当てない。
- 値のある `type: IPAddress`（静的なアドレス）は managed では Service の `externalIPs` になり、chart の `managed.addressCIDRs` の範囲の内だけ使える。詳しくは [PLATFORM.md](PLATFORM.md)。

## 8. managed と fleet

| | managed（既定） | fleet |
|---|---|---|
| rproxy | Gateway ごとに Deployment と Service を Gateway の namespace に | chart の DaemonSet（`hostNetwork: true`）がすべての Gateway を受け持つ |
| アドレス | Service の LB のアドレス（MetalLB・クラウドなど） | ノードの IP か `fleet.addresses`（VIP はプラットフォームが用意する） |
| 鍵・トークン | Gateway ごと | すべての Gateway で共有 |
| テナントを分けるか | 分ける | 分けない（1 人の管理者のため） |
| `RproxyGatewayParameters` | 使える | Gateway の参照は使えない（DaemonSet は chart の `fleet.*` で） |
| RproxyRule | 使える | 既定で読まない（`fleet.rproxyRules`） |
| 同じポートを 2 つの Gateway で | 使える（Service が別） | 古い Gateway が持つ。後のリスナーは `Accepted: False`（`PortUnavailable`）。`fleet.listen: addresses` ならアドレスが違えば使える |
| NetworkPolicy | Gateway ごとに作る | 効かない（ファイアウォールで守る） |

**選び方**：ふつうは managed。LB の実装がない・ノードのアドレスで直接受けたい・1 人で使うクラスタで Pod の数を減らしたいときに fleet。

fleet の例：

```yaml
# values.yaml
fleet:
  enabled: true
  addresses: [192.0.2.10]    # Gateway の status に書くアドレス（VIP や前段の LB）
  # listen: addresses        # Gateway の spec.addresses ごとに待ち受ける（managed.addressCIDRs の内）
  # nodeSelector: {node-role.example.com/edge: "true"}
```

前に置くロードバランサ・VIP のヘルスチェックは `https://<ノードの IP>:9443/readyz` に向ける（止まり始めると 503）。書き方は [PLATFORM.md](PLATFORM.md) の「fleet」。

**切り替え**（`helm upgrade` で `fleet.enabled` を変える）：

- managed → fleet：コントローラは managed の Deployment・Service を消し、fleet の Pod にルールを送る。Gateway のアドレスが変わる（LB のアドレス → ノードの IP・`fleet.addresses`）。Gateway の `parametersRef` は `InvalidParameters` になる。RproxyRule は `fleet.rproxyRules: true` でないと使われない。
- fleet → managed：helm が DaemonSet を消し、コントローラが Gateway ごとの Deployment・Service を作る。アドレスは新しい LB のアドレスになる。
- どちらも通信が途切れ、アドレスが変わる。メンテナンスの時間に、DNS とファイアウォールを合わせて行う。

```bash
helm upgrade rproxy-gateway oci://ghcr.io/max3584/charts/rproxy-gateway -n rproxy-gateway-system -f values.yaml
kubectl get gateway -A        # ADDRESS が変わったことを確かめる
```

## 9. 動いているときに変えると

コントローラは変更のたびに Gateway のルールセット全体を描き直して rproxy に送る（[10.](#10-rproxy-との話し方)）。rproxy は前との差だけを当てる。差には 3 つの当て方がある（rproxy-api の docs/API.md の「ルールの組」）。

| rproxy の当て方 | 今の接続 |
|---|---|
| `in_place`（その場で変える） | 切れない。新しい接続・リクエストから効く |
| `recreate`（待ち受けを作り直す） | そのポートの接続が切れる |
| 削除（ルールセットから外れた） | そのポートの接続が切れる |

### 9.1 よくある変更

| 変更 | どうなるか |
|---|---|
| ルートを足す・変える・消す（パス・ヘッダ・重み・フィルタ） | `in_place`。接続は切れない（`rate_limit`・`in_flight` の数は最初から） |
| 転送先の Pod が増える・減る（スケール・rollout） | `in_place`。終わる Pod には新しいリクエストを送らない |
| 証明書の Secret の更新（cert-manager の更新） | `in_place`。ファイルの名前が中身のハッシュなので、新しい接続から新しい証明書 |
| RproxyPolicy（`allowFrom`・`limits`・時間など） | `in_place`。次の接続から |
| 同じポートにリスナー（ホスト名）を足す・消す | そのポートのルールの `in_place`（ほかのホスト名の接続は切れない） |
| リスナーのポートを変える | 古いポートのルールは消える（その接続は切れる）。新しいポートのルールができる。managed の Service のポートも変わる |
| リスナー（ポートの最後の 1 つ）を消す | そのポートのルールが消え、接続が切れる |
| 同じポートのプロトコルを変える（`HTTP` ↔ `TCP` など） | `recreate`。そのポートの接続が切れる |
| Gateway を消す | managed：Deployment・Service が消える（Gateway が持ち主）。すべての接続が切れ、アドレスもなくなる。fleet：rproxy のルールセットを消す。その Gateway の接続が切れる |
| `RproxyGatewayParameters`（replicas 以外の Pod の形）・`spec.infrastructure` のラベル・注釈 | Pod の rolling update（下の 9.3）。replicas は Pod を足す・減らすだけ |
| `rproxy.http` の値 | Pod は入れ替わらない（rproxy が設定ファイルを読み直す。kubelet の更新まで 1 分ほど）。初めて付けた・全部外したときは rolling update |
| `service.type` の変更 | Service が変わり、LB のアドレスが変わる |
| コントローラの更新（rproxy・certsync のイメージが変わる） | managed のすべての Gateway で rolling update |

### 9.2 誤りは状態に出る

rproxy が断ったものは外し、残りを動かす。1 つの誤りでセット全体を止めない。

| 起きたこと | 出るところ |
|---|---|
| rproxy がルートを断った | そのルートの `Accepted: False`（`UnsupportedValue`、rproxy の答え）。同じポートのほかのルートは動く |
| rproxy が RproxyRule を断った | RproxyRule の `Accepted: False`（`Invalid`） |
| rproxy がルールセット全体を断った（上限など） | Gateway とリスナーの `Programmed: False`（`Invalid`、「rproxy refused the rule set: ...」） |
| 証明書のファイルがまだ Pod にない | `Programmed: False`（`Pending`）。揃えば `True` |
| 待ち受けを開けない（ポートが使われている） | リスナーの `Programmed: False` |
| 参照の誤り（Service・Secret がない） | ルートの `ResolvedRefs: False`（`BackendNotFound`・`RefNotPermitted`）、リスナーの `ResolvedRefs: False`（`InvalidCertificateRef`） |

断られたものは、中身が変わるか 5 分たつまで送り直さない。

### 9.3 Pod の入れ替えと readiness gate

managed の rproxy の Pod は、次のしかけで入れ替えのときも通信を止めない（[DESIGN.md](DESIGN.md) の「rproxy の可用性」）。

- **readiness gate**（`rproxy.max3584.net/ruleset-applied`）：新しい Pod は、コントローラがルールセットを送り終えてから Ready になり、Service に入る。ルールのない rproxy に通信が来ない。
- **rolling update**：`maxUnavailable: 0`。新しい Pod が Ready になってから古い Pod を止める。
- **止まり方**：SIGTERM の後 `delay`（既定 15 秒）は受け付けを続けながら `/readyz` を 503 にして Service から外れ、その後 `drain`（既定 25 秒）まで今の接続の終わりを待つ。drain を過ぎても続く接続（長い WebSocket・DB の接続）は切れる。長い接続を待つなら `rproxy.shutdown.drain` を延ばす。
- `replicas` が 2 以上なら PDB（`maxUnavailable: 1`）とノードへの分散が付く。1 つだと、入れ替えの間に 1 つしかない Pod が止まる時間がある。

```bash
kubectl -n default get pod -l gateway.networking.k8s.io/gateway-name=web -o wide
# READINESS GATES の列が 1/1 なら、ルールセットが載っている
```

### 9.4 再起動したとき

| 再起動するもの | どうなるか |
|---|---|
| コントローラ | rproxy は今のルールのまま動き続ける（通信は止まらない）。新しいリーダーが rproxy のルールセットを読み、違えば送り直す |
| rproxy のコンテナ | ルールセットはメモリにだけあるので消える。コントローラが readiness gate を `False` に戻し（Service から外れる）、`/readyz` を待って送り直し、`True` に戻す |
| rproxy の Pod（削除・drain） | 9.3 の rolling update と同じ |

コントローラが止まっている間は、変更が rproxy に届かない（今の形のまま動く）。

## 10. rproxy との話し方

コントローラは Gateway 1 つを rproxy のルールセット 1 つにし、rproxy-api の制御 API で送る。rproxy の VM で `curl` で使う API と同じもので、ルールの JSON も同じ形。

```
PUT https://<rproxy の Pod の IP>:9443/rulesets/k8s/<Gateway の namespace>/<Gateway の名前>
Authorization: Bearer <その Gateway のトークン>
If-Match: <今の etag>
{"generation": <Gateway の metadata.generation>, "rules": [ ... ]}
```

- (プロトコル, アドレス, ポート) 1 つがルール 1 つ。同じポートのリスナーとルートは 1 つのルールにまとまる。
- 中身が前と同じなら送らない。`--resync-secs`（既定 30 秒）ごとにも描き直す。

2.2 の Gateway `web` と HTTPRoute `app` は、次のルールセットになる（`rproxy-gateway render -f <ファイル>` でクラスタなしに描ける）。

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
              {"url": "http://10.244.1.10:8080"},
              {"url": "http://10.244.2.11:8080"}
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

- `servers` は Service の EndpointSlice の Pod の IP。
- 本物のコントローラは、これに chart の `backends` の既定（サービスの `outlier_detection`、`timeouts.connect: 1s`、`timeouts.response: 30s`）を足して送る。

**制御 API を通らないもの**（鍵を API に流さない。rproxy の Pod は Kubernetes の API を使わない）：

| もの | 渡し方 |
|---|---|
| 証明書・鍵（`certificateRefs`、BackendTLSPolicy の CA など） | コントローラが Secret `rproxy-<id>-certs` にまとめ、kubelet が Pod の `/var/run/rproxy-gateway/certs` にファイルとしてつなぐ。ルールは `cert_file` などでファイルの名前（中身のハッシュ）を指す。同じ Pod の `certsync` が、ファイルが揃ったかをコントローラに答える |
| トークンファイル（`tokens.yaml`、ハッシュだけ）と制御 API の証明書 | Secret `rproxy-<id>-api` をボリュームで |
| rproxy の設定ファイル（`config.json`、`rproxy.http` のとき） | 同じ `rproxy-<id>-api` の項目と `RPROXY_CONFIG` |
| ログの段階・性能・止まり方・`extraEnv` | Pod の環境変数 |

fleet では、証明書は `rproxy-fleet-certs`、トークンファイルは `rproxy-gateway-token`、`fleet.http` は ConfigMap `rproxy-fleet-config`。

## 11. セキュリティの基本

詳しくは [SECURITY.md](SECURITY.md)。

- **トークンは Gateway ごと**：managed の rproxy は、Gateway ごとの制御 API の証明書とトークン（マスタートークンから導いたもの）で動く。トークンは自分のルールセットの名前（`k8s/<namespace>/<name>`）だけを書ける。ある Gateway の namespace の Secret を読めても、ほかの Gateway の rproxy にはつなげない。マスタートークンと CA の鍵はコントローラの namespace（`rproxy-gateway-system`）だけにある。
- **rproxy の Pod は Kubernetes の API を使わない**：ServiceAccount のトークンをつながず、RBAC もない。
- **制御 API の Service はない**：Gateway ごとの Service が持つのはリスナーのポート（と RproxyRule の待ち受けのポート）だけで、制御 API（9443）と certsync（9444）の Service は作らない。コントローラは Pod ごとに `https://<Pod の IP>:9443` へ直接 PUT する。Pod ごとに自分のルールセットを持つので、Service を通すと 1 つの Pod にしか届かず、正しくない。利用者が 9443・9444 のための Service や NetworkPolicy を作る必要はない。
- **NetworkPolicy（managed）**：Gateway ごとに自動で作る。制御 API（9443）と certsync（9444）はコントローラの Pod からだけ（chart の `ui.namespace` を決めたときは、見せる Gateway の 9443 に UI の Pod からも）、リスナーのポートは誰からでも。CNI が NetworkPolicy を扱わないと効かない。
- **fleet と hostNetwork**：NetworkPolicy は効かない。制御 API（9443）と certsync（9444）はノードの IP で待ち受けるので、外からのノードの 9443・9444 への通信をファイアウォールで絞る（ヘルスチェックをするロードバランサだけ）。fleet は 1 つの信頼の範囲のためのもので、テナントを分けない。
- **既定で閉じているもの**：`spec.addresses`（`managed.addressCIDRs`）、ExternalName の Service、外への URL のミドルウェア（`policy.middlewares.outboundHosts`）、アドレスを決める LB の注釈（`serviceAnnotationPrefixes`）。開けるときは信頼するテナントのクラスだけ。
- **ほかの namespace の秘密鍵**：ReferenceGrant で使うと、managed ではその鍵が Gateway の namespace に写る。困るなら `controller.crossNamespaceSecrets: false`。
- **コントローラの権限を絞る**：`controller.watchNamespaces` と `rbac.admissionPolicy.enabled: true`（Kubernetes 1.30 以上）。

## 12. 運用

### 12.1 更新

rproxy-gateway の更新：

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

- chart の版ごとに同梱の rproxy の版が決まっている（chart の `rproxy.image.tag`。chart 0.4.22 は rproxy 0.4.22）。rproxy のイメージ（とコントローラのイメージの certsync）が変わると、managed のすべての Gateway の Pod が rolling update で入れ替わる（9.3。replicas が 2 以上なら途切れはふつう 1 秒ほどまで。[README](../README.md) の「可用性」）。fleet では DaemonSet が入れ替わる。
- 版ごとの変更はリリースノート（GitHub の Releases）で確かめる。v0.4.x の間は、今の値・Gateway を壊さない（[DESIGN-v0.4.x.md](DESIGN-v0.4.x.md) の 1.）。
- Kustomize・`install.yaml` で入れたなら、新しい版の `install.yaml` を `kubectl apply --server-side` する（CRD も含む）。
- Gateway API の CRD を上げるときは、新しい版の `standard-install.yaml`（か experimental）を `kubectl apply --server-side` する。コントローラは新しい種類を 30 秒ごとに探すので、再起動は要らない。Kubernetes の版との組み合わせは [README](../README.md) の「対応する版」。

### 12.2 監視

**状態（conditions）**：まず見るもの。アラートにするなら次が `True` でないとき。

| 対象 | 条件 |
|---|---|
| Gateway | `Accepted`、`Programmed` |
| リスナー（`status.listeners[]`） | `Accepted`、`ResolvedRefs`、`Programmed` |
| ルート（`status.parents[]`） | `Accepted`、`ResolvedRefs` |
| RproxyPolicy・BackendTLSPolicy（`status.ancestors[]`） | `Accepted` |
| RproxyRule | `Accepted`、`Programmed` |
| Gateway の rproxy の条件 | `rproxy.max3584.net/HTTPSettings`・`rproxy.max3584.net/ProxyProtocol`（`False` のとき） |

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

（リスナーの `Conflicted` は `False` がふつう。）Prometheus で見るなら kube-state-metrics の Custom Resource State で、これらの条件を指標にできる。

**Pod**：コントローラの Deployment（`rproxy-gateway`）と rproxy の Pod の Ready、再起動の回数。rproxy の Pod の READINESS GATES が `0/1` のままなら、ルールセットが載っていない。

**ログ**：12.5 と [14.2](#142-コントローラのログ)。rproxy のログは `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy`（`ruleset.apply`、`conn.*`、`http.access` など。rproxy-api の docs/API.md）。

**rproxy の `/metrics`**：rproxy は制御 API（9443）の `GET /metrics` に Prometheus の形の指標を出す（`metrics:read` のトークンが要る）。rproxy-gateway がそのトークンを渡すのは管理 UI（chart の `ui.namespace`）だけで、NetworkPolicy も UI の Pod からだけを通す。Prometheus が直接読む設定は今の版にはない。指標は管理 UI で見る。

### 12.3 冗長化とスケール

| もの | 値 | 中身 |
|---|---|---|
| コントローラ | `controller.replicas`（既定 2） | Lease `rproxy-gateway`（コントローラの namespace）で 1 つがリーダーになり、反映する。リーダーが止まると 15 秒ほどでほかが引き継ぐ（SIGTERM ならすぐ）。1 つで動かすなら `controller.leaderElection: false` |
| rproxy（全 Gateway の既定） | `managed.replicas` か `managed.parameters.replicas` | 2 以上で PDB（`maxUnavailable: 1`）と `kubernetes.io/hostname` の topologySpread（`ScheduleAnyway`） |
| rproxy（Gateway ごと） | `RproxyGatewayParameters` の `replicas`・`podDisruptionBudget`・`pod.topologySpreadConstraints`・`pod.resources` | テナントは `policy.maxReplicas`（既定 10）まで |
| fleet | DaemonSet（ノードごとに 1 つ） | `fleet.nodeSelector`・`tolerations` で置くノードを選ぶ |

```bash
kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}'   # 今のリーダー
kubectl -n default get pdb -l gateway.networking.k8s.io/gateway-name=web
```

- 本番は rproxy を 2 つ以上にする。1 つでは、Pod の入れ替え・ノードの drain の間に途切れる。
- 形ごとの途切れの値は [README](../README.md) の「可用性」と [PLATFORM.md](PLATFORM.md)。
- fleet の rproxy は組を 1000（`RPROXY_MAX_RULESETS`）、ルールを 10000（`RPROXY_MAX_RULES`）まで持つ。Gateway がもっと多いなら `fleet.extraEnv` で上げる（値は引用符で囲む）。

### 12.4 バックアップ

ルールは rproxy のメモリにだけあり、正は Kubernetes のオブジェクト。戻すのは次のもの。

| もの | 要るか |
|---|---|
| Gateway API のオブジェクト（Gateway・各ルート・ReferenceGrant・ListenerSet・BackendTLSPolicy） | 要る |
| rproxy の CRD のオブジェクト（RproxyPolicy・RproxyMiddleware・RproxyRule・RproxyGatewayParameters） | 要る |
| 証明書の Secret（`certificateRefs` が指すもの）、CA の ConfigMap | 要る（cert-manager が出すものは出し直せる） |
| chart の values（`values.yaml`） | 要る |
| コントローラの Secret（`rproxy-gateway-ca`・`rproxy-gateway-api-tls`・`rproxy-gateway-token`） | なくてもよい。ないとコントローラが作り直し、rproxy の Pod が 1 回入れ替わる。管理 UI の発見の Secret も書き直される |
| Gateway ごとの `rproxy-<id>-certs`・`rproxy-<id>-api`、rproxy の Deployment・Service | 要らない（コントローラが作る） |

```bash
kubectl get gateways,httproutes,grpcroutes,tlsroutes,tcproutes,udproutes,referencegrants,listenersets,backendtlspolicies -A -o yaml > gateway-api.yaml
kubectl get rproxypolicies,rproxymiddlewares,rproxyrules,rproxygatewayparameters -A -o yaml > rproxy-crs.yaml
```

戻すときは、Gateway API の CRD と chart を入れてから当てる。Velero などで namespace ごとに取るなら、上の種類を含める。新しいクラスタでは、Service の LB のアドレスが変わることがある（決めるなら LB の注釈か `spec.addresses`。[PLATFORM.md](PLATFORM.md)）。

### 12.5 ログの段階

- コントローラ：chart の `controller.logFormat`（既定 `json`）。段階は環境変数 `RPROXY_GATEWAY_LOG`（既定 `info`、`debug` など）。試しに上げるなら `kubectl -n rproxy-gateway-system set env deploy/rproxy-gateway RPROXY_GATEWAY_LOG=debug`（次の `helm upgrade` で戻る）。
- rproxy：`RproxyGatewayParameters` の `rproxy.logLevel`（`error`〜`trace`）。

## 13. レシピ

### 13.1 HTTPS の Web アプリ（cert-manager）

2.1 のアプリに、Let's Encrypt の証明書と HTTP → HTTPS のリダイレクトを付ける。cert-manager の Gateway API の対応を有効にしておく（3.2）。

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

1. 当てる。`kubectl -n default get gateway web` で `ADDRESS` を見て、DNS の `app.example.com` をそのアドレスに向ける。
2. `kubectl -n default get certificate app-tls` が `READY True` になるまで待つ（その間 `https` のリスナーは `ResolvedRefs: False`）。
3. `curl -I http://app.example.com/` が 301、`curl https://app.example.com/` が 200。

### 13.2 DB への TCP（長い idleTimeout）

rproxy は何も運ばない L4 の接続を既定で 1 時間で閉じる。コネクションプールのように長く黙る接続には `idleTimeout` を延ばす。

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

- `kubectl -n default get rproxypolicy db -o yaml` の `status.ancestors[].conditions` に `rproxy.max3584.net/RelayTimeouts: True`（`Applied`）が出れば効いている。
- DB をクラスタの外に出すなら、`allowFrom` や `service.loadBalancerSourceRanges`（[7.4](#74-rproxygatewayparameters)）で接続元を絞る。
- rproxy の Pod の入れ替えでは、`drain`（既定 25 秒）を過ぎた接続は切れる。アプリは再接続できるようにする。

### 13.3 UDP

DNS のように UDP と TCP の同じポートを使う例。

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

- 転送先の Service `dns` には TCP と UDP の 53 の両方のポートを書く。
- LB の実装が、1 つの Service の TCP と UDP の混在に対応していること。
- クライアントのアドレスを転送先に見せたいなら `externalTrafficPolicy: Local`（`LoadBalancer` の既定）。

### 13.4 443 で SNI の passthrough

同じ 443 で、ある名前は rproxy が復号して HTTP で振り分け、別の名前は復号せずに転送先へそのまま送る。

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

- rproxy は 443 の 1 つのルールで、`secure.example.com` の SNI だけを復号せずに送る。
- `secure-app` の Pod が `secure.example.com` の証明書を持つ。
- 確かめる：`openssl s_client -connect $ADDR:443 -servername secure.example.com` で `secure-app` の証明書が、`-servername app.example.com` で `app-tls` の証明書が見える。

### 13.5 重みでカナリア

1. `app-v2`（新しい版）の Deployment と Service を作る。
2. 4.3 の HTTPRoute で `app: 90`・`app-v2: 10` を当てる。
3. 状態とエラーを見ながら、重みを書き換えて当て直す（`90/10` → `50/50` → `0/100`）。どれも接続は切れない。
4. 戻すときは `app-v2` の重みを 0 にする。

```bash
# 重みだけ変える（rules[0] の 2 つの backendRefs）
kubectl -n default patch httproute app --type=json -p '[
  {"op": "replace", "path": "/spec/rules/0/backendRefs/0/weight", "value": 50},
  {"op": "replace", "path": "/spec/rules/0/backendRefs/1/weight", "value": 50}]'
# 割合を見る（echo は Pod の名前を返す）
for i in $(seq 100); do curl -s -H 'Host: app.example.com' http://$ADDR/ | jq -r .pod; done | sort | uniq -c
```

ヘッダで選ぶカナリア（試す人だけ新しい版へ）は 4.2 の `headers` の一致で書ける。

### 13.6 認証のミドルウェア（ExtensionRef）

oauth2-proxy でサインインさせる。7.2 の `login`（`forward_auth`）と HTTPRoute をそのまま使う。

1. oauth2-proxy を Service `oauth2-proxy`（4180）で動かす。リダイレクトの URL は `https://app.example.com/oauth2/callback`、`--reverse-proxy`、`--set-xauthrequest`（oauth2-proxy の文書で確かめる）。
2. 7.2 の RproxyMiddleware `login` と HTTPRoute `app` を当てる（Gateway は 13.1 の `web`）。
3. サインインしていないと、oauth2-proxy の `/oauth2/auth` が 401 を返し、それがそのままクライアントに返る（2xx 以外の答えは rproxy がそのまま返す）。サインインは `https://app.example.com/oauth2/start` から。ブラウザを自動でサインインの画面へ送る形は oauth2-proxy の文書（forward auth の使い方）で確かめる。

- `service` は同じ HTTPRoute のルールの `name`（ここでは `oauth2`）。ほかのルートのルールは名指せない。
- クラスタの外の認証サーバなら `address` を使い、GatewayClass の `policy.middlewares.outboundHosts` で許す（7.2）。
- Gateway API の `ExternalAuth` フィルタ（experimental の CRD）でも同じことができる（rproxy の `forward_auth` になる）。

### 13.7 リスナーのフィルタ

ホストやパスにかかわらず、そのポートのすべてのリクエストに認証と流量の制限をかける。7.5 の YAML をそのまま使う（Gateway API v1.7 の experimental の CRD）。

```bash
kubectl -n default get gateway web -o jsonpath='{range .status.listeners[*]}{.name}{" "}{range .conditions[*]}{.type}={.status}({.reason}) {end}{"\n"}{end}'
# http Accepted=True(Accepted) ResolvedRefs=True(ResolvedRefs) Conflicted=False(NoConflicts) Programmed=True(Programmed)
curl -i -H 'Host: anything.example.com' http://$ADDR/no-such-path
# 認可サーバが断れば、ルートがなくても 403 などが先に返る
```

- ルートの `ExtensionRef` と同じ RproxyMiddleware をリスナーにも書くと、別々に 2 回働く。
- 認可サーバの Service を消すと、リスナーが `ResolvedRefs: False`（`InvalidListenerFilterRef`）になり、500 を返す（認証なしで通さない）。

## 14. 困ったとき

### 14.1 よく見る状態

| どこ | 条件（reason） | 意味 | すること |
|---|---|---|---|
| GatewayClass | `Accepted: False`（`InvalidParameters`） | クラスの parameters（chart の `managed.parameters`）が誤り | message の項目を直す。コントローラの namespace のものだけ指せる |
| Gateway | `Programmed: False`（`AddressNotAssigned`） | Service にアドレスが付かない | LB の実装を入れる（[PLATFORM.md](PLATFORM.md)）か `serviceType` を変える。`kubectl get svc` の `EXTERNAL-IP` が `<pending>` |
| Gateway | `Accepted: False`（`InvalidParameters`） | `RproxyGatewayParameters` の誤り・許されていない項目 | message の項目を直す。動いていた Gateway は前の形のまま（`Programmed: True`） |
| Gateway | `Accepted: False`（`UnsupportedAddress`） | `spec.addresses` の型が `IPAddress` でない | `IPAddress` にする |
| Gateway | `Programmed: False`（`AddressNotUsable`） | `spec.addresses` が `managed.addressCIDRs` の外、使えない IP | 管理者に範囲を開けてもらう、アドレスを直す |
| Gateway・リスナー | `Programmed: False`（`Invalid`、「rproxy refused the rule set: 409 limit ...」） | rproxy の数の上限 | `RPROXY_MAX_RULESETS`・`RPROXY_MAX_RULES` を上げる（[DESIGN.md](DESIGN.md) の「rproxy の置き方」） |
| Gateway・リスナー | `Programmed: False`（`Pending`） | 反映を待っている（証明書のファイル、Pod の起動） | 1〜2 分待つ。続くならコントローラのログ |
| Gateway・リスナー | `Programmed: False`（「rproxy ... has no rule sets (features.rulesets; rproxy v0.4.0 or later)」） | rproxy の版が古い（`rproxy.image`） | イメージを chart の版に戻す |
| リスナー | `ResolvedRefs: False`（`InvalidCertificateRef`） | 証明書の Secret がない・`tls.crt`・`tls.key` がない | Secret を作る（cert-manager なら Certificate の状態） |
| リスナー | `ResolvedRefs: False`（`RefNotPermitted`） | ほかの namespace の Secret に ReferenceGrant がない | Secret の namespace に ReferenceGrant |
| リスナー | `ResolvedRefs: False`（`InvalidRouteKinds`） | `allowedRoutes.kinds` に受けられない種類 | プロトコルに合う種類にする（[5.3](#53-ルートごとのリスナー)） |
| リスナー | `Accepted: False`（`PortUnavailable`） | fleet で、古い Gateway が同じポートを持つ | ポートを変える、`fleet.listen: addresses` |
| リスナー | `Accepted: False`（`UnsupportedProtocol`） | 知らないプロトコル | `HTTP`・`HTTPS`・`TLS`・`TCP`・`UDP` |
| リスナー | `Accepted: False`（`UnsupportedValue`） | リスナーのフィルタが使えない・そろっていない | message を見る（[7.5](#75-リスナーのフィルタ)） |
| ルート | `Accepted: False`（`NotAllowedByListeners`） | リスナーの `allowedRoutes` が namespace か種類を許していない | Gateway の `allowedRoutes`、`sectionName` |
| ルート | `Accepted: False`（`NoMatchingListenerHostname`） | ルートとリスナーのホスト名が交わらない | ホスト名を直す |
| ルート | `Accepted: False`（`NoMatchingParent`） | `parentRefs` の Gateway・リスナーがない | 名前・namespace・`sectionName` を直す |
| ルート | `Accepted: False`（`UnsupportedValue`） | rproxy が受け付けない値、RproxyMiddleware の誤り、許されていない外への URL | message の項目を直す |
| ルート | `ResolvedRefs: False`（`BackendNotFound`） | Service がない | Service の名前・ポート。その backend の分は 500 |
| ルート | `ResolvedRefs: False`（`RefNotPermitted`） | ほかの namespace の Service に ReferenceGrant がない | Service の namespace に ReferenceGrant（[6.2](#62-referencegrantほかの-namespace-の-servicesecret-を使う)） |
| RproxyPolicy | `Accepted: False`（`Invalid`・`TargetNotFound`） | 時間の形の誤り、指したリスナーがない | 値・`sectionName` を直す |
| RproxyRule | `Accepted: False`（`Conflicted`・`Invalid`・`NotAllowed`） | 同じキーのルールがある、rproxy が断った、クラスが切っている | ポートを変える、message を見る、`policy.rproxyRules` |
| BackendTLSPolicy | `Accepted: False`（`NoValidCACertificate`・`Conflicted`） | 使える CA がない、同じ対象のポリシーが古いものにある | CA の ConfigMap（`ca.crt`）、1 つにする |

ほかに：

- **404 が返る**：ホスト名（`Host` ヘッダ）がどのルートにも合わない。`curl -H 'Host: ...'` で試す。
- **500 が返る**：使える backend がない（Service の Pod が ready でない、`BackendNotFound`、`RefNotPermitted`）。
- **502・504**：転送先につながらない・応答が遅い（`backends.connectTimeout`・`responseTimeout`）。
- **つながらない（タイムアウト）**：Service の `EXTERNAL-IP`、ファイアウォール、`loadBalancerSourceRanges`、RproxyPolicy の `allowFrom`。rproxy の Pod が Ready か（READINESS GATES）。

### 14.2 コントローラのログ

```bash
# リーダーのログ（2 つのうちリーダーだけが反映する）
LEADER=$(kubectl -n rproxy-gateway-system get lease rproxy-gateway -o jsonpath='{.spec.holderIdentity}')
kubectl -n rproxy-gateway-system logs $LEADER --since=1h
# info 以外だけを読みやすく
kubectl -n rproxy-gateway-system logs $LEADER --since=1h \
  | jq -r 'select(.level != "INFO") | "\(.timestamp) \(.level) \(.fields.message) \(.fields | del(.message) | tostring)"'
```

ログは 1 行 1 つの JSON（`timestamp`・`level`・`fields.message`・ほかの項目）。よく見るもの：

| message | 意味 |
|---|---|
| `became the leader`・`lost the Lease to another replica` | リーダーが替わった（コントローラの入れ替えでふつう） |
| `could not renew the Lease in time; stepping down` | API サーバに届かず降りた（API サーバ・etcd の遅れ） |
| `rproxy refused a rule; applying the others` | rproxy が 1 つのルールを断った。状態にも出る |
| `rproxy refused routes; applying the others` | rproxy がルートを断り、それを外した |
| `rproxy refused the rule set` | セット全体を断られた（上限など） |
| `listener port taken by another Gateway` | fleet でポートの取り合い |
| `rule not running` | rproxy でルールが動いていない（ポートを開けない・名前解決） |
| `cannot deploy rproxy` | Deployment・Service を作れない（RBAC・admission・ResourceQuota） |
| `reconcile failed` | 反映の 1 回が失敗した。続けてやり直す |
| `not served by the API server (CRD not installed); looked for again later` | その種類の CRD がない。入れれば 30 秒ほどで見つける |
| `migration: not converted` | 移行で変換できなかったもの（[MIGRATION.md](MIGRATION.md)） |

もっと見るなら 12.5 で `RPROXY_GATEWAY_LOG=debug`。rproxy の側は `kubectl -n <ns> logs deploy/rproxy-<id> -c rproxy`、ファイルの受け渡しは `-c certsync`。

### 14.3 クラスタなしで確かめる

`rproxy-gateway render` は、マニフェストから rproxy のルールセットと状態を描く（クラスタも rproxy も要らない）。GatewayClass・Service・EndpointSlice も同じファイルに入れる。

```bash
docker run --rm -v $PWD:/w ghcr.io/max3584/rproxy-gateway:0.4.22 render -f /w/manifests.yaml
```

出力の `listeners`・`routes` の `conditions` に、クラスタで出るのと同じ理由が出る（rproxy の Pod に聞く項目、たとえばリスナーのフィルタは「not known (yet)」になる）。
