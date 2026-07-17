# ADR-0001: 1コア＋薄いシェル構成と facts IR

日付: 2026-07-17 / 状態: 採択

## 決定
- 解析コア（cowl-core）は facts（構文事実IR）を入力とし、
  フロントエンド（C→facts）とは facts スキーマだけで接続する
- 外部消費者（CLI/MCP/VSCode拡張）は cowl-api の JSON 契約
  （`dispatch_json`、`serve --stdio` の1行1JSON）だけに依存する

## 理由
- フロントエンドは L1(tree-sitter)→L2(libclang)→L3(LLM補助) と
  差し替え・併用する計画があり、継ぎ目を facts に固定すると
  コア・描画・指標を書き直さずに精度だけ上げられる
- MCP/拡張をプロセス境界の JSON にしたのは、コアの型を外部に
  露出させないため。スキーマバージョンで互換を管理できる

## 帰結
- スキーマ変更は重い（3点セット手順）。代わりに層の独立性を得る
- facts firewall: 事実と解釈の分離が構造として強制される
