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

/// ヒープ確保として扱う関数（戻り値が新しい所有権になる）。
///
/// fopen/fdopen/freopen/tmpfile/popen/opendir は本来 FILE*/DIR* であり
/// malloc 系のヒープメモリとは別物だが、facts のスキーマは不変が
/// W3の決定事項（AllocSource に専用 variant を足すのはL2以降の課題）なので
/// そのまま AllocSource::Heap{func} を流用する。
/// これに伴い「確保関数と解放関数の対応が正しいか」（例: fopen したものを
/// fclose ではなく free してしまう誤り）の検証はL1のスコープ外とする。
/// L1は「解放系の呼び出しがあったか」だけを見て対応関係の妥当性は問わない
const ALLOC_FNS: &[&str] = &[
    "malloc",
    "calloc",
    "realloc",
    "strdup",
    "strndup",
    "aligned_alloc",
    "fopen",
    "fdopen",
    "freopen",
    "tmpfile",
    "popen",
    "opendir",
];

/// ポインタ引数の所有権を「消費」する関数の (関数名, 消費する引数位置=0始まり)。
/// free は Free イベントとして特別扱いするのでこの表には含めない。
///
/// 表に載っている関数は manpage で**全引数**の意味論を確認済みという意味であり、
/// 消費位置**以外**の引数は曖昧(None)にせず consumed:Some(false)（非消費と断定）
/// にしてよい。realloc/freopen は「引数を消費しつつ戻り値で新しい所有権を返す」
/// 関数なので ALLOC_FNS にも載っている（`p = realloc(p, n)` のような自己代入は
/// 「右辺で旧pを消費→左辺で新pを獲得」という順序になる。イベント順の扱いは
/// classify_occurrence の assignment_expression ケースのコメント参照）
const CONSUMER_FNS: &[(&str, usize)] = &[
    ("fclose", 0),
    ("realloc", 0),
    ("reallocarray", 0),
    ("pclose", 0),
    ("closedir", 0),
    ("freopen", 2),
];

/// ポインタを借りるだけで消費しないことが自明な標準関数。
/// ここに載っていない未知関数は consumed=None（曖昧）になる。
///
/// strdup/strndup/fopen/fdopen/popen/opendir は ALLOC_FNS にも載っているが
/// 矛盾ではない：ALLOC_FNS は「その関数の**戻り値**を受け取ったとき」
/// （interp_rhs が参照）、BENIGN_FNS は「その関数へ**引数として**渡したとき」
/// （argument_list ケースが参照）と、参照される文脈が違う。
/// 例えば `q = strdup(p)` は q への Alloc（新しい所有権）であると同時に、
/// p は借用のまま（strdup は p の指す内容をコピーするだけで p 自体は
/// 消費しない）— これが現状 p を曖昧扱いにしていた穴を塞ぐ。
/// freopen は CONSUMER_FNS（第3引数=stream を消費）に載せたのでここには含めない
const BENIGN_FNS: &[&str] = &[
    "printf",
    "fprintf",
    "snprintf",
    "sprintf",
    "puts",
    "fputs",
    "putchar",
    "perror",
    "strlen",
    "strcpy",
    "strncpy",
    "strcat",
    "strncat",
    "strcmp",
    "strncmp",
    "strchr",
    "strstr",
    "memcpy",
    "memmove",
    "memset",
    "memcmp",
    "fwrite",
    "fread",
    "fgets",
    "sscanf",
    "strdup",
    "strndup",
    "fopen",
    "fdopen",
    "popen",
    "opendir",
    "strrchr",
    "memchr",
    "strcasecmp",
    "strncasecmp",
    "strtol",
    "strtod",
    "atoi",
];

/// callee の呼び出しに対して、引数位置 arg_pos に渡した追跡ポインタが
/// 「消費されるか」を既知関数テーブルから判定する。
/// 判定順（呼び出し元で free は先に弾いている前提）:
///   CONSUMER表にあり位置一致→Some(true) / 位置不一致→Some(false) /
///   BENIGN表にあり→Some(false) / どちらにも無い未知関数→None（曖昧）
fn consumed_for(callee: &str, arg_pos: Option<usize>) -> Option<bool> {
    if let Some(&(_, consumer_pos)) = CONSUMER_FNS.iter().find(|(name, _)| *name == callee) {
        // CONSUMER表にあり位置一致→消費、位置不一致→非消費と断定
        // （表に載っている＝manpageで全引数の意味論を確認済みのため）。
        // 位置を特定できない(None)のは理論上到達しない防御的分岐だが、
        // 起きたら「わからない」を尊重して曖昧側に倒す
        arg_pos.map(|p| p == consumer_pos)
    } else if BENIGN_FNS.contains(&callee) {
        Some(false)
    } else {
        None // 未知関数 = 曖昧。analysis が曖昧度に計上する
    }
}

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
            // ソートキーは init の開始ではなく**終端**の byte。
            // `char *q = realloc(p, 8);` のように初期化式の中で他の追跡変数
            // (p) を消費する呼び出しがある場合、実行順は
            // 「右辺評価（pの消費）→ qへの束縛（獲得）」。開始byteで揃えると
            // 獲得(Alloc)が消費(PassedTo)より前に並んでしまい、偽の
            // overwrite_owned/leak_suspect を生む（詳細はB-1側の同種の
            // コメント参照）。span（表示位置）は従来どおり init のまま
            evs.push((
                init.end_byte(),
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
            Occ::EventAt(kind, sort_byte) => {
                // ソートキーだけ呼び出し元(classify_occurrence)指定の位置に
                // 差し替える。表示位置(span)は出現位置(id)のまま揃える
                evs.push((
                    sort_byte,
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
    /// イベント化するが、ソートキー（実行順の代理指標）を出現位置(id.start_byte())
    /// ではなく指定の byte 位置に上書きする。
    /// 代入 `p = <RHS>` の左辺出現がこれに該当する：構文上は左辺が右辺より
    /// 前に出現するが、実行順は「右辺評価→代入」なので、そのまま出現順で
    /// ソートすると `p = realloc(p, 8)` のような自己代入で
    /// 獲得(Alloc)が消費(PassedTo)より前に並んでしまう（呼び出し元のコメント参照）。
    ///
    /// 【拡張時の警告】この機構は現在**同一文内**の並べ替えにのみ使っている。
    /// 文や分岐をまたぐ順序調整に使いたくなったら、それは規約2
    /// （制御フロー非考慮=L1の割り切り）への抵触なので、L2 のタスクとして
    /// 起票する（レビューで必ず確認すること）
    EventAt(EventKind, usize),
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
                        // 実行意味論は「右辺を評価（旧資源の消費）→ 結果を代入
                        // （新資源の獲得）」の順。だがこのイベントは左辺 id の
                        // 出現位置で処理しており、左辺は右辺よりバイト位置が
                        // 前にある。ソートキーをそのまま id 側に取ると
                        // `p = realloc(p, 8)` で Alloc(realloc) が
                        // PassedTo(realloc, consumed:true) より前に並んでしまい、
                        // 「獲得→直後に消費」という逆順で解析され、偽の
                        // overwrite_owned/leak_suspect を生む（実測済みのバグ）。
                        // ソートキーだけ右辺終端(right.end_byte())に差し替えて
                        // 実行順に一致させる。span（表示位置）は従来どおり
                        // 左辺 id のまま＝ユーザーには `p = ` の行が表示される
                        return Occ::EventAt(interp_rhs(right, src, tracked), right.end_byte());
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
                // 引数位置の特定: cast/paren は1段ずつ登る既存ループのおかげで、
                // argument_list に到達した時点の cur は必ずその直接の named
                // child（`(void*)p` のような cast 越しでも、cur は cast_expression
                // ノードそのものまで登り切っている）。よって「cur と id() が
                // 一致する named child の添字」が構文上の引数位置に一致する
                let arg_pos = (0..par.named_child_count())
                    .find(|&i| par.named_child(i).map(|c| c.id()) == Some(cur.id()));
                let consumed = consumed_for(&callee, arg_pos);
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

    /// 1関数目の (所属変数, イベント種別) ペア列を取り出す補助。
    /// 同名の別変数間で「どちらのイベントか」を区別したいテスト用
    /// （引数位置ごとに consumed が変わるケースなど）
    fn events_of(src: &str) -> Vec<(VarId, EventKind)> {
        let f = extract_source(src, "t.c").unwrap();
        f.functions[0]
            .events
            .iter()
            .map(|e| (e.var, e.kind.clone()))
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

    // -----------------------------------------------------------------------
    // W3: 既知関数テーブル拡充 — 表駆動テスト
    // 「追加1関数につきテスト1本（1関数1アサーション以上）」をここで満たす
    // -----------------------------------------------------------------------

    #[test]
    fn alloc_fns_added_are_heap_sources() {
        // ストリーム/ディレクトリ系の追加分。戻り値代入が
        // Alloc{Heap{当該関数}} になることを1関数ずつ確認する
        for callee in ["fopen", "fdopen", "freopen", "tmpfile", "popen", "opendir"] {
            let src = format!("void f(void) {{ char *x = {callee}(); }}");
            let ks = kinds_of(&src);
            assert_eq!(
                ks[0],
                EventKind::Alloc {
                    source: AllocSource::Heap {
                        func: callee.into()
                    }
                },
                "callee={callee}"
            );
        }
    }

    #[test]
    fn benign_fns_added_do_not_consume() {
        // POSIX頻出関数の追加分。引数に渡した追跡変数が
        // consumed:Some(false) になることを1関数ずつ確認する。
        // strdup/fopen/fdopen/popen/opendir は ALLOC_FNS にも載っているが、
        // ここで見ているのは「引数として渡した側」の解釈なので矛盾しない
        for callee in [
            "strdup",
            "strndup",
            "fopen",
            "fdopen",
            "popen",
            "opendir",
            "strrchr",
            "memchr",
            "strcasecmp",
            "strncasecmp",
            "strtol",
            "strtod",
            "atoi",
        ] {
            let src = format!("void f(void) {{ char *p = malloc(4); {callee}(p); free(p); }}");
            let ks = kinds_of(&src);
            assert_eq!(
                ks[1],
                EventKind::PassedTo {
                    callee: callee.into(),
                    consumed: Some(false)
                },
                "callee={callee}"
            );
        }
    }

    #[test]
    fn consumer_fns_added_consume_at_declared_position() {
        // fclose/realloc 以外に新設した CONSUMER_FNS エントリ。
        // 表の消費位置に置いた追跡変数が Some(true) になることを確認する
        // （reallocarray/pclose/closedirは位置0、freopenは位置2）
        let cases: &[(&str, &str)] = &[
            ("reallocarray", "reallocarray(p, 2, 4)"),
            ("pclose", "pclose(p)"),
            ("closedir", "closedir(p)"),
            ("freopen", r#"freopen("f", "r", p)"#),
        ];
        for (callee, call) in cases {
            let src = format!("void f(void) {{ char *p = malloc(4); {call}; }}");
            let ks = kinds_of(&src);
            assert_eq!(
                ks[1],
                EventKind::PassedTo {
                    callee: (*callee).into(),
                    consumed: Some(true)
                },
                "callee={callee}"
            );
        }
    }

    // -----------------------------------------------------------------------
    // W3: 個別テスト — 意味論が非自明なものを個別に固定する
    // -----------------------------------------------------------------------

    #[test]
    fn realloc_first_arg_consumed_second_arg_not() {
        // realloc(ptr, size) は位置0(ptr)だけを消費する。位置1に追跡ポインタを
        // 置いた場合は「表にある関数の非消費位置」として Some(false) と断定する
        // （表に無い関数のように曖昧Noneには倒さない、という規約の確認）
        let ev = events_of(
            "void f(void) { char *p = malloc(4); char *n = malloc(1); char *q = realloc(p, n); }",
        );
        assert_eq!(
            ev[2],
            (
                VarId(0), // p: 消費位置
                EventKind::PassedTo {
                    callee: "realloc".into(),
                    consumed: Some(true)
                }
            )
        );
        assert_eq!(
            ev[3],
            (
                VarId(1), // n: 非消費位置
                EventKind::PassedTo {
                    callee: "realloc".into(),
                    consumed: Some(false)
                }
            )
        );
    }

    #[test]
    fn realloc_self_assign_event_order_matches_execution_semantics() {
        // 実測されたバグの回帰テスト。`p = realloc(p, 8);` の実行順は
        // 「右辺評価(旧pの消費)→代入(新pの獲得)」だが、出現バイト順
        // （構文上は左辺pが右辺より前）でそのままソートすると
        // Alloc(realloc)がPassedTo(realloc)より前に並んでしまい、
        // 「獲得→直後に消費」という逆順で解析され、偽の
        // overwrite_owned/leak_suspect を生んでいた。
        // 代入イベントのソートキーを右辺終端に上書きする修正の直接の回帰テスト
        let ks = kinds_of(
            r#"
void f(void) {
    char *p = malloc(4);
    p = realloc(p, 8);
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
                EventKind::PassedTo {
                    callee: "realloc".into(),
                    consumed: Some(true)
                },
                EventKind::Alloc {
                    source: AllocSource::Heap {
                        func: "realloc".into()
                    }
                },
                EventKind::Free,
            ]
        );
    }

    #[test]
    fn freopen_consumes_third_arg_only() {
        // freopen(path, mode, stream) は第3引数(stream, 位置2)だけを消費する。
        // 第1引数(path)に追跡変数を置いても、表にある関数の非消費位置として
        // Some(false) になる（曖昧のNoneにはならない）
        let ev = events_of(
            r#"
void f(void) {
    char *path = malloc(4);
    char *fp = malloc(8);
    freopen(path, "r", fp);
}
"#,
        );
        assert_eq!(
            ev[2],
            (
                VarId(0), // path: 非消費位置
                EventKind::PassedTo {
                    callee: "freopen".into(),
                    consumed: Some(false)
                }
            )
        );
        assert_eq!(
            ev[3],
            (
                VarId(1), // fp: 消費位置(2)
                EventKind::PassedTo {
                    callee: "freopen".into(),
                    consumed: Some(true)
                }
            )
        );
    }

    #[test]
    fn fopen_alloc_and_argument_are_consistent() {
        // fopen は戻り値側では Alloc{Heap{fopen}}（新しい所有権）、
        // 引数側では BENIGN（path文字列を借用するだけ）。
        // 同一関数が ALLOC_FNS/BENIGN_FNS の両方に載っていても、
        // 参照される文脈（戻り値の解釈 vs 引数の解釈）が違うため
        // 矛盾しないことを確認する
        let ev = events_of(
            r#"
void f(void) {
    char *path = malloc(4);
    char *fp = fopen(path, "r");
}
"#,
        );
        assert_eq!(
            ev[1],
            (
                VarId(0), // path
                EventKind::PassedTo {
                    callee: "fopen".into(),
                    consumed: Some(false)
                }
            )
        );
        assert_eq!(
            ev[2],
            (
                VarId(1), // fp
                EventKind::Alloc {
                    source: AllocSource::Heap {
                        func: "fopen".into()
                    }
                }
            )
        );
    }

    #[test]
    fn realloc_self_assign_analysis_has_no_false_positive() {
        // W3の本丸: フロントエンドのイベント順序修正が、analysis層の誤診断
        // （overwrite_owned/leak_suspectの偽陽性2件・カバレッジ0.0）を
        // 実際に解消することを確認する統合テスト。
        // analysis のロジックには一切手を入れていない —
        // facts側が正しい実行順でイベントを出すようになった、という
        // フロントエンド側の修正だけで解消されることの確認（cowl-core は
        // Cargo.toml で通常依存として引いているのでテストから直接呼べる）
        let facts = extract_source(
            r#"
void f(void) {
    char *p = malloc(4);
    p = realloc(p, 8);
    free(p);
}
"#,
            "t.c",
        )
        .unwrap();
        let report = cowl_core::analysis::analyze(&facts);
        let fr = &report.functions[0];
        assert!(fr.issues.is_empty(), "issues: {:?}", fr.issues);
        assert_eq!(report.metrics.ownership_coverage, 1.0);
    }

    #[test]
    fn plain_overwrite_without_self_ref_still_flags_overwrite_owned() {
        // 上のテストの対: realloc越しの自己代入（偽陽性→解消済み）と違い、
        // 消費関数を介さない単純な再代入はイベント順序修正後も引き続き
        // OverwriteOwned として検出されるべき（真陽性が誤って消えていない
        // ことの回帰テスト。examples/leak.c の overwrite() と同じパターン）。
        // 順序ロジック（EventAt）に再度手が入ったとき、この2本が対で
        // 偽陽性・真陽性の両側を守る
        let facts = extract_source(
            "void f(void) { char *p = malloc(16); p = malloc(32); free(p); }",
            "t.c",
        )
        .unwrap();
        let report = cowl_core::analysis::analyze(&facts);
        let fr = &report.functions[0];
        // 単純上書きは「上書き(OverwriteOwned)」と「上書きされた旧資源の
        // リーク(LeakSuspect)」の2件セットで出るのが仕様。ここで守りたい
        // 真陽性は OverwriteOwned が消えないこと
        use cowl_core::analysis::IssueKind;
        assert!(
            fr.issues
                .iter()
                .any(|i| i.kind == IssueKind::OverwriteOwned),
            "issues: {:?}",
            fr.issues
        );
        assert!(
            fr.issues.iter().any(|i| i.kind == IssueKind::LeakSuspect),
            "issues: {:?}",
            fr.issues
        );
    }
}
