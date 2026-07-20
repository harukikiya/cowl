# ADR-0009: 別名圧力（Aliasing Pressure）と facts への pointee_const 追加

日付: 2026-07-19 / 状態: 採択

## 決定
1. facts の `VarDecl` に `pointee_const: Option<bool>` を追加する
   （FACTS_SCHEMA_VERSION 0.1.0 → 0.2.0。追加のみ＝マイナー。
   `#[serde(default)]` で旧 JSON の欠落は None に落ち、後方互換）。
   これは「宣言型の pointee が const 修飾されているか」という**構文事実**であり、
   「書込可能かどうか」の解釈は analysis 層が行う（facts firewall 維持）。
2. 指標 **Aliasing Pressure（別名圧力）** を Metrics に追加する
   （REPORT_SCHEMA_VERSION 0.2.0 → 0.3.0）。
   定義: Site ごとの「同時に生存する書込可能な束縛（エイリアス）の最大数」。
   ファイル集計はその最大値と、圧力2以上のサイト数。
   API_VERSION は不変（Request/Response の形は変わらず、facts/report は
   ペイロード内の schema_version で自己申告するため。W4 と同じ扱い）。

## なぜこの指標か
Rust の借用規則（可変参照は排他）が禁じているのは「同じリソースへ同時に
書ける名前が複数ある」状態そのもの。C でそれを測るのが別名圧力であり、
圧力が高い Site ほど「どの名前から書き換えられたか」の追跡が難しく、
リファクタリング時の退行リスクが高い、という仮説を持つ。

## pointee_const の測定規則
- 対象は「pointee の const」のみ（`const char *p` → Some(true)、
  `char *p` → Some(false)）。ポインタ自体の const（`char * const p`）は
  pointee の書込可能性と無関係なのでこの指標では見ない
  （記録もしない。必要になったら別フィールドとして起票する）
- L1 (tree-sitter): 宣言指定子列（最初の `*` より前）に const があるかの
  構文判定。typedef 名・マクロ由来の型は中身を見通せないので None
- L2 (libclang): pointee 型の is_const_qualified。typedef を解決できるので
  L1 で None になる一部が Some に解消される（W5 と同型の「精度向上」であり、
  L1/L2 互換テストではこの差分だけを許容する）
- None は「測れなかった」の自己申告として扱い、unknowns には**重複記録しない**
  （Option フィールド自体が曖昧さを表現しているため。unknowns は
  「追跡自体を諦めた箇所」用という既存の役割分担を崩さない）

## 指標の L1 操作化（ADR-0006 と同じ流儀）
原定義の「同時生存」は制御フローを前提とするが、L1/L2 とも解析は
イベント列の線形走査なので、次のように操作化する:

- 束縛の生存区間: Alloc / AssignFromVar による束縛開始から、
  (a) その Var への再代入（Alloc/AssignFromVar/AssignNull/AssignOpaque）
  (b) 消費（PassedTo consumed:Some(true) / Free）
  (c) EscapeReturn / EscapeStore による追跡範囲外への離脱
  (d) Site 自体の解放・Moved・Escaped 遷移
  のいずれか最初に起きた時点まで
- **書込可能束縛** = pointee_const が Some(false) の Var による生存中の束縛。
  Some(true) は数えない（それは「const 圧力」であり別物。
  この区別は受け入れ条件としてテストで固定する）
- pointee_const = None の束縛は書込可能数に**入れない**（過大評価より過小申告、
  add-metric スキルの原則）。ただし黙って捨てず、専用の ambiguous 系
  フィールドで計上して可視化する
- Site が Live の間のみ数える（解放後の dangling 別名は既存の
  use_after_free 診断の領分であり、指標を重複させない）
- 対象 Site はヒープ確保（AllocSource::Heap）**のみ**。AddressOf（`&x`）は
  対象外とする。概念上は借用でも「同時に書ける名前の数」は同じ意味を持つが、
  現行 facts の AllocSource::AddressOf は取得元の識別情報を持たない unit
  variant であり、「同じ x への `&x` が2箇所」を同一 Site に束ねること自体が
  原理的にできない（qa レビューで判明。当初この ADR は「両方」としていたが
  実装可能な範囲を超えていたため縮小した）。AddressOf の追跡は facts 拡張
  （取得元識別子の追加＝スキーマ変更の3点セット）を伴う別タスクとして
  起票する

## 表示（render_html）
- 指標カードに「別名圧力（最大）」「圧力2以上のサイト数」を出す（カードの
  実ラベル。render_html の実装・テストと一致させること）
- warn は最大値 >= 2 のとき。根拠: 2 以上は「可変参照の排他」相当の規律が
  破れている状態そのものであり、閾値に恣意性がない（1 は単独所有で正常）

## 却下案
- facts に `writable: bool` を直接入れる: 「書込可能」は解釈であり
  firewall 違反。const 修飾の有無という測定値を入れ、解釈は analysis に置く
- ポインタ演算・構造体フィールド経由の別名追跡: L1/L2 とも追わない
  （既存の割り切りを維持。将来の L3 課題として残す）
- 平均圧力・分布の同時掲載: 最大値と危険 Site 数で十分。
  指標の乱立を避ける（Safe-Rust Distance の「安易に合成しない」と同じ姿勢）
- ポインタ自体の const（`* const`）の記録: この指標に不要。
  使い道が出た時点でフィールド追加を再起票する
