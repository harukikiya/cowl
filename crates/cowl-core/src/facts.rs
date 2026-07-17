//! # facts — 事実IR（Intermediate Representation）
//!
//! ## この層の役割
//! フロントエンド（tree-sitter版L1、将来のlibclang版L2、LLM補助のL3）が
//! Cソースから抽出した「観測事実」を、解析層が読める共通形式に落とす。
//!
//! 設計上の最重要ルール:
//! **facts には『測った事実』だけを入れる。『推定・解釈』は入れない。**
//!
//! 例えば `q = p;` はC言語上「所有権のムーブ」かもしれないし「借用（別名）」
//! かもしれない。それを決めるのは解析層の仕事であって、フロントエンドは
//! 「変数qに変数pが代入された」という構文事実 (`AssignFromVar`) だけを記録する。
//! この分離が facts firewall であり、L3(LLM)を将来足すときに
//! 「LLMの推測が測定値のふりをして混入する」事故を構造的に防ぐ。
//!
//! ## スキーマバージョニング
//! facts は JSON でシリアライズされ、外部ツール（MCP/VSCode拡張）にも
//! 露出しうる。フィールドの追加・削除・意味変更を行うときは必ず
//! `FACTS_SCHEMA_VERSION` を上げ、ADRを書くこと（.claude/skills/facts-schema 参照）。

use serde::{Deserialize, Serialize};

/// facts スキーマのバージョン。
/// 互換性が壊れる変更（フィールド削除・意味変更）ではメジャーを上げる。
pub const FACTS_SCHEMA_VERSION: &str = "0.1.0";

/// ソース上の位置。行・列とも **1始まり**（エディタ表示と揃えるため）。
/// tree-sitter は0始まりの row/column を返すので、フロントエンド側で +1 する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub line_start: u32,
    pub line_end: u32,
    pub col_start: u32,
    pub col_end: u32,
}

impl Span {
    /// 1行内の範囲を作る補助。テストで頻出するので用意しておく。
    pub fn line(line: u32) -> Self {
        Span {
            line_start: line,
            line_end: line,
            col_start: 1,
            col_end: 1,
        }
    }
}

/// 関数内でポインタ変数を一意に指すID。
/// 名前(String)ではなくIDで参照するのは、
/// (a) 将来スコープ対応で同名変数を区別するため
/// (b) JSONサイズと比較コストを抑えるため
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct VarId(pub u32);

/// 1ファイル分の抽出結果。
/// `source` を同梱するのは「一度解析したら、元ファイルが手元に無くても
/// レポートを再描画できる」ようにするため（API越しの利用を想定）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Facts {
    pub schema_version: String,
    /// 表示用のファイル名（パスでも仮名でもよい。意味は持たせない）
    pub file: String,
    /// 元ソース全文。描画層が行単位で参照する
    pub source: String,
    pub functions: Vec<FunctionFacts>,
}

/// 関数単位の事実。L1解析は関数内(intraprocedural)に閉じる方針なので、
/// 関数がそのまま解析の単位になる。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionFacts {
    pub name: String,
    pub span: Span,
    /// 追跡対象のポインタ変数（宣言順）
    pub vars: Vec<VarDecl>,
    /// 観測イベント列。**必ずソース出現順（行→列）でソート済み**であること。
    /// 解析層はこの順序を前提に線形走査する（制御フローは見ない=L1の割り切り）。
    pub events: Vec<Event>,
    /// 解析器が「わからなかった」と自己申告する箇所。
    /// これはノイズではなく一級の信号：所有権カバレッジ指標の分母/分子に効くし、
    /// L2/L3で優先的に潰すべき箇所のワークリストにもなる。
    pub unknowns: Vec<Unknown>,
}

/// ポインタ変数の宣言事実
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarDecl {
    pub id: VarId,
    pub name: String,
    pub decl: Span,
}

/// 変数に対して観測された1イベント
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub var: VarId,
    pub span: Span,
    pub kind: EventKind,
}

/// イベントの種類。
/// ここに新しい種類を足すときは analysis.rs の状態機械と
/// render の凡例を必ずセットで更新する（skills/add-metric の手順参照）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EventKind {
    /// リソースの獲得。`p = malloc(...)` や `p = &x`
    Alloc { source: AllocSource },
    /// 追跡中の別ポインタ変数からの代入。`q = p;`
    /// ムーブか借用かは**ここでは決めない**（解析層が両様に扱う）
    AssignFromVar { src: VarId },
    /// `p = NULL;` / `p = 0;`。ダングリング状態のリセットとして重要
    AssignNull,
    /// 右辺を解釈できなかった代入。`p = get_buf();` など。
    /// 所有権曖昧度のカウント対象
    AssignOpaque { detail: String },
    /// 参照・演算・条件式などでの使用。`*p`, `p->x`, `p[i]`, `if (p)` 等
    Use { mode: UseMode },
    /// `free(p)` 相当。標準の free のみフロントエンドが直接発行する
    Free,
    /// 関数呼び出しへの引き渡し。`f(p)`
    /// consumed:
    ///   Some(true)  = 既知の消費関数（fclose等）。所有権が移る
    ///   Some(false) = 既知の非消費関数（printf, strlen等）。ただの使用
    ///   None        = 未知の関数。**曖昧**（L1では決められない）
    PassedTo {
        callee: String,
        consumed: Option<bool>,
    },
    /// `return p;` — 呼び出し元へ所有権が脱出
    EscapeReturn,
    /// 構造体フィールド・グローバル等、追跡範囲外への格納。`g->buf = p;`
    EscapeStore { target: String },
}

/// 獲得元の分類
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AllocSource {
    /// ヒープ確保。func は malloc / calloc / strdup など呼び出し名
    Heap { func: String },
    /// `&x` によるアドレス取得。これは**借用**であり free してはいけない。
    /// Rustの参照に相当する概念なので区別して持つ
    AddressOf,
}

/// 使用の向き。L1では読み書きの厳密判定はせず、
/// 代入LHSの `*p = ...` だけを Write と分類する程度に留める
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UseMode {
    Read,
    Write,
}

/// 「解析器がわからなかった」の記録。
/// reason は人間（とオーケストレータ）向けの日本語で書く
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Unknown {
    pub span: Span,
    pub reason: String,
}

impl Facts {
    /// source から指定行のテキストを取り出す（1始まり）。
    /// 描画層のためのユーティリティ。範囲外は空文字を返し、パニックしない
    pub fn line_text(&self, line: u32) -> &str {
        self.source
            .lines()
            .nth(line.saturating_sub(1) as usize)
            .unwrap_or("")
    }
}
