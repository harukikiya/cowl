---
name: add-metric
description: cowl に新しい品質指標（Free-Site Multiplicity / Live-Range Length / Transfer Density など）を追加するときの標準手順。指標の追加・変更を依頼されたら必ずこの手順に従う。
---

# 新指標の追加手順

対象ファイル: crates/cowl-core/src/analysis.rs（と render_html.rs）

1. **定義を書く**: Metrics 構造体の doc コメントに、指標の定義・分母分子・
   分母0のときの値を日本語で書く。「何と相関するはずか」の仮説も1行残す
2. **フィールド追加**: Metrics にフィールドを足す（serde はそのまま導出される）
3. **計上ロジック**: analyze_function 内で数える。曖昧なケースは
   「数えない」のではなく ambiguous 側に倒す（過大評価より過小申告）
4. **集計**: Metrics::absorb（関数→ファイル集計）と finalize（率の計算）を更新
5. **表示**: render_html.rs の write_metric_cards にカードを足す。
   警告色（warn）を付ける閾値があるなら doc コメントに理由を書く
6. **テスト**: analysis::tests に「この facts ならこの値」を最低2本
   （ゼロ件のときの値も必ず1本）
7. **記録**: ROADMAP.md の該当 W 項目にチェックを付け、
   report スキーマが変わるので REPORT_SCHEMA_VERSION をマイナー上げ
8. make check → make demo で out/demo.html を目視確認

## 指標のバックログ（前段の設計議論より）
- Free-Site Multiplicity: 1確保サイトから到達しうる相異なる解放サイト数（>1は脆い）
- Live-Range Length: 生成→最終使用/解放までの行数
- Transfer Density: 関数境界をまたぐ所有権移譲回数 / KLOC
- Safe-Rust Distance: 曖昧度・ポインタ演算密度等の合成（重みは実証で決める。安易に合成しない）
