# cowl VS Code 拡張（W2 骨格）

アクティブな C ファイルの**編集中バッファ**（保存不要）を
`cowl serve --stdio` に送り、ライフタイム帯レポート HTML を Webview に表示する。
設計判断は `docs/adr/0005-vscode-extension-skeleton.md` に固定してある。

## 前提

- `cargo build -p cowl-cli` 済み（リポジトリの `target/debug/cowl` ができる）、
  または PATH の通った場所に `cowl` があること
- Node.js（開発・テスト用。拡張の実行時 npm 依存はゼロ）

## 開発起動（F5）

1. **このフォルダ（`editors/vscode/`）を VS Code で開く**
2. `npm install`
3. F5（Run cowl Extension）→ Extension Development Host が立ち上がる

## 設定: `cowl.serverPath`

既定は `"cowl"`（PATH 解決）。PATH に無い場合は実行ファイルのパスを指定する。

```jsonc
// Extension Development Host 側の settings.json
{
  // 注意: VS Code の設定値では ${workspaceFolder} などの変数は展開されない。
  // 必ず実際の絶対パスを書くこと
  "cowl.serverPath": "/workspaces/cowl/target/debug/cowl"
}
```

注意: `cowl.serverPath` の変更は起動済みの接続には反映されない。
次の再接続（サーバ異常終了後の再 spawn、または VS Code 再起動）から有効になる。

## 手動確認手順（W2 受け入れ条件 1）

1. Extension Development Host で `examples/demo.c` を開く
2. コマンドパレット → `cowl: Show Ownership Report`
3. ライフタイム帯レポートが横（Beside）の Webview に出る
4. バッファを編集する（**保存しない**。例: `free(p);` の行を消す）
5. 約 300ms 後にレポートが自動更新される（デバウンス追従）

バイナリが見つからない場合は「cowl.serverPath を設定せよ」という
エラーメッセージが出るので、上の設定例に従うこと。

## テスト

```bash
npm test          # tsc（strict）+ node:test。実バイナリ相手の統合テスト
# バイナリ位置の上書き:
COWL_BIN=/path/to/cowl npm test
```

リポジトリルートからは `make check-vscode`（`cargo build -p cowl-cli` →
`npm ci && npm test`）が同じことをする。

VS Code 本体を起動する E2E はコンテナに X サーバが無いため自動化していない
（ADR-0005）。stdio 層の統合テストと上記の手動確認手順で受け入れ条件を満たす。
