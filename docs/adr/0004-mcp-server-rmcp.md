# ADR-0004: MCPサーバ（cowl-mcp）— rmcp 採用と cowl-api 直接リンク

日付: 2026-07-17 / 状態: 採択

## 決定
`crates/cowl-mcp/` を新設し、MCP公式 Rust SDK **rmcp 2.2** による
stdio MCPサーバを実装する（ROADMAP W1）。

- ツールは `cowl_analyze` / `cowl_report_html` / `cowl_graph_dot` の3本のみ。
  それぞれ `cowl_api::Request::{Analyze, RenderHtml, RenderDot}` への1対1写像
- 実装は「MCPツール引数 → `cowl_api::Request` を構築 → JSON化 →
  `dispatch_json` → 返ってきたエンベロープJSONをそのまま text で返す」だけ。
  解析ロジック・レスポンス整形は一切持たない
- エンベロープが `ok:false` のときは `CallToolResult::error`（MCPの
  ツールレベルエラー）に写す。dispatch_json は panic しない契約なので、
  サーバプロセスも落ちない
- ROADMAP の「`cowl serve --stdio` をラップする」は、子プロセス spawn ではなく
  **同一プロセスで同じ JSON 契約（dispatch_json）を叩く**形で実現する

## 依存追加（workspace.dependencies）
| 依存 | 用途 |
|---|---|
| rmcp 2.2（server, macros, transport-io, schemars） | MCP公式SDK。プロトコル追随を委譲 |
| tokio 1（macros, rt-multi-thread, io-std） | rmcp が要求する非同期ランタイム |
| schemars 1 | ツール引数の JSON Schema 導出（rmcp の要求と同版） |

tokio は cowl-mcp の中に閉じ込める。cowl-core / cowl-front-ts / cowl-api /
cowl-cli は同期のまま変えない。依存の向きは cowl-mcp ──► cowl-api のみ
（一方向DAGを維持。cowl-mcp は cowl-cli と同格の「薄い皮」）。

## 理由
- **rmcp**: MCP仕様（プロトコル版数・initialize手順・スキーマ）は動きが速い。
  公式SDKに追随を任せ、cowl 側はツール写像だけを持つのが保守コスト最小
- **直接リンク**: dispatch_json は `cowl serve --stdio` が回している関数
  そのものなので、挙動は spawn 方式と同一。spawn 方式は cowl バイナリの
  パス解決・プロセス監視という複雑さだけを追加し、利点がない
- **Request 型で構築してから JSON 化**: 契約とのズレをコンパイル時に検出できる。
  JSON文字列を手組みすると typo が実行時まで漏れる

## 却下案
- **`cowl serve --stdio` を子プロセス spawn**: 上記の通り複雑さのみ増える
- **cowl-cli に `cowl mcp` サブコマンド追加**: tokio が CLI に混ざり
  「薄い殻」規約に反する。重い依存は独立クレートに隔離する
  （tree-sitter を cowl-front-ts に隔離したのと同じ論法）
- **Rust関数API（`analyze` 等）を直接呼ぶ**: JSON契約を経由しないため、
  MCP経由とCLI経由でエラーエンベロープの形が食い違う余地が生まれる
- **自前JSON-RPC実装**: プロトコル追随の保守が発生する。論外
