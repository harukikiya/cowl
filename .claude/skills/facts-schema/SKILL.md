---
name: facts-schema
description: facts / report / API の JSON スキーマを変更するときの必須手順。EventKind・Phase・Request の追加削除、フィールドの意味変更を行う前に必ず読む。
---

# スキーマ進化の掟

facts は L1/L2/L3 すべてのフロントエンドと、CLI/MCP/VSCode拡張すべての
消費者が共有する**契約**。軽率に変えると全レイヤが同時に壊れる。

## 手順
1. 変更理由と代替案を ADR（docs/adr/NNNN-*.md）に書く。**書いてから**実装する
2. バージョン定数を上げる:
   - facts の形 → FACTS_SCHEMA_VERSION（cowl-core/src/facts.rs）
   - report の形 → REPORT_SCHEMA_VERSION（analysis.rs）
   - リクエスト/レスポンスの形 → API_VERSION（cowl-api/src/lib.rs）
   互換が壊れる変更（削除・意味変更）はメジャー、追加はマイナー
3. 全フロントエンドを同期する（現在: cowl-front-ts。W5以降: cowl-front-clang も）
4. ゴールデンテスト更新: cowl-front-ts のテストは「このC構文→このイベント列」を
   凍結している。差分が意図通りかを1件ずつ確認してから書き換える
5. EventKind を足した場合: analysis の状態機械の match を埋め、
   render_html の凡例/CSS/マーカーを render-worker と同期する
6. make check 緑 + make demo の目視確認

## 禁止事項
- facts に解釈済みの値（「これはムーブ」「これはリーク」）を入れること。
  facts は構文事実、解釈は analysis。この firewall がプロジェクトの背骨
