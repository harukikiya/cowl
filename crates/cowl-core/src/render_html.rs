//! # render_html — ライフタイム帯の可視化（単一HTML）
//!
//! 出力は **依存ゼロの自己完結HTML** 1ファイル。CDNもビルドも不要で、
//! ブラウザで開くだけ・チャットに貼るだけで見られることを最優先にする
//! （pbacid_lab.html などと同じ思想）。
//!
//! ## レイアウト戦略
//! SVGで絶対座標を計算する案もあったが、**HTMLテーブル**を選んだ。
//! 理由: 行 = ソース行、列 = 変数、という構造がテーブルと同型で、
//! 座標計算・フォントメトリクス依存・折返し問題が全部消える。
//! セルの背景色がそのまま「ライフタイム帯」になる。
//!
//! ## 色の意味（凡例と厳密に一致させること）
//! Phase と色は1対1。新しいPhaseを足すワーカーは必ずここと
//! `phase_class`/CSS/凡例の3点を同時に更新する。

use crate::analysis::*;
use crate::facts::Facts;
use std::collections::BTreeMap;
use std::fmt::Write;

/// HTMLエスケープ。ソースコードをそのまま埋め込むので必須
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Phase → CSSクラス名。色定義は下のCSSにある
fn phase_class(p: Phase) -> &'static str {
    match p {
        Phase::Uninit => "ph-uninit",
        Phase::Null => "ph-null",
        Phase::Owned => "ph-owned",
        Phase::Alias => "ph-alias",
        Phase::Borrowed => "ph-borrow",
        Phase::Dangling => "ph-dangling",
        Phase::Moved => "ph-moved",
        Phase::Escaped => "ph-escaped",
        Phase::OpaqueVal => "ph-opaque",
    }
}

fn mark_glyph(k: MarkKind) -> &'static str {
    match k {
        MarkKind::Alloc => "●",  // 確保
        MarkKind::Free => "✕",   // 解放
        MarkKind::Use => "·",    // 使用（控えめに）
        MarkKind::Move => "➤",   // ムーブ
        MarkKind::Escape => "↗", // 脱出
        MarkKind::Issue => "⚠",  // 問題
    }
}

/// facts（ソース原文のため）と report（解析結果）からHTML全文を生成する
pub fn render_html(facts: &Facts, report: &Report) -> String {
    let mut h = String::with_capacity(64 * 1024);
    // --- ヘッダ・CSS・凡例 ---------------------------------------------------
    // CSSカスタムプロパティで色を一元管理（teal=所有 / indigo=別名・借用 /
    // coral=危険、というプロジェクト共通のデザイントークン）
    let _ = write!(
        h,
        r#"<!doctype html>
<html lang="ja"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>cowl report — {file}</title>
<style>
:root {{
  --owned:#99f6e4; --owned-b:#0d9488;   /* teal   : 所有 */
  --alias:#c7d2fe; --alias-b:#6366f1;   /* indigo : 別名 */
  --borrow:#e0e7ff;                      /* indigo淡: 借用 */
  --danger:#fecaca; --danger-b:#ef4444; /* coral  : 危険 */
  --moved:#ddd6fe; --escaped:#fde68a; --null:#e5e7eb; --opaque:#e7e5e4;
  --ink:#1c1917; --sub:#78716c; --line:#e7e5e4; --bg:#fafaf9;
}}
* {{ box-sizing:border-box; }}
body {{ margin:0; padding:24px; background:var(--bg); color:var(--ink);
  font-family:"Hiragino Sans","Noto Sans JP",system-ui,sans-serif; }}
h1 {{ font-size:20px; margin:0 0 4px; }}
h2 {{ font-size:16px; margin:32px 0 8px; }}
.sub {{ color:var(--sub); font-size:12px; }}
.cards {{ display:flex; gap:12px; flex-wrap:wrap; margin:16px 0; }}
.card {{ background:#fff; border:1px solid var(--line); border-radius:10px;
  padding:10px 16px; min-width:130px; }}
.card .k {{ font-size:11px; color:var(--sub); }}
.card .v {{ font-size:22px; font-weight:700; }}
.card.warn .v {{ color:var(--danger-b); }}
table {{ border-collapse:collapse; background:#fff; border:1px solid var(--line);
  border-radius:8px; overflow:hidden; }}
th,td {{ border-bottom:1px solid var(--line); font-size:12px; }}
th {{ background:#f5f5f4; padding:4px 8px; position:sticky; top:0; }}
td.ln {{ color:var(--sub); text-align:right; padding:0 8px; user-select:none;
  font-family:ui-monospace,Menlo,Consolas,monospace; }}
td.code {{ font-family:ui-monospace,Menlo,Consolas,monospace; white-space:pre;
  padding:0 12px; min-width:320px; }}
tr.hasissue td.code {{ background:#fff1f0; }}
td.cell {{ width:64px; min-width:64px; text-align:center;
  font-family:ui-monospace,monospace; cursor:default; }}
/* 帯の本体：フェーズ色。左ボーダーで帯の輪郭を出す */
.ph-owned   {{ background:var(--owned);   border-left:3px solid var(--owned-b); }}
.ph-alias   {{ background:var(--alias);   border-left:3px solid var(--alias-b); }}
.ph-borrow  {{ background:var(--borrow);  border-left:3px solid var(--alias-b); }}
.ph-dangling{{ background:var(--danger);  border-left:3px solid var(--danger-b); }}
.ph-moved   {{ background:var(--moved); }}
.ph-escaped {{ background:var(--escaped); }}
.ph-null    {{ background:var(--null); }}
.ph-opaque  {{ background:var(--opaque); }}
.ph-uninit  {{ background:transparent; }}
td.cell.hl {{ outline:2px solid var(--ink); outline-offset:-2px; }}
.issues li {{ font-size:13px; margin:4px 0; }}
.issues .tag {{ display:inline-block; background:var(--danger);
  color:#7f1d1d; border-radius:4px; padding:0 6px; font-size:11px; margin-right:6px; }}
.unknowns li {{ font-size:12px; color:var(--sub); }}
.legend {{ display:flex; gap:10px; flex-wrap:wrap; margin:8px 0 16px; font-size:12px; }}
.legend span {{ display:inline-flex; align-items:center; gap:4px; }}
.sw {{ width:14px; height:14px; border-radius:3px; display:inline-block;
  border:1px solid var(--line); }}
footer {{ margin-top:32px; font-size:11px; color:var(--sub); }}
</style></head><body>
<h1>cowl — 所有権・ライフタイム可視化</h1>
<div class="sub">{file} ｜ facts {fv} / report {rv} ｜ L1（構文近似）解析：制御フローは未考慮</div>
"#,
        file = esc(&report.file),
        fv = esc(&facts.schema_version),
        rv = esc(&report.schema_version),
    );

    // ファイル全体の指標カード
    write_metric_cards(&mut h, &report.metrics);

    // 凡例（Phase の意味を利用者へ）
    h.push_str(
        r#"<div class="legend">
<span><i class="sw" style="background:var(--owned)"></i>所有(Owned)</span>
<span><i class="sw" style="background:var(--alias)"></i>別名(Alias)</span>
<span><i class="sw" style="background:var(--borrow)"></i>借用(&amp;x)</span>
<span><i class="sw" style="background:var(--danger)"></i>ダングリング</span>
<span><i class="sw" style="background:var(--moved)"></i>ムーブ済</span>
<span><i class="sw" style="background:var(--escaped)"></i>脱出(return/store)</span>
<span><i class="sw" style="background:var(--null)"></i>NULL</span>
<span><i class="sw" style="background:var(--opaque)"></i>追跡不能</span>
<span>● 確保 ✕ 解放 · 使用 ➤ ムーブ ↗ 脱出 ⚠ 問題</span>
</div>
"#,
    );

    // --- 関数ごとの本体 -------------------------------------------------------
    for func in &report.functions {
        let _ = writeln!(h, "<h2>関数 <code>{}</code></h2>", esc(&func.name));
        write_metric_cards(&mut h, &func.metrics);

        // 行→(フェーズ, マーク列) を変数ごとに引けるよう前計算しておく。
        // segments は昇順・非重複なのでBTreeMapに展開するだけでよい
        let mut phase_at: Vec<BTreeMap<u32, Phase>> = Vec::new();
        let mut marks_at: Vec<BTreeMap<u32, Vec<&Mark>>> = Vec::new();
        for v in &func.vars {
            let mut pm = BTreeMap::new();
            for s in &v.segments {
                for l in s.from_line..=s.to_line {
                    pm.insert(l, s.phase);
                }
            }
            let mut mm: BTreeMap<u32, Vec<&Mark>> = BTreeMap::new();
            for m in &v.marks {
                mm.entry(m.line).or_default().push(m);
            }
            phase_at.push(pm);
            marks_at.push(mm);
        }
        // Issueのある行をコード側でも薄く塗るための集合
        let issue_lines: std::collections::BTreeSet<u32> =
            func.issues.iter().map(|i| i.line).collect();

        h.push_str("<table><thead><tr><th>行</th><th style=\"text-align:left\">コード</th>");
        for v in &func.vars {
            let _ = write!(h, "<th class=\"vh\">{}</th>", esc(&v.name));
        }
        h.push_str("</tr></thead><tbody>\n");

        for line in func.span.line_start..=func.span.line_end {
            let cls = if issue_lines.contains(&line) {
                " class=\"hasissue\""
            } else {
                ""
            };
            let _ = write!(
                h,
                "<tr{}><td class=\"ln\">{}</td><td class=\"code\">{}</td>",
                cls,
                line,
                esc(facts.line_text(line)),
            );
            for (vi, _) in func.vars.iter().enumerate() {
                let phase = phase_at[vi].get(&line).copied().unwrap_or(Phase::Uninit);
                let (glyphs, title) = match marks_at[vi].get(&line) {
                    Some(ms) => {
                        let g: String = ms.iter().map(|m| mark_glyph(m.kind)).collect();
                        let t: Vec<String> = ms.iter().map(|m| m.note.clone()).collect();
                        (g, t.join(" / "))
                    }
                    None => (String::new(), String::new()),
                };
                // data-col でJSの列ハイライトを可能にする
                let _ = write!(
                    h,
                    "<td class=\"cell {} \" data-col=\"{}\" title=\"{}\">{}</td>",
                    phase_class(phase),
                    vi,
                    esc(&title),
                    glyphs,
                );
            }
            h.push_str("</tr>\n");
        }
        h.push_str("</tbody></table>\n");

        // 診断リスト
        if !func.issues.is_empty() {
            h.push_str("<ul class=\"issues\">\n");
            for is in &func.issues {
                let _ = writeln!(
                    h,
                    "<li><span class=\"tag\">{:?}</span>L{} <code>{}</code>: {}</li>",
                    is.kind,
                    is.line,
                    esc(&is.var),
                    esc(&is.message),
                );
            }
            h.push_str("</ul>\n");
        }
        // 「わからなかった」も隠さず表示する（信頼度の材料）
        if !func.unknowns.is_empty() {
            h.push_str("<details><summary class=\"sub\">解析器が追跡できなかった箇所</summary><ul class=\"unknowns\">\n");
            for u in &func.unknowns {
                let _ = writeln!(h, "<li>L{}: {}</li>", u.span.line_start, esc(&u.reason));
            }
            h.push_str("</ul></details>\n");
        }
    }

    // --- 最小限のJS: 同一変数列のホバー強調（依存ゼロ） -----------------------
    h.push_str(
        r#"<script>
// マウスが乗った列（=変数）の全セルを縁取りして、帯を目で追いやすくする。
// テーブルごとに data-col が振り直されるので、同じテーブル内だけを対象にする
document.addEventListener('mouseover', (e) => {
  const td = e.target.closest('td.cell');
  document.querySelectorAll('td.cell.hl').forEach(x => x.classList.remove('hl'));
  if (!td) return;
  const col = td.dataset.col;
  const table = td.closest('table');
  table.querySelectorAll(`td.cell[data-col="${col}"]`).forEach(x => x.classList.add('hl'));
});
</script>
<footer>generated by cowl（L1構文近似。分岐・ループの制御フローは考慮していません。
「追跡不能」「曖昧」はL2/libclang・L3/LLM補助で解消予定の箇所です）</footer>
</body></html>
"#,
    );
    h
}

/// 指標カード群（ファイル/関数で共用）
fn write_metric_cards(h: &mut String, m: &Metrics) {
    let warn_amb = if m.ambiguity_rate > 0.0 { " warn" } else { "" };
    let warn_iss = if m.issues_total > 0 { " warn" } else { "" };
    // 解放サイト多重度は 1.0 が理想値（資源が唯一の箇所でのみ解放される状態）。
    // 1.0 超は multiple-free のリスク構造を示唆するため、warn の根拠が自明。
    // 一方 live_range_avg / transfer_density は閾値の根拠が実コード分布の観察を
    // 必要とするため、現段階では warn 付けない（根拠なき閾値は付けない方針）。
    let warn_mult = if m.free_site_multiplicity > 1.0 {
        " warn"
    } else {
        ""
    };
    // 別名圧力は 2 以上が「同じリソースに同時に書ける名前が複数ある」
    // ＝ Rust の借用規則排他が破れている状態そのもの。
    // 1 は単独所有で正常。2 以上は明らかな警告対象（ADR-0009）。
    let warn_alias = if m.aliasing_pressure_max >= 2 {
        " warn"
    } else {
        ""
    };
    let _ = write!(
        h,
        r#"<div class="cards">
<div class="card"><div class="k">所有権カバレッジ</div><div class="v">{:.0}%</div></div>
<div class="card{}"><div class="k">所有権曖昧度</div><div class="v">{:.0}%</div></div>
<div class="card"><div class="k">確保サイト</div><div class="v">{}</div></div>
<div class="card{}"><div class="k">診断</div><div class="v">{}</div></div>
<div class="card{}"><div class="k">解放サイト多重度</div><div class="v">{:.2}</div></div>
<div class="card"><div class="k">平均生存区間</div><div class="v">{:.1}行</div></div>
<div class="card"><div class="k">移譲密度</div><div class="v">{:.1}/KLOC</div></div>
<div class="card{}"><div class="k">別名圧力（最大）</div><div class="v">{}</div></div>
<div class="card"><div class="k">圧力2以上のサイト数</div><div class="v">{}</div></div>
<div class="card"><div class="k">別名不明束縛数</div><div class="v">{}</div></div>
</div>
"#,
        m.ownership_coverage * 100.0,
        warn_amb,
        m.ambiguity_rate * 100.0,
        m.sites_total,
        warn_iss,
        m.issues_total,
        warn_mult,
        m.free_site_multiplicity,
        m.live_range_avg,
        m.transfer_density,
        warn_alias,
        m.aliasing_pressure_max,
        m.aliasing_pressure_sites,
        m.aliasing_unknown_bindings,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::{Metrics, Report};
    use crate::facts::Facts;

    fn make_test_facts() -> Facts {
        Facts {
            schema_version: "0.1.0".into(),
            file: "test.c".into(),
            source: "int main() {\n  int *p = malloc(10);\n  free(p);\n}\n".into(),
            functions: vec![],
        }
    }

    fn make_test_report_with_metrics(metrics: Metrics) -> Report {
        Report {
            schema_version: "0.3.0".into(),
            file: "test.c".into(),
            functions: vec![],
            metrics,
        }
    }

    #[test]
    fn test_new_metric_labels_in_output() {
        // 3つの新指標のラベルが HTML 出力に含まれることを確認（W6指標含む）
        let facts = make_test_facts();
        let metrics = Metrics {
            sites_total: 10,
            sites_resolved: 9,
            sites_ambiguous: 1,
            ownership_coverage: 0.9,
            ambiguity_rate: 0.1,
            issues_total: 0,
            issues_by_kind: Default::default(),
            sites_freed: 4,
            free_sites_total: 5,
            sites_multi_free: 1,
            free_site_multiplicity: 1.25,
            live_range_lines_total: 30,
            live_range_avg: 3.0,
            transfers_total: 31,
            lines_analyzed: 1000,
            transfer_density: 31.0,
            aliasing_pressure_max: 2,
            aliasing_pressure_sites: 3,
            aliasing_unknown_bindings: 1,
        };
        let report = make_test_report_with_metrics(metrics);
        let html = render_html(&facts, &report);

        assert!(
            html.contains("解放サイト多重度"),
            "新指標「解放サイト多重度」ラベルが出力に含まれるべき"
        );
        assert!(
            html.contains("平均生存区間"),
            "新指標「平均生存区間」ラベルが出力に含まれるべき"
        );
        assert!(
            html.contains("移譲密度"),
            "新指標「移譲密度」ラベルが出力に含まれるべき"
        );
        assert!(
            html.contains("別名圧力（最大）"),
            "新指標「別名圧力（最大）」ラベルが出力に含まれるべき"
        );
        assert!(
            html.contains("圧力2以上のサイト数"),
            "新指標「圧力2以上のサイト数」ラベルが出力に含まれるべき"
        );
        assert!(
            html.contains("別名不明束縛数"),
            "新指標「別名不明束縛数」ラベルが出力に含まれるべき"
        );
    }

    #[test]
    fn test_free_site_multiplicity_warn_condition() {
        // free_site_multiplicity > 1.0 のときに warn クラスが付き、
        // 1.0 ちょうどでは付かないことを確認
        let facts = make_test_facts();

        // Case 1: multiplicity > 1.0 → warn が付く
        let metrics_warn = Metrics {
            sites_total: 10,
            sites_resolved: 9,
            sites_ambiguous: 1,
            ownership_coverage: 0.9,
            ambiguity_rate: 0.1,
            issues_total: 0,
            issues_by_kind: Default::default(),
            sites_freed: 4,
            free_sites_total: 5,
            sites_multi_free: 1,
            free_site_multiplicity: 1.5, // > 1.0
            live_range_lines_total: 30,
            live_range_avg: 3.0,
            transfers_total: 31,
            lines_analyzed: 1000,
            transfer_density: 31.0,
            aliasing_pressure_max: 0,
            aliasing_pressure_sites: 0,
            aliasing_unknown_bindings: 0,
        };
        let report_warn = make_test_report_with_metrics(metrics_warn);
        let html_warn = render_html(&facts, &report_warn);

        // "解放サイト多重度" ラベルの直後のカード要素に warn クラスが付いているか確認
        let pattern_warn = r#"<div class="card warn"><div class="k">解放サイト多重度"#;
        assert!(
            html_warn.contains(pattern_warn),
            "multiplicity > 1.0 のとき card に warn クラスが付くべき。\nHTML:\n{}",
            html_warn
        );

        // Case 2: multiplicity == 1.0 → warn が付かない
        let metrics_no_warn = Metrics {
            sites_total: 10,
            sites_resolved: 9,
            sites_ambiguous: 1,
            ownership_coverage: 0.9,
            ambiguity_rate: 0.1,
            issues_total: 0,
            issues_by_kind: Default::default(),
            sites_freed: 4,
            free_sites_total: 4,
            sites_multi_free: 0,
            free_site_multiplicity: 1.0, // == 1.0（理想値）
            live_range_lines_total: 30,
            live_range_avg: 3.0,
            transfers_total: 31,
            lines_analyzed: 1000,
            transfer_density: 31.0,
            aliasing_pressure_max: 0,
            aliasing_pressure_sites: 0,
            aliasing_unknown_bindings: 0,
        };
        let report_no_warn = make_test_report_with_metrics(metrics_no_warn);
        let html_no_warn = render_html(&facts, &report_no_warn);

        let pattern_no_warn = r#"<div class="card"><div class="k">解放サイト多重度"#;
        assert!(
            html_no_warn.contains(pattern_no_warn),
            "multiplicity == 1.0 のとき card に warn クラスが付かないべき。\nHTML:\n{}",
            html_no_warn
        );
    }

    #[test]
    fn test_aliasing_pressure_max_warn_condition() {
        // aliasing_pressure_max >= 2 のときに warn クラスが付き、
        // < 2 では付かないことを確認（ADR-0009）
        let facts = make_test_facts();

        // Case 1: max >= 2 → warn が付く
        let metrics_warn = Metrics {
            sites_total: 10,
            sites_resolved: 9,
            sites_ambiguous: 1,
            ownership_coverage: 0.9,
            ambiguity_rate: 0.1,
            issues_total: 0,
            issues_by_kind: Default::default(),
            sites_freed: 4,
            free_sites_total: 4,
            sites_multi_free: 0,
            free_site_multiplicity: 1.0,
            live_range_lines_total: 30,
            live_range_avg: 3.0,
            transfers_total: 31,
            lines_analyzed: 1000,
            transfer_density: 31.0,
            aliasing_pressure_max: 2, // >= 2
            aliasing_pressure_sites: 1,
            aliasing_unknown_bindings: 0,
        };
        let report_warn = make_test_report_with_metrics(metrics_warn);
        let html_warn = render_html(&facts, &report_warn);

        let pattern_warn = r#"<div class="card warn"><div class="k">別名圧力（最大）"#;
        assert!(
            html_warn.contains(pattern_warn),
            "aliasing_pressure_max >= 2 のとき card に warn クラスが付くべき。\nHTML:\n{}",
            html_warn
        );

        // Case 2: max < 2 → warn が付かない（単独所有・理想値）
        let metrics_no_warn = Metrics {
            sites_total: 10,
            sites_resolved: 9,
            sites_ambiguous: 1,
            ownership_coverage: 0.9,
            ambiguity_rate: 0.1,
            issues_total: 0,
            issues_by_kind: Default::default(),
            sites_freed: 4,
            free_sites_total: 4,
            sites_multi_free: 0,
            free_site_multiplicity: 1.0,
            live_range_lines_total: 30,
            live_range_avg: 3.0,
            transfers_total: 31,
            lines_analyzed: 1000,
            transfer_density: 31.0,
            aliasing_pressure_max: 1, // < 2（理想値）
            aliasing_pressure_sites: 0,
            aliasing_unknown_bindings: 0,
        };
        let report_no_warn = make_test_report_with_metrics(metrics_no_warn);
        let html_no_warn = render_html(&facts, &report_no_warn);

        let pattern_no_warn = r#"<div class="card"><div class="k">別名圧力（最大）"#;
        assert!(
            html_no_warn.contains(pattern_no_warn),
            "aliasing_pressure_max < 2 のとき card に warn クラスが付かないべき。\nHTML:\n{}",
            html_no_warn
        );
    }

    #[test]
    fn test_aliasing_unknown_bindings_zero_display() {
        // aliasing_unknown_bindings が 0 でも値が表示されることを確認
        let facts = make_test_facts();
        let metrics = Metrics {
            sites_total: 10,
            sites_resolved: 9,
            sites_ambiguous: 1,
            ownership_coverage: 0.9,
            ambiguity_rate: 0.1,
            issues_total: 0,
            issues_by_kind: Default::default(),
            sites_freed: 4,
            free_sites_total: 4,
            sites_multi_free: 0,
            free_site_multiplicity: 1.0,
            live_range_lines_total: 30,
            live_range_avg: 3.0,
            transfers_total: 31,
            lines_analyzed: 1000,
            transfer_density: 31.0,
            aliasing_pressure_max: 1,
            aliasing_pressure_sites: 0,
            aliasing_unknown_bindings: 0, // ゼロでも表示される
        };
        let report = make_test_report_with_metrics(metrics);
        let html = render_html(&facts, &report);

        assert!(
            html.contains("別名不明束縛数"),
            "「別名不明束縛数」ラベルが出力に含まれるべき"
        );
        assert!(
            html.contains("<div class=\"card\"><div class=\"k\">別名不明束縛数</div><div class=\"v\">0</div></div>"),
            "aliasing_unknown_bindings が 0 のときも値が表示されるべき"
        );
    }
}
