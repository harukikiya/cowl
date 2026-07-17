# ADR-0003: devcontainer での Claude Code 認証情報の扱い

日付: 2026-07-17 / 状態: 採択

## 背景
kanicc で、コンテナ内 ~/.claude が root 所有になり Claude Code が
毎回ログインを要求する事象を踏んだ。原因は
(a) 存在しないファイルの bind mount で Docker がroot所有ディレクトリを作る
(b) named volume の初期所有権が root になる、の複合。

## 決定
ホストの ~/.claude と ~/.claude.json を bind mount で共有する。
- initializeCommand でホスト側に mkdir/touch（(a) の根絶）
- updateRemoteUserUID + post-create の chown（(b) 系の救済）

## 理由（named volume 案との比較）
- bind mount はホストのログイン状態をそのまま使えるため、
  コンテナ内での `claude login` が一切不要になる（要件そのもの）
- 欠点はコンテナがホストの設定を書き換えうること。単一利用者の
  開発機では許容し、共有マシンでは named volume 方式に切り替える
  （その場合も post-create の chown は必須）
