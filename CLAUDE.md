# CLAUDE.md — rproxy-gateway

rproxy（`../rproxy-api`）の Kubernetes Gateway API コントローラ（Rust / kube-rs）。Gateway API のリソースを rproxy のルールセットにして、rproxy の制御 API で反映する。設計は rproxy-api の `docs/DESIGN-v0.4.md` 3.（#28）と 14.（決めたこと）。管理 UI は `../TCP-UDP-rproxy-ui`。3 つのリポジトリを同じフォルダに置いて一緒に扱う。

## コマンド

```bash
cargo build
cargo test                    # 単体テスト
cargo clippy --all-targets -- -D warnings
cargo fmt --check             # rustfmt.toml（タブ、幅 140）
RPROXY_BIN=.../rproxy-api cargo test --test rproxy   # 本物の rproxy（v0.4、ルールセット）で。CI の e2e の real rproxy ジョブ
cargo run -- crds > charts/rproxy-gateway/crds/rproxy.max3584.net.yaml   # CRD を変えたら（テストが食い違いを見つける）
cargo run -- render -f manifests.yaml                # クラスタなしでルールセットを描く
scripts/render-config.sh      # chart を変えたら・版を上げたら config/（Kustomize の base）を描き直す（CI の config (kustomize) が食い違いを見つける）
scripts/e2e.sh                # kind での e2e（Docker が要るので CI のランナーの VM で。手元にはない）
scripts/conformance.sh        # Gateway API の conformance（同上）
scripts/acceptance.sh         # 受け入れテスト（同上。手動の acceptance ワークフローだけ。SOURCE=published|checkout、TOPOLOGY=l2-local|l2-cluster|bgp|nodeport-lb）
```

## 構成

- `src/render/`：変換（純粋な関数。`World` のスナップショット → `GatewayPlan`）。`http.rs`（HTTPRoute）、`l4.rs`（TLS・TCP・UDP）、`policy.rs`（RproxyPolicy・RproxyRule）、`params.rs`（RproxyGatewayParameters の参照・検証・合わせ方。docs/DESIGN-v0.4.x.md の A）、`migrate.rs`・`traefik_mw.rs`（Ingress・Traefik、rproxy-api の `contrib/traefik2rproxy.py` と同じ変換）、`backends.rs`（EndpointSlice）、`hostname.rs`、`status.rs`
- `src/controller/`：watch（`cache.rs`）、反映のループと `sync_pod`（`mod.rs`）、状態（`status.rs`）、rproxy の配置（`provision.rs`）、CA・トークンの Secret（`bootstrap.rs`）。`tests.rs` は制御 API の文書どおりの偽の rproxy
- `src/rproxy/`：制御 API のクライアントと形。`src/certsync.rs`：rproxy の Pod の中で証明書をファイルにする
- `charts/rproxy-gateway/`：Helm chart（`crds/` は `rproxy-gateway crds` の出力。コントローラの設定は ConfigMap `rproxy-gateway-config` の `RPROXY_GATEWAY_*`）。`config/`：chart から描いた Kustomize の base（`default`・`fleet`・`crd`）と `samples/`（手で書く overlay の例）。`Dockerfile`（コントローラ）、`Dockerfile.rproxy`（rproxy）は Alpine で、Alpine のジョブが作った musl のバイナリを入れる
- 決めごとは docs/DESIGN.md、移行は docs/MIGRATION.md、conformance の結果は docs/CONFORMANCE.md（どれも日本語・英語）

## 約束

- rproxy とは制御 API（rproxy-api の `docs/openapi.json`）だけでつながる。rproxy のコードや DB を直接使わない。制御 API に足りないものがあれば rproxy-api に issue を立てる。
- `Cargo.lock` はコミットしている。CI・イメージのビルドは `--locked`。依存を変えたら `Cargo.lock` も同じ PR に入れる。Renovate は cargo を `rangeStrategy: update-lockfile` で更新する。
- `deny.toml` と `.github/workflows/deny.yml`（`cargo deny --locked check`）：RustSec の勧告、ライセンス、取得元を、依存を変える PR と毎日確かめる（必須のチェックではない）。直せない勧告は `advisories.ignore` に理由と外す条件を書いて足す。
- CI の実行環境：ジョブは Alpine のコンテナ（`container: alpine:3.24`）で動かし、Debian / Ubuntu の重いイメージは使わない（rproxy-api と同じ）。最初のステップで `apk add` する（`actions/checkout` には git、`actions/cache`・`rust-cache` には GNU の tar と zstd、`run:` を bash で動かすには bash が要る）。コンテナの中では `$RUNNER_TEMP`・`$GITHUB_WORKSPACE` を使う。例外は kind を使うジョブ（e2e・conformance）だけで、kind がノードを Docker のコンテナとして作るのでランナーの VM で直接動かす（そこでは Rust をビルドせず、Alpine のジョブが作ったバイナリを使う）。
- 必須のチェックはジョブの `name:` で照合するので、名前を変えない。必須のチェックはジョブが main に入ってから足す（先に足すとほかの PR が止まる）。
- 利用者向けの文書は日本語と英語の両方がある（日本語は `README.md`・`docs/*.md`、英語は `README.en.md`・`docs/en/`）。片方を変えたら、同じ PR でもう片方も直す。文書を足したら英語版も作り、互いの先頭のリンクを揃える。
- コミットのメッセージと PR は日本語（rproxy-api と同じ形：`feat: ...`、`fix: ...`、`docs: ...`、`ci: ...`、`chore: ...`）。PR にはマイルストーン（いまは v0.4.0）を付ける。
- PR のブランチに追加で push する前に、その PR がまだ開いているか（`gh pr view <n> --json state`）を確かめる。マージ後に push したコミットは main に入らない。
- e2e・conformance は rproxy-api の master（`RPROXY_REF`、手動の実行ではタグやブランチも指定できる）から rproxy をビルドする。rproxy v0.4.0 が出たら、そのタグに固定するか考える。
- managed の rproxy の Pod・Service の形（preStop、readiness gate、プローブ、PDB、externalTrafficPolicy）を変えたら、受け入れテストをブランチから回す（`gh workflow run acceptance.yml --ref <branch> -f source=checkout -f topology=<形>`）。replicas が 2 以上では Pod の削除・drain・rollout restart・parameters の変更の途切れが `GAP_LIMIT`（既定 3 秒）を超えると失敗する（l2-local・l2-cluster・nodeport-lb。`bgp` とノードの喪失は CNI・ロードバランサ・ネットワークの側の時間なので記録だけ）。
- 変換を変えたら、rproxy が受け付けるか（`tests/rproxy.rs`）と conformance の結果（docs/CONFORMANCE.md）も確かめる。`SUPPORTED_FEATURES`（`src/controller/mod.rs`）と `scripts/conformance.sh` の `FEATURES` を揃える。
- バージョンは rproxy-api・UI と別々に進める。最初のリリース（v0.4.0）は rproxy v0.4.0 と一緒に出す。
