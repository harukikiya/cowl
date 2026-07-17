//! # cowl-front-ts — tree-sitter による L1 フロントエンド
//!
//! 役割はただひとつ: **Cソース → facts（構文事実）** の変換。
//! 所有権の解釈・診断・指標計算は一切しない（それは cowl-core::analysis の仕事）。
//!
//! ## なぜ tree-sitter か（L1 の位置づけ）
//! - 依存が軽い（libclang不要）ので、devcontainer外・CI・WASM化まで見通せる
//! - エラー耐性が高く、コンパイルできない断片でも「見えた範囲」を返せる
//! - ただし **型情報・マクロ展開・制御フローを持たない**。これがL1の限界で、
//!   同シグネチャの facts を返す libclang 版（L2, cowl-front-clang 予定）に
//!   差し替えることで精度を上げる設計（factsが差し替えの継ぎ目）
//!
//! ## L1 の割り切り一覧（ワーカーはここを「バグ」として直さないこと）
//! - スコープ非対応: 同名変数の再宣言は両方とも追跡対象外にし、unknowns に記録
//! - 制御フロー非対応: イベントはソース出現順。if/loop の分岐は考慮しない
//! - マクロ非展開: `MY_ALLOC(p)` などは未知関数呼び出しとして「曖昧」に落ちる
//! - ポインタ演算 `p+1` は単なる使用(Use)として扱う（別名の派生は追わない）
//! - `&p`（ポインタ自身のアドレス取得）は追跡外とし、unknowns に記録
//!
//! これらは「検出漏れ」ではなく「わからないことを曖昧・unknownsとして
//! 明示する」というプロジェクト方針（facts firewall）の実装である。

use anyhow::{anyhow, Context, Result};
use cowl_core::facts::*;
use std::collections::HashMap;
use tree_sitter::{Node, Parser};

// ---------------------------------------------------------------------------
// 既知関数テーブル
// ここに関数名を足すだけで解析の解像度が上がる（ワーカー向けの安全な拡張点）。
// 追加時は「本当に常にそう振る舞うか」を manpage で確認し、テストを1本足すこと
// ---------------------------------------------------------------------------

/// ヒープ確保として扱う関数（戻り値が新しい所有権になる）
const ALLOC_FNS: &[&str] = &[
    "malloc",
    "calloc",
    "realloc",
    "strdup",
    "strndup",
    "aligned_alloc",
];

/// ポインタ引数の所有権を「消費」する関数（渡した側の解放責任が消える）。
/// free は Free イベントとして特別扱いするのでこの表には含めない。
/// realloc は第1引数を消費する（成功時。失敗時は残るがL1では追わない＝既知の妥協）
const CONSUMER_FNS: &[&str] = &["fclose", "realloc"];

/// ポインタを借りるだけで消費しないことが自明な標準関数。
/// ここに載っていない未知関数は consumed=None（曖昧）になる
const BENIGN_FNS: &[&str] = &[
    "printf", "fprintf", "snprintf", "sprintf", "puts", "fputs", "putchar", "perror", "strlen",
    "strcpy", "strncpy", "strcat", "strncat", "strcmp", "strncmp", "strchr", "strstr", "memcpy",
    "memmove", "memset", "memcmp", "fwrite", "fread", "fgets", "sscanf",
];

// ---------------------------------------------------------------------------
// 公開API
// ---------------------------------------------------------------------------

/// ファイルパスから facts を抽出する
pub fn extract_file(path: &str) -> Result<Facts> {
    let source = std::fs::read_to_string(path).with_context(|| format!("読み込み失敗: {path}"))?;
    extract_source(&source, path)
}

/// ソース文字列から facts を抽出する（テスト・API経由の入力用）
pub fn extract_source(source: &str, file_name: &str) -> Result<Facts> {
    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_c::LANGUAGE.into())
        .context("tree-sitter-c 文法のロードに失敗")?;
    let tree = parser
        .parse(source, None)
        .ok_or_else(|| anyhow!("パース失敗（tree-sitterがNoneを返した）"))?;

    // 関数定義を全部拾う（#if 内などネストしていてもDFSで到達する）
    let mut fn_nodes = Vec::new();
    collect_kind(tree.root_node(), "function_definition", &mut fn_nodes);

    let mut functions = Vec::new();
    for f in fn_nodes {
        if let Some(ff) = extract_function(f, source) {
            functions.push(ff);
        }
    }

    Ok(Facts {
        schema_version: FACTS_SCHEMA_VERSION.to_string(),
        file: file_name.to_string(),
        source: source.to_string(),
        functions,
    })
}

// ---------------------------------------------------------------------------
// 関数単位の抽出
// ---------------------------------------------------------------------------

/// 宣言収集の中間表現（イベント化は名前→ID表が完成した後に行うため一旦貯める）
struct PendingDecl<'t> {
    name: String,
    name_span: Span,
    byte: usize,
    /// 初期化式（あれば）。`char *p = malloc(4);` の右辺
    init: Option<Node<'t>>,
    /// 関数引数由来か（引数は「呼び出し元由来の不透明な値」として扱う）
    is_param: bool,
}

fn extract_function(fn_node: Node, src: &str) -> Option<FunctionFacts> {
    // --- 関数名の特定 -----------------------------------------------------
    // `char *f(void)` のように戻り値ポインタだと declarator は
    // pointer_declarator( function_declarator( identifier ) ) と入れ子になる。
    // そこで declarator 部分木から最初の function_declarator を探し、
    // その declarator フィールド内の最初の identifier を関数名とする
    let decl = fn_node.child_by_field_name("declarator")?;
    let fdecl = find_first_kind(decl, "function_declarator")?;
    let name_node = find_first_kind(fdecl.child_by_field_name("declarator")?, "identifier")?;
    let fn_name = text(name_node, src).to_string();
    let body = fn_node.child_by_field_name("body")?;

    // --- Pass A: 追跡対象ポインタ変数の収集 --------------------------------
    let mut pendings: Vec<PendingDecl> = Vec::new();

    // (A-1) 引数のポインタ。所有権は呼び出し規約に依存するため、
    //       宣言と同時に AssignOpaque を発行して「追跡不能な値を持つ」状態から始める
    if let Some(params) = fdecl.child_by_field_name("parameters") {
        for i in 0..params.named_child_count() {
            let p = params.named_child(i)?;
            if p.kind() != "parameter_declaration" {
                continue;
            }
            let Some(pd) = p.child_by_field_name("declarator") else {
                continue;
            };
            if find_first_kind(pd, "pointer_declarator").is_none() {
                continue; // ポインタでない引数は対象外
            }
            let Some(id) = find_first_kind(pd, "identifier") else {
                continue;
            };
            pendings.push(PendingDecl {
                name: text(id, src).to_string(),
                name_span: span_of(id),
                byte: p.start_byte(),
                init: None,
                is_param: true,
            });
        }
    }

    // (A-2) 関数本体内のローカル宣言。
    //       `char *p = malloc(4), *q;` は init_declarator と bare な
    //       pointer_declarator が declaration の子として並ぶ
    let mut decl_nodes = Vec::new();
    collect_kind(body, "declaration", &mut decl_nodes);
    for d in decl_nodes {
        for i in 0..d.named_child_count() {
            let Some(c) = d.named_child(i) else { continue };
            match c.kind() {
                "init_declarator" => {
                    let Some(dd) = c.child_by_field_name("declarator") else {
                        continue;
                    };
                    if dd.kind() != "pointer_declarator" {
                        continue; // `int x = ...` などポインタでない宣言
                    }
                    let Some(id) = find_first_kind(dd, "identifier") else {
                        continue;
                    };
                    pendings.push(PendingDecl {
                        name: text(id, src).to_string(),
                        name_span: span_of(id),
                        byte: c.start_byte(),
                        init: c.child_by_field_name("value"),
                        is_param: false,
                    });
                }
                "pointer_declarator" => {
                    // 初期化なし宣言 `char *q;`
                    let Some(id) = find_first_kind(c, "identifier") else {
                        continue;
                    };
                    pendings.push(PendingDecl {
                        name: text(id, src).to_string(),
                        name_span: span_of(id),
                        byte: c.start_byte(),
                        init: None,
                        is_param: false,
                    });
                }
                _ => {}
            }
        }
    }

    let mut unknowns: Vec<Unknown> = Vec::new();

    // (A-3) 同名の再宣言はスコープ非対応のL1では区別できないため、
    //       その名前ごと追跡を諦めて unknowns に自己申告する。
    //       黙って片方に紐づけると偽の帯・偽の診断を生むので「諦める」が正解
    let mut counts: HashMap<&str, u32> = HashMap::new();
    for p in &pendings {
        *counts.entry(p.name.as_str()).or_insert(0) += 1;
    }
    let banned: Vec<String> = counts
        .iter()
        .filter(|(_, &c)| c > 1)
        .map(|(n, _)| n.to_string())
        .collect();
    for b in &banned {
        if let Some(p) = pendings.iter().find(|p| &p.name == b) {
            unknowns.push(Unknown {
                span: p.name_span,
                reason: format!(
                    "同名変数 `{}` の再宣言（L1はスコープ未対応のため追跡対象外）",
                    b
                ),
            });
        }
    }
    pendings.retain(|p| !banned.contains(&p.name));
    // VarId は「出現バイト順の連番」— analysis 側の debug_assert と合わせた契約
    pendings.sort_by_key(|p| p.byte);

    let vars: Vec<VarDecl> = pendings
        .iter()
        .enumerate()
        .map(|(i, p)| VarDecl {
            id: VarId(i as u32),
            name: p.name.clone(),
            decl: p.name_span,
        })
        .collect();
    let name_to_id: HashMap<String, VarId> = vars.iter().map(|v| (v.name.clone(), v.id)).collect();

    // --- イベント生成 -------------------------------------------------------
    // (byte位置, Event) で貯めて最後にソート。フロントエンドの契約
    // 「events はソース出現順」をここで保証する
    let mut evs: Vec<(usize, Event)> = Vec::new();

    // (B-0) 宣言由来のイベント（引数の不透明値・初期化式）
    for p in &pendings {
        let vid = name_to_id[&p.name];
        if p.is_param {
            evs.push((
                p.byte,
                Event {
                    var: vid,
                    span: p.name_span,
                    kind: EventKind::AssignOpaque {
                        detail: "関数引数（呼び出し元由来。所有権は呼び出し規約に依存）".into(),
                    },
                },
            ));
        } else if let Some(init) = p.init {
            let kind = interp_rhs(init, src, &name_to_id);
            evs.push((
                init.start_byte(),
                Event {
                    var: vid,
                    span: span_of(init),
                    kind,
                },
            ));
        }
    }

    // (B-1) 本体中の識別子出現を分類してイベント化
    let mut ids = Vec::new();
    collect_kind(body, "identifier", &mut ids);
    for id in ids {
        let name = text(id, src);
        let Some(&vid) = name_to_id.get(name) else {
            continue;
        };
        match classify_occurrence(id, src, &name_to_id) {
            Occ::Skip => {}
            Occ::Event(kind) => {
                evs.push((
                    id.start_byte(),
                    Event {
                        var: vid,
                        span: span_of(id),
                        kind,
                    },
                ));
            }
            Occ::EventWithUnknown(kind, reason) => {
                evs.push((
                    id.start_byte(),
                    Event {
                        var: vid,
                        span: span_of(id),
                        kind,
                    },
                ));
                unknowns.push(Unknown {
                    span: span_of(id),
                    reason,
                });
            }
        }
    }

    // (B-2) goto があると「出現順＝実行順」の前提が崩れるので信頼度低下を申告
    let mut gotos = Vec::new();
    collect_kind(body, "goto_statement", &mut gotos);
    if let Some(g) = gotos.first() {
        unknowns.push(Unknown {
            span: span_of(*g),
            reason: "goto による非構造フローあり（線形順序前提のL1解析は信頼度低下）".into(),
        });
    }

    evs.sort_by_key(|(b, _)| *b);

    Some(FunctionFacts {
        name: fn_name,
        span: span_of(fn_node),
        vars,
        events: evs.into_iter().map(|(_, e)| e).collect(),
        unknowns,
    })
}

// ---------------------------------------------------------------------------
// 識別子出現の分類 — L1の心臓部
// ---------------------------------------------------------------------------

/// 分類結果
enum Occ {
    /// イベント化しない（宣言名の位置・Pass Aで処理済みの初期化RHSなど）
    Skip,
    Event(EventKind),
    /// イベント化しつつ unknowns にも記録（`&p` など）
    EventWithUnknown(EventKind, String),
}

/// 追跡変数の識別子 1 出現を、**最も近い意味のある祖先**に基づいて分類する。
///
/// 方針: id から親方向へ登りながら、最初にマッチした文脈で確定する。
/// 「近い祖先ほど具体的な意味を持つ」ため、この順序で
/// `free(p)` は Free、`free(p->next)` は（p にとっては）Use になる、
/// といった自然な優先度が得られる。
fn classify_occurrence(id: Node, src: &str, tracked: &HashMap<String, VarId>) -> Occ {
    let mut cur = id;
    loop {
        let Some(par) = cur.parent() else {
            return Occ::Event(EventKind::Use {
                mode: UseMode::Read,
            });
        };
        match par.kind() {
            // --- 宣言の名前位置。イベントではない ---------------------------
            "pointer_declarator"
            | "array_declarator"
            | "function_declarator"
            | "parameter_declaration"
            | "declaration" => return Occ::Skip,

            "init_declarator" => {
                // value 側で、かつ（cast/parenを剥いた）右辺そのものが id なら
                // Pass A が AssignFromVar として処理済み → 二重計上を防ぐ
                if in_field(par, "value", id) {
                    if let Some(v) = par.child_by_field_name("value") {
                        if strip(v).id() == id.id() {
                            return Occ::Skip;
                        }
                    }
                    // ここに来るのは理論上ラッパ越しのみで strip 済みのはずだが、
                    // 想定外でも安全側（Skip）に倒す：偽イベントより取りこぼし
                    return Occ::Skip;
                }
                return Occ::Skip; // declarator 側（名前位置）
            }

            // --- 代入 --------------------------------------------------------
            "assignment_expression" => {
                let left = par.child_by_field_name("left");
                let right = par.child_by_field_name("right");
                let (Some(left), Some(right)) = (left, right) else {
                    return Occ::Event(EventKind::Use {
                        mode: UseMode::Read,
                    });
                };
                if contains(left, id) {
                    if strip(left).id() == id.id() {
                        // `p = <RHS>` : ポインタ変数そのものへの代入。
                        // 複合代入 `p += 1` は再束縛ではないので Use 扱いに落とす
                        if op_text(par, src) != "=" {
                            return Occ::Event(EventKind::Use {
                                mode: UseMode::Read,
                            });
                        }
                        return Occ::Event(interp_rhs(right, src, tracked));
                    }
                    // `*p = ...` / `p->x = ...` / `p[i] = ...` : 指す先への書き込み
                    return Occ::Event(EventKind::Use {
                        mode: UseMode::Write,
                    });
                }
                if contains(right, id) && strip(right).id() == id.id() {
                    // 右辺そのものが id。左辺が追跡変数なら AssignFromVar を
                    // 左辺側の出現が発行するのでここは Skip（二重計上防止）
                    let sl = strip(left);
                    if sl.kind() == "identifier" && tracked.contains_key(text(sl, src)) {
                        return Occ::Skip;
                    }
                    // 追跡外への格納 = 脱出。グローバル・構造体・配列要素など
                    let target = truncate(text(left, src), 40);
                    return Occ::Event(EventKind::EscapeStore { target });
                }
                // 右辺の内部で使われている（`x = p->len` 等）→ より近い祖先で
                // 分類済みのはずだが、届いたら使用として扱う
                return Occ::Event(EventKind::Use {
                    mode: UseMode::Read,
                });
            }

            // --- 参照・演算 ----------------------------------------------------
            "pointer_expression" => {
                // `*p`（デリファレンス）か `&p`（アドレス取得）かで意味が全く違う
                match op_text(par, src).as_str() {
                    "*" => {
                        let mode = if is_assign_lhs(par) {
                            UseMode::Write
                        } else {
                            UseMode::Read
                        };
                        return Occ::Event(EventKind::Use { mode });
                    }
                    "&" => {
                        // ポインタ変数自身のアドレス。`f(&p)` は呼び先が p を
                        // 書き換えうるが L1 では追えない → 使用扱い＋自己申告
                        return Occ::EventWithUnknown(
                            EventKind::Use { mode: UseMode::Read },
                            format!(
                                "`&{}`（ポインタ自身のアドレス取得）は追跡外。呼び先で書き換えられる可能性",
                                text(id, src)
                            ),
                        );
                    }
                    _ => {
                        return Occ::Event(EventKind::Use {
                            mode: UseMode::Read,
                        })
                    }
                }
            }
            "field_expression" | "subscript_expression" => {
                // `p->x` `p[i]` はデリファレンスを伴う使用
                let mode = if is_assign_lhs(par) {
                    UseMode::Write
                } else {
                    UseMode::Read
                };
                return Occ::Event(EventKind::Use { mode });
            }

            // --- 関数呼び出し --------------------------------------------------
            "argument_list" => {
                let Some(call) = par.parent() else {
                    return Occ::Event(EventKind::Use {
                        mode: UseMode::Read,
                    });
                };
                if call.kind() != "call_expression" {
                    return Occ::Event(EventKind::Use {
                        mode: UseMode::Read,
                    });
                }
                let callee = callee_name(call, src);
                if callee == "free" {
                    return Occ::Event(EventKind::Free);
                }
                let consumed = if CONSUMER_FNS.contains(&callee.as_str()) {
                    Some(true)
                } else if BENIGN_FNS.contains(&callee.as_str()) {
                    Some(false)
                } else {
                    None // 未知関数 = 曖昧。analysis が曖昧度に計上する
                };
                return Occ::Event(EventKind::PassedTo { callee, consumed });
            }
            "call_expression" => {
                if in_field(par, "function", id) {
                    // 変数名と同名の関数呼び出し。追跡変数の使用ではない
                    return Occ::Skip;
                }
                cur = par;
            }

            "return_statement" => return Occ::Event(EventKind::EscapeReturn),

            // --- 透過ラッパは1段登って判定し直す --------------------------------
            "parenthesized_expression" | "cast_expression" => cur = par,

            // --- 「使用」で確定する文脈 -----------------------------------------
            // 条件式・算術・単独文など。ポインタ演算 `p+1` もここに落ちる
            "binary_expression"
            | "unary_expression"
            | "update_expression"
            | "conditional_expression"
            | "comma_expression"
            | "sizeof_expression"
            | "expression_statement"
            | "if_statement"
            | "while_statement"
            | "do_statement"
            | "for_statement"
            | "switch_statement"
            | "case_statement" => {
                return Occ::Event(EventKind::Use {
                    mode: UseMode::Read,
                })
            }

            // --- 未知のノード種：安全側＝1段登って再判定 -------------------------
            // （最終的に文レベルの kind で Use に落ちるか、根で Use になる）
            _ => cur = par,
        }
    }
}

/// 初期化式・代入右辺の解釈。
/// **ここで返すのは構文事実であって所有権判断ではない**点に注意
/// （AssignFromVar がムーブか借用かは analysis が決める）
fn interp_rhs(rhs: Node, src: &str, tracked: &HashMap<String, VarId>) -> EventKind {
    let v = strip(rhs);
    match v.kind() {
        "call_expression" => {
            let callee = callee_name(v, src);
            if ALLOC_FNS.contains(&callee.as_str()) {
                EventKind::Alloc {
                    source: AllocSource::Heap { func: callee },
                }
            } else {
                // 未知関数の戻り値。所有権が来たのか借用なのか判断不能
                EventKind::AssignOpaque {
                    detail: format!("{}() の戻り値（所有権不明）", callee),
                }
            }
        }
        "pointer_expression" if op_text(v, src) == "&" => EventKind::Alloc {
            source: AllocSource::AddressOf,
        },
        // tree-sitter-c は NULL/nullptr を `null` ノードにする。
        // 環境差に備えて identifier "NULL" とリテラル 0 も拾う
        "null" => EventKind::AssignNull,
        "number_literal" if text(v, src) == "0" => EventKind::AssignNull,
        "identifier" => {
            let t = text(v, src);
            if t == "NULL" {
                EventKind::AssignNull
            } else if tracked.contains_key(t) {
                EventKind::AssignFromVar { src: tracked[t] }
            } else {
                EventKind::AssignOpaque {
                    detail: format!("追跡外の変数 `{}`", t),
                }
            }
        }
        _ => EventKind::AssignOpaque {
            detail: truncate(text(v, src), 40),
        },
    }
}

// ---------------------------------------------------------------------------
// 小さな構文ユーティリティ
// ---------------------------------------------------------------------------

/// 部分木を先行順DFSで走査し、指定 kind のノードを出現順に集める
fn collect_kind<'t>(node: Node<'t>, kind: &str, out: &mut Vec<Node<'t>>) {
    if node.kind() == kind {
        out.push(node);
    }
    for i in 0..node.child_count() {
        if let Some(c) = node.child(i) {
            collect_kind(c, kind, out);
        }
    }
}

/// 部分木から最初に見つかった kind のノードを返す
fn find_first_kind<'t>(node: Node<'t>, kind: &str) -> Option<Node<'t>> {
    if node.kind() == kind {
        return Some(node);
    }
    for i in 0..node.child_count() {
        if let Some(hit) = node.child(i).and_then(|c| find_first_kind(c, kind)) {
            return Some(hit);
        }
    }
    None
}

/// `( expr )` と `(T*)expr` を透過して中身に到達する。
/// 「キャストや括弧は所有権の意味を変えない」という判断をコードにしたもの
fn strip(node: Node) -> Node {
    let mut n = node;
    loop {
        match n.kind() {
            "parenthesized_expression" => {
                if let Some(c) = n.named_child(0) {
                    n = c;
                    continue;
                }
                return n;
            }
            "cast_expression" => {
                if let Some(v) = n.child_by_field_name("value") {
                    n = v;
                    continue;
                }
                return n;
            }
            _ => return n,
        }
    }
}

/// node が ancestor の指定フィールド部分木に含まれるか（バイト範囲で判定）
fn in_field(ancestor: Node, field: &str, node: Node) -> bool {
    ancestor
        .child_by_field_name(field)
        .map(|f| f.start_byte() <= node.start_byte() && node.end_byte() <= f.end_byte())
        .unwrap_or(false)
}

/// container の部分木に node が含まれるか
fn contains(container: Node, node: Node) -> bool {
    container.start_byte() <= node.start_byte() && node.end_byte() <= container.end_byte()
}

/// この式（デリファレンス等）が代入の左辺側に位置するか。
/// `*p = x` の p を Write と分類するためだけの局所判定
fn is_assign_lhs(node: Node) -> bool {
    let mut c = node;
    while let Some(p) = c.parent() {
        match p.kind() {
            "parenthesized_expression"
            | "cast_expression"
            | "pointer_expression"
            | "field_expression"
            | "subscript_expression" => c = p,
            "assignment_expression" => return in_field(p, "left", c),
            _ => return false,
        }
    }
    false
}

/// call_expression の呼び先名。`obj->fn(...)` のような間接呼び出しは
/// 式テキスト全体を返す（テーブル不一致 → 未知関数として曖昧に落ちる）
fn callee_name(call: Node, src: &str) -> String {
    match call.child_by_field_name("function") {
        Some(f) if f.kind() == "identifier" => text(f, src).to_string(),
        Some(f) => truncate(text(f, src), 40),
        None => "(不明)".into(),
    }
}

/// assignment_expression / pointer_expression の演算子テキスト
fn op_text(node: Node, src: &str) -> String {
    node.child_by_field_name("operator")
        .map(|o| text(o, src).to_string())
        .unwrap_or_default()
}

fn text<'s>(node: Node, src: &'s str) -> &'s str {
    node.utf8_text(src.as_bytes()).unwrap_or("")
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

/// tree-sitter の 0 始まり Point を、facts の 1 始まり Span へ変換
fn span_of(node: Node) -> Span {
    let s = node.start_position();
    let e = node.end_position();
    Span {
        line_start: s.row as u32 + 1,
        line_end: e.row as u32 + 1,
        col_start: s.column as u32 + 1,
        col_end: e.column as u32 + 1,
    }
}

// ---------------------------------------------------------------------------
// テスト — 「このC構文からこのイベント列が出る」を凍結するゴールデン群。
// 解析層のテストと合わせて、フロントエンド差し替え(L2)時の互換性検証にも使う
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    /// 1関数目のイベント種別だけを取り出す補助
    fn kinds_of(src: &str) -> Vec<EventKind> {
        let f = extract_source(src, "t.c").unwrap();
        f.functions[0]
            .events
            .iter()
            .map(|e| e.kind.clone())
            .collect()
    }

    #[test]
    fn basic_malloc_use_free() {
        let ks = kinds_of(
            r#"
#include <stdlib.h>
void f(void) {
    char *p = malloc(4);
    *p = 'a';
    free(p);
}
"#,
        );
        assert_eq!(
            ks,
            vec![
                EventKind::Alloc {
                    source: AllocSource::Heap {
                        func: "malloc".into()
                    }
                },
                EventKind::Use {
                    mode: UseMode::Write
                },
                EventKind::Free,
            ]
        );
    }

    #[test]
    fn alias_then_free_then_deref() {
        // q = p; free(q); *p; の3点セット。AssignFromVar の src 解決も確認
        let f = extract_source(
            r#"
void f(void) {
    char *p = malloc(4);
    char *q = p;
    free(q);
    char c = *p;
}
"#,
            "t.c",
        )
        .unwrap();
        let ff = &f.functions[0];
        assert_eq!(ff.vars.len(), 2);
        let ks: Vec<_> = ff.events.iter().map(|e| (e.var, e.kind.clone())).collect();
        assert_eq!(ks[0].0, VarId(0));
        assert_eq!(
            ks[1],
            (VarId(1), EventKind::AssignFromVar { src: VarId(0) })
        );
        assert_eq!(ks[2], (VarId(1), EventKind::Free));
        assert_eq!(
            ks[3],
            (
                VarId(0),
                EventKind::Use {
                    mode: UseMode::Read
                }
            )
        );
    }

    #[test]
    fn return_is_escape() {
        let ks = kinds_of("char *f(void) { char *p = malloc(4); return p; }");
        assert_eq!(ks[1], EventKind::EscapeReturn);
    }

    #[test]
    fn unknown_callee_is_ambiguous() {
        let ks = kinds_of("void f(void) { char *p = malloc(4); mystery(p); }");
        assert_eq!(
            ks[1],
            EventKind::PassedTo {
                callee: "mystery".into(),
                consumed: None
            }
        );
    }

    #[test]
    fn benign_callee_is_nonconsuming_use() {
        let ks = kinds_of(r#"void f(void) { char *p = malloc(4); printf("%s", p); free(p); }"#);
        assert_eq!(
            ks[1],
            EventKind::PassedTo {
                callee: "printf".into(),
                consumed: Some(false)
            }
        );
        assert_eq!(ks[2], EventKind::Free);
    }

    #[test]
    fn null_and_address_of_inits() {
        let src = r#"
void f(void) {
    int x = 0;
    int *a = NULL;
    int *b = &x;
}
"#;
        let ks = kinds_of(src);
        assert_eq!(ks[0], EventKind::AssignNull);
        assert_eq!(
            ks[1],
            EventKind::Alloc {
                source: AllocSource::AddressOf
            }
        );
    }

    #[test]
    fn param_pointer_starts_opaque() {
        let f = extract_source("void f(char *p) { free(p); }", "t.c").unwrap();
        let ff = &f.functions[0];
        assert_eq!(ff.vars[0].name, "p");
        assert!(matches!(ff.events[0].kind, EventKind::AssignOpaque { .. }));
        assert_eq!(ff.events[1].kind, EventKind::Free);
    }

    #[test]
    fn store_to_untracked_is_escape() {
        let ks = kinds_of("char *g; void f(void) { char *p = malloc(4); g = p; }");
        assert!(matches!(ks[1], EventKind::EscapeStore { .. }));
    }

    #[test]
    fn duplicate_name_is_banned_with_unknown() {
        // 同名再宣言 → 追跡0本・unknowns 1件以上
        let f = extract_source(
            "void f(void) { { char *p = malloc(4); } { char *p = malloc(8); } }",
            "t.c",
        )
        .unwrap();
        let ff = &f.functions[0];
        assert!(ff.vars.is_empty());
        assert!(!ff.unknowns.is_empty());
    }

    #[test]
    fn cast_is_transparent() {
        let ks = kinds_of("void f(void) { char *p = (char *)malloc(4); free((void *)p); }");
        assert_eq!(
            ks[0],
            EventKind::Alloc {
                source: AllocSource::Heap {
                    func: "malloc".into()
                }
            }
        );
        assert_eq!(ks[1], EventKind::Free);
    }
}
