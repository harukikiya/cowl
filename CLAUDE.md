# CLAUDE.md — cowl プロジェクト憲法

cowl は **Cソースの所有権・借用・ライフタイムを可視化する静的解析ツール**。
構成は「1コア＋薄いシェル」：解析コアが JSON in/out の安定APIを持ち、
CLI / MCPサーバ / VSCode拡張 はすべてその皮に過ぎない。

## アーキテクチャ（依存は一方向のDAG。逆流させたらレビューで差し戻し）

```
cowl-cli ──► cowl-api ──► cowl-front-ts ──► cowl-core
   │            │              │                ├─ facts       事実IR（契約）
 薄い殻     JSON契約      C→facts変換のみ       ├─ analysis    状態機械・指標・グラフ
                          (tree-sitter L1)      └─ render_*    HTML / DOT
```

| 層 | やってよいこと | やってはいけないこと |
|---|---|---|
| cowl-core | facts→解析→描画。純粋計算 | ファイルI/O・CLI・ネットワークを知ること |
| cowl-front-ts | 構文事実の抽出 | 所有権の**解釈**・診断・指標計算 |
| cowl-api | Request/Response の写像 | 解析ロジックの実装 |
| cowl-cli | 引数→Request の写像 | ロジック全般 |

## コマンド

```bash
make check     # fmt --check + clippy -D warnings + test（ワーカーの完了条件）
make test      # cargo test --workspace
make demo      # examples/ → out/*.html, out/*.dot を再生成
cargo run -p cowl-cli -- report examples/demo.c -o out/demo.html
echo '{"cmd":"version"}' | cargo run -q -p cowl-cli -- serve --stdio   # JSON API手打ち
```

## 絶対規約（違反はマージ不可）

1. **facts firewall**: facts には「測った構文事実」だけを入れる。推定・解釈は
   analysis 層へ。将来のL3(LLM補助)の出力は advisory であり、facts に混ぜない。
2. **L1の割り切りを「直さない」**: 制御フロー非考慮・スコープ非対応・マクロ非展開は
   仕様（cowl-front-ts/src/lib.rs 冒頭に一覧）。精度が欲しければ L2(libclang) の
   タスク（ROADMAP W5）として起票する。L1に小細工のパスを足すことは禁止。
3. **わからないは unknowns へ**: 追跡を諦めた箇所は黙って捨てず
   `Unknown { reason }` として自己申告する。曖昧さは一級の信号であり、
   所有権カバレッジ/曖昧度の指標がそれを測っている。
4. **スキーマ変更は3点セット**: facts/report/API の形を変えるときは
   ①バージョン定数を上げる ②ADRを書く（docs/adr/） ③ゴールデンテスト更新。
   手順の詳細は `.claude/skills/facts-schema/SKILL.md`。
5. **依存追加はADR必須**: workspace.dependencies にしか依存を書かない。
   新規クレート追加は理由をADRに残してから。
6. **テスト必須**: 挙動変更には必ずテストを足す。完了条件は `make check` 緑。
   Phase / EventKind / IssueKind を追加したら、analysis のテスト・
   render_html の凡例/CSS・（必要なら）DOTのスタイルを**同時に**更新する。
7. **コメント規約**: 日本語で「なぜこの形か」を書く。何をしているかの逐語訳は
   不要。分類ロジック・状態機械・妥協点には特に厚く。既存密度を下回らないこと。

## オーケストレーション運用

- オーケストレータ: Fable（このプロジェクトの設計判断・タスク分割・レビューを担当）
- ワーカー: `.claude/agents/` のサブエージェントに委任する
  - `frontend-worker` (sonnet) … tree-sitter分類・将来のlibclang。構文は繊細なのでsonnet
  - `analysis-worker` (haiku) … 状態機械・指標の追加。仕様が固まっている作業向け
  - `render-worker` (haiku) … HTML/DOT/凡例。見た目の作業
  - `qa-reviewer` (sonnet) … 差分レビューと `make check` の確認
- タスクは ROADMAP.md の W 番号単位で切る。1タスク = 1トピックの小さな差分。
  ワーカーへの指示には必ず「受け入れ条件」と「触ってよいファイル」を明記する。
- 挙動保存: モデル更新でオーケストレーションの質が変わる事故を防ぐため、
  重要な判断はこのファイルと ADR に**文章として**固定する。examples/ と
  テストは事実上の eval セットなので、勝手に削除・改変しない（追加は歓迎）。

## 安全な拡張ポイント（ここから触ると壊しにくい）

- `cowl-front-ts` の `BENIGN_FNS` / `CONSUMER_FNS` / `ALLOC_FNS` 表
  （関数を1つ足すだけで解像度が上がる。manpage確認＋テスト1本が条件）
- `cowl-core::analysis::Metrics` への指標追加（skills/add-metric の手順で）
- `cowl-api::Request` へのコマンド追加（CLI/MCP/拡張に同時に生える）
