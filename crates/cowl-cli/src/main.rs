//! # cowl-cli — コマンドラインシェル
//!
//! ここは徹底して**薄く**保つ。ロジックは書かない。
//! やることは「引数を cowl-api の Request に写して結果を出す」だけ。
//! MCPサーバやVSCode拡張を作るときも同じ写像を書くだけになるのが狙い。
//!
//! ## サブコマンド
//! - `cowl analyze <file.c>`         … facts+report を JSON で標準出力へ
//! - `cowl report  <file.c> -o out`  … ライフタイム帯HTMLを生成
//! - `cowl graph   <file.c> -o out`  … 所有権グラフDOTを生成
//! - `cowl serve --stdio`            … 1行1リクエストのJSONサーバ（MCP/拡張が spawn する）
//! - `cowl version`

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::io::{BufRead, Write};

#[derive(Parser)]
#[command(
    name = "cowl",
    about = "C OWnership & Lifetime visualizer — Cの所有権・ライフタイム可視化",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// 解析結果(facts+report)をJSONで出力する
    Analyze {
        /// 対象のCソースファイル
        file: String,
    },
    /// ライフタイム帯の自己完結HTMLレポートを生成する
    Report {
        file: String,
        /// 出力先HTML（省略時は標準出力）
        #[arg(short, long)]
        out: Option<String>,
    },
    /// 所有権グラフのGraphviz DOTを生成する
    Graph {
        file: String,
        /// 出力先DOT（省略時は標準出力）
        #[arg(short, long)]
        out: Option<String>,
    },
    /// JSONリクエストサーバ（改行区切り、1行=1リクエスト）
    Serve {
        /// 標準入出力モード（現状これのみ。将来 --port 等を足す余地）
        #[arg(long)]
        stdio: bool,
    },
    /// APIとスキーマのバージョンを表示する
    Version,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Analyze { file } => {
            let req = serde_json::json!({ "cmd": "analyze", "path": file }).to_string();
            print_stdout(&cowl_api::dispatch_json(&req));
        }
        Cmd::Report { file, out } => run_render("render_html", &file, out)?,
        Cmd::Graph { file, out } => run_render("render_dot", &file, out)?,
        Cmd::Serve { stdio: _ } => serve_stdio()?,
        Cmd::Version => {
            print_stdout(&cowl_api::dispatch_json(r#"{"cmd":"version"}"#));
        }
    }
    Ok(())
}

/// 標準出力への書き出し。`cowl analyze x | head` のようにパイプ先が
/// 先に閉じると write が BrokenPipe になるが、これはCLIとして正常系
/// なので**正常終了**扱いにする（println! だと panic してしまう）。
/// jq / head / grep と組み合わせて使う前提の道具としての作法
fn print_stdout(s: &str) {
    use std::io::Write;
    let mut out = std::io::stdout().lock();
    match out
        .write_all(s.as_bytes())
        .and_then(|_| out.write_all(b"\n"))
    {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => {
            eprintln!("stdout への書き込みに失敗: {e}");
            std::process::exit(1);
        }
    }
}

/// report/graph 共通の実行部。out 指定の有無で「ファイルに書く/そのまま出す」を
/// API層の契約（emit）に合わせて切り替える
fn run_render(cmd: &str, file: &str, out: Option<String>) -> Result<()> {
    let req = serde_json::json!({ "cmd": cmd, "path": file, "out": out }).to_string();
    let res_s = cowl_api::dispatch_json(&req);
    let res: serde_json::Value = serde_json::from_str(&res_s)?;
    if res["ok"] != true {
        // エラーはstderrへ。パイプ先（jq等）を汚さない
        eprintln!("{}", res_s);
        std::process::exit(1);
    }
    if let Some(p) = res["written"].as_str() {
        eprintln!("書き出しました: {p}");
    } else {
        // 本文キーは html か dot のどちらか。素の本文だけを標準出力へ
        for key in ["html", "dot"] {
            if let Some(body) = res[key].as_str() {
                print_stdout(body);
                return Ok(());
            }
        }
        print_stdout(&res_s); // 想定外の形はそのまま透過（デバッグ可能性優先）
    }
    Ok(())
}

/// stdioサーバ本体。プロトコルは意図的に最も素朴な「1行1JSON」。
/// - MCPサーバはこのプロセスを spawn してツール呼び出しを1行ずつ流す
/// - VSCode拡張も child_process で同じことをする
///
/// dispatch_json は決してpanicしない契約なので、このループも落ちない
fn serve_stdio() -> Result<()> {
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue; // 空行は無視（手打ちデバッグのしやすさのため)
        }
        let res = cowl_api::dispatch_json(&line);
        stdout.write_all(res.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()?; // 1リクエストごとに必ずフラッシュ（対話性の担保）
    }
    Ok(())
}
