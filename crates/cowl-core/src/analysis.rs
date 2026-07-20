//! # analysis — 所有権・ライフタイムの解析層
//!
//! facts（構文事実）を受け取り、次を計算する:
//!   1. 各ポインタ変数の **ライフタイム区間**（描画用の帯セグメント列）
//!   2. **診断**（use-after-free / double free / リーク疑い 等）
//!   3. **品質指標**（所有権カバレッジ・曖昧率）
//!   4. **所有権グラフ**（確保サイト・変数・移譲/別名エッジ）
//!
//! ## モデル：確保サイト(Site)と変数(Var)の二層
//! 「ポインタ変数」と「その指す先のリソース」を分けて追跡するのが肝。
//!   - Site … `malloc` 1回分のリソース。free されるのは Site
//!   - Var  … Site への束縛を持つ名前。`q = p` で同じ Site に複数 Var が束縛される
//!
//! こうすると `q = p; free(q); *p;` のような **別名経由の use-after-free** が
//! 自然に検出できる（Site が Freed になった瞬間、束縛中の全 Var が Dangling になる）。
//!
//! ## L1 の割り切り（重要・ワーカーへの注意）
//! イベントは**ソース出現順に線形走査**する。if/loop の分岐は考慮しない。
//! つまり `if (err) free(p); use(p);` は UAF と誤検出しうる。
//! これは欠陥ではなく L1 の仕様（構文近似）。精度が必要な箇所は
//! L2(libclang + CFG) の仕事であり、L1 側に小細工でパスを足して
//! 「直そう」としないこと。誤検出が問題になったら unknowns/信頼度で表現する。

use crate::facts::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

// ---------------------------------------------------------------------------
// 出力型（レポート）— これがそのまま JSON / 描画層の入力になる
// ---------------------------------------------------------------------------

/// 解析レポートのスキーマバージョン（factsとは独立に進化する）
/// 0.2.0: Free-Site Multiplicity、Live-Range Length、Transfer Density の3指標を追加（フィールド追加のみ）
/// 0.3.0: Aliasing Pressure（別名圧力）の3指標を追加（フィールド追加のみ）
/// 0.4.0: Aliasing Pressure の計上対象を AddressOf 借用 Site に拡張（ADR-0010・W8-2）
///        形は不変だが、既存入力の値が増えうるため、対象拡大を自己申告するマイナー版上げ
pub const REPORT_SCHEMA_VERSION: &str = "0.4.0";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub schema_version: String,
    pub file: String,
    pub functions: Vec<FunctionReport>,
    /// ファイル全体の集計指標
    pub metrics: Metrics,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionReport {
    pub name: String,
    pub span: Span,
    pub vars: Vec<VarReport>,
    pub issues: Vec<Issue>,
    pub metrics: Metrics,
    pub graph: Graph,
    /// フロントエンド由来の「わからなかった」箇所をそのまま透過する。
    /// レポート消費側（人間・オーケストレータ）が信頼度を判断する材料
    pub unknowns: Vec<Unknown>,
}

/// 変数1本ぶんの描画データ
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VarReport {
    pub name: String,
    pub decl_line: u32,
    /// ライフタイム帯。行区間ごとの状態（Owned/Alias/Dangling...）
    /// 区間は昇順・重なりなし。描画層はこれを塗るだけでよい
    pub segments: Vec<Segment>,
    /// 帯の上に打つマーカー（A=確保, F=解放, ▲=使用 など）
    pub marks: Vec<Mark>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Segment {
    pub from_line: u32,
    pub to_line: u32,
    pub phase: Phase,
}

/// 変数の状態フェーズ。**描画色と1対1対応**させる（render側の凡例参照）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// 未初期化（宣言のみ）
    Uninit,
    /// NULL を保持
    Null,
    /// ヒープ資源を保持し、解放責任がこの変数にあるとみなせる状態
    Owned,
    /// 他変数と同じ Site を共有（別名）。解放責任の所在は曖昧
    Alias,
    /// `&x` 由来の借用。free 対象ではない
    Borrowed,
    /// 指す先が free 済み。使用すれば UAF
    Dangling,
    /// 消費関数に渡してムーブ済み
    Moved,
    /// return / 外部格納で関数外へ脱出済み
    Escaped,
    /// 右辺不明の代入など、追跡不能
    OpaqueVal,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mark {
    pub line: u32,
    pub kind: MarkKind,
    /// ホバー表示用の日本語説明
    pub note: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkKind {
    Alloc,
    Free,
    Use,
    Move,
    Escape,
    Issue,
}

/// 診断。message は利用者向け日本語
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Issue {
    pub kind: IssueKind,
    pub line: u32,
    pub var: String,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IssueKind {
    UseAfterFree,
    DoubleFree,
    /// スコープ終端で未解放・未脱出のヒープ資源
    LeakSuspect,
    /// 所有中の変数への上書き代入（旧資源への参照を失う）
    OverwriteOwned,
    /// `&x` 由来ポインタや NULL への free
    FreeInvalid,
}

/// 品質指標。前段の議論で定義した
/// 「所有権カバレッジ」「所有権曖昧度」をここで実装する。
///
/// - 所有権カバレッジ = ライフサイクル（確保→…→終端）を解析器が
///   曖昧さなしに追い切れた Site / 全 Site
/// - 曖昧度 = 曖昧フラグが立った Site / 全 Site
///
/// 曖昧フラグが立つ条件: 未知関数への引き渡し / 右辺不明の上書き /
/// 別名を経由した所有の分岐 など（コード中の `mark_ambiguous` 呼び出し箇所を参照）
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Metrics {
    pub sites_total: u32,
    /// 終端状態が Freed / Moved / Escaped に確定し、かつ曖昧でない Site 数
    pub sites_resolved: u32,
    pub sites_ambiguous: u32,
    /// sites_resolved / sites_total（分母0のとき1.0=「不明箇所なし」と定義）
    pub ownership_coverage: f64,
    /// sites_ambiguous / sites_total
    pub ambiguity_rate: f64,
    pub issues_total: u32,
    /// 種類別の件数（表示・集計用）。BTreeMapなのはJSON出力を安定させるため
    pub issues_by_kind: BTreeMap<String, u32>,

    // --- 指標第2陣（ADR-0006）---
    /// **Free-Site Multiplicity** (解放サイト多重度)
    /// 意図: 1つの資源が複数の箇所で解放されうる構造は脆い
    /// 分母分子: free_sites_total / sites_freed
    /// 分母0のとき: 1.0 （理想値。解放なし=多重解放なし）
    /// 相関仮説: 1.0超は double free 混入率・リファクタ時の解放漏れと相関するはず
    pub sites_freed: u32,
    /// 1回以上解放された Site ごとの「相異なる解放行」の合計
    pub free_sites_total: u32,
    /// 解放行が2つ以上ある Site 数
    pub sites_multi_free: u32,
    /// free_sites_total / sites_freed（分母0のとき1.0）
    pub free_site_multiplicity: f64,

    /// **Live-Range Length** (生存区間長)
    /// 意図: 確保から最終接触までが長いほど、レビューで追う距離が長く
    ///       管理ミス（解放漏れ・二重解放）が混入しやすい
    /// 分母分子: live_range_lines_total / sites_total
    /// 分母0のとき: 0.0 （ヒープなし）
    /// 相関仮説: 平均生存行数はリーク・UAF の混入率と正の相関を持つはず
    pub live_range_lines_total: u32,
    /// 全 Site の平均生存行数（live_range_lines_total / sites_total）
    pub live_range_avg: f64,

    /// **Transfer Density** (移譲密度)
    /// 意図: 関数境界をまたぐ所有権移譲が多いほど、呼び出し規約という
    ///       暗黙知への依存が増え、誤解によるリーク/二重解放が出やすい
    /// 分母分子: transfers_total / (lines_analyzed / 1000)
    /// 分母0のとき: 0.0 （解析対象行なし）
    /// 相関仮説: 高密度のファイルほど所有権の所在の文書化が必要になるはず
    pub transfers_total: u32,
    /// 解析対象行数（各関数 span の line_end - line_start + 1 の合計）
    pub lines_analyzed: u32,
    /// transfers_total / (lines_analyzed / 1000)（分母0のとき0.0）
    pub transfer_density: f64,

    // --- 指標第3陣（ADR-0009）---
    /// **Aliasing Pressure Maximum** (別名圧力の最大値)
    /// 意図: 「同じリソースに同時に書き込める名前が複数ある」状態は
    ///       Rust の借用規則が禁じる排他性違反に相当し、追跡難度と
    ///       リファクタリング時の退行リスクが高い
    /// 分子・分母: ヒープ確保 Site と AddressOf 借用 Site の全体にわたって
    ///       「その Site に同時に生存した書込可能な束縛（Var）数」の最大値
    ///       （ADR-0010）
    /// 分母0のとき: 0 （ヒープ確保・借用なし。理想値）
    /// 相関仮説: 別名圧力が2以上の Site ほど、use-after-free や
    ///       データ競争（将来のマルチスレッド対応）のリスクが高いはず
    pub aliasing_pressure_max: u32,

    /// **Aliasing Pressure Sites** (圧力2以上の Site 数)
    /// 意図: 「書き込み別名がある」という状態にある Site の数を測る。
    ///       カバレッジ指標と並べて「どの程度危ないのか」を定量化する
    /// 分子: ヒープ確保と AddressOf 借用の全 Site のうち、
    ///       「同時最大書込可能束縛数が2以上」の Site 数（ADR-0010）
    /// 分母0のとき: 0 （理想状態）
    /// 相関仮説: 圧力 Site 数が多いほど、全体の所有権複雑性が高く、
    ///       設計見直しのターゲットになるはず
    pub aliasing_pressure_sites: u32,

    /// **Aliasing Unknown Bindings** (測定不能な別名束縛数)
    /// 意図: pointee_const = None の Var による束縛をカウント。
    ///       「測れなかった」ことを可視化し、L2/L3 で優先的に
    ///       解決すべき箇所のシグナルにする
    /// 分子: ヒープ確保と AddressOf 借用の両 Site において、
    ///       pointee_const が None のまま束縛が生存した Var-Site 対の
    ///       総数（時系列集計。同一対が複数回生存すれば複数回カウント）（ADR-0010）
    /// 分母0のとき: 0 （すべて測定可能。理想値）
    /// 相関仮説: unknown が少ないほど、L1 の構文解析だけで十分に
    ///       信頼度の高い解析ができる（L2 導入の効果も測れる）
    pub aliasing_unknown_bindings: u32,
}

impl Metrics {
    fn finalize(&mut self) {
        let t = self.sites_total as f64;
        // 分母0（ヒープ確保が1つもない関数）は「追えていない物が無い」ので
        // カバレッジ100%と定義する。0%にすると綺麗なコードほど低スコアになり
        // 指標として逆向きになってしまう
        self.ownership_coverage = if t == 0.0 {
            1.0
        } else {
            self.sites_resolved as f64 / t
        };
        self.ambiguity_rate = if t == 0.0 {
            0.0
        } else {
            self.sites_ambiguous as f64 / t
        };

        // Free-Site Multiplicity: free_sites_total / sites_freed
        // 分母0のとき 1.0（理想値）
        self.free_site_multiplicity = if self.sites_freed == 0 {
            1.0
        } else {
            self.free_sites_total as f64 / self.sites_freed as f64
        };

        // Live-Range Length: live_range_lines_total / sites_total
        // 分母0のとき 0.0（ヒープなし）
        self.live_range_avg = if self.sites_total == 0 {
            0.0
        } else {
            self.live_range_lines_total as f64 / self.sites_total as f64
        };

        // Transfer Density: transfers_total / (lines_analyzed / 1000)
        // 分母0のとき 0.0（解析対象行なし）
        self.transfer_density = if self.lines_analyzed == 0 {
            0.0
        } else {
            self.transfers_total as f64 / (self.lines_analyzed as f64 / 1000.0)
        };
    }
    fn absorb(&mut self, other: &Metrics) {
        self.sites_total += other.sites_total;
        self.sites_resolved += other.sites_resolved;
        self.sites_ambiguous += other.sites_ambiguous;
        self.issues_total += other.issues_total;
        for (k, v) in &other.issues_by_kind {
            *self.issues_by_kind.entry(k.clone()).or_insert(0) += v;
        }
        // 指標第2陣の集計：カウンタを単純加算
        self.sites_freed += other.sites_freed;
        self.free_sites_total += other.free_sites_total;
        self.sites_multi_free += other.sites_multi_free;
        self.live_range_lines_total += other.live_range_lines_total;
        self.transfers_total += other.transfers_total;
        self.lines_analyzed += other.lines_analyzed;
        // 指標第3陣の集計：最大値は max() で、件数は加算
        self.aliasing_pressure_max = self.aliasing_pressure_max.max(other.aliasing_pressure_max);
        self.aliasing_pressure_sites += other.aliasing_pressure_sites;
        self.aliasing_unknown_bindings += other.aliasing_unknown_bindings;
    }
}

// ---------------------------------------------------------------------------
// 所有権グラフ — DOT 描画やVSCode拡張のグラフビューが消費する
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Graph {
    pub nodes: Vec<GNode>,
    pub edges: Vec<GEdge>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GNode {
    /// DOT でそのまま使えるID（英数字のみにする）
    pub id: String,
    pub label: String,
    pub kind: GNodeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GNodeKind {
    /// 確保サイト（資源そのもの）
    Site,
    /// ポインタ変数
    Var,
    /// 関数外世界（return先・外部格納先）を1ノードに畳んだもの
    Outside,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GEdge {
    pub from: String,
    pub to: String,
    pub label: String,
    pub kind: GEdgeKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GEdgeKind {
    /// Site → Var: 確保による束縛
    Bind,
    /// Var → Var: 代入による別名/ムーブ（L1では区別しない）
    Assign,
    /// Var → Site: free 実行
    Free,
    /// Var → Outside: 脱出
    Escape,
    /// Var → callee: 消費関数へのムーブ
    Move,
    /// 問題エッジ（二重free等）。描画で強調する
    Issue,
}

// ---------------------------------------------------------------------------
// 内部状態 — 状態機械の実装
// ---------------------------------------------------------------------------

/// 確保サイトの内部ID（関数ローカル）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct SiteId(u32);

/// 変数が「今なにを持っているか」
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Binding {
    Uninit,
    Null,
    /// ヒープSiteへの束縛。owner=true はその Site の初代所有者
    /// （＝Owned と Alias の描き分けにだけ使う。責任の厳密なモデルではない）
    Site {
        site: SiteId,
        owner: bool,
    },
    /// `&x` 由来
    Borrow,
    /// 追跡不能な値
    Opaque,
}

/// Site のライフサイクル終端
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SiteLife {
    Live,
    Freed { line: u32 },
    Moved,
    Escaped,
}

struct SiteState {
    life: SiteLife,
    /// 曖昧フラグ。一度立ったら降りない（保守的）
    ambiguous: bool,
    alloc_line: u32,
    alloc_func: String,

    // --- 指標計上用 ---
    /// 相異なる解放行の集合（Free イベントか consumed:Some(true) な PassedTo の行）
    /// 同一行での複数回解放は1行と数える
    free_lines: BTreeSet<u32>,
    /// Site に関わる最後のイベント行（指標「生存区間」の終点）
    /// Free / Moved / Escaped / Use のいずれかの最大行
    last_event_line: u32,
    /// Moved / Escaped へ遷移した回数（同一 Site が複数回遷移することを想定）
    /// L1 では同じ Site への代入がない場合、最後の遷移で確定するが、
    /// 複数回遷移のケース（同じポインタが2箇所で return される等）も存在するため
    /// 回数をカウント。これは「移譲の複雑さ」を測る指標値
    transfer_count: u32,

    // --- 別名圧力指標用 ---
    /// 現在この Site に束縛されている Var → その Var の pointee_const
    /// 束縛の開始・終了とともに更新される
    current_bindings: BTreeMap<VarId, Option<bool>>,
    /// このサイトで観測された「同時に生存した書込可能束縛の最大数」
    /// 書込可能 = pointee_const が Some(false) の束縛
    aliasing_pressure_at_site: u32,
}

/// AddressOf 借用 Site の軽量構造（ヒープ Site と独立に追跡）
/// sites_total / ownership_coverage / leak判定の分母を汚さないため、
/// ヒープ sites 配列には混ぜない（ADR-0010）。
/// 束縛の追跡規則はヒープ Site と同じ
struct BorrowSite {
    /// 現在この借用 Site に束縛されている Var → その Var の pointee_const
    current_bindings: BTreeMap<VarId, Option<bool>>,
    /// この Site で観測された「同時に生存した書込可能束縛の最大数」
    /// 書込可能 = pointee_const が Some(false) の束縛
    aliasing_pressure_at_site: u32,
}

/// 各変数が借用 Site に束縛しているか追跡する
/// Var が AddressOf を通じて借用 Site に束縛されている場合、
/// この変数 VarId に対応する BorrowSite ID を持つ
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct BorrowSiteId(u32);

/// 解析エントリポイント：facts 全体 → Report
pub fn analyze(facts: &Facts) -> Report {
    let mut functions = Vec::new();
    let mut total = Metrics::default();
    for f in &facts.functions {
        let fr = analyze_function(f);
        total.absorb(&fr.metrics);
        functions.push(fr);
    }
    total.finalize();
    Report {
        schema_version: REPORT_SCHEMA_VERSION.to_string(),
        file: facts.file.clone(),
        functions,
        metrics: total,
    }
}

/// 関数単位の解析本体。
/// 実装方針: イベントを1つ処理するたびに全変数のフェーズを再計算し、
/// 変化があった行でセグメント境界を切る。O(events × vars) だが
/// 関数内解析なので実用上問題ない（読みやすさを優先）。
fn analyze_function(f: &FunctionFacts) -> FunctionReport {
    let n = f.vars.len();
    let mut bindings: Vec<Binding> = vec![Binding::Uninit; n];
    let mut sites: Vec<SiteState> = Vec::new();
    let mut borrow_sites: Vec<BorrowSite> = Vec::new();
    // Var → BorrowSiteId の対応（変数が借用 Site に束縛している場合）
    let mut var_to_borrow: Vec<Option<BorrowSiteId>> = vec![None; n];
    // target 名 → BorrowSiteId の対応（同一 target は同一 BorrowSite に束ねる）
    let mut borrow_key: std::collections::HashMap<String, BorrowSiteId> =
        std::collections::HashMap::new();
    let mut issues: Vec<Issue> = Vec::new();
    let mut graph = Graph::default();
    let mut metrics = Metrics::default();

    // 描画用: 変数ごとの「現在フェーズ」と「区間開始行」「セグメント蓄積」
    let mut cur_phase: Vec<Phase> = vec![Phase::Uninit; n];
    let mut seg_start: Vec<u32> = f.vars.iter().map(|v| v.decl.line_start).collect();
    let mut segments: Vec<Vec<Segment>> = vec![Vec::new(); n];
    let mut marks: Vec<Vec<Mark>> = vec![Vec::new(); n];

    // VarId は宣言順の連番である前提（フロントエンドの責務）。
    // 前提が崩れたときに黙って誤動作しないよう debug_assert で守る
    for (i, v) in f.vars.iter().enumerate() {
        debug_assert_eq!(v.id.0 as usize, i, "VarIdは宣言順の連番であること");
        graph.nodes.push(GNode {
            id: format!("v{}", i),
            label: v.name.clone(),
            kind: GNodeKind::Var,
        });
    }
    // 「関数の外」を1ノードで表す（return / 外部格納の行き先）
    graph.nodes.push(GNode {
        id: "outside".into(),
        label: "関数外".into(),
        kind: GNodeKind::Outside,
    });

    // --- 小さなヘルパ群 -----------------------------------------------------

    // 束縛からフェーズを導出する。Site 系は Site の生死も見る点が肝
    let phase_of = |b: Binding, sites: &Vec<SiteState>| -> Phase {
        match b {
            Binding::Uninit => Phase::Uninit,
            Binding::Null => Phase::Null,
            Binding::Borrow => Phase::Borrowed,
            Binding::Opaque => Phase::OpaqueVal,
            Binding::Site { site, owner } => match sites[site.0 as usize].life {
                SiteLife::Live => {
                    if owner {
                        Phase::Owned
                    } else {
                        Phase::Alias
                    }
                }
                SiteLife::Freed { .. } => Phase::Dangling,
                SiteLife::Moved => Phase::Moved,
                SiteLife::Escaped => Phase::Escaped,
            },
        }
    };

    // 別名圧力指標の計上ヘルパー群
    // Site への束縛を開始する。pointee_const が None なら unknown をカウント
    let bind_to_site = |sites: &mut Vec<SiteState>,
                        site_id: SiteId,
                        var_id: VarId,
                        pointee_const: Option<bool>,
                        metrics: &mut Metrics| {
        let ss = &mut sites[site_id.0 as usize];
        ss.current_bindings.insert(var_id, pointee_const);
        if pointee_const.is_none() {
            metrics.aliasing_unknown_bindings += 1;
        }
    };

    // Site からの束縛を解除する。
    let unbind_from_site = |sites: &mut Vec<SiteState>, site_id: SiteId, var_id: VarId| {
        let ss = &mut sites[site_id.0 as usize];
        ss.current_bindings.remove(&var_id);
    };

    // Site の現在の書込可能束縛を数え、最大値を更新する。
    // Site が Live の間のみカウント（Freed/Moved/Escaped後は圧力を数えない）。
    // 返り値：true なら最大値が初めて2以上に達した（aliasing_pressure_sites のカウント対象）
    let update_aliasing_pressure =
        |sites: &mut Vec<SiteState>, site_id: SiteId, metrics: &mut Metrics| -> bool {
            let ss = &mut sites[site_id.0 as usize];
            // Site が Live でなければ、生存中の束縛の圧力をカウントしない
            // （解放後の dangling 別名は既存の use_after_free 診断に任せる）
            if !matches!(ss.life, SiteLife::Live) {
                return false;
            }
            // 現在の書込可能束縛数を数える（pointee_const = Some(false) のみ）
            let writable_count = ss
                .current_bindings
                .values()
                .filter(|pc| matches!(pc, Some(false)))
                .count() as u32;
            // 最大値を更新
            let old_pressure = ss.aliasing_pressure_at_site;
            ss.aliasing_pressure_at_site = ss.aliasing_pressure_at_site.max(writable_count);
            // グローバル最大値を更新
            metrics.aliasing_pressure_max = metrics
                .aliasing_pressure_max
                .max(ss.aliasing_pressure_at_site);
            // 最大値が初めて2以上に達した場合は true を返す
            old_pressure < 2 && ss.aliasing_pressure_at_site >= 2
        };

    // 借用 Site への束縛を開始する（W8-2）。pointee_const が None なら unknown をカウント
    let bind_to_borrow = |borrow_sites: &mut Vec<BorrowSite>,
                          borrow_id: BorrowSiteId,
                          var_id: VarId,
                          pointee_const: Option<bool>,
                          metrics: &mut Metrics| {
        let bs = &mut borrow_sites[borrow_id.0 as usize];
        bs.current_bindings.insert(var_id, pointee_const);
        if pointee_const.is_none() {
            metrics.aliasing_unknown_bindings += 1;
        }
    };

    // 借用 Site からの束縛を解除する（W8-2）
    let unbind_from_borrow =
        |borrow_sites: &mut Vec<BorrowSite>, borrow_id: BorrowSiteId, var_id: VarId| {
            let bs = &mut borrow_sites[borrow_id.0 as usize];
            bs.current_bindings.remove(&var_id);
        };

    // 借用 Site の現在の書込可能束縛を数え、最大値を更新する（W8-2）。
    // 借用 Site は常に「生存」状態と見なす（free されないため）。
    // 返り値：true なら最大値が初めて2以上に達した（aliasing_pressure_sites のカウント対象）
    let update_aliasing_pressure_borrow = |borrow_sites: &mut Vec<BorrowSite>,
                                           borrow_id: BorrowSiteId,
                                           metrics: &mut Metrics|
     -> bool {
        let bs = &mut borrow_sites[borrow_id.0 as usize];
        // 現在の書込可能束縛数を数える（pointee_const = Some(false) のみ）
        let writable_count = bs
            .current_bindings
            .values()
            .filter(|pc| matches!(pc, Some(false)))
            .count() as u32;
        // 最大値を更新
        let old_pressure = bs.aliasing_pressure_at_site;
        bs.aliasing_pressure_at_site = bs.aliasing_pressure_at_site.max(writable_count);
        // グローバル最大値を更新（ヒープと借用の全体最大）
        metrics.aliasing_pressure_max = metrics
            .aliasing_pressure_max
            .max(bs.aliasing_pressure_at_site);
        // 最大値が初めて2以上に達した場合は true を返す
        old_pressure < 2 && bs.aliasing_pressure_at_site >= 2
    };

    // 全変数のフェーズを再計算し、変化した変数のセグメントを line で切り替える。
    // 「イベントの行から新フェーズが始まる」規約（確保行はもうOwned色で塗る）
    macro_rules! commit_phases {
        ($line:expr) => {{
            let line: u32 = $line;
            for i in 0..n {
                let np = phase_of(bindings[i], &sites);
                if np != cur_phase[i] {
                    // 直前フェーズの区間を閉じる（開始行より前には戻さない）
                    let end = line.saturating_sub(1).max(seg_start[i]);
                    if line > seg_start[i] {
                        segments[i].push(Segment {
                            from_line: seg_start[i],
                            to_line: end,
                            phase: cur_phase[i],
                        });
                        seg_start[i] = line;
                    } else {
                        // 同一行内で複数回状態が変わるケース（decl即alloc等）は
                        // 最後の状態だけ残す＝開始行を維持してフェーズのみ更新
                    }
                    cur_phase[i] = np;
                }
            }
        }};
    }

    macro_rules! push_issue {
        ($kind:expr, $line:expr, $var:expr, $msg:expr) => {{
            issues.push(Issue {
                kind: $kind,
                line: $line,
                var: $var,
                message: $msg,
            });
        }};
    }

    // --- イベント線形走査（L1の心臓部） -------------------------------------

    let mut site_seq = 0u32;
    for ev in &f.events {
        let vi = ev.var.0 as usize;
        let vname = f.vars[vi].name.clone();
        let line = ev.span.line_start;

        match &ev.kind {
            EventKind::Alloc { source } => {
                // 所有中の変数に再確保を上書き → 旧資源への最後の参照を
                // 失っている可能性が高い（典型的なループ内リーク）
                if let Binding::Site { site, owner: true } = bindings[vi] {
                    if matches!(sites[site.0 as usize].life, SiteLife::Live) {
                        push_issue!(
                            IssueKind::OverwriteOwned,
                            line,
                            vname.clone(),
                            format!(
                                "所有中の資源(L{}確保)を解放せずに上書きしています（リーク疑い）",
                                sites[site.0 as usize].alloc_line
                            )
                        );
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Issue,
                            note: "上書きによるリーク疑い".into(),
                        });
                    }
                }
                // Alloc はどの source でも vi の旧束縛を終わらせる（Heap なら
                // 新 Site へ、AddressOf なら借用 Site へ上書き）。だから旧束縛
                // からの解除は分岐に入る前にここで共通に行う（AssignFromVar /
                // AssignNull / AssignOpaque の各分岐と対称。W8-2で借用解除も追加）。
                // この対称性を欠くと旧 Site の current_bindings に stale な束縛が残り、
                // 以後その Site に別の変数が束縛されたとき、もう指していない
                // 変数まで書込可能数に数えて別名圧力を過大計上してしまう
                if let Binding::Site { site: old_site, .. } = bindings[vi] {
                    unbind_from_site(&mut sites, old_site, VarId(vi as u32));
                    if update_aliasing_pressure(&mut sites, old_site, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                }
                // W8-2: 前の借用 Site からも解除
                if let Some(old_borrow_id) = var_to_borrow[vi] {
                    unbind_from_borrow(&mut borrow_sites, old_borrow_id, VarId(vi as u32));
                    if update_aliasing_pressure_borrow(
                        &mut borrow_sites,
                        old_borrow_id,
                        &mut metrics,
                    ) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    var_to_borrow[vi] = None;
                }
                match source {
                    AllocSource::Heap { func } => {
                        let sid = SiteId(site_seq);
                        site_seq += 1;
                        // Var の pointee_const を取得（このアロケーションで束縛する変数から）
                        let pointee_const = f.vars[vi].pointee_const;
                        // 新しい Site を作成（束縛は bind_to_site で追加する）
                        sites.push(SiteState {
                            life: SiteLife::Live,
                            ambiguous: false,
                            alloc_line: line,
                            alloc_func: func.clone(),
                            free_lines: BTreeSet::new(),
                            last_event_line: line,
                            transfer_count: 0,
                            current_bindings: BTreeMap::new(),
                            aliasing_pressure_at_site: 0,
                        });
                        // 最初の束縛を追加し、unknown カウント
                        bind_to_site(
                            &mut sites,
                            sid,
                            VarId(vi as u32),
                            pointee_const,
                            &mut metrics,
                        );
                        // 圧力を更新（最初の束縛なので通常 Site 圧力は1だが、念のため呼び出す）
                        if update_aliasing_pressure(&mut sites, sid, &mut metrics) {
                            metrics.aliasing_pressure_sites += 1;
                        }

                        bindings[vi] = Binding::Site {
                            site: sid,
                            owner: true,
                        };
                        let sid_str = format!("s{}", sid.0);
                        graph.nodes.push(GNode {
                            id: sid_str.clone(),
                            label: format!("{}@L{}", func, line),
                            kind: GNodeKind::Site,
                        });
                        graph.edges.push(GEdge {
                            from: sid_str,
                            to: format!("v{}", vi),
                            label: "alloc".into(),
                            kind: GEdgeKind::Bind,
                        });
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Alloc,
                            note: format!("{} で確保", func),
                        });
                    }
                    // W8-2: AddressOf の target により借用 Site を作成・共有
                    AllocSource::AddressOf { target } => {
                        let pointee_const = f.vars[vi].pointee_const;
                        let borrow_id = match target {
                            Some(target_name) => {
                                // target が Some のとき: 同じ target 名は同じ借用 Site に束ねる
                                let next_id = BorrowSiteId(borrow_sites.len() as u32);
                                *borrow_key.entry(target_name.clone()).or_insert_with(|| {
                                    borrow_sites.push(BorrowSite {
                                        current_bindings: BTreeMap::new(),
                                        aliasing_pressure_at_site: 0,
                                    });
                                    next_id
                                })
                            }
                            None => {
                                // target が None のとき: 複合式なので出現ごとに新規 Site
                                let next_id = BorrowSiteId(borrow_sites.len() as u32);
                                borrow_sites.push(BorrowSite {
                                    current_bindings: BTreeMap::new(),
                                    aliasing_pressure_at_site: 0,
                                });
                                next_id
                            }
                        };
                        // 新規に束縛を追加
                        bind_to_borrow(
                            &mut borrow_sites,
                            borrow_id,
                            VarId(vi as u32),
                            pointee_const,
                            &mut metrics,
                        );
                        // 圧力を更新
                        if update_aliasing_pressure_borrow(
                            &mut borrow_sites,
                            borrow_id,
                            &mut metrics,
                        ) {
                            metrics.aliasing_pressure_sites += 1;
                        }
                        // 変数→借用 Site の対応を記録
                        var_to_borrow[vi] = Some(borrow_id);
                        bindings[vi] = Binding::Borrow;
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Alloc,
                            note: "&x（借用。freeしてはいけない）".into(),
                        });
                    }
                }
            }

            EventKind::AssignFromVar { src } => {
                let si = src.0 as usize;
                // 代入前に、既存の束縛を解除する（もし存在すれば）
                if let Binding::Site { site: old_site, .. } = bindings[vi] {
                    unbind_from_site(&mut sites, old_site, VarId(vi as u32));
                    if update_aliasing_pressure(&mut sites, old_site, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                }
                // W8-2: 前の借用 Site からも解除
                if let Some(old_borrow_id) = var_to_borrow[vi] {
                    unbind_from_borrow(&mut borrow_sites, old_borrow_id, VarId(vi as u32));
                    if update_aliasing_pressure_borrow(
                        &mut borrow_sites,
                        old_borrow_id,
                        &mut metrics,
                    ) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    var_to_borrow[vi] = None;
                }

                match bindings[si] {
                    Binding::Site { site, .. } => {
                        // L1では「別名」として扱う（ムーブ断定はしない）。
                        // 所有の所在が2箇所になった時点で Site は曖昧マーク。
                        // ※ ここが将来 L2/L3 で「src以後未使用ならムーブ」等に
                        //   精緻化される拡張ポイント
                        let pointee_const = f.vars[vi].pointee_const;
                        bind_to_site(
                            &mut sites,
                            site,
                            VarId(vi as u32),
                            pointee_const,
                            &mut metrics,
                        );
                        if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                            metrics.aliasing_pressure_sites += 1;
                        }

                        bindings[vi] = Binding::Site { site, owner: false };
                        sites[site.0 as usize].ambiguous = true;
                        graph.edges.push(GEdge {
                            from: format!("v{}", si),
                            to: format!("v{}", vi),
                            label: "assign".into(),
                            kind: GEdgeKind::Assign,
                        });
                    }
                    Binding::Borrow => {
                        // W8-2: src が借用に束縛されていれば、dst もその借用 Site に束ねる
                        if let Some(src_borrow_id) = var_to_borrow[si] {
                            let pointee_const = f.vars[vi].pointee_const;
                            bind_to_borrow(
                                &mut borrow_sites,
                                src_borrow_id,
                                VarId(vi as u32),
                                pointee_const,
                                &mut metrics,
                            );
                            if update_aliasing_pressure_borrow(
                                &mut borrow_sites,
                                src_borrow_id,
                                &mut metrics,
                            ) {
                                metrics.aliasing_pressure_sites += 1;
                            }
                            var_to_borrow[vi] = Some(src_borrow_id);
                        }
                        bindings[vi] = Binding::Borrow;
                    }
                    Binding::Null => bindings[vi] = Binding::Null,
                    _ => bindings[vi] = Binding::Opaque,
                }
            }

            EventKind::AssignNull => {
                // free 後の NULL 代入は良い作法：ダングリングが解消される。
                // だからこそ Null を独立フェーズとして持つ価値がある
                // 既存の束縛を解除（圧力に影響）
                if let Binding::Site { site, .. } = bindings[vi] {
                    unbind_from_site(&mut sites, site, VarId(vi as u32));
                    if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                }
                // W8-2: 借用 Site からも解除
                if let Some(old_borrow_id) = var_to_borrow[vi] {
                    unbind_from_borrow(&mut borrow_sites, old_borrow_id, VarId(vi as u32));
                    if update_aliasing_pressure_borrow(
                        &mut borrow_sites,
                        old_borrow_id,
                        &mut metrics,
                    ) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    var_to_borrow[vi] = None;
                }
                bindings[vi] = Binding::Null;
            }

            EventKind::AssignOpaque { detail } => {
                if let Binding::Site { site, owner: true } = bindings[vi] {
                    if matches!(sites[site.0 as usize].life, SiteLife::Live) {
                        push_issue!(
                            IssueKind::OverwriteOwned,
                            line,
                            vname.clone(),
                            "所有中の資源を解放せずに上書きしています（リーク疑い）".into()
                        );
                    }
                    // 既存の束縛を解除（圧力に影響）
                    unbind_from_site(&mut sites, site, VarId(vi as u32));
                    if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                }
                // W8-2: 借用 Site からも解除
                if let Some(old_borrow_id) = var_to_borrow[vi] {
                    unbind_from_borrow(&mut borrow_sites, old_borrow_id, VarId(vi as u32));
                    if update_aliasing_pressure_borrow(
                        &mut borrow_sites,
                        old_borrow_id,
                        &mut metrics,
                    ) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    var_to_borrow[vi] = None;
                }
                // 由来不明の値は以後追跡不能。free されても正当性を判断できない
                bindings[vi] = Binding::Opaque;
                marks[vi].push(Mark {
                    line,
                    kind: MarkKind::Use,
                    note: format!("追跡不能な代入: {}", detail),
                });
            }

            EventKind::Use { .. } => {
                if let Binding::Site { site, .. } = bindings[vi] {
                    sites[site.0 as usize].last_event_line = line;
                    if let SiteLife::Freed { line: fl } = sites[site.0 as usize].life {
                        push_issue!(
                            IssueKind::UseAfterFree,
                            line,
                            vname.clone(),
                            format!("L{} で解放済みの資源を使用しています (use-after-free)", fl)
                        );
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Issue,
                            note: "use-after-free".into(),
                        });
                    } else {
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Use,
                            note: "使用".into(),
                        });
                    }
                } else if bindings[vi] == Binding::Uninit {
                    // 未初期化使用は L1 の対象外だが、marksには残しておく
                    marks[vi].push(Mark {
                        line,
                        kind: MarkKind::Use,
                        note: "使用（未初期化の可能性）".into(),
                    });
                } else {
                    marks[vi].push(Mark {
                        line,
                        kind: MarkKind::Use,
                        note: "使用".into(),
                    });
                }
            }

            EventKind::Free => {
                match bindings[vi] {
                    Binding::Site { site, .. } => {
                        let ss = &mut sites[site.0 as usize];
                        // 指標「Free-Site Multiplicity」用：解放行を記録
                        ss.free_lines.insert(line);
                        // 指標「Live-Range Length」用：最後のイベント行を更新
                        ss.last_event_line = line;
                        match ss.life {
                            SiteLife::Live => {
                                ss.life = SiteLife::Freed { line };
                                // 束縛を解除（Site の生存終了）
                                unbind_from_site(&mut sites, site, VarId(vi as u32));
                                if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                                    metrics.aliasing_pressure_sites += 1;
                                }
                                graph.edges.push(GEdge {
                                    from: format!("v{}", vi),
                                    to: format!("s{}", site.0),
                                    label: format!("free@L{}", line),
                                    kind: GEdgeKind::Free,
                                });
                                marks[vi].push(Mark {
                                    line,
                                    kind: MarkKind::Free,
                                    note: "free".into(),
                                });
                            }
                            SiteLife::Freed { line: fl } => {
                                push_issue!(
                                    IssueKind::DoubleFree,
                                    line,
                                    vname.clone(),
                                    format!(
                                        "L{} で解放済みの資源を再度 free しています (double free)",
                                        fl
                                    )
                                );
                                graph.edges.push(GEdge {
                                    from: format!("v{}", vi),
                                    to: format!("s{}", site.0),
                                    label: format!("double free@L{}", line),
                                    kind: GEdgeKind::Issue,
                                });
                                marks[vi].push(Mark {
                                    line,
                                    kind: MarkKind::Issue,
                                    note: "double free".into(),
                                });
                            }
                            _ => {
                                // ムーブ/脱出済みSiteへのfree。L1では起こりにくいが
                                // 起きたら曖昧として記録（誤検出の可能性もあるため
                                // Issueにはしない保守的判断）
                                ss.ambiguous = true;
                            }
                        }
                    }
                    Binding::Borrow => {
                        // W8-2: free(借用) は invalid だが、束縛は終わらせる（ADR-0010）
                        push_issue!(
                            IssueKind::FreeInvalid,
                            line,
                            vname.clone(),
                            "&x 由来のポインタ（借用）を free しています".into()
                        );
                        // 借用 Site からの束縛を解除
                        if let Some(borrow_id) = var_to_borrow[vi] {
                            unbind_from_borrow(&mut borrow_sites, borrow_id, VarId(vi as u32));
                            if update_aliasing_pressure_borrow(
                                &mut borrow_sites,
                                borrow_id,
                                &mut metrics,
                            ) {
                                metrics.aliasing_pressure_sites += 1;
                            }
                            var_to_borrow[vi] = None;
                        }
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Issue,
                            note: "借用へのfree".into(),
                        });
                    }
                    Binding::Null => {
                        // free(NULL) は合法なので Issue にしない。marksにだけ残す
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Free,
                            note: "free(NULL)（合法・無害）".into(),
                        });
                    }
                    _ => {
                        marks[vi].push(Mark {
                            line,
                            kind: MarkKind::Free,
                            note: "free（対象を追跡できず）".into(),
                        });
                    }
                }
            }

            EventKind::PassedTo { callee, consumed } => match consumed {
                Some(true) => {
                    // 既知の消費関数：所有権が移り、以後この関数の責任ではない
                    if let Binding::Site { site, .. } = bindings[vi] {
                        let ss = &mut sites[site.0 as usize];
                        ss.life = SiteLife::Moved;
                        // 指標「Free-Site Multiplicity」用：消費=解放と扱う
                        ss.free_lines.insert(line);
                        // 指標「Transfer Density」用：遷移回数をカウント
                        ss.transfer_count += 1;
                        // 指標「Live-Range Length」用：最後のイベント行を更新
                        ss.last_event_line = line;
                        // 束縛を解除（Site の生存終了）
                        unbind_from_site(&mut sites, site, VarId(vi as u32));
                        if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                            metrics.aliasing_pressure_sites += 1;
                        }
                        graph.edges.push(GEdge {
                            from: format!("v{}", vi),
                            to: "outside".into(),
                            label: format!("move: {}", callee),
                            kind: GEdgeKind::Move,
                        });
                    }
                    // W8-2: 消費される借用も束縛終了
                    if let Some(borrow_id) = var_to_borrow[vi] {
                        unbind_from_borrow(&mut borrow_sites, borrow_id, VarId(vi as u32));
                        if update_aliasing_pressure_borrow(
                            &mut borrow_sites,
                            borrow_id,
                            &mut metrics,
                        ) {
                            metrics.aliasing_pressure_sites += 1;
                        }
                        var_to_borrow[vi] = None;
                    }
                    marks[vi].push(Mark {
                        line,
                        kind: MarkKind::Move,
                        note: format!("{} へ所有権を移譲", callee),
                    });
                }
                Some(false) => {
                    // 既知の非消費関数：ただの使用として扱う（UAF検査だけ効かせる）
                    if let Binding::Site { site, .. } = bindings[vi] {
                        let ss = &mut sites[site.0 as usize];
                        // 指標「Live-Range Length」用：最後のイベント行を更新
                        ss.last_event_line = line;
                        if let SiteLife::Freed { line: fl } = ss.life {
                            push_issue!(
                                IssueKind::UseAfterFree,
                                line,
                                vname.clone(),
                                format!("L{} で解放済みの資源を {} に渡しています", fl, callee)
                            );
                        }
                    }
                    marks[vi].push(Mark {
                        line,
                        kind: MarkKind::Use,
                        note: format!("{} に渡す（非消費）", callee),
                    });
                }
                None => {
                    // 未知関数：ここが「所有権曖昧度」の主要な発生源。
                    // 保守的に Live のままにする（= 呼び先が free していれば
                    // 後段の free が double free になるが、それは検出できない）。
                    // 曖昧マークだけ確実に立て、指標とレポートに現れるようにする
                    if let Binding::Site { site, .. } = bindings[vi] {
                        let ss = &mut sites[site.0 as usize];
                        ss.ambiguous = true;
                        // 指標「Live-Range Length」用：最後のイベント行を更新
                        ss.last_event_line = line;
                    }
                    marks[vi].push(Mark {
                        line,
                        kind: MarkKind::Use,
                        note: format!("{} に渡す（消費するか不明＝曖昧）", callee),
                    });
                }
            },

            EventKind::EscapeReturn => {
                if let Binding::Site { site, .. } = bindings[vi] {
                    let ss = &mut sites[site.0 as usize];
                    ss.life = SiteLife::Escaped;
                    // 指標「Transfer Density」用：遷移回数をカウント
                    ss.transfer_count += 1;
                    // 指標「Live-Range Length」用：最後のイベント行を更新
                    ss.last_event_line = line;
                    // 束縛を解除（Site の生存終了）
                    unbind_from_site(&mut sites, site, VarId(vi as u32));
                    if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    graph.edges.push(GEdge {
                        from: format!("v{}", vi),
                        to: "outside".into(),
                        label: "return".into(),
                        kind: GEdgeKind::Escape,
                    });
                }
                // W8-2: 脱出する借用も束縛終了
                if let Some(borrow_id) = var_to_borrow[vi] {
                    unbind_from_borrow(&mut borrow_sites, borrow_id, VarId(vi as u32));
                    if update_aliasing_pressure_borrow(&mut borrow_sites, borrow_id, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    var_to_borrow[vi] = None;
                }
                marks[vi].push(Mark {
                    line,
                    kind: MarkKind::Escape,
                    note: "return で脱出".into(),
                });
            }

            EventKind::EscapeStore { target } => {
                if let Binding::Site { site, .. } = bindings[vi] {
                    let ss = &mut sites[site.0 as usize];
                    ss.life = SiteLife::Escaped;
                    // 外部格納は return と違い、格納先経由で free される「かも」
                    // しれない。追い切れてはいないので曖昧も同時に立てる
                    ss.ambiguous = true;
                    // 指標「Transfer Density」用：遷移回数をカウント
                    ss.transfer_count += 1;
                    // 指標「Live-Range Length」用：最後のイベント行を更新
                    ss.last_event_line = line;
                    // 束縛を解除（Site の生存終了）
                    unbind_from_site(&mut sites, site, VarId(vi as u32));
                    if update_aliasing_pressure(&mut sites, site, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    graph.edges.push(GEdge {
                        from: format!("v{}", vi),
                        to: "outside".into(),
                        label: format!("store: {}", target),
                        kind: GEdgeKind::Escape,
                    });
                }
                // W8-2: 格納される借用も束縛終了
                if let Some(borrow_id) = var_to_borrow[vi] {
                    unbind_from_borrow(&mut borrow_sites, borrow_id, VarId(vi as u32));
                    if update_aliasing_pressure_borrow(&mut borrow_sites, borrow_id, &mut metrics) {
                        metrics.aliasing_pressure_sites += 1;
                    }
                    var_to_borrow[vi] = None;
                }
                marks[vi].push(Mark {
                    line,
                    kind: MarkKind::Escape,
                    note: format!("{} へ格納（脱出）", target),
                });
            }
        }

        commit_phases!(line);
    }

    // --- 関数終端処理 --------------------------------------------------------

    let end_line = f.span.line_end;
    // 生き残った Live サイト = リーク疑い（脱出もムーブもしていない）
    for (si, ss) in sites.iter().enumerate() {
        if matches!(ss.life, SiteLife::Live) {
            // その Site を最後に握っていた変数名を探して報告に使う
            let holder = bindings
                .iter()
                .enumerate()
                .find(|(_, b)| matches!(b, Binding::Site { site, .. } if site.0 as usize == si))
                .map(|(i, _)| f.vars[i].name.clone())
                .unwrap_or_else(|| "(不明)".into());
            push_issue!(
                IssueKind::LeakSuspect,
                end_line,
                holder,
                format!(
                    "L{} で {} により確保した資源が関数終端まで解放も脱出もしていません（リーク疑い）",
                    ss.alloc_line, ss.alloc_func
                )
            );
        }
    }

    // 開いたままのセグメントを関数末尾で閉じる
    for i in 0..n {
        if end_line >= seg_start[i] {
            segments[i].push(Segment {
                from_line: seg_start[i],
                to_line: end_line,
                phase: cur_phase[i],
            });
        }
    }

    // --- 指標計算 ------------------------------------------------------------
    metrics.sites_total = sites.len() as u32;
    for ss in &sites {
        if ss.ambiguous {
            metrics.sites_ambiguous += 1;
        }
        // 「解決済み」= 終端が確定し、かつ曖昧でない。
        // Live のまま終わった Site はリーク疑いなので resolved には数えない
        let terminal = matches!(
            ss.life,
            SiteLife::Freed { .. } | SiteLife::Moved | SiteLife::Escaped
        );
        if terminal && !ss.ambiguous {
            metrics.sites_resolved += 1;
        }

        // 指標第2陣の計上：Free-Site Multiplicity
        if !ss.free_lines.is_empty() {
            metrics.sites_freed += 1;
            metrics.free_sites_total += ss.free_lines.len() as u32;
            if ss.free_lines.len() >= 2 {
                metrics.sites_multi_free += 1;
            }
        }

        // 指標第2陣の計上：Live-Range Length
        // Site に関わるイベントが存在する場合のみ生存区間をカウント
        // （初期値 last_event_line = alloc_line なので、alloc のみの場合は 1 になる）
        // 異常な facts で last_event_line < alloc_line の場合も対応（saturating_sub）
        let range = ss.last_event_line.saturating_sub(ss.alloc_line) + 1;
        metrics.live_range_lines_total += range;

        // 指標第2陣の計上：Transfer Density
        // Site ごとの遷移回数（複数回遷移の可能性も考慮）
        metrics.transfers_total += ss.transfer_count;
    }
    metrics.issues_total = issues.len() as u32;
    for is in &issues {
        let key = format!("{:?}", is.kind);
        *metrics.issues_by_kind.entry(key).or_insert(0) += 1;
    }

    // 関数の解析対象行数を記録（Transfer Density の分母）
    metrics.lines_analyzed = f.span.line_end - f.span.line_start + 1;

    metrics.finalize();

    FunctionReport {
        name: f.name.clone(),
        span: f.span,
        vars: f
            .vars
            .iter()
            .enumerate()
            .map(|(i, v)| VarReport {
                name: v.name.clone(),
                decl_line: v.decl.line_start,
                segments: std::mem::take(&mut segments[i]),
                marks: std::mem::take(&mut marks[i]),
            })
            .collect(),
        issues,
        metrics,
        graph,
        unknowns: f.unknowns.clone(),
    }
}

// ---------------------------------------------------------------------------
// テスト — facts を手組みして状態機械の判定を固定する。
// フロントエンドを介さないので「解析層だけの仕様」がここで凍結される
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の facts ビルダ。行番号だけで組み立てる
    fn func(vars: &[&str], events: Vec<(u32, u32, EventKind)>, end: u32) -> Facts {
        Facts {
            schema_version: FACTS_SCHEMA_VERSION.into(),
            file: "test.c".into(),
            source: String::new(),
            functions: vec![FunctionFacts {
                name: "f".into(),
                span: Span {
                    line_start: 1,
                    line_end: end,
                    col_start: 1,
                    col_end: 1,
                },
                vars: vars
                    .iter()
                    .enumerate()
                    .map(|(i, n)| VarDecl {
                        id: VarId(i as u32),
                        name: (*n).into(),
                        decl: Span::line(2),
                        // W6-1機械的追随: このテストビルダは pointee_const を
                        // 検証対象にしていないため None で固定する（アサーション不変）
                        pointee_const: None,
                    })
                    .collect(),
                events: events
                    .into_iter()
                    .map(|(v, l, k)| Event {
                        var: VarId(v),
                        span: Span::line(l),
                        kind: k,
                    })
                    .collect(),
                unknowns: vec![],
            }],
        }
    }

    fn heap() -> EventKind {
        EventKind::Alloc {
            source: AllocSource::Heap {
                func: "malloc".into(),
            },
        }
    }

    /// W8-2: AddressOf (借用) イベントを生成する。target がある場合は Some で、
    /// 複合式の場合は None で生成
    fn borrow_of(target: Option<&str>) -> EventKind {
        EventKind::Alloc {
            source: AllocSource::AddressOf {
                target: target.map(|s| s.to_string()),
            },
        }
    }

    /// テスト用の facts ビルダ（pointee_const 対応）。変数ごとに const 指定を指定できる
    /// vars_with_const: (var_name, pointee_const) のペア
    fn func_with_const(
        vars_with_const: &[(&str, Option<bool>)],
        events: Vec<(u32, u32, EventKind)>,
        end: u32,
    ) -> Facts {
        Facts {
            schema_version: FACTS_SCHEMA_VERSION.into(),
            file: "test.c".into(),
            source: String::new(),
            functions: vec![FunctionFacts {
                name: "f".into(),
                span: Span {
                    line_start: 1,
                    line_end: end,
                    col_start: 1,
                    col_end: 1,
                },
                vars: vars_with_const
                    .iter()
                    .enumerate()
                    .map(|(i, (n, pc))| VarDecl {
                        id: VarId(i as u32),
                        name: (*n).into(),
                        decl: Span::line(2),
                        pointee_const: *pc,
                    })
                    .collect(),
                events: events
                    .into_iter()
                    .map(|(v, l, k)| Event {
                        var: VarId(v),
                        span: Span::line(l),
                        kind: k,
                    })
                    .collect(),
                unknowns: vec![],
            }],
        }
    }

    fn kinds(r: &Report) -> Vec<IssueKind> {
        r.functions[0].issues.iter().map(|i| i.kind).collect()
    }

    #[test]
    fn clean_lifecycle_is_fully_covered() {
        // 確保→使用→解放の教科書コース。Issueゼロ・カバレッジ1.0
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    4,
                    EventKind::Use {
                        mode: UseMode::Write,
                    },
                ),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        assert!(kinds(&r).is_empty(), "issues: {:?}", r.functions[0].issues);
        assert_eq!(r.metrics.ownership_coverage, 1.0);
        assert_eq!(r.metrics.ambiguity_rate, 0.0);
        // フェーズ遷移: Owned → (free後は) Dangling…だが使用が無いので
        // 帯としては Owned 区間の後に Dangling 区間が来る
        let ph: Vec<Phase> = r.functions[0].vars[0]
            .segments
            .iter()
            .map(|s| s.phase)
            .collect();
        assert!(ph.contains(&Phase::Owned));
    }

    #[test]
    fn double_free_detected() {
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (0, 4, EventKind::Free),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        assert_eq!(kinds(&r), vec![IssueKind::DoubleFree]);
    }

    #[test]
    fn use_after_free_via_alias() {
        // q = p; free(q); *p; — Site単位追跡が効いていることの確認。
        // 別名越しの free をちゃんと「同じ資源の死」として伝播できるか
        let f = func(
            &["p", "q"],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (1, 5, EventKind::Free),
                (
                    0,
                    6,
                    EventKind::Use {
                        mode: UseMode::Read,
                    },
                ),
            ],
            7,
        );
        let r = analyze(&f);
        assert_eq!(kinds(&r), vec![IssueKind::UseAfterFree]);
        // 別名が発生した時点で曖昧マークが立つ仕様（解放責任が2重になるため）
        assert_eq!(r.metrics.sites_ambiguous, 1);
    }

    #[test]
    fn leak_suspect_at_function_end() {
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    4,
                    EventKind::Use {
                        mode: UseMode::Read,
                    },
                ),
            ],
            6,
        );
        let r = analyze(&f);
        assert_eq!(kinds(&r), vec![IssueKind::LeakSuspect]);
        assert_eq!(r.metrics.ownership_coverage, 0.0); // 追い切れていない
    }

    #[test]
    fn escape_by_return_is_not_leak() {
        let f = func(
            &["p"],
            vec![(0, 3, heap()), (0, 5, EventKind::EscapeReturn)],
            6,
        );
        let r = analyze(&f);
        assert!(kinds(&r).is_empty());
        assert_eq!(r.metrics.ownership_coverage, 1.0);
    }

    #[test]
    fn unknown_callee_marks_ambiguous_not_issue() {
        // 未知関数へ渡す→Issueにはしないが曖昧に数える。
        // 「わからないことをわからないと言う」設計の要のテスト
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    4,
                    EventKind::PassedTo {
                        callee: "mystery".into(),
                        consumed: None,
                    },
                ),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        assert!(kinds(&r).is_empty());
        assert_eq!(r.metrics.sites_ambiguous, 1);
        assert_eq!(r.metrics.ownership_coverage, 0.0); // 曖昧はresolvedに数えない
    }

    #[test]
    fn free_of_borrow_is_invalid() {
        let f = func(
            &["p"],
            vec![
                (
                    0,
                    3,
                    EventKind::Alloc {
                        // target(W8-1): この束縛遷移テストではtargetの値自体は
                        // 無関係（束縛先の変数名は問わない）なのでNoneで足りる
                        source: AllocSource::AddressOf { target: None },
                    },
                ),
                (0, 4, EventKind::Free),
            ],
            5,
        );
        let r = analyze(&f);
        assert_eq!(kinds(&r), vec![IssueKind::FreeInvalid]);
    }

    #[test]
    fn null_reset_clears_dangling() {
        // free → p=NULL → 使用。NULLチェック的使用はUAFではない（L1定義）
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (0, 4, EventKind::Free),
                (0, 5, EventKind::AssignNull),
                (
                    0,
                    6,
                    EventKind::Use {
                        mode: UseMode::Read,
                    },
                ),
            ],
            7,
        );
        let r = analyze(&f);
        assert!(kinds(&r).is_empty(), "issues: {:?}", r.functions[0].issues);
    }

    #[test]
    fn overwrite_owned_flags_leak_suspect() {
        // p = malloc(); p = malloc(); — 旧資源が迷子になる典型
        let f = func(
            &["p"],
            vec![(0, 3, heap()), (0, 4, heap()), (0, 5, EventKind::Free)],
            6,
        );
        let r = analyze(&f);
        assert!(kinds(&r).contains(&IssueKind::OverwriteOwned));
    }

    // --- 指標第2陣のテスト（ADR-0006） ---

    #[test]
    fn free_site_multiplicity_single_free() {
        // malloc→free 1回 → sites_freed=1, free_sites_total=1, multiplicity=1.0
        let f = func(&["p"], vec![(0, 3, heap()), (0, 5, EventKind::Free)], 6);
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.sites_freed, 1);
        assert_eq!(m.free_sites_total, 1);
        assert_eq!(m.sites_multi_free, 0);
        assert_eq!(m.free_site_multiplicity, 1.0);
    }

    #[test]
    fn free_site_multiplicity_double_free() {
        // double free（同じ Site を2行で解放）→ free_sites_total=2, sites_multi_free=1
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (0, 5, EventKind::Free),
                (0, 7, EventKind::Free), // 同一 Site の2度目の解放
            ],
            8,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.sites_freed, 1);
        assert_eq!(m.free_sites_total, 2, "同じ Site を2行で解放");
        assert_eq!(m.sites_multi_free, 1, "Site の解放行が複数");
        assert_eq!(m.free_site_multiplicity, 2.0);
    }

    #[test]
    fn free_site_multiplicity_no_heap_returns_ideal() {
        // ヒープなし関数 → sites_freed=0, multiplicity=1.0（分母0の値）
        let f = func(&["p"], vec![], 10);
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.sites_freed, 0);
        assert_eq!(m.sites_total, 0);
        // 分母0のとき 1.0
        assert_eq!(m.free_site_multiplicity, 1.0);
    }

    #[test]
    fn live_range_length_simple() {
        // 確保行3 → 最後のイベント行5 → 範囲 5-3+1=3
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    4,
                    EventKind::Use {
                        mode: UseMode::Write,
                    },
                ),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        // 1つの Site: alloc_line=3, last_event_line=5
        // 生存区間 = 5 - 3 + 1 = 3
        assert_eq!(m.live_range_lines_total, 3);
        assert_eq!(m.sites_total, 1);
        assert_eq!(m.live_range_avg, 3.0);
    }

    #[test]
    fn live_range_length_multiple_sites() {
        // 複数の Site の生存区間の合計
        // p: L3確保 → L5解放 (範囲 3)
        // q: L4確保 → L6解放 (範囲 3)
        let f = func(
            &["p", "q"],
            vec![
                (0, 3, heap()),
                (1, 4, heap()),
                (0, 5, EventKind::Free),
                (1, 6, EventKind::Free),
            ],
            7,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        // sites_total=2, live_range_lines_total = 3 + 3 = 6, avg = 3.0
        assert_eq!(m.sites_total, 2);
        assert_eq!(m.live_range_lines_total, 6);
        assert_eq!(m.live_range_avg, 3.0);
    }

    #[test]
    fn live_range_length_no_heap_returns_zero_avg() {
        // ヒープなし関数 → sites_total=0, live_range_avg=0.0（分母0の値）
        let f = func(&["p"], vec![], 10);
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.sites_total, 0);
        assert_eq!(m.live_range_lines_total, 0);
        // 分母0のとき 0.0
        assert_eq!(m.live_range_avg, 0.0);
    }

    #[test]
    fn transfer_density_consumed_function() {
        // 既知の消費関数への渡し → transfers_total = 1
        // 関数 span: 1-10 (10行) → density = 1 / (10 / 1000) = 1 / 0.01 = 100.0
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    5,
                    EventKind::PassedTo {
                        callee: "free".into(),
                        consumed: Some(true),
                    },
                ),
            ],
            10,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.transfers_total, 1);
        assert_eq!(m.lines_analyzed, 10);
        assert_eq!(m.transfer_density, 100.0);
    }

    #[test]
    fn transfer_density_escape_return() {
        // return による脱出 → transfers_total = 1
        // 関数 span: 1-20 (20行) → density = 1 / (20 / 1000) = 1 / 0.02 = 50.0
        let f = func(
            &["p"],
            vec![(0, 3, heap()), (0, 5, EventKind::EscapeReturn)],
            20,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.transfers_total, 1);
        assert_eq!(m.lines_analyzed, 20);
        assert_eq!(m.transfer_density, 50.0);
    }

    #[test]
    fn transfer_density_multiple_transfers() {
        // 複数の遷移がある場合 → transfers_total = 2
        // p: L3 malloc → L5 消費関数へ
        // q: L4 malloc → L6 return
        // 関数 span: 1-10 (10行) → density = 2 / (10 / 1000) = 200.0
        let f = func(
            &["p", "q"],
            vec![
                (0, 3, heap()),
                (1, 4, heap()),
                (
                    0,
                    5,
                    EventKind::PassedTo {
                        callee: "free".into(),
                        consumed: Some(true),
                    },
                ),
                (1, 6, EventKind::EscapeReturn),
            ],
            10,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.transfers_total, 2);
        assert_eq!(m.lines_analyzed, 10);
        assert_eq!(m.transfer_density, 200.0);
    }

    #[test]
    fn transfer_density_no_transfers() {
        // 移譲なし（単純な free のみ）→ transfers_total = 0, density = 0.0
        let f = func(&["p"], vec![(0, 3, heap()), (0, 5, EventKind::Free)], 10);
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.transfers_total, 0);
        assert_eq!(m.transfer_density, 0.0);
    }

    #[test]
    fn transfer_density_no_heap_returns_zero() {
        // ヒープなし関数 → transfers_total=0, lines_analyzed>0 → density=0.0
        let f = func(&["p"], vec![], 20);
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.transfers_total, 0);
        assert_eq!(m.lines_analyzed, 20);
        assert_eq!(m.transfer_density, 0.0);
    }

    #[test]
    fn metrics_aggregation_via_absorb() {
        // 複数関数の指標が absorb で正しく集計される
        // f1: 1つの Site, 1つの free 行, 生存区間 3
        let f1 = func(&["p"], vec![(0, 3, heap()), (0, 5, EventKind::Free)], 6);
        let r1 = analyze(&f1);

        // f2: 1つの Site, 1つの消費関数移譲
        let f2 = func(
            &["q"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    5,
                    EventKind::PassedTo {
                        callee: "free".into(),
                        consumed: Some(true),
                    },
                ),
            ],
            10,
        );
        let r2 = analyze(&f2);

        // 手動で absorb シミュレーション
        let mut total = r1.metrics.clone();
        total.absorb(&r2.metrics);

        // 期待値：
        // sites_freed: 1 + 1 = 2
        // free_sites_total: 1 + 1 = 2
        // sites_multi_free: 0 + 0 = 0
        // live_range_lines_total: (5-3+1) + (5-3+1) = 3 + 3 = 6
        // transfers_total: 0 + 1 = 1
        // lines_analyzed: 6 + 10 = 16
        assert_eq!(total.sites_freed, 2);
        assert_eq!(total.free_sites_total, 2);
        assert_eq!(total.sites_multi_free, 0);
        assert_eq!(total.live_range_lines_total, 6);
        assert_eq!(total.transfers_total, 1);
        assert_eq!(total.lines_analyzed, 16);
    }

    // --- P0対応: 複数回遷移テスト ---

    #[test]
    fn transfer_density_multiple_escapes_same_site() {
        // 同一 Site に EscapeReturn を2回（行を変えて）
        // → transfers_total = 2（遷移回数）
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (0, 5, EventKind::EscapeReturn), // 1回目の遷移
                (0, 7, EventKind::EscapeReturn), // 2回目の遷移（同一 Site）
            ],
            8,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.transfers_total, 2, "同一 Site への複数回遷移");
    }

    // --- P1-1対応: PassedTo 分岐での last_event_line 更新テスト ---

    #[test]
    fn live_range_with_passed_to_non_consuming() {
        // 確保 → PassedTo{Some(false)} のみ → live_range が伸びる
        // p: L3確保 → L5 非消費関数へ
        // 生存区間 = 5 - 3 + 1 = 3
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    5,
                    EventKind::PassedTo {
                        callee: "strlen".into(),
                        consumed: Some(false),
                    },
                ),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.live_range_lines_total, 3);
        assert_eq!(m.live_range_avg, 3.0);
    }

    #[test]
    fn live_range_with_passed_to_unknown() {
        // 確保 → PassedTo{None}（未知関数） → live_range が伸びる
        // p: L3確保 → L5 未知関数へ
        // 生存区間 = 5 - 3 + 1 = 3
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (
                    0,
                    5,
                    EventKind::PassedTo {
                        callee: "mystery".into(),
                        consumed: None,
                    },
                ),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.live_range_lines_total, 3);
        assert_eq!(m.live_range_avg, 3.0);
    }

    // --- P1-2対応: 異常 facts (last < alloc) での panic 防止テスト ---

    #[test]
    fn no_panic_on_inverted_line_numbers() {
        // 異常な facts: alloc_line > last_event_line の逆転をテスト。
        // L1 では通常行番号は昇順だが、L2/L3 フロントエンドまたは手組み facts で
        // 逆転が発生する可能性がある。例: alloc@L5 の後に Free@L2 というイベント列。
        // このとき last_event_line=2, alloc_line=5 → 素の減算ではオーバーフロー panic。
        // saturating_sub により安全に 1（最小値）になるべき。
        let f = func(
            &["p"],
            vec![
                (0, 5, heap()),          // alloc_line = 5
                (0, 2, EventKind::Free), // last_event_line = 2 (逆転！)
            ],
            10,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        // last_event_line.saturating_sub(alloc_line) + 1
        // = 2.saturating_sub(5) + 1 = 0 + 1 = 1
        assert_eq!(
            m.live_range_lines_total, 1,
            "逆転時も saturating_sub で 1 になる"
        );
        // panic しなかった ✓
    }

    // --- P2対応: 軽微なテスト ---

    #[test]
    fn free_same_line_counted_once() {
        // 同一行で free を2回呼ぶ → free_sites_total=1（BTreeSet の重複排除）
        let f = func(
            &["p"],
            vec![
                (0, 3, heap()),
                (0, 5, EventKind::Free),
                (0, 5, EventKind::Free), // 同じ行で2回
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.sites_freed, 1);
        assert_eq!(m.free_sites_total, 1, "同一行での複数回解放は1行と数える");
        assert_eq!(m.free_site_multiplicity, 1.0);
    }

    #[test]
    fn empty_functions_transfer_density() {
        // 空 Facts（functions: vec![]）→ metrics は全ゼロ → transfer_density=0.0
        let facts = Facts {
            schema_version: FACTS_SCHEMA_VERSION.into(),
            file: "test.c".into(),
            source: String::new(),
            functions: vec![],
        };
        let r = analyze(&facts);
        let m = &r.metrics;
        assert_eq!(m.transfers_total, 0);
        assert_eq!(m.lines_analyzed, 0);
        assert_eq!(m.transfer_density, 0.0, "分母0の場合 0.0");
    }

    // --- 指標第3陣のテスト（ADR-0009：別名圧力） ---

    #[test]
    fn aliasing_pressure_no_heap() {
        // ヒープ確保なし → aliasing_pressure_max=0, sites=0, unknown=0
        let f = func_with_const(&[("p", Some(false))], vec![], 10);
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.aliasing_pressure_max, 0, "ヒープなしなら圧力0");
        assert_eq!(m.aliasing_pressure_sites, 0);
        assert_eq!(m.aliasing_unknown_bindings, 0);
    }

    #[test]
    fn aliasing_pressure_single_ownership() {
        // p = malloc(); use(p); free(p);
        // pointee_const=Some(false)（書込可能）だが、単独所有なので圧力1
        // 圧力1は「正常」であり Site には数えない（2以上のみ）
        let f = func_with_const(
            &[("p", Some(false))],
            vec![
                (0, 3, heap()),
                (
                    0,
                    4,
                    EventKind::Use {
                        mode: UseMode::Write,
                    },
                ),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.aliasing_pressure_max, 1, "単独束縛の最大は1");
        assert_eq!(m.aliasing_pressure_sites, 0, "圧力2以上の Site はなし");
        assert_eq!(m.aliasing_unknown_bindings, 0, "すべて測定可能");
    }

    #[test]
    fn aliasing_pressure_two_writable_bindings() {
        // p = malloc(); q = p;（両方 pointee_const=Some(false)）
        // → Site Live 中に書込可能束縛2つ → max=2, sites=1
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false))],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "p と q が同時に同じ Site に書込可能束縛"
        );
        assert_eq!(m.aliasing_pressure_sites, 1, "max≥2 な Site は1個");
        assert_eq!(m.aliasing_unknown_bindings, 0);
    }

    #[test]
    fn aliasing_pressure_const_vs_writable() {
        // p = malloc(); q = p; のとき p が Some(false)（書込可能）だが
        // q が Some(true)（const）なら、書込可能束縛は p だけ → max=1, sites=0
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(true))],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 1,
            "const 束縛は書込可能数に数えない"
        );
        assert_eq!(m.aliasing_pressure_sites, 0);
        assert_eq!(m.aliasing_unknown_bindings, 0);
    }

    #[test]
    fn aliasing_pressure_unknown_binding() {
        // p = malloc(); q = p; のとき q が None（測定不能）なら
        // 書込可能は p だけなので max=1、だが q は unknown カウント
        let f = func_with_const(
            &[("p", Some(false)), ("q", None)],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (0, 5, EventKind::Free),
            ],
            6,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.aliasing_pressure_max, 1, "ここは書込可能1のみ");
        assert_eq!(m.aliasing_pressure_sites, 0);
        assert_eq!(m.aliasing_unknown_bindings, 1, "q の None 束縛を可視化");
    }

    #[test]
    fn aliasing_pressure_binding_ends_at_free() {
        // p = malloc(); q = p;（圧力2）→ free(p);（p の束縛終了）→ q はまだ生存
        // free 後に q が再度 p に関わらないなら、圧力は解放後 Site Live でなくなるので
        // 最大値に基づく計上は変わらず max=2, sites=1 のまま
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false))],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (0, 5, EventKind::Free),
                (1, 6, EventKind::AssignNull),
            ],
            7,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.aliasing_pressure_max, 2, "free 前の最大値が記録される");
        assert_eq!(m.aliasing_pressure_sites, 1);
    }

    #[test]
    fn aliasing_pressure_three_bindings() {
        // p = malloc(); q = p; r = p;（3つ全部 Some(false)）
        // → max=3, sites=1
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false)), ("r", Some(false))],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (2, 5, EventKind::AssignFromVar { src: VarId(0) }),
                (0, 6, EventKind::Free),
            ],
            7,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.aliasing_pressure_max, 3, "3つの書込可能束縛");
        assert_eq!(m.aliasing_pressure_sites, 1);
    }

    #[test]
    fn aliasing_pressure_realloc_unbinds_old_site() {
        // Alloc 再代入の unbind 漏れの回帰テスト（qa W6 レビュー P0 の再現）。
        //   p = malloc(A); q = p;   // Site A: {p,q} → max=2
        //   p = malloc(B);          // p の直接再確保。A から p が外れること
        //   r = q;                  // A に r 追加 → A は {q,r} の2
        // 真の最大は2。修正前は Alloc 分岐だけ旧 Site からの unbind を欠いて
        // いたため、stale な p が A に残り {p,q,r}=3 と過大計上されていた
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false)), ("r", Some(false))],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (0, 5, heap()),
                (2, 6, EventKind::AssignFromVar { src: VarId(1) }),
            ],
            7,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "再確保で A から外れた p を数えてはいけない（stale なら3になる）"
        );
        assert_eq!(m.aliasing_pressure_sites, 1, "圧力2以上は Site A のみ");
    }

    #[test]
    fn aliasing_pressure_addressof_realloc_unbinds_old_site() {
        // AddressOf 再代入の unbind 漏れの回帰テスト（Heap 版と同一クラス）。
        //   p = malloc(A); q = p;   // Site A: {p,q} → max=2
        //   p = &x;                 // p は Borrow へ。A から p が外れること
        //   r = q;                  // A に r 追加 → A は {q,r} の2
        // 真の最大は2。Alloc の source が AddressOf でも旧束縛は終わるので、
        // unbind を欠くと stale な p が A に残り {p,q,r}=3 と過大計上される
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false)), ("r", Some(false))],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (
                    0,
                    5,
                    EventKind::Alloc {
                        // target(W8-1): コメント通り `p = &x;` を模すのでSome("x")。
                        // この段では圧力計上はtargetを見ない（次段の仕事）ので
                        // 値そのものはunbind検証の結果に影響しない
                        source: AllocSource::AddressOf {
                            target: Some("x".to_string()),
                        },
                    },
                ),
                (2, 6, EventKind::AssignFromVar { src: VarId(1) }),
            ],
            7,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "&x 再代入で A から外れた p を数えてはいけない（stale なら3になる）"
        );
        assert_eq!(m.aliasing_pressure_sites, 1, "圧力2以上は Site A のみ");
    }

    // --- W8-2: 借用 Site の別名圧力計上テスト ---

    #[test]
    fn borrow_site_same_target_is_bundled() {
        // W8-2: int *p = &x; int *q = &x; → 同じ target は同じ借用 Site に束ねる
        // → max=2, sites=1
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false))],
            vec![(0, 3, borrow_of(Some("x"))), (1, 4, borrow_of(Some("x")))],
            5,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "同じ target &x を2つ束ねた借用 Site"
        );
        assert_eq!(m.aliasing_pressure_sites, 1, "圧力2以上の借用 Site は1個");
    }

    #[test]
    fn borrow_site_different_targets_separate() {
        // W8-2: &x と &y は異なる target → 異なる借用 Site に分ける
        // → max=1, sites=0 （圧力2未満）
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false))],
            vec![(0, 3, borrow_of(Some("x"))), (1, 4, borrow_of(Some("y")))],
            5,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 1,
            "異なる target は異なる Site なので max=1"
        );
        assert_eq!(
            m.aliasing_pressure_sites, 0,
            "どの Site も圧力1のため sites=0"
        );
    }

    #[test]
    fn borrow_site_const_target_reduces_pressure() {
        // W8-2: int *const q = &x; だと pointee_const=Some(true)
        // → q は書込可能ではない。p = &x; で max=1, sites=0
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(true))],
            vec![(0, 3, borrow_of(Some("x"))), (1, 4, borrow_of(Some("x")))],
            5,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(m.aliasing_pressure_max, 1, "p だけが書込可能なので max=1");
        assert_eq!(m.aliasing_pressure_sites, 0, "圧力1未満のため sites=0");
    }

    #[test]
    fn borrow_site_none_target_each_time_separate() {
        // W8-2: &arr[i] / &s.field のような複合式は target=None
        // → 出現ごとに新規 Site。2回出現で max=1, sites=0
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false))],
            vec![(0, 3, borrow_of(None)), (1, 4, borrow_of(None))],
            5,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 1,
            "None target は出現ごとに新規 Site なので max=1"
        );
        assert_eq!(
            m.aliasing_pressure_sites, 0,
            "どの Site も圧力1のため sites=0"
        );
    }

    #[test]
    fn borrow_site_propagate_via_assignfromvar() {
        // W8-2: p = &x; q = p; → q も &x の借用 Site に束ねられる
        // → max=2, sites=1
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false))],
            vec![
                (0, 3, borrow_of(Some("x"))),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
            ],
            5,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "q が p 経由で &x 借用 Site に伝播"
        );
        assert_eq!(m.aliasing_pressure_sites, 1, "借用 Site 1個が圧力2に達した");
    }

    #[test]
    fn borrow_site_stale_unbind_on_null() {
        // W8-2: p = &x; q = &x; (max=2, sites=1) → p = NULL; r = &x;
        // → 真の最大は2。p の unbind により、p = NULL 後の &x は r だけ
        // つまり max は2のまま（3にはならない）
        let f = func_with_const(
            &[("p", Some(false)), ("q", Some(false)), ("r", Some(false))],
            vec![
                (0, 3, borrow_of(Some("x"))),
                (1, 4, borrow_of(Some("x"))),
                (0, 5, EventKind::AssignNull),
                (2, 6, borrow_of(Some("x"))),
            ],
            7,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "null 後の新規束縛が max を増やさない（stale unbind が効く）"
        );
        assert_eq!(m.aliasing_pressure_sites, 1, "借用 Site は1個のまま");
    }

    #[test]
    fn borrow_and_heap_mixed_independence() {
        // W8-2: ヒープ Site と借用 Site が独立に計上される
        // p = malloc(); q = p; (heap site: max=2) + r = &x; s = &x; (borrow: max=2)
        // → 両 Site の全体最大 = 2
        let f = func_with_const(
            &[
                ("p", Some(false)),
                ("q", Some(false)),
                ("r", Some(false)),
                ("s", Some(false)),
            ],
            vec![
                (0, 3, heap()),
                (1, 4, EventKind::AssignFromVar { src: VarId(0) }),
                (2, 5, borrow_of(Some("x"))),
                (3, 6, borrow_of(Some("x"))),
                (0, 7, EventKind::Free),
            ],
            8,
        );
        let r = analyze(&f);
        let m = &r.functions[0].metrics;
        assert_eq!(
            m.aliasing_pressure_max, 2,
            "ヒープ max=2, 借用 max=2 → 全体 max=2"
        );
        assert_eq!(
            m.aliasing_pressure_sites, 2,
            "圧力2以上の Site は heap 1個 + borrow 1個 = 2個"
        );
    }
}
