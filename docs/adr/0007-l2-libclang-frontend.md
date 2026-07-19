# ADR-0007: L2 フロントエンド — libclang で cowl-front-clang を新設

日付: 2026-07-19 / 状態: 採択

## 決定
`crates/cowl-front-clang/` を新設し、libclang による L2 フロントエンドを
追加する（ROADMAP W5）。tree-sitter 版 L1（cowl-front-ts）は**凍結**し、
挙動を変えない。facts スキーマ（0.1.0）・EventKind・AllocSource・Unknown も
不変。公開APIは L1 と同じ2関数（`extract_file` / `extract_source`）のみで、
cowl-api/cowl-cli/cowl-mcp への接続は本タスクのスコープ外（後述）。

L2 で「自然に」解決するのは次の2点だけとし、それ以外の精度向上
（スコープ対応・制御フロー考慮・関数戻り値の所有権推定など）は実装しない:

- (a) マクロ展開: libclang はプリプロセス後のASTを見るため、
  `#define MY_ALLOC(n) malloc(n)` 越しの呼び出しも展開後の姿
  （callee="malloc"）で観測できる
- (b) const ポインタ引数: 呼び出し先の宣言が同一TU内に実際に書かれていて、
  対応する仮引数の型が `const T*` なら consumed:Some(false) と断定する

## 依存選定とスパイクの検証結果

### クレート選定
安全ラッパの `clang` クレート（2.0.0）を採用した。生FFIの `clang-sys`
直叩きは、`clang_Cursor_getVarDeclInitializer`（VarDeclの初期化式取得。
libclang 12.0+ 限定）と `clang_getCursorBinaryOperatorKind` /
`clang_getCursorUnaryOperatorKind`（演算子種別。libclang 17.0+ 限定）の
3関数について検討したが、いずれも `clang` クレート2.0.0が定義する
バージョン機能の上限（`clang_10_0`）を超えるため素直には呼べない。
呼ぶには clang-sys を直接追加して独自に `unsafe` ブロックを書く必要があり、
「どうしても不可の場合のみ」の閾値には届かないと判断し、代替手段
（後述のヒューリスティック）で回避した。

- VarDeclの初期化式: 「宣言型を表す TypeRef を除いた最後の子ノード」を
  初期化式とみなすヒューリスティックで代替（`var_init` 関数）。
  対象がポインタ変数の単純な初期化式に限られる現スコープでは
  tree-sitter版の `child_by_field_name("value")` と同程度に頑健
- 演算子種別: BinaryOperator/UnaryOperator の「左の子の終端 〜 右の子の
  始端」のソーステキストをそのまま演算子とみなすヒューリスティックで
  代替（`binary_op_text` / `unary_op_text`）。空白の有無・`p=malloc(4)` と
  `p == 0` のどちらでも正しく取れることをスパイクで確認済み。
  後置 `++`/`--` では前提（前置の順序）が崩れて空文字列になりうるが、
  判定対象が "*" と "&"（Cに後置形が無い）だけなので実害はない

### スパイク手順
`examples/demo.c` を最小コードでパースし、関数カーソルを列挙する捨てコード
（`/tmp` 配下、リポジトリには含めない）で以下を確認してから本実装に着手した。

### スパイクで判明した実装上の必須事項
1. **libclang 18 の検出には `runtime` フィーチャが必須**。デフォルト
   （ビルド時リンク）では clang-sys が `libclang.so` / `libclang-*.so`
   という無版数ファイル名しか探さないが、Ubuntu の `libclang1` パッケージは
   版数付きファイル（`libclang-18.so.1` 等）しか提供しない
   （`libclang-dev` 相当の無版数シンボリックリンクが無い）。`runtime`
   フィーチャは実行時 dlopen に切り替わり、版数付きファイル名も探索対象に
   加わる。ビルド時に `LIBCLANG_PATH` は不要（探索コード自体がバイナリに
   埋め込まれ、実行時に標準ライブラリ検索パス
   `/usr/lib/x86_64-linux-gnu` 等から自動発見する）。見つからない環境
   （libclang が非標準の場所にある等）では実行時に `LIBCLANG_PATH`
   環境変数で明示する
2. **`Entity::evaluate()` 等を使うには `clang_10_0` フィーチャが必要**
   （`clang` クレートのバージョン機能の中で最大）。実行時の libclang が18
   でも安全（新しいlibclangは古いバージョン機能が要求するシンボル集合を
   完全に包含する）
3. **`clang::Clang` はプロセス全体で同時に1インスタンスまで**という制約が
   ある（2つ目の `Clang::new()` は Err になる）。`cargo test`
   はスレッド並列でテストを実行するため、`CLANG_GATE`（`Mutex<()>`）で
   extract 呼び出し全体を直列化した。詳細は crate 内のコメント参照
4. **診断（コンパイルエラー相当のメッセージ）が出ても `parse()` 自体は
   失敗しない**。`malloc`/`free`等の未宣言呼び出し（`-Wimplicit-function-declaration`）
   がError重大度で出てもASTは完全な形で得られることを実測した
   （requirement 7 の「読めた範囲の facts と unknowns で自己申告する」を
   libclang 側が自然に満たす）
5. **値の未宣言識別子（`NULL`）は関数呼び出しと異なり復元されない**。
   `malloc`等の未宣言"呼び出し"はlibclangが「暗黙宣言」を合成して
   ASTを保つが、`NULL`のような未宣言の**値**識別子はVarDeclの初期化式
   ごとASTから消える（`int *a = NULL;` が `int *a;` 相当になる）。
   cowl-front-ts のゴールデンテスト（`null_and_address_of_inits`）は
   `#include` 無しでこのパターンを使うため、デフォルト引数に
   `-DNULL=((void*)0)` を加えて回避した。実ヘッダ（stdlib.h等）を
   `#include` している場合も、標準ヘッダは NULL を再定義する前に
   `#undef` するため衝突しないことを `examples/demo.c` /
   `examples/stream_realloc.c` で確認済み（診断ゼロ）
6. **`-std=gnu11` を採用**（`-std=c11` ではなく）。`-std=c11`
   では POSIX関数（`strdup`等）が「未宣言関数」として `int` 型の
   暗黙宣言にフォールバックし、戻り値をポインタ変数へ代入する箇所で
   `-Wint-conversion` 診断が追加で出る。`-std=gnu11` では libclang の
   組み込み関数認識が広がりこの診断が消える。**AST構造自体は
   どちらの `-std` でも壊れないこと**（診断があっても呼び出し式・
   引数は完全な形で残ること）を実測済みで、こちらは「エラー耐性の
   確保」ではなく「診断ノイズの削減」が採用理由

## const ポインタ引数規則の根拠と限界
- 根拠: C では `const T*` を受け取った関数がその領域を `free` するには
  const を外すキャストが要る。それをしない（大多数の）関数にとって、
  `const T*` 引数は「借用」であるという慣習が事実上の契約になっている
- 判定方法: 呼び出し式の `get_reference()` で解決した宣言の対応する
  仮引数について、`get_type().get_pointee_type().is_const_qualified()`
  を見る
- 「宣言が同一TU内に実際に書かれている」ことの判定: 仮引数に**名前が
  付いているか**で判定する。libclang は宣言の見えない呼び出しに対して
  「暗黙宣言」を合成し、`get_reference()` はその合成宣言に解決される
  （未知関数呼び出しが致命傷にならない仕組みそのもの）。組み込み認識
  されている関数（例: `-std=gnu11` での `strdup`）では戻り値・引数の
  型まで正しく推論されるが、**合成された仮引数は常に無名になる**ことを
  スパイクで実証した。一方、ソースに実際に書かれた宣言の仮引数は
  （本ADRが対象とする関数群では）名前を持つ。「宣言位置と呼び出し位置が
  一致するか」を見る案も検討したが、同じ未宣言関数を複数箇所で呼ぶと
  2回目以降の呼び出しでは不一致になり不安定なため採らなかった
- 優先順位: 既存の既知関数テーブル（CONSUMER_FNS/BENIGN_FNS。manpage
  確認済み）を必ず先に引き、そこで判定できない（None を返す）関数に
  ついてのみ const 規則を試す。表の知識を上書きしない
- 限界（検出できないケース）:
  - const を外して free する病的コード（`free((void*)p)` を
    `const char*` 引数の中で行うような関数）は誤って
    consumed:Some(false) と断定してしまう。C の慣習を破るコードは
    最初から検出対象外というのが本規則のスコープ
  - 宣言は見えるが定義（本体）が見えない関数について、規則は
    「型シグネチャ」だけを見て「実装が本当にconstを守っているか」は
    検証しない（そもそも静的に決定不能な場合が多い）
  - ヘッダに `const` を付け忘れている実際は借用のみの関数は解決されない
    （曖昧のまま）。これは「安全側に倒れる」誤りなので許容する
  - 宣言は実在するが仮引数名を省略した書き方（`void f(const char *);`）は、
    暗黙宣言の合成仮引数（常に無名）と AST 上区別できないため規則が発火せず
    曖昧のまま残る（qa レビューの `-ast-dump` 実測で確認）。誤発火ではなく
    取りこぼしであり、上と同じく安全側に倒れる誤りとして許容する

## 既知関数テーブルの pub 化と依存方向の判断
`cowl-front-ts::{ALLOC_FNS, CONSUMER_FNS, BENIGN_FNS, consumed_for}` を
`pub` にし、cowl-front-clang から**通常依存**として再利用する
（依存方向: `cowl-front-clang → cowl-front-ts → cowl-core` の一方向DAG）。

- 二重定義しない理由: 表は「manpageを確認済みの知識」そのものであり、
  L1/L2で意味論が変わるわけではない。二重に持つと W3のような表の
  拡充が起きるたびに同期漏れでドリフトする恐れがある
- pub化のみで挙動は変えていない: cowl-front-ts 側の変更は `pub` 修飾と
  「なぜ公開するか」のコメント追加のみ。分類ロジック・既存テストは
  無変更（cowl-front-ts の19個のテストは本タスクの前後でバイト単位も
  含め無変更のまま全通過することを確認済み）
- 将来 front-ts/front-clang 双方から使う純粋ロジックがさらに増えたら
  `cowl-front-common` への切り出しを検討する。W5時点では表4つと
  `consumed_for` だけなので、専用クレートを作るコストに見合わないと
  判断し見送った（過剰設計を避ける）

## API 未接続の理由と follow-up
`cowl-api::Request` にフロントエンド選択肢を足す変更は行っていない。
現状 cowl-api は暗黙に cowl-front-ts を使っており、CLI/MCP/VSCode拡張の
どこからも cowl-front-clang は呼ばれない。

- 理由: フロントエンド切替をAPIに露出するには `Request` へのフィールド
  追加（例: `frontend: "l1" | "l2"`）が要り、CLAUDE.mdの絶対規約4
  「スキーマ変更は3点セット」（バージョン定数・ADR・ゴールデンテスト）を
  伴う。これはW5「facts互換のL2フロントエンドを作る」という検証目的の
  スコープを超える別の意思決定（デフォルトをどちらにするか、切替の
  UIをどう見せるか等）を含むため、本タスクでは意図的に見送った
- follow-up（次のタスクとして起票する）: cowl-api に
  `Request::Analyze`（他コマンドも同様）へ `frontend` 選択オプションを
  追加し、report/facts のスキーマ改定（3点セット）とセットで設計する。
  既定値は当面 L1 のまま（L2は精度が高いがビルドに libclang を要求する
  ため、環境非依存を優先するCLIのデフォルトには不向き）とし、
  明示的にL2を選んだ利用者だけがlibclang依存の恩恵を受ける形を想定する

## ビルド要件: libclang と環境変数
- 要件: libclang 18 系の共有ライブラリ（`libclang-18.so.*` 等）が
  動的リンカの標準検索パス、または `llvm-config --prefix` の
  `lib`/`lib64` 配下、または `LIBCLANG_PATH` の指す場所に存在すること。
  Ubuntu では `libclang1-18` （または同等パッケージ）で足りる
  （`libclang-dev` の無版数シンボリックリンクは不要 — 上記スパイク
  結果1参照）
- 環境変数: 通常は不要（実行時 dlopen が標準パスから自動発見する）。
  非標準の場所にインストールされている環境では `LIBCLANG_PATH` に
  ディレクトリを指定する（例:
  `LIBCLANG_PATH=/usr/lib/llvm-18/lib cargo test -p cowl-front-clang`）
- devcontainer には libclang が導入済み（ROADMAP W5 備考）。CI/開発機で
  libclang が見つからない場合のみ `LIBCLANG_PATH` の設定を検討する

## 却下案
- **clang-sys を直接使う**: 上記スパイクの通り3関数だけが版数の壁で
  安全ラッパから呼べないが、「どうしても不可」と言えるほどではなく
  ヒューリスティックで代替可能だったため見送った。将来これらの関数が
  真に必要な機能（C++対応等、本プロジェクトのスコープ外）を要求したら
  再検討する
- **`clang` クレートより新しいメジャーバージョンへの追随**: 執筆時点の
  crates.io に `clang_18_0` 相当のバージョン機能を安全ラッパとして
  公開するリリースが見当たらなかった。`clang_10_0` の機能で本タスクの
  要件（Entity走査・型情報・evaluate()）は全て満たせたため、
  無理に追随しなかった
- **L1を書き換えてlibclangに一本化する**: ROADMAP W5の前提
  「L1は撤去せず併存」に反する。tree-sitterの軽さ（libclang不要・
  エラー耐性・編集中バッファ耐性）はVSCode拡張の要件（ADR-0002）で
  あり、L2はあくまで精度が欲しい利用者向けの選択肢として追加する設計
- **`-DNULL` を使わずソース側にプリアンブルを注入する**: `#define NULL ...`
  を文字列としてソース先頭に連結する案も検討したが、それだと行番号が
  ずれ、facts の Span がユーザーのソースとずれてしまう。`-D`
  コマンドライン引数はソーステキストに触れずマクロだけを定義できるため
  この問題が起きない
- **NULL問題をunknownsで自己申告するだけにして-Dを使わない**: 「読めた
  範囲で自己申告する」というrequirement 7の精神には合うが、
  `null_and_address_of_inits` という**必須**の互換ゴールデンテストが
  `#include` 無しの `NULL` を使っており、これを満たせなくなる
  （requirement 2 で「どうしても一致させられない本質的な差分は
  ごまかさず報告」とあるが、`-DNULL` という1行の対処で解決できる
  問題を「本質的な差分」として片付けるのは妥当でないと判断した）
