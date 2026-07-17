//! # cowl-mcp サーバエントリーポイント
//!
//! stdio 上でMCPサーバを起動。標準入力で MCP リクエストを受け取り、
//! 標準出力で MCP レスポンスを返す。
//!
//! stdin を閉じると自動的に終了する（MCPクライアントが子プロセス管理できる形）

use anyhow::Result;
use rmcp::ServiceExt;

#[tokio::main]
async fn main() -> Result<()> {
    // CowlServer を生成してstdio transport に接続
    // （stdout は MCP プロトコルが占有。ログ・診断が必要なら stderr へ）
    let server = cowl_mcp::CowlServer::new();

    // serve() は、stdin/stdout で MCP プロトコルの会話を始める
    // ServiceExt トレイト経由で waiting() を呼ぶまで動く
    let service = server.serve(rmcp::transport::stdio()).await?;

    // waiting() は stdin が閉じられるまでブロック
    service.waiting().await?;

    Ok(())
}
