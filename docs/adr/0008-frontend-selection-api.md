# ADR-0008: フロントエンド選択の API 露出 — Request.frontend で L2 を選択可能に

日付: 2026-07-19 / 状態: 採択

## 決定
`cowl_api::Request` の Analyze / RenderHtml / RenderDot に
`frontend: Option<Frontend>` を追加し、W5 で新設した cowl-front-clang
（libclang L2。ADR-0007）を CLI / MCP / VSCode 拡張から選べるようにする。

- `pub enum Frontend { Ts, Clang }` を **cowl-api に**定義する
  （serde: `rename_all = "snake_case"` → JSON 値は `"ts"` / `"clang"`）
- 省略時（None）は従来どおり Ts（tree-sitter L1）。serde の Option は
  フィールド欠落を None にするので、**既存クライアントは無変更で従来挙動**
- `API_VERSION` を 0.1.0 → **0.2.0** に上げる（フィールド追加のみ＝マイナー）
- facts / report のスキーマは**不変**（FACTS_SCHEMA_VERSION /
  REPORT_SCHEMA_VERSION は触らない）。フロントエンドがどちらでも facts の
  形は同一 — それが ADR-0002/0007 の「facts がフロントエンド差し替えの
  継ぎ目」という設計の検証でもある
- 各シェルの露出:
  - cowl-cli: `analyze` / `report` / `graph` に `--frontend <ts|clang>`
    （clap ValueEnum のローカル型、既定 ts）。`serve` は素通しなので変更不要
    （JSON クライアントは新フィールドをそのまま使える）
  - cowl-mcp: 3 ツールの引数に `frontend`（文字列。写像方式は後述）
  - VSCode 拡張: 設定 `cowl.frontend`（enum "ts"/"clang"、既定 "ts"）を
    リクエストへ素通し。拡張は値を**検証しない**: 不正値や libclang 不在は
    サーバの ok:false エンベロープに委ね、既存のエラー表示経路をそのまま使う
    （拡張にロジックを持たせないという層責務の実践）

## なぜ Request へのフィールド追加か
CLAUDE.md の「安全な拡張ポイント」に *cowl-api::Request へのコマンド追加
（CLI/MCP/拡張に同時に生える）* とある。フィールド追加も同じ性質を持つ:
API 層の 1 箇所に選択肢を足せば、3 つのシェルすべてが同じ契約で同じ機能を
得る。シェルごとに独自のフロントエンド起動パス（例: CLI だけ直接
cowl-front-clang を呼ぶ）を作ると、エンベロープ契約（エラーでも JSON、
panic しない）の外に抜け道ができてしまう。ADR-0007 の follow-up
（「cowl-api に frontend 選択オプションを追加し、3点セットとセットで設計」）
を実行するのが本 ADR である。

## なぜ Frontend 型は cowl-api に置くか（cowl-core ではなく）
フロントエンド選択は「**入力をどう facts にするか**」という入力解決の概念で
あり、リクエストの語彙 = API 層の所有物。cowl-core は facts しか知らない
（CLAUDE.md のアーキテクチャ表: core はファイル I/O・CLI を知ってはならない）。
core に置くと「core がフロントエンドの存在を知る」ことになり依存 DAG の
向きが崩れる。

## 既定 ts の理由
1. **後方互換**: `Option<Frontend>` の欠落 = None = Ts なので、旧クライアント
   （フィールドを知らない JSON）の挙動・レスポンス形状は 1 バイトも変わらない
2. **役割分担の維持**（ADR-0002 / ADR-0007）: L1 (tree-sitter) の存在理由は
   libclang 不要・エラー耐性・**編集中バッファ耐性**であり、これは VSCode
   拡張の要件。L2 は精度（マクロ展開・const ポインタ引数）が欲しい利用者が
   明示的に選ぶ選択肢。既定を clang にすると「実行環境に libclang 共有
   ライブラリがあること」が全消費者の暗黙の前提になってしまい、
   環境非依存を優先する既定として不適切（ADR-0007 の想定どおり）

## エラー挙動（panic しない契約の維持）
- **未知の値**（例: `"gcc"`）: serde のパース失敗として既存の err_json
  エンベロープ（`{"ok":false, ...}`）に落ちる。dispatch_json は panic しない
  契約のまま
- **L2 選択時に libclang が実行環境に無い場合**: cowl-front-clang は
  `runtime` フィーチャの実行時 dlopen で libclang を探す（ADR-0007）。
  見つからない場合も `Clang::new()` / parse が `Err` を返すだけなので、
  dispatch_json のエラーエンベロープで返り**プロセスは落ちない**。
  serve --stdio / MCP サーバは次のリクエストを処理し続けられる

## MCP の写像方式の判断
**採用**: ツール引数は `frontend: Option<String>` で受け、
`serde_json::from_value::<cowl_api::Frontend>` で型付き Frontend へ写像して
`Request` に詰める。

- 判断基準（オーケストレータ指定）: **cowl-api に schemars 依存を漏らさない**
  ことを最優先する。rmcp の Parameters は引数構造体に
  `schemars::JsonSchema` を要求するため、`cowl_api::Frontend` を引数構造体に
  直接使うには cowl-api 側へ derive を足すしかない。API 層は「CLI・MCP・拡張
  が唯一依存してよい層」であり、特定シェル（MCP）の都合のスキーマ生成依存を
  背負わせるのは依存の向きとして不健全（CLI が clap の ValueEnum を API 層に
  求めないのと同じ理屈）
- 文字列 → Frontend の写像に **cowl-api の serde 定義そのものを使う**
  （`serde_json::from_value`）ことで、有効値の集合（"ts"/"clang"）の定義は
  cowl-api の serde 属性の 1 箇所に留まる。cowl-mcp 側に手書きの
  match 表を持つと、将来フロントエンドが増えたときに同期漏れでドリフトする
- 不正値（"gcc" 等）はツールレベルエラー（rmcp の is_error:true）として
  「"ts" か "clang"」という案内文で返す。引数の型検証はもともと MCP 層
  （rmcp のスキーマ検証）の責務であり、エンベロープの再解釈には当たらない

## 却下案
- **cowl-api の Frontend に `#[derive(schemars::JsonSchema)]` を足す**:
  上記のとおり依存汚染。feature フラグで隠す案も、workspace.dependencies に
  schemars を「API 層のため」に載せる時点で同じ問題
- **version レスポンスへ `frontends` 一覧（利用可能フロントエンドの列挙）を
  足す**: 今回は見送り。「clang が実行時に本当に使えるか」は dlopen して
  みないと分からず、version コマンドが libclang を触りにいくのは責務過剰。
  必要になったら「能力問い合わせ」として別タスクで設計する
- **既定を clang にする / 自動フォールバック（clang が無ければ ts）**:
  自動フォールバックは「同じリクエストが環境によって別の facts を返す」
  という再現性の穴になる。どちらで解析したかは利用者の明示選択のみで決まる
  方が、曖昧さを一級の信号として扱う本プロジェクトの精神に合う
- **cowl-cli の `serve` に `--frontend` を足す（サーバ既定値の上書き）**:
  serve は素通しの殻であり、選択はリクエスト側（JSON の frontend フィールド）
  に既に露出している。サーバ側既定値という第 2 の決定箇所を作ると
  「どちらが勝つか」という新しい契約が要る。不要
- **Version リクエストにも frontend を足す**: version は入力を解決しない
  コマンドなので対象外（フィールドの意味が無い）

## 3点セットの充足（CLAUDE.md 絶対規約 4）
1. バージョン定数: `API_VERSION` 0.1.0 → 0.2.0
2. ADR: 本文書
3. ゴールデンテスト: 既存テストは**無変更のまま**全通過（省略時挙動の凍結）。
   新規に (a) frontend:"clang" の analyze 成功 (b) 省略と "ts" 明示の facts
   一致 (c) 不正値のエラーエンベロープ (d) examples/*.c 全 7 本の
   frontend:"clang" render_html 成功、を cowl-api に追加。MCP / VSCode 側も
   frontend 経由の呼び出しをそれぞれのテスト形式で固定
