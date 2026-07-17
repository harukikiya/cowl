# =============================================================================
# cowl 開発イメージ
#
# ベース: devcontainers 公式 Rust イメージ（vscode ユーザ・rustup 済み）。
# multi-arch (amd64/arm64) なので platform 指定は不要（devcontainer.json 参照）
# =============================================================================
FROM mcr.microsoft.com/devcontainers/rust:1-bookworm

# 追加パッケージの意図:
#   clang / libclang-dev / llvm-dev … L2フロントエンド(cowl-front-clang, libclang版)
#                                     の開発を見越して先に入れておく（ROADMAP W5）
#   graphviz                        … `cowl graph` の DOT を `dot -Tsvg` で画像化するため
#   jq                              … JSON API の手動デバッグ用（serve --stdio と相性が良い）
RUN apt-get update && export DEBIAN_FRONTEND=noninteractive \
    && apt-get install -y --no-install-recommends \
       clang libclang-dev llvm-dev graphviz jq \
    && apt-get clean && rm -rf /var/lib/apt/lists/*

# clippy / rustfmt はワーカーの完了条件（make check）に必須なのでイメージに焼く
RUN rustup component add clippy rustfmt
