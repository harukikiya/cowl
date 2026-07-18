# ADR-0006: 指標第2陣の定義と report スキーマ 0.2.0

日付: 2026-07-17 / 状態: 採択

## 決定
ROADMAP W4 の3指標を `Metrics` に追加し、`REPORT_SCHEMA_VERSION` を
0.1.0 → **0.2.0** に上げる（フィールド追加のみ＝マイナー。facts / API は不変）。

設計スキル（.claude/skills/add-metric）のバックログ定義は「到達しうる」など
**分岐を前提とした言葉**を含むが、L1 は制御フローを見ない。そこで本 ADR で
「L1 でどう測るか」の操作化を固定する。L2(libclang+CFG) 導入後も
**指標名と意図は変えず、操作化だけ精密化する**（値の連続性より意図の連続性を優先）。

## 指標の定義（L1 操作化）

### 1. Free-Site Multiplicity（解放サイト多重度）
- 意図: 1つの資源が複数の箇所で解放されうる構造は脆い
  （どの経路で解放済みかの追跡が読み手の負担になる）
- 操作化: Site ごとに「その Site を解放した**相異なるソース行**」を数える。
  解放 = `Free` イベント、または `consumed:Some(true)` の `PassedTo`
- フィールド:
  - `sites_freed` … 1回以上解放された Site 数
  - `free_sites_total` … 相異なる解放行数の全 Site 合計
  - `sites_multi_free` … 解放行が2つ以上ある Site 数
  - `free_site_multiplicity` = free_sites_total / sites_freed
    （**分母0のとき 1.0**＝理想値。解放が無ければ多重解放も無い、の意）
- 相関仮説: 1.0 超は double free 混入率・リファクタ時の解放漏れと相関するはず

### 2. Live-Range Length（生存区間長）
- 意図: 確保から最終接触までが長いほど、レビューで追う距離が長く
  管理ミス（解放漏れ・二重解放）が混入しやすい
- 操作化: Site ごとに「Alloc 行 → その Site に関わる**最後のイベント**の行」
  の行数（last - first + 1。同一行なら 1）。最後のイベントは
  Free / Move / Escape / 使用のうち最も後のもの
- フィールド:
  - `live_range_lines_total` … 全 Site の生存行数合計
  - `live_range_avg` = live_range_lines_total / sites_total（**分母0のとき 0.0**）
- 相関仮説: 平均生存行数はリーク・UAF の混入率と正の相関を持つはず

### 3. Transfer Density（移譲密度）
- 意図: 関数境界をまたぐ所有権移譲が多いほど、呼び出し規約という
  暗黙知への依存が増え、誤解によるリーク/二重解放が出やすい
- 操作化: Site が **Moved**（消費関数への引き渡し）または **Escaped**
  （return / 追跡外への格納）へ遷移した回数の合計を、解析対象 KLOC で割る。
  解析対象行数 = 各関数 span の行数（line_end - line_start + 1）の合計
- フィールド:
  - `transfers_total` … Moved / Escaped への遷移回数
  - `lines_analyzed` … 解析対象行数
  - `transfer_density` = transfers_total / (lines_analyzed / 1000)
    （**分母0のとき 0.0**）
- 相関仮説: 高密度のファイルほど所有権の所在の文書化が必要になるはず

## 集計の形
既存パターンを踏襲する: カウンタ（u32）は `absorb` で単純加算し、
率（f64）は `finalize` で計算する。関数単位・ファイル単位の両方で
同じ構造体を使う既存設計は変えない。

## 曖昧ケースの方針
曖昧マークされた Site も分母・分子に**含める**（既存の coverage/ambiguity と
同じ土俵）。「曖昧だから数えない」は指標の過大評価（良く見せる方向）に
働くため採らない。曖昧さ自体は ambiguity_rate が既に測っている。

## 表示（render_html のカード）
- 3指標をファイル/関数のカード列に追加する
- warn 色は free_site_multiplicity > 1.0 のみ（唯一「理想値からの逸脱」が
  自明な指標のため）。Live-Range / Transfer Density の警告閾値は
  実コードでの分布を見るまで**付けない**（根拠のない閾値は付けない方針。
  Safe-Rust Distance の「安易に合成しない」と同じ理由）

## 却下案
- **Free-Site Multiplicity を issues（double free 診断）から導出**:
  指標は診断と直交させる方針（カバレッジは「追い切れたか」、診断は
  「正しいか」、多重度は「構造が脆いか」）。診断ゼロでも多重解放構造は
  ありうる（L2 で分岐対応すれば顕在化する）ため独立に数える
- **Live-Range を Var 単位で測る**: 資源の生存を測りたいので Site 単位。
  Var 単位は別名の数だけ水増しされる
- **Transfer の分母を関数数にする**: 行数正規化の方が「読む量あたりの
  移譲頻度」という意図に合う。KLOC は業界慣行との比較可能性もある
