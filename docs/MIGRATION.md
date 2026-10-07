English: [en/MIGRATION.md](en/MIGRATION.md)

# Ingress・Traefik からの移行

`--migrate-to <namespace>/<name>` を付けると、コントローラは Ingress と Traefik の CRD を読んで、その Gateway のルールセットに足す（rproxy-api の docs/DESIGN-v0.4.md 3.3。既定では読まない）。Gateway API に移るまでの間、同じ rproxy で動かすためのもの。

- 読むだけで、Ingress・Traefik のリソースには何も書かない（状態も書かない）。
- 変換は rproxy-api の `contrib/traefik2rproxy.py`（docs/MIGRATING-FROM-TRAEFIK.md）と同じ。変換できないものは外して、コントローラのログ（`migration: not converted`、内容が変わったときに 1 回）と `rproxy-gateway render` の `notes` に出す。
- 移行先の Gateway に同じポートのリスナーがあれば、そのルールに足す（`http` どうし・TLS の有無が同じとき）。合わなければ移行するほうを外す。
- 移行したルートは Traefik と同じ優先（`priority`、なければ `match` の長さ）。Gateway API のルートの `priority` は 1 からの番号なので、同じポートでは移行したルートが先に試されることが多い。

## フラグ

| フラグ | 既定 | 意味 |
|---|---|---|
| `--migrate-to` | なし（読まない） | 移行先の Gateway（`namespace/name`） |
| `--ingress-class` | `rproxy` | 読む Ingress のクラス（`spec.ingressClassName`、なければ注釈 `kubernetes.io/ingress.class`） |
| `--traefik-entrypoint` | `web=80,websecure=443` | Traefik のエントリーポイント（`名前=ポート[/udp]`）。`entryPoints` を書かないルートは、そのプロトコルのすべてのエントリーポイント |

## 変換

| 元 | rproxy |
|---|---|
| IngressRoute の `match` | そのまま（rproxy の `match` は Traefik v3 の書き方）。v2 の `Headers`・`HeadersRegexp`・`HostHeader`・`Query(a=b)` は書き直す。v2 のプレースホルダ（`{name:regex}`）とほかの matcher は外す |
| IngressRoute の `services`（Service、port は番号か名前） | EndpointSlice の Pod の IP の `servers`。`scheme`（なければポート 443・名前が https で始まるものは https）、`weight`、`passHostHeader: false`。`TraefikService` は外す |
| IngressRoute の `tls.secretName` | 証明書のファイル（certsync） |
| `tls.certResolver` | `{acme: <resolver>, domains: [...]}`（`domains`、なければ `Host()` の名前）。rproxy の設定ファイルの `global.acme` にそのリゾルバが要る |
| `tls.options`（TLSOption） | `tls.options`（`minVersion`・`cipherSuites`）、`client_auth`（`clientAuth.secretNames` の Secret の `tls.ca` / `ca.crt` をファイルに）、`alpn`。`maxVersion`・`curvePreferences`・`sniStrict` は外す |
| Middleware | rproxy のミドルウェア（`redirectScheme`、`redirectRegex`、`stripPrefix`、`addPrefix`、`replacePath(Regex)`、`headers`、`rateLimit`、`inFlightReq`、`ipAllowList`、`basicAuth`（Secret の `users` を htpasswd のファイルに）、`forwardAuth`、`compress`、`retry`、`circuitBreaker`、`errors`、`buffering`、CrowdSec のプラグイン、`chain` は展開） |
| TLS のルーターと TLS でないルーターが同じポート | TLS のほうだけ（rproxy のルールは TLS か平文のどちらか） |
| IngressRouteTCP `HostSNI(*)` | `targets`（Pod の IP）、`proxyProtocol` は `source_ip: proxy_v1/v2` |
| IngressRouteTCP `HostSNI(名前)` / `HostSNIRegexp`（単純な接尾辞だけ） | `tls.routes`（宛先は Service の ClusterIP）。`tls.passthrough` は `sni`、そうでなければ `terminate`。HTTP のルーターと同じポートなら、名前つきの passthrough だけ `passthrough: true` で足す |
| IngressRouteUDP | udp のルールの `targets`（エントリーポイントごとに 1 つ） |
| Ingress（`networking.k8s.io/v1`） | `web` のポートの `http` のルート。`tls` があれば `websecure` のポートにも（証明書は `secretName`）。`pathType` は Prefix → 区切りの `/` で合わせる、Exact → `Path`、ImplementationSpecific → `PathPrefix`。`defaultBackend` → `http.default` |
