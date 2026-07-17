# ROADMAP — ワーカータスク台帳

オーケストレータ（Fable）はここから W 番号単位でタスクを切り出し、
担当エージェント・受け入れ条件・触ってよいファイルを添えて委任する。
完了したら [x] を付け、成果への1行リンク（コミット/PR）を残す。

## W1: MCP サーバ [x]
- 内容: `cowl serve --stdio` をラップする MCP サーバを追加する
  （crates/cowl-mcp/ 新設。rmcp を想定。ツールは cowl_analyze /
  cowl_report_html / cowl_graph_dot の3本、Request と1対1写像）
- 担当: frontend-worker 以外なら可（analysis-worker 推奨）＋ qa-reviewer
- 受け入れ: MCP Inspector で3ツールが呼べる / 依存追加のADRあり / make check 緑
- 備考: JSON契約は cowl-api にしか無い前提を崩さない（ロジック再実装禁止）
- [x] 完了 (2026-07-17): 4c3af69 — rmcp 2.2 で crates/cowl-mcp 新設。
  「ラップ」は spawn でなく同一プロセスで dispatch_json を叩く形（ADR-0004）。
  テスト4本（duplex での MCP プロトコル統合含む）。Inspector CLI で
  3ツールの tools/call とエラー系（isError:true）を確認。analysis-worker
  実装＋qa-reviewer 承認（P0ゼロ、P1のドキュメント鮮度指摘は同コミットで反映）

## W2: VS Code 拡張の骨格 [x]
- 内容: `cowl serve --stdio` を child_process で spawn し、アクティブな
  Cファイルのライフタイム帯HTMLを Webview に表示するだけの最小拡張
- 受け入れ: 拡張から demo.c のレポートが表示される / 編集中バッファを
  source で送るパスが通っている（保存不要で更新）
- 備考: HTML は自己完結なので Webview にそのまま流せばよい
- [x] 完了 (2026-07-17): 0e108cb — editors/vscode 新設（ADR-0005）。
  受け入れ条件のうち source 送信パスは実バイナリ統合テスト8本で自動化
  （make check-vscode）。Webview 表示はコンテナに X が無く E2E 不可のため、
  demo.c 実バッファでのデータパス確認＋手動手順（editors/vscode/README.md）
  で受け入れ。claude ワーカー実装＋qa-reviewer（P0ゼロ、P1 3点反映済み）

## W3: L1 精度向上（表の拡充と小さな穴埋め）
- 内容: BENIGN_FNS / CONSUMER_FNS の拡充（POSIX頻出分）、
  `realloc` の引数位置対応（第1引数のみ消費）、`fopen`系の追加検討
- 担当: frontend-worker ＋ qa-reviewer
- 受け入れ: 追加1関数につきテスト1本 / manpage 確認をコミットメッセージに明記

## W4: 指標の第2陣
- 内容: Free-Site Multiplicity / Live-Range Length / Transfer Density を
  Metrics に追加（.claude/skills/add-metric の手順厳守）
- 担当: analysis-worker ＋ render-worker（カード表示）＋ qa-reviewer
- 受け入れ: 指標ごとにテスト2本以上（ゼロ件ケース含む）/ demo.html に表示

## W5: L2 フロントエンド（libclang）
- 内容: crates/cowl-front-clang/ 新設。clang-sys/clang クレートで
  同一 facts を出力。型情報により AssignOpaque と未知関数の一部を解消する
- 受け入れ: cowl-front-ts のゴールデンテストと同じCソースで facts 互換
  （イベント列の差分が「精度向上」として説明できること）/ ADRあり
- 備考: devcontainer に libclang は導入済み。ここまでは L1 を凍結して進む

## W6: 別名圧力（Aliasing Pressure）
- 内容: 同一Siteに同時生存する書込可能エイリアスの最大数。W5の型情報が前提
- 受け入れ: 指標定義のADR / add-metric 手順 / const圧力との区別をテストで固定
