# rproxy-gateway

[![CI](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml)
[![cargo-deny](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Renovate](https://img.shields.io/badge/renovate-enabled-brightgreen?logo=renovatebot)](https://github.com/max3584/rproxy-gateway/issues?q=is%3Aissue+is%3Aopen+%22Dependency+Dashboard%22)

English: [README.en.md](README.en.md)

[rproxy](https://github.com/max3584/rproxy-api) の Kubernetes コントローラ。Gateway API のリソースを rproxy のルールにして、rproxy の制御 API（ルールセット、`PUT /rulesets/{name}`）で反映する（max3584/rproxy-api#28、設計は rproxy-api の docs/DESIGN-v0.4.md 3.）。

- rproxy は Kubernetes の API を知らない。このコントローラと rproxy は制御 API（rproxy-api の `docs/openapi.json`）だけでつながる。
- 開発中。最初のリリースは rproxy v0.4.0 と一緒に出す（マイルストーン v0.4.0）。

## 開発

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## ライセンス

MIT（[LICENSE](LICENSE)）
