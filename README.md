# cowl — C OWnership & Lifetime visualizer

Cソースコードの**所有権・借用・ライフタイム**を静的解析し、
ソース行に重ねた「ライフタイム帯」と「所有権グラフ」として可視化するツール。
同時に、所有権カバレッジ・所有権曖昧度などの品質指標を算出する。

C には所有権が言語仕様として存在しないため、cowl がやるのは
「プログラマの頭の中にあった暗黙の規律の推定」である。だからこそ
**わからなかった箇所を隠さない**（unknowns / 曖昧度）ことを設計の背骨にしている。

## クイックスタート

```bash
# devcontainer で開く（VS Code: Reopen in Container）と全部入り。
# 素の環境なら Rust stable だけあればよい
make demo                 # examples/ → out/*.html, out/*.dot
open out/demo.html        # ライフタイム帯レポート（自己完結・依存ゼロ）

cargo run -p cowl-cli -- analyze examples/demo.c | jq .report.metrics
cargo run -p cowl-cli -- graph examples/uaf_alias.c | dot -Tsvg > g.svg
```

JSON API（MCP / VSCode拡張はこれを spawn する）:

```bash
echo '{"cmd":"analyze","source":"void f(void){char*p=malloc(4);}","file_name":"x.c"}' \
  | cargo run -q -p cowl-cli -- serve --stdio | jq .report.functions[0].issues
```

## 構成（1コア＋薄いシェル）

```
cowl-cli ──► cowl-api ──► cowl-front-ts ──► cowl-core
 (clap)     (JSON契約)    (tree-sitter L1)   (facts / analysis / render)
```

- 詳細な規約とレイヤ責務: **CLAUDE.md**（プロジェクト憲法）
- 設計判断: docs/adr/
- 今後のタスク: ROADMAP.md（W1: MCPサーバ, W2: VSCode拡張, W5: libclang L2 …）

## 何が見えるか（examples/demo.c）

| 関数 | 帯に現れるもの | 診断 |
|---|---|---|
| textbook | 所有(teal)→解放→NULL | なし（カバレッジに寄与） |
| leaky | 所有帯が関数末尾まで途切れない | leak_suspect |
| twice | 解放後にもう一度 ✕ | double_free |
| alias_uaf | 別名(indigo)経由のfreeで両変数がダングリング(coral) | use_after_free |
| ambiguous | 未知関数への引き渡しで「曖昧」計上 | なし（曖昧度に寄与） |

指標は診断と直交している：カバレッジは「追い切れたか」、診断は「正しいか」。
double free でも追い切れていればカバレッジには入る。

## オーケストレーションの始め方（Claude Code）

devcontainer 内でホストのログインがそのまま使える
（`.devcontainer/` の認証マウント3点セット。詳細は ADR-0003）。

```bash
claude   # コンテナ内で起動。ログイン不要のはず
```

最初のプロンプト例:

```
CLAUDE.md と ROADMAP.md を読んで。W1（MCPサーバ）を開始する。
タスクを分割し、analysis-worker に実装を、qa-reviewer にレビューを
委任して。受け入れ条件は ROADMAP の記載どおり。依存追加はADRを先に。
```

## ライセンス

MIT
