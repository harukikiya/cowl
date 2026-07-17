//! # cowl-api — 安定API層
//!
//! CLI・MCPサーバ・VSCode拡張が**唯一**依存してよい層。
//! 提供するのは次の2形態で、中身は同じ:
//!
//! 1. **Rust関数API** … CLIのように同一プロセスでリンクする消費者向け
//! 2. **JSON API** … `dispatch_json(&str) -> String`。
//!    プロセス境界を越える消費者（MCPサーバ、VSCode拡張が spawn する
//!    `cowl serve --stdio`）はこちらを叩く
//!
//! ## 契約
//! - リクエストは `{"cmd": "...", ...params}`（internally tagged）
//! - レスポンスは必ず `{"ok": true/false, "api_version": "...", ...}`
//! - **エラーでもJSONを返す**（panicや空文字で落ちない）。呼び出し側が
//!   プロセス管理だけに集中できるようにするため
//! - 破壊的変更をするときは `API_VERSION` を上げ、ADRを書く
//!
//! ## MCP化の見取り図（将来ワーカーへの指示）
//! MCPツール `cowl_analyze` / `cowl_report_html` / `cowl_graph_dot` は
//! それぞれ Request::Analyze / RenderHtml / RenderDot に1対1で写像すればよい。
//! 新しい機能はまずここに Request を足す → CLI/MCP/拡張が同時に使えるようになる。

use anyhow::Result;
use cowl_core::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// JSON API の版。req/res の形が変わったら上げる
pub const API_VERSION: &str = "0.1.0";

// ---------------------------------------------------------------------------
// リクエスト / レスポンス型
// ---------------------------------------------------------------------------

/// 受け付けるコマンド一覧。
/// `path` と `source` は排他ではなく **source 優先**（両方来たら source を使う）。
/// source を受けられるようにしてあるのは、VSCode拡張が「保存前の
/// 編集中バッファ」をそのまま投げられるようにするため
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// API・スキーマのバージョン情報
    Version,
    /// 解析して facts + report を返す
    Analyze {
        path: Option<String>,
        source: Option<String>,
        /// source 指定時の表示名（省略時 "<memory>"）
        file_name: Option<String>,
    },
    /// ライフタイム帯HTML（自己完結・単一ファイル）を返す/書き出す
    RenderHtml {
        path: Option<String>,
        source: Option<String>,
        file_name: Option<String>,
        /// 指定時はファイルに書き、レスポンスにはパスだけ載せる
        out: Option<String>,
    },
    /// 所有権グラフの Graphviz DOT を返す/書き出す
    RenderDot {
        path: Option<String>,
        source: Option<String>,
        file_name: Option<String>,
        out: Option<String>,
    },
}

/// 型付きで使いたいRust消費者向けの結果。
/// JSON消費者は dispatch_json の返すエンベロープを直接読む
pub struct Analyzed {
    pub facts: Facts,
    pub report: Report,
}

// ---------------------------------------------------------------------------
// Rust関数API
// ---------------------------------------------------------------------------

/// 入力（path/source）を facts に解決する共通処理
fn load(
    path: &Option<String>,
    source: &Option<String>,
    file_name: &Option<String>,
) -> Result<Facts> {
    match (source, path) {
        (Some(src), _) => {
            let name = file_name.clone().unwrap_or_else(|| "<memory>".into());
            cowl_front_ts::extract_source(src, &name)
        }
        (None, Some(p)) => cowl_front_ts::extract_file(p),
        (None, None) => anyhow::bail!("path か source のどちらかが必要です"),
    }
}

pub fn analyze(
    path: Option<String>,
    source: Option<String>,
    file_name: Option<String>,
) -> Result<Analyzed> {
    let facts = load(&path, &source, &file_name)?;
    let report = cowl_core::analysis::analyze(&facts);
    Ok(Analyzed { facts, report })
}

pub fn render_html_string(a: &Analyzed) -> String {
    render_html(&a.facts, &a.report)
}

pub fn render_dot_string(a: &Analyzed) -> String {
    render_dot(&a.report)
}

// ---------------------------------------------------------------------------
// JSON API
// ---------------------------------------------------------------------------

/// JSON文字列を受けてJSON文字列を返す。**決してpanicしない**ことが契約。
/// stdio サーバ（cowl serve）はこの関数を1行=1リクエストで回すだけ
pub fn dispatch_json(input: &str) -> String {
    let req: Request = match serde_json::from_str(input) {
        Ok(r) => r,
        Err(e) => return err_json(&format!("リクエストのJSONを解釈できません: {e}")),
    };
    match handle(req) {
        Ok(v) => v.to_string(),
        Err(e) => err_json(&format!("{e:#}")),
    }
}

fn handle(req: Request) -> Result<serde_json::Value> {
    Ok(match req {
        Request::Version => json!({
            "ok": true,
            "api_version": API_VERSION,
            "facts_schema": FACTS_SCHEMA_VERSION,
            "report_schema": REPORT_SCHEMA_VERSION,
        }),
        Request::Analyze {
            path,
            source,
            file_name,
        } => {
            let a = analyze(path, source, file_name)?;
            json!({
                "ok": true,
                "api_version": API_VERSION,
                "facts": a.facts,
                "report": a.report,
            })
        }
        Request::RenderHtml {
            path,
            source,
            file_name,
            out,
        } => {
            let a = analyze(path, source, file_name)?;
            let html = render_html_string(&a);
            emit(html, out, "html")?
        }
        Request::RenderDot {
            path,
            source,
            file_name,
            out,
        } => {
            let a = analyze(path, source, file_name)?;
            let dot = render_dot_string(&a);
            emit(dot, out, "dot")?
        }
    })
}

/// out 指定時はファイル書き出し（本文はレスポンスに載せない：
/// 大きなHTMLをパイプに流さないための配慮）。未指定なら本文を返す
fn emit(body: String, out: Option<String>, key: &str) -> Result<serde_json::Value> {
    match out {
        Some(p) => {
            std::fs::write(&p, body)?;
            Ok(json!({ "ok": true, "api_version": API_VERSION, "written": p }))
        }
        None => Ok(json!({ "ok": true, "api_version": API_VERSION, key: body })),
    }
}

fn err_json(msg: &str) -> String {
    json!({ "ok": false, "api_version": API_VERSION, "error": msg }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatch_analyze_from_source() {
        let req = serde_json::json!({
            "cmd": "analyze",
            "source": "void f(void){ char *p = malloc(4); free(p); }",
            "file_name": "mem.c"
        })
        .to_string();
        let res: serde_json::Value = serde_json::from_str(&dispatch_json(&req)).unwrap();
        assert_eq!(res["ok"], true);
        // レポートの中身が本当に通っているかを1点だけ突く
        assert_eq!(res["report"]["metrics"]["ownership_coverage"], 1.0);
    }

    #[test]
    fn dispatch_never_panics_on_garbage() {
        let res: serde_json::Value = serde_json::from_str(&dispatch_json("not json")).unwrap();
        assert_eq!(res["ok"], false);
    }

    #[test]
    fn version_reports_schemas() {
        let res: serde_json::Value =
            serde_json::from_str(&dispatch_json(r#"{"cmd":"version"}"#)).unwrap();
        assert_eq!(res["api_version"], API_VERSION);
    }
}
