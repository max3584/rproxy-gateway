#!/bin/bash
# Builds rproxy (rproxy-api) from a git ref into dist/<arch>/rproxy-api (static musl,
# in an Alpine container). Usage: scripts/build-rproxy.sh <ref> [<out dir>]
# A ref that does not exist (a branch merged and deleted) falls back to master.
set -euo pipefail
ref=${1:-master}
out=${2:-dist/amd64}
repo=${RPROXY_REPO:-https://github.com/max3584/rproxy-api.git}
src=${RUNNER_TEMP:-/tmp}/rproxy-api-src
if ! git ls-remote --exit-code "$repo" "$ref" > /dev/null 2>&1 && ! [[ $ref =~ ^[0-9a-f]{40}$ ]]; then
  echo "::warning::rproxy-api ref $ref not found; using master"
  ref=master
fi
rm -rf "$src"
git init -q "$src"
git -C "$src" fetch -q --depth 1 "$repo" "$ref"
git -C "$src" checkout -q FETCH_HEAD
echo "rproxy-api $ref at $(git -C "$src" rev-parse HEAD)"
(cd "$src" && cargo build --locked --release)
mkdir -p "$out"
cp "$src/target/release/rproxy-api" "$out/rproxy-api"
