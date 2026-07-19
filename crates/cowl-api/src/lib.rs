//! # cowl-api — 安定API層
//!
//! CLI・MCPサーバ・VSCode拡張が**唯一**依存してよい層。
//! 提供するのは次の2形態で、中身は同じ:
//!
//! 1. **Rust関数API** … CLIのように同一プロセスでリンクする消費者向け
//! 2. **JSON API** … `dispatch_json(&str) -> String`。
//!    プロセス境界を越える消費者（VSCode拡張が spawn する `cowl serve --stdio`）はこちらを叩く。
//!    MCPサーバ cowl-mcp は同一プロセスで dispatch_json を直接呼ぶ
//!    （エンベロープ形状をCLIと一致させるため。ADR-0004）
//!
//! ## 契約
//! - リクエストは `{"cmd": "...", ...params}`（internally tagged）
//! - レスポンスは必ず `{"ok": true/false, "api_version": "...", ...}`
//! - **エラーでもJSONを返す**（panicや空文字で落ちない）。呼び出し側が
//!   プロセス管理だけに集中できるようにするため
//! - 破壊的変更をするときは `API_VERSION` を上げ、ADRを書く
//!
//! ## MCP化の見取り図（実装済み: crates/cowl-mcp、ADR-0004）
//! MCPツール `cowl_analyze` / `cowl_report_html` / `cowl_graph_dot` は
//! それぞれ Request::Analyze / RenderHtml / RenderDot に1対1で写像している。
//! 新しい機能はまずここに Request を足す → CLI/MCP/拡張が同時に使えるようになる。

use anyhow::Result;
use cowl_core::prelude::*;
use serde::{Deserialize, Serialize};
use serde_json::json;

/// JSON API の版。req/res の形が変わったら上げる
/// （0.2.0: Analyze/RenderHtml/RenderDot に frontend を追加。追加のみ＝マイナー。ADR-0008）
pub const API_VERSION: &str = "0.2.0";

// ---------------------------------------------------------------------------
// リクエスト / レスポンス型
// ---------------------------------------------------------------------------

/// 解析フロントエンドの選択肢。JSON 値は "ts" / "clang"。
///
/// **cowl-core ではなく API 層に置く**: フロントエンド選択は「入力をどう
/// facts にするか」という入力解決の概念＝リクエストの語彙であり、
/// core は facts しか知らないという依存 DAG を守るため（ADR-0008）。
/// 未知の値（例: "gcc"）は serde のパース失敗として err_json エンベロープに
/// 落ちる — panic しない契約はここでも維持される
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Frontend {
    /// tree-sitter L1（既定）。libclang 不要・編集中バッファ耐性（ADR-0002）
    Ts,
    /// libclang L2。マクロ展開・const ポインタ引数の精度向上。
    /// 実行環境に libclang 共有ライブラリが必要（無ければエラーエンベロープ。ADR-0007）
    Clang,
}

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
        /// 解析フロントエンド（省略時 = Ts。欠落を None にする serde の
        /// 挙動で旧クライアントの後方互換が自動的に成り立つ。ADR-0008）
        frontend: Option<Frontend>,
    },
    /// ライフタイム帯HTML（自己完結・単一ファイル）を返す/書き出す
    RenderHtml {
        path: Option<String>,
        source: Option<String>,
        file_name: Option<String>,
        /// 指定時はファイルに書き、レスポンスにはパスだけ載せる
        out: Option<String>,
        frontend: Option<Frontend>,
    },
    /// 所有権グラフの Graphviz DOT を返す/書き出す
    RenderDot {
        path: Option<String>,
        source: Option<String>,
        file_name: Option<String>,
        out: Option<String>,
        frontend: Option<Frontend>,
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

/// 入力（path/source）を facts に解決する共通処理。
/// フロントエンドの分岐は**ここ1箇所だけ**: L1/L2 は W5 で意図的に同形の
/// 公開API（extract_file / extract_source）を持たされており（ADR-0007）、
/// facts が差し替えの継ぎ目であることをこの関数の薄さが体現している
fn load(
    frontend: Option<Frontend>,
    path: &Option<String>,
    source: &Option<String>,
    file_name: &Option<String>,
) -> Result<Facts> {
    // 省略時 Ts: 後方互換＋「編集中バッファ耐性は L1 の担当」という
    // 役割分担（ADR-0002/0007）を既定値の形で維持する（ADR-0008）
    let fe = frontend.unwrap_or(Frontend::Ts);
    match (source, path) {
        (Some(src), _) => {
            let name = file_name.clone().unwrap_or_else(|| "<memory>".into());
            match fe {
                Frontend::Ts => cowl_front_ts::extract_source(src, &name),
                Frontend::Clang => cowl_front_clang::extract_source(src, &name),
            }
        }
        (None, Some(p)) => match fe {
            Frontend::Ts => cowl_front_ts::extract_file(p),
            Frontend::Clang => cowl_front_clang::extract_file(p),
        },
        (None, None) => anyhow::bail!("path か source のどちらかが必要です"),
    }
}

pub fn analyze(
    path: Option<String>,
    source: Option<String>,
    file_name: Option<String>,
    frontend: Option<Frontend>,
) -> Result<Analyzed> {
    let facts = load(frontend, &path, &source, &file_name)?;
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
            frontend,
        } => {
            let a = analyze(path, source, file_name, frontend)?;
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
            frontend,
        } => {
            let a = analyze(path, source, file_name, frontend)?;
            let html = render_html_string(&a);
            emit(html, out, "html")?
        }
        Request::RenderDot {
            path,
            source,
            file_name,
            out,
            frontend,
        } => {
            let a = analyze(path, source, file_name, frontend)?;
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

    // -----------------------------------------------------------------------
    // フロントエンド選択（ADR-0008）のゴールデン
    // -----------------------------------------------------------------------

    /// (a) L2 (libclang) を明示選択した analyze が通り、**本当に L2 へ配線
    /// されている**こと。fixture には L1/L2 で結果が分岐するマクロ展開
    /// （ADR-0007 の精度向上(a)）を使う: L1 はマクロを展開できず
    /// `AA` という未知関数の戻り値として assign_opaque に落ちるが、
    /// L2 は展開後の callee="malloc" を見て alloc になる。
    /// 単純な malloc/free では両フロントエンドの結果が同一になり、
    /// 誤って L1 に配線されていても検出できない（qa の変異実験で実証済み）
    /// ため、この分岐する fixture がイベント断定込みで配線を証明する
    #[test]
    fn dispatch_analyze_with_clang_frontend() {
        let req = serde_json::json!({
            "cmd": "analyze",
            "source": "#define AA(n) malloc(n)\nvoid f(void){ char *p = AA(4); free(p); }",
            "file_name": "mem.c",
            "frontend": "clang"
        })
        .to_string();
        let res: serde_json::Value = serde_json::from_str(&dispatch_json(&req)).unwrap();
        assert_eq!(res["ok"], true);
        // L2 の証拠: マクロ越しの獲得が alloc として観測される
        // （L1 に誤配線されていれば assign_opaque になりここで落ちる）
        assert_eq!(
            res["facts"]["functions"][0]["events"][0]["kind"]["type"],
            "alloc"
        );
        assert_eq!(res["report"]["metrics"]["ownership_coverage"], 1.0);
    }

    /// (b) frontend 省略と "ts" 明示は同一の facts を返すこと
    /// （省略時既定 = Ts の凍結。後方互換の証拠その1）
    #[test]
    fn omitted_frontend_equals_explicit_ts() {
        let base = serde_json::json!({
            "cmd": "analyze",
            "source": "void f(void){ char *p = malloc(4); free(p); }",
            "file_name": "mem.c",
        });
        let mut with_ts = base.clone();
        with_ts["frontend"] = serde_json::json!("ts");

        let res_omitted: serde_json::Value =
            serde_json::from_str(&dispatch_json(&base.to_string())).unwrap();
        let res_ts: serde_json::Value =
            serde_json::from_str(&dispatch_json(&with_ts.to_string())).unwrap();
        assert_eq!(res_omitted["ok"], true);
        // facts だけでなくレスポンス全体を比較する（report・エンベロープの
        // 形まで含めて「省略 = ts 明示」であることを強く固定）
        assert_eq!(res_omitted, res_ts);
    }

    /// (c) 未知のフロントエンド値は serde のパース失敗として ok:false の
    /// エンベロープに落ちること（panic しない契約の維持）
    #[test]
    fn unknown_frontend_value_is_err_envelope() {
        let req = serde_json::json!({
            "cmd": "analyze",
            "source": "void f(void){}",
            "frontend": "gcc"
        })
        .to_string();
        let res: serde_json::Value = serde_json::from_str(&dispatch_json(&req)).unwrap();
        assert_eq!(res["ok"], false);
        assert!(res["error"].is_string());
    }

    /// (d) examples/*.c 全7本が frontend:"clang" の render_html で ok:true。
    /// 「両フロントエンドで examples が通る」証拠の L2 側
    /// （L1 側は既存テスト＋make demo が担う）。
    /// これは疎通の確認であって配線先の判別ではない
    /// （判別は dispatch_analyze_with_clang_frontend が分岐 fixture で担う）
    #[test]
    fn all_examples_render_html_with_clang_frontend() {
        // cargo test 実行時の CWD 契約に依存しないよう CARGO_MANIFEST_DIR
        // から絶対パスを組み立てる（cowl-front-clang のテストと同じパターン）
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples");
        let mut files: Vec<std::path::PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("examples/ が読めない: {} ({e})", dir.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("c"))
            .collect();
        files.sort();
        assert_eq!(files.len(), 7, "examples/*.c の本数が想定と違う: {files:?}");

        for path in files {
            let req = serde_json::json!({
                "cmd": "render_html",
                "path": path.to_str().unwrap(),
                "frontend": "clang"
            })
            .to_string();
            let res: serde_json::Value = serde_json::from_str(&dispatch_json(&req)).unwrap();
            assert_eq!(res["ok"], true, "{}: ok:true でない: {res}", path.display());
            assert!(
                res["html"].is_string(),
                "{}: html キーが無い",
                path.display()
            );
        }
    }
}
