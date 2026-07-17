# ADR-0005: VS Code 拡張の骨格（editors/vscode）

日付: 2026-07-17 / 状態: 採択

## 決定
`editors/vscode/` に TypeScript 製の最小 VS Code 拡張を新設する（ROADMAP W2）。

- 通信: 拡張が `cowl serve --stdio` を child_process で spawn し、
  1行1JSON で往復する。cowl-api の JSON 契約のみに依存する。
  cowl-mcp（ADR-0004）は同一プロセスリンクを選んだが、拡張は Rust と
  リンクできないため spawn が正当。cowl-api の doc にある
  「プロセス境界を越える消費者」とはまさにこれ
- コマンド `cowl.showReport`: アクティブな C エディタの**編集中バッファ**を
  `source` として `render_html` に送り、返った自己完結 HTML を Webview に
  表示する。保存不要。変更は onDidChangeTextDocument を300msデバウンスで追従
- Webview は `enableScripts: true`。cowl の HTML はホバー強調の inline
  script を含む（render_html.rs）。外部リソースはゼロなので露出は最小
- バイナリのパスは設定 `cowl.serverPath` で上書き可能
  （既定 `cowl`。開発時は `target/debug/cowl` を指す）
- stdio クライアント（spawn + 1行1JSON + FIFO対応付け）は **vscode API
  非依存のモジュール**（src/cowlClient.ts）に分離する。node:test から
  実バイナリを相手に回すことで、「source を送るパスが通っている」という
  W2 受け入れ条件を自動テストで固定するため
- E2E（VS Code 本体で Webview 表示）はコンテナに Xサーバが無いため
  自動化しない。Extension Development Host での手動確認手順を
  editors/vscode/README.md に記す
- `make check` は Rust ワーカーの完了条件のまま変えない。拡張の検証は
  独立ターゲット `make check-vscode` とする（Rust だけを触るワーカーに
  npm を要求しないため）

## 依存追加（devDependencies のみ。実行時 npm 依存ゼロ）
| 依存 | 用途 |
|---|---|
| typescript | tsc でのコンパイル（バンドラ不使用） |
| @types/vscode | 拡張APIの型。engines.vscode と版を揃える |
| @types/node | child_process / readline の型 |

実行時依存がゼロなのでバンドラ（esbuild 等）は導入しない。
tsc の出力（out/）をそのまま `main` に使う。

## 却下案
- **LSP（vscode-languageclient）**: 診断モデルへの写像が W2 時点では
  過剰。cowl の JSON 契約で足りる。LSP 化は必要になった時に別ADRで
- **cowl-api への直接リンク（napi 等のネイティブブリッジ）**:
  ビルドが激重になる。JSON 契約はプロセス境界を越えるために作った
- **E2E テスト自動化（@vscode/test-electron + xvfb）**: コンテナに
  X が無く導入コストが骨格の価値を超える。stdio 層の実バイナリテストと
  手動手順で受け入れ条件を満たす
- **保存時のみ更新**: W2 受け入れ条件（保存不要で更新）に反する。
  source フィールドを cowl-api に用意したのはこのためだった
