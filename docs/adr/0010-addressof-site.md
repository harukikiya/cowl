# ADR-0010: AddressOf 別名の Site 化 — facts への取得元識別子の追加

日付: 2026-07-20 / 状態: 採択

## 決定
1. facts の `AllocSource::AddressOf` に `target: Option<String>` を追加する
   （FACTS_SCHEMA_VERSION 0.2.0 → 0.3.0。フィールド追加のみ＝マイナー。
   旧 JSON の欠落は None に落ち後方互換）。
   これは「`&` の被演算子として現れた構文テキスト」という**構文事実**であり、
   「同じ場所を指すか」の解釈は analysis 層が行う（facts firewall 維持）。
2. analysis は同一 target の AddressOf を同一の**借用 Site** に束ね、
   別名圧力（Aliasing Pressure）の計上対象に加える。これにより
   ADR-0009 で「原理的に不可能」として Heap に縮小した対象制限を解除する
   （REPORT_SCHEMA_VERSION 0.3.0 → 0.4.0。フィールドの形は不変だが、
   既存3指標の測定対象が広がり同一入力で値が増えうるため、
   その事実をバージョンで自己申告する）。

## target の測定規則（L1・L2 で同一規則にする）
- `&x` の被演算子が**単純識別子のみ**の場合、その識別子テキストを Some で記録
- `&arr[i]` / `&s.field` / `&*p` などの複合式は None（「同一性を構文だけでは
  断定できない」の自己申告。`arr[i]` は i の実行時値で別アドレスになりうる）
- L2 も意味解析で深追いせず**L1 と同じ構文規則**を使う。ここで L2 だけ
  賢くすると、同じソースで Site の切り方が L1/L2 で変わり、facts 互換
  （フロントエンド差し替えの継ぎ目）が壊れるため。複合式の同一性解決は
  やるとしても L3 以降の課題
- unknowns への重複記録はしない（None が自己申告。ADR-0009 と同じ扱い）

## analysis の扱い（借用 Site の意味論）
- target=Some(name) の AddressOf は name をキーに関数内で借用 Site を共有する
  （スコープ非対応の L1 モデルでは同名=同一変数として扱う。追跡対象ポインタの
  name→VarId 表と同じ割り切り）
- target=None は従来どおり出現ごとに独立（束ねない）。AssignFromVar による
  別名付与（`q = p`）は従来どおり束縛の伝播として数える
- 借用 Site に Freed は無い（free(借用) は既存の invalid_free 診断の領分）。
  圧力計上上の束縛終了は Heap と同じ: 再代入・AssignNull/Opaque・
  consumed:Some(true)・Escape 系
- **既存の所有権診断（invalid_free・escape・dangling 等）と Binding::Borrow の
  遷移は 1 ビットも変えない**。W8 は圧力計上の対象拡大のみ

## 影響
- 別名圧力の3指標は、スタック変数への複数の書込可能別名
  （例: `int *p = &x; int *q = &x;` → 圧力2）を報告するようになる。
  examples の実測値が増える場合があるが、これは検出漏れの解消であり
  劣化ではない（W5 のマクロ展開と同型の「精度向上」）
- render のカード・warn 閾値は不変（対象が広がるだけ）

## 却下案
- `Binding::Borrow` を Site 機構に統合する全面改修: 既存診断の挙動保存
  （W6 で worktree diff により実証した「既存出力 1 ビット不変」の運用）を
  危険に晒す規模の割に、得られるのは同じ圧力値。計上側の拡張に留める
- report をメジャー（1.0.0）に上げる: 指標の定義文（ROADMAP W6 原文
  「同一Siteに同時生存する書込可能エイリアスの最大数」）自体は不変で、
  実装制限の解除＝測定範囲の拡大。削除・意味変更ではないためマイナーとする
- 複合式 target の正規化（空白除去した式テキストで束ねる等）: `arr[i]` の
  実行時多義性を構文で潰せない以上、同一テキスト=同一アドレスの断定は
  firewall 違反。None の自己申告に倒す
