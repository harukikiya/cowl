#!/usr/bin/env bash
# =============================================================================
# コンテナ作成後の初期化。devcontainer.json の postCreateCommand から呼ばれる。
# 冪等に書くこと（Rebuild Container のたびに走る）
# =============================================================================
set -uo pipefail

echo "== [1/4] Claude Code 認証情報の健全性チェック =="
# 既知の踏み抜き: ホストに ~/.claude.json（ファイル）が無い状態で bind mount すると
# Docker がディレクトリを作ってしまい、Claude Code が認証情報を読めなくなる。
# initializeCommand で touch しているので通常は起きないが、検知だけは残す
if [ -d "$HOME/.claude.json" ]; then
  echo "!! 警告: ~/.claude.json がディレクトリになっています"
  echo "   ホスト側で以下を実行してからコンテナを Rebuild してください:"
  echo "     rm -rf ~/.claude.json && touch ~/.claude.json"
fi

# root 所有で作られてしまった場合の救済（認証ループの直接原因）。
# bind mount の中身を chown するのはホスト側にも波及するが、
# どのみち自分の $HOME 配下なので実害はない
sudo chown -R "$(id -u):$(id -g)" "$HOME/.claude" 2>/dev/null || true
sudo chown "$(id -u):$(id -g)" "$HOME/.claude.json" 2>/dev/null || true

echo "== [2/4] Claude Code のインストール =="
if ! command -v claude >/dev/null 2>&1; then
  npm install -g @anthropic-ai/claude-code
else
  echo "   claude は導入済み: $(claude --version 2>/dev/null || echo unknown)"
fi

echo "== [3/4] Rust 依存の事前取得 =="
cargo fetch || true

echo "== [4/4] 動作確認ビルド =="
cargo build --workspace -q && echo "   OK: cargo build --workspace"

echo ""
echo "セットアップ完了。次の一歩:"
echo "  make demo      # examples/ からレポート一式を out/ に生成"
echo "  claude         # オーケストレーション開始（初回は CLAUDE.md を読む）"
