# Gateway API への conformance レポートの提出（下書き）

このフォルダの `conformance/` は、kubernetes-sigs/gateway-api のリポジトリの同じ場所にそのまま置くもの：

| ここ | upstream |
|---|---|
| `conformance/reports/v1.6/max3584-rproxy-gateway/README.md` | 同じ（提出の README。必須） |
| `conformance/reports/v1.6/max3584-rproxy-gateway/experimental-v0.4.5-default-report.yaml` | 同じ（suite が書いたまま。手を入れない） |
| `conformance/list/implementations/max3584-rproxy-gateway/details.yaml` | 同じ（実装一覧の載せ方。`organization`・`project` はレポートと同じ） |

レポートは `e2e` ワークフローの手動の実行（`report_version=0.4.5`）の成果物 `conformance-report-v0.4.5` から、リリースした chart 0.4.5 とイメージ（`ghcr.io/max3584/rproxy-gateway:0.4.5`、`ghcr.io/max3584/rproxy-gateway/rproxy:0.4.3`）で作った（[docs/CONFORMANCE.md](../../CONFORMANCE.md)）。upstream の `tools/implist`（実装一覧とバッジを作る道具）にこのフォルダを足して動かすと、`Gateway Conformance v1.6.3 — Conformant` のバッジになる。

## オーナーがすること

1. **CLA（EasyCLA）に署名する**：Kubernetes のプロジェクトは CNCF の CLA が要る（<https://git.k8s.io/community/CLA.md>）。個人で、コミットに使うメールアドレス（GitHub のアカウントに登録したもの）で。最初の PR を開くと `linux-foundation-easycla` のボットが署名のリンクを出すので、そこからでもよい。署名がないと `cncf-cla: no` のまま止まる。
2. **AI ポリシーを読む**（<https://github.com/kubernetes-sigs/gateway-api/blob/main/AI-POLICY.md>）：
   - PR の説明は自分で書く（AI が書いた文章でのやりとりは不可。下の下書きは中身の確認用にして、自分の言葉で書き直す）。PR やレビューへの返事も自分で書く。
   - AI を使ったことを PR に書き、AIL のラベル（`/label ail/N`）を付ける（このレポートの手順・README・details.yaml は AI が用意したので、少なくとも `ail/3` 相当。自分で全部読んで確かめたうえで判断する）。
   - upstream へのコミットに `Co-Authored-By: Claude ...`・`Assisted-by` などの AI の trailer を付けない。
   - 出すものはすべて自分で読み、説明できるようにしておく（下の「確かめたこと」）。
3. **fork して PR を開く**：

   ```shell
   gh repo fork kubernetes-sigs/gateway-api --clone=true -- --depth 50
   cd gateway-api
   git fetch upstream main && git checkout -b rproxy-gateway-v0.4.5-conformance upstream/main   # gh の fork の clone：origin が自分の fork、upstream が kubernetes-sigs
   cp -r <rproxy-gateway>/docs/conformance/submission/conformance/. conformance/
   git add conformance/list/implementations/max3584-rproxy-gateway conformance/reports/v1.6/max3584-rproxy-gateway
   git commit -m "conformance: add rproxy-gateway v0.4.5 report and implementation entry"
   git push -u origin rproxy-gateway-v0.4.5-conformance
   gh pr create -R kubernetes-sigs/gateway-api --title "conformance: add rproxy-gateway v0.4.5 report and implementation entry"
   ```

   レポートの YAML は 1 バイトも変えない（upstream の README：「exactly as they have been created by the conformance suite」）。この PR の sha256：`8b40c41c233df807a8394d13b01cf47641372e4af131dbb0c53300ec9c837dbc`。
4. 初めての人の PR は org のメンバーが `/ok-to-test` を付けるまで CI が動かない。レビュアー（これまでは robscott さんなど）の `/lgtm`・`/approve` でマージされる。
5. 出したあと：Gateway API の新しい版（v1.7）が出たら、その版のレポートを出し直すと Conformant のまま（upstream の `tools/implist` は直近の 2 つのマイナー版のレポートだけを Conformant にし、それより古いものは Partial にする）。

## PR のタイトルと説明（下書き。自分の言葉で書き直す）

タイトル（最近のものに揃える。例：#5332 `conformance: add Portus v0.2.12 report and implementation entry`）：

```
conformance: add rproxy-gateway v0.4.5 report and implementation entry
```

説明（upstream の PR テンプレートの形）：

````markdown
**What type of PR is this?**

/kind documentation
/area conformance-test

**What this PR does / why we need it**:

Adds rproxy-gateway to the implementations list, with a conformance report for Gateway API v1.6.3
(experimental channel) from the v0.4.5 release: all five Gateway profiles (HTTP, GRPC, TLS, TCP, UDP),
core and extended, no failures and no skipped tests. The supported features were inferred from the
GatewayClass status (no `--supported-features`). The README has the steps to reproduce it from the
released Helm chart and images.

**Which issue(s) this PR fixes**:

None

**Does this PR introduce a user-facing change?**:
```release-note
NONE
```

**Was AI used in preparing this PR?**

/label ail/<N>
````

## 確かめたこと（レビューで聞かれたとき用）

- 2 回動かして同じ結果（https://github.com/max3584/rproxy-gateway/actions/runs/37894639981 （提出するレポート）、https://github.com/max3584/rproxy-gateway/actions/runs/37895657152 ）。成果物は 90 日で消えるので、要るなら早めに落としておく。
- `implementation.version` は `v0.4.5`（リリースのタグ。ブランチ名ではない）、README の表は semver でリリースのページへのリンク、`gatewayAPIChannel`（experimental）・`mode`（default）・ファイル名（`<channel>-<version>-<mode>-report.yaml`）が揃っている。
- フォルダは `v1.6`（いまの upstream はマイナー版のフォルダ。`gatewayAPIVersion` v1.6.3 のマイナーと一致すること（`hack/verify-reports.sh`）を満たす）。
- 5 つのプロファイルとも core・extended が `success`、Failed 0・Skipped 0、`unsupportedFeatures` なし（各プロファイルの extended の機能をすべて名乗る）。provisional の試験は飛ばしていない（`--skip-provisional-tests` なし）。
- 使ったのは公開した chart とイメージ（環境のファイルにダイジェスト）：コントローラ `ghcr.io/max3584/rproxy-gateway@sha256:51ed1cfd20bf0a709473930ffa32b137053d2988ad3922946ae0750cb33a801b`、rproxy `ghcr.io/max3584/rproxy-gateway/rproxy@sha256:29de180c864017a093fa15c40dc3a11ed8e1bb4906f9c740b46cd0a048e04e24`。
- 既定から変えた chart の値は 2 つ：`managed.serviceType=ClusterIP`（kind にロードバランサがない）と `managed.addressCIDRs={192.0.2.0/24}`（`GatewayStaticAddresses` の usable address を許す。既定では静的なアドレスを受け付けない）。README の手順に書いてある。
- GatewayClass が名乗る `HTTPRouteExternalAuth*` は Gateway API v1.6.3 の suite にない名前なので、レポートには出ない（suite が無視する）。
- 実装一覧のページの表（`hack/docsy-generate-conformance.py`）が出すのは HTTP・GRPC・TLS のプロファイルと Mesh だけで、TCP・UDP はレポートにあっても表には出ない。バッジ（`tools/implist`）は全プロファイルを見る。
