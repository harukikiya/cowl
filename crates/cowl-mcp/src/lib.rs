//! # cowl-mcp — MCPサーバ層
//!
//! rmcp 2.2 による stdio MCPサーバ実装。
//! MCP ツール引数 → `cowl_api::Request` 構築 → `dispatch_json` 経由で JSON処理 →
//! レスポンスを MCP 形式で返す。
//!
//! ツール3本: `cowl_analyze` / `cowl_report_html` / `cowl_graph_dot` は
//! `cowl_api::Request::{Analyze, RenderHtml, RenderDot}` に1対1で写像する。
//!
//! ## 設計
//! - エンベロープJSONは dispatch_json から返ってくるものを**再解釈しない**。
//!   そのまま text コンテンツとして返す（ロジック重複禁止）
//! - stdout は MCP プロトコルが占有。ログ・診断は stderr へ
//! - dispatch_json は panic しない契約なので、サーバプロセスも落ちない

use rmcp::handler::server::{router::tool::ToolRouter, wrapper::Parameters, ServerHandler};
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_router};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 引数構造体
// ---------------------------------------------------------------------------

/// cowl_analyze ツールの引数
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct AnalyzeArgs {
    /// 解析対象のCソースファイルパス。
    /// source が指定された場合、source が優先される
    pub path: Option<String>,

    /// Cソースコードの直接入力。
    /// path と source が両方指定された場合、source を使用する。
    /// （VSCode拡張などで「保存前の編集中バッファ」を投げるユースケースに対応）
    pub source: Option<String>,

    /// source 指定時の表示名。省略時は "<memory>"
    pub file_name: Option<String>,

    /// 解析フロントエンド。"ts" = tree-sitter L1（既定。libclang 不要）、
    /// "clang" = libclang L2（マクロ展開・constポインタ引数の精度向上。
    /// 実行環境に libclang 共有ライブラリが必要）。省略時は "ts"
    pub frontend: Option<String>,
}

/// cowl_report_html / cowl_graph_dot ツールの引数
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct RenderArgs {
    /// 解析対象のCソースファイルパス。
    /// source が指定された場合、source が優先される
    pub path: Option<String>,

    /// Cソースコードの直接入力。
    /// path と source が両方指定された場合、source を使用する。
    /// （VSCode拡張などで「保存前の編集中バッファ」を投げるユースケースに対応）
    pub source: Option<String>,

    /// source 指定時の表示名。省略時は "<memory>"
    pub file_name: Option<String>,

    /// 出力ファイルパス。指定時はファイルに書き、
    /// レスポンスには「written: path」のみ載る（大きなHTMLをパイプに流さないため）。
    /// 未指定なら本文をそのまま返す
    pub out: Option<String>,

    /// 解析フロントエンド。"ts" = tree-sitter L1（既定。libclang 不要）、
    /// "clang" = libclang L2（マクロ展開・constポインタ引数の精度向上。
    /// 実行環境に libclang 共有ライブラリが必要）。省略時は "ts"
    pub frontend: Option<String>,
}

// ---------------------------------------------------------------------------
// MCP サーバ本体
// ---------------------------------------------------------------------------

/// MCPサーバ。tool_router マクロが ToolRouter を管理し、
/// ServerHandler impl で tool_handler を使う
#[derive(Clone)]
pub struct CowlServer {
    tool_router: ToolRouter<CowlServer>,
}

impl Default for CowlServer {
    fn default() -> Self {
        Self::new()
    }
}

// ServerHandler 実装は tool_handler マクロが自動生成
#[rmcp::tool_handler(router = self.tool_router)]
impl ServerHandler for CowlServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "cowl MCPサーバ. 3つのツールで Cソースの所有権・ライフタイム解析を提供: \
                cowl_analyze (facts + report), cowl_report_html (lifetime-band HTML), \
                cowl_graph_dot (ownership graph in DOT format). \
                各ツールは path または source のいずれかでソース指定（source 優先）。\
                frontend で解析フロントエンドを選択可能（\"ts\"=tree-sitter L1（既定）/ \
                \"clang\"=libclang L2）"
                    .to_string(),
            )
    }
}

// ---------------------------------------------------------------------------
// ツール実装（tool_router マクロが ToolRouter を生成）
// ---------------------------------------------------------------------------

/// MCP 引数の frontend 文字列を型付き `cowl_api::Frontend` へ写像する。
///
/// - なぜ引数構造体で `cowl_api::Frontend` を直接受けないか: rmcp の
///   Parameters は引数構造体に schemars::JsonSchema を要求するため、
///   直接使うには cowl-api 側へ derive を足すしかない。MCP という特定シェル
///   の都合のスキーマ生成依存を API 層に漏らさないことを優先した（ADR-0008。
///   CLI が clap の ValueEnum を API 層に求めないのと同じ理屈）
/// - なぜ手書きの match 表でなく serde_json::from_value か: 有効値の集合
///   （"ts"/"clang"）の定義を cowl-api の serde 属性の1箇所に留めるため。
///   ここに対応表を書くと、将来フロントエンドが増えたとき同期漏れでドリフトする
/// - 不正値は Err(String) → rmcp がツールレベルエラー（is_error:true）に
///   変換する。引数の型検証はもともと MCP 層（rmcp のスキーマ検証）の責務
///   なので、エンベロープの再解釈には当たらない
fn parse_frontend(s: Option<String>) -> Result<Option<cowl_api::Frontend>, String> {
    match s {
        None => Ok(None),
        Some(s) => serde_json::from_value(serde_json::Value::String(s.clone()))
            .map(Some)
            .map_err(|_| {
                format!("frontend は \"ts\" か \"clang\" を指定してください（受領値: {s:?}）")
            }),
    }
}

/// dispatch の返り値を MCP Result に変換する共通処理。
/// dispatch_json から返ってくるエンベロープJSONをそのまま text コンテンツとして返す
/// （ロジック再実装禁止。CLI/serve --stdio と同一挙動を保証する ADR-0004 の実装）
///
/// Err(String) を返すと rmcp の `IntoCallToolResult for Result<T, E>` 実装が
/// is_error:true のツールレベルエラー（プロトコルエラーではない）に変換する。
/// エンベロープの ok:false を Err に写すのはこの機構に乗るため。
fn dispatch(req: &cowl_api::Request) -> Result<String, String> {
    // Request を JSON に変換。コンパイル時に構造チェック（手組みJSONより安全）
    let json =
        serde_json::to_string(req).map_err(|e| format!("Request シリアライズ失敗: {}", e))?;

    // dispatch_json は panic しない契約。返ってきたエンベロープJSONをそのまま使う。
    // stdout を汚さないため、この関数は文字列として返して、
    // tool メソッドが text content に包む責任を持つ
    let res_json = cowl_api::dispatch_json(&json);

    // ok フィールドで成功/失敗を判定（dispatch_json の契約）
    match serde_json::from_str::<serde_json::Value>(&res_json) {
        Ok(obj) => {
            if obj["ok"] == true {
                // 成功: エンベロープJSONをそのまま返す
                Ok(res_json)
            } else {
                // 失敗: エラーエンベロープを エラーメッセージとして返す
                Err(res_json)
            }
        }
        Err(e) => {
            // dispatch_json の出力がJSONでない（想定外）
            Err(format!("dispatch_json の返値を parse 失敗: {}", e))
        }
    }
}

/// ツール実装。tool_router マクロが ToolRouter を生成し、
/// 各 #[tool] メソッドを登録する
#[tool_router(router = tool_router)]
impl CowlServer {
    pub fn new() -> Self {
        Self {
            tool_router: Self::tool_router(),
        }
    }

    /// 所有権解析を実行する。
    /// facts と report を JSON で返す。
    /// path または source でソース指定（source が優先）。
    /// frontend で解析フロントエンドを選択（"ts"=tree-sitter L1（既定）/
    /// "clang"=libclang L2。要 libclang）
    #[tool]
    pub async fn cowl_analyze(
        &self,
        Parameters(args): Parameters<AnalyzeArgs>,
    ) -> Result<String, String> {
        let req = cowl_api::Request::Analyze {
            path: args.path,
            source: args.source,
            file_name: args.file_name,
            frontend: parse_frontend(args.frontend)?,
        };
        dispatch(&req)
    }

    /// ライフタイム帯の自己完結HTMLレポートを生成する。
    /// out 指定時はファイルに書き、レスポンスにはパスだけ載る。
    /// 未指定なら本文をそのまま返す。
    /// path または source でソース指定（source が優先）。
    /// frontend で解析フロントエンドを選択（"ts"=tree-sitter L1（既定）/
    /// "clang"=libclang L2。要 libclang）
    #[tool]
    pub async fn cowl_report_html(
        &self,
        Parameters(args): Parameters<RenderArgs>,
    ) -> Result<String, String> {
        let req = cowl_api::Request::RenderHtml {
            path: args.path,
            source: args.source,
            file_name: args.file_name,
            out: args.out,
            frontend: parse_frontend(args.frontend)?,
        };
        dispatch(&req)
    }

    /// 所有権グラフの Graphviz DOT を生成する。
    /// out 指定時はファイルに書き、レスポンスにはパスだけ載る。
    /// 未指定なら本文をそのまま返す。
    /// path または source でソース指定（source が優先）。
    /// frontend で解析フロントエンドを選択（"ts"=tree-sitter L1（既定）/
    /// "clang"=libclang L2。要 libclang）
    #[tool]
    pub async fn cowl_graph_dot(
        &self,
        Parameters(args): Parameters<RenderArgs>,
    ) -> Result<String, String> {
        let req = cowl_api::Request::RenderDot {
            path: args.path,
            source: args.source,
            file_name: args.file_name,
            out: args.out,
            frontend: parse_frontend(args.frontend)?,
        };
        dispatch(&req)
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dispatch_analyze_success() {
        // 成功系写像: source 付きで dispatch が ok:true を返すことを確認
        let req = cowl_api::Request::Analyze {
            path: None,
            source: Some("void f(void){ char *p = malloc(4); free(p); }".to_string()),
            file_name: Some("mem.c".to_string()),
            // W7 のフィールド追加に伴う機械的追記（None = 従来挙動。
            // このテストの検証内容・アサーションは W7 前から無変更）
            frontend: None,
        };

        let result = dispatch(&req);
        assert!(result.is_ok(), "dispatch should succeed for valid C source");

        let json_str = result.unwrap();
        let json: serde_json::Value =
            serde_json::from_str(&json_str).expect("dispatch result should be valid JSON");
        assert_eq!(json["ok"], true, "dispatch result should have ok:true");
    }

    #[test]
    fn test_dispatch_analyze_error() {
        // エラー系写像: 存在しないパスで dispatch が ok:false を返すことを確認
        // （サーバが panic しないことの固定）
        let req = cowl_api::Request::Analyze {
            path: Some("/nonexistent/file.c".to_string()),
            source: None,
            file_name: None,
            // W7 のフィールド追加に伴う機械的追記（None = 従来挙動。
            // このテストの検証内容・アサーションは W7 前から無変更）
            frontend: None,
        };

        let result = dispatch(&req);
        assert!(result.is_err(), "dispatch should fail for nonexistent file");

        // エラーのメッセージを JSON パース。ok:false を確認
        if let Err(err_msg) = result {
            let json: serde_json::Value =
                serde_json::from_str(&err_msg).expect("error message should be valid JSON");
            assert_eq!(json["ok"], false, "error response should have ok:false");
        }
    }

    #[test]
    fn test_tool_registration() {
        // ツール登録: tool_router に3本のツール名が揃っていることを確認
        let server = CowlServer::new();
        let tools = server.tool_router.list_all();

        let has_analyze = tools.iter().any(|t| t.name == "cowl_analyze");
        let has_html = tools.iter().any(|t| t.name == "cowl_report_html");
        let has_dot = tools.iter().any(|t| t.name == "cowl_graph_dot");

        assert!(has_analyze, "should have cowl_analyze tool");
        assert!(has_html, "should have cowl_report_html tool");
        assert!(has_dot, "should have cowl_graph_dot tool");
    }

    #[tokio::test]
    async fn test_tools_over_mcp_protocol() -> anyhow::Result<()> {
        // インプロセス統合テスト。MCP プロトコル経由で tools/list と tools/call を確認
        use rmcp::model::CallToolRequestParams;
        use rmcp::ServiceExt;

        let (server_io, client_io) = tokio::io::duplex(1 << 16);

        // サーバ側: duplex の片端で serve。waiting はクライアント切断まで待つ
        let server_task = tokio::spawn(async move {
            let service = CowlServer::new().serve(server_io).await?;
            service.waiting().await?;
            anyhow::Ok(())
        });

        // クライアント側: () には ClientHandler のデフォルト実装がある。
        // serve() が initialize handshake を完了させてから返る（自前同期は不要）
        let client = ().serve(client_io).await?;

        // tools/list に3本揃っていること
        let tools = client.list_all_tools().await?;
        let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
        assert!(names.contains(&"cowl_analyze"));
        assert!(names.contains(&"cowl_report_html"));
        assert!(names.contains(&"cowl_graph_dot"));

        let source_code = "void f(void){ char *p = malloc(4); free(p); }";
        let file_name = "mem.c";

        // cowl_analyze の call_tool: ok:true を確認
        let mut call_req = CallToolRequestParams::new("cowl_analyze");
        call_req.arguments = Some(rmcp::object!({
            "source": source_code,
            "file_name": file_name
        }));
        let res = client.call_tool(call_req).await?;
        assert_ne!(res.is_error, Some(true));
        let text = &res.content[0].as_text().expect("text content").text;
        let json: serde_json::Value = serde_json::from_str(text)?;
        assert_eq!(json["ok"], true);

        // cowl_report_html の call_tool: ok:true + "html" キーを確認
        let mut call_req = CallToolRequestParams::new("cowl_report_html");
        call_req.arguments = Some(rmcp::object!({
            "source": source_code,
            "file_name": file_name
        }));
        let res = client.call_tool(call_req).await?;
        assert_ne!(res.is_error, Some(true));
        let text = &res.content[0].as_text().expect("text content").text;
        let json: serde_json::Value = serde_json::from_str(text)?;
        assert_eq!(json["ok"], true);
        assert!(json["html"].is_string(), "should have html key in response");

        // cowl_graph_dot の call_tool: ok:true + "dot" キーを確認
        let mut call_req = CallToolRequestParams::new("cowl_graph_dot");
        call_req.arguments = Some(rmcp::object!({
            "source": source_code,
            "file_name": file_name
        }));
        let res = client.call_tool(call_req).await?;
        assert_ne!(res.is_error, Some(true));
        let text = &res.content[0].as_text().expect("text content").text;
        let json: serde_json::Value = serde_json::from_str(text)?;
        assert_eq!(json["ok"], true);
        assert!(json["dot"].is_string(), "should have dot key in response");

        // 後始末: クライアントを閉じ、サーバタスクを落とす
        client.cancel().await?;
        server_task.abort();
        Ok(())
    }

    #[tokio::test]
    async fn test_tool_call_with_frontend() -> anyhow::Result<()> {
        // frontend 付きツール呼び出し（W7）。MCP プロトコル経由で
        // 「JSON引数 → schemars/serde → parse_frontend → Request」の
        // 写像経路全体を通す（メソッド直呼びでは引数のデシリアライズを
        // 通らないため、あえてプロトコル越しにする）
        use rmcp::model::CallToolRequestParams;
        use rmcp::ServiceExt;

        let (server_io, client_io) = tokio::io::duplex(1 << 16);
        let server_task = tokio::spawn(async move {
            let service = CowlServer::new().serve(server_io).await?;
            service.waiting().await?;
            anyhow::Ok(())
        });
        let client = ().serve(client_io).await?;

        // frontend:"clang" の cowl_analyze が ok:true になり、かつ MCP 経路でも
        // 本当に L2 へ配線されていること。fixture は L1/L2 で結果が分岐する
        // マクロ展開（L1 なら assign_opaque / L2 なら alloc。cowl-api 側の
        // dispatch_analyze_with_clang_frontend と同一の判別手法）。
        // 単純な malloc/free では両フロントエンドの結果が同一になり
        // 配線ミスを検出できないため、分岐する fixture で断定する
        let mut call_req = CallToolRequestParams::new("cowl_analyze");
        call_req.arguments = Some(rmcp::object!({
            "source": "#define AA(n) malloc(n)\nvoid f(void){ char *p = AA(4); free(p); }",
            "file_name": "mem.c",
            "frontend": "clang"
        }));
        let res = client.call_tool(call_req).await?;
        assert_ne!(res.is_error, Some(true));
        let text = &res.content[0].as_text().expect("text content").text;
        let json: serde_json::Value = serde_json::from_str(text)?;
        assert_eq!(json["ok"], true);
        // L2 の証拠: マクロ越しの獲得が alloc（L1 に誤配線なら assign_opaque）
        assert_eq!(
            json["facts"]["functions"][0]["events"][0]["kind"]["type"],
            "alloc"
        );

        // 不正値 "gcc" はツールレベルエラー（is_error:true）になり、
        // かつサーバは落ちず次の呼び出しを処理し続けること
        let mut call_req = CallToolRequestParams::new("cowl_analyze");
        call_req.arguments = Some(rmcp::object!({
            "source": "void f(void){}",
            "frontend": "gcc"
        }));
        let res = client.call_tool(call_req).await?;
        assert_eq!(res.is_error, Some(true), "不正な frontend はツールエラー");

        // 生存確認: 直後の frontend 省略呼び出し（従来挙動）が通る
        let mut call_req = CallToolRequestParams::new("cowl_analyze");
        call_req.arguments = Some(rmcp::object!({
            "source": "void f(void){ char *p = malloc(4); free(p); }"
        }));
        let res = client.call_tool(call_req).await?;
        assert_ne!(res.is_error, Some(true));

        client.cancel().await?;
        server_task.abort();
        Ok(())
    }
}
