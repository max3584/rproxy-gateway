# CLAUDE.md — rproxy-gateway

rproxy（`../rproxy-api`）の Kubernetes Gateway API コントローラ（Rust / kube-rs）。Gateway API のリソースを rproxy のルールセットにして、rproxy の制御 API で反映する。設計は rproxy-api の `docs/DESIGN-v0.4.md` 3.（#28）と 14.（決めたこと）。管理 UI は `../TCP-UDP-rproxy-ui`。3 つのリポジトリを同じフォルダに置いて一緒に扱う。

## コマンド

```bash
cargo build
cargo test                    # 単体テスト
cargo clippy --all-targets -- -D warnings
cargo fmt --check             # rustfmt.toml（タブ、幅 140）
```

## 約束

- rproxy とは制御 API（rproxy-api の `docs/openapi.json`）だけでつながる。rproxy のコードや DB を直接使わない。制御 API に足りないものがあれば rproxy-api に issue を立てる。
- `Cargo.lock` はコミットしている。CI・イメージのビルドは `--locked`。依存を変えたら `Cargo.lock` も同じ PR に入れる。Renovate は cargo を `rangeStrategy: update-lockfile` で更新する。
- `deny.toml` と `.github/workflows/deny.yml`（`cargo deny --locked check`）：RustSec の勧告、ライセンス、取得元を、依存を変える PR と毎日確かめる（必須のチェックではない）。直せない勧告は `advisories.ignore` に理由と外す条件を書いて足す。
- CI の実行環境：ジョブは Alpine のコンテナ（`container: alpine:3.24`）で動かし、Debian / Ubuntu の重いイメージは使わない（rproxy-api と同じ）。最初のステップで `apk add` する（`actions/checkout` には git、`actions/cache`・`rust-cache` には GNU の tar と zstd、`run:` を bash で動かすには bash が要る）。コンテナの中では `$RUNNER_TEMP`・`$GITHUB_WORKSPACE` を使う。例外は kind を使うジョブ（e2e・conformance）だけで、kind がノードを Docker のコンテナとして作るのでランナーの VM で直接動かす（そこでは Rust をビルドせず、Alpine のジョブが作ったバイナリを使う）。
- 必須のチェックはジョブの `name:` で照合するので、名前を変えない。必須のチェックはジョブが main に入ってから足す（先に足すとほかの PR が止まる）。
- 利用者向けの文書は日本語と英語の両方がある（日本語は `README.md`・`docs/*.md`、英語は `README.en.md`・`docs/en/`）。片方を変えたら、同じ PR でもう片方も直す。文書を足したら英語版も作り、互いの先頭のリンクを揃える。
- コミットのメッセージと PR は日本語（rproxy-api と同じ形：`feat: ...`、`fix: ...`、`docs: ...`、`ci: ...`、`chore: ...`）。PR にはマイルストーン（いまは v0.4.0）を付ける。
- PR のブランチに追加で push する前に、その PR がまだ開いているか（`gh pr view <n> --json state`）を確かめる。マージ後に push したコミットは main に入らない。
- バージョンは rproxy-api・UI と別々に進める。最初のリリース（v0.4.0）は rproxy v0.4.0 と一緒に出す。
