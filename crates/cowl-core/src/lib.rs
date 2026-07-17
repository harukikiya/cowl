//! # cowl-core — 解析コア
//!
//! 依存の向き: facts(事実IR) → analysis(解析) → render_*（描画）。
//! このクレートはファイルシステム・CLI・ネットワークを一切知らない
//! 純粋層として保つこと（フロントエンドやシェルの都合を持ち込まない）。

pub mod analysis;
pub mod facts;
pub mod render_dot;
pub mod render_html;

/// 利用側（api層）の取り回し用に主要型をまとめて再輸出する
pub mod prelude {
    pub use crate::analysis::{analyze, Report, REPORT_SCHEMA_VERSION};
    pub use crate::facts::{Facts, FACTS_SCHEMA_VERSION};
    pub use crate::render_dot::render_dot;
    pub use crate::render_html::render_html;
}
