# rproxy-gateway

[![CI](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/ci.yml)
[![cargo-deny](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml/badge.svg?branch=main)](https://github.com/max3584/rproxy-gateway/actions/workflows/deny.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue)](LICENSE)
[![Renovate](https://img.shields.io/badge/renovate-enabled-brightgreen?logo=renovatebot)](https://github.com/max3584/rproxy-gateway/issues?q=is%3Aissue+is%3Aopen+%22Dependency+Dashboard%22)

日本語: [README.md](README.md)

A Kubernetes controller for [rproxy](https://github.com/max3584/rproxy-api). It turns Gateway API resources into rproxy rules and applies them through rproxy's control API (rule sets, `PUT /rulesets/{name}`) (max3584/rproxy-api#28; the design is docs/en/DESIGN-v0.4.md 3. in rproxy-api).

- rproxy knows nothing of the Kubernetes API. This controller and rproxy meet only at the control API (`docs/openapi.json` in rproxy-api).
- Work in progress. The first release ships together with rproxy v0.4.0 (milestone v0.4.0).

## Development

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt --check
```

## License

MIT ([LICENSE](LICENSE))
