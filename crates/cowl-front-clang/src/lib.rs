//! # cowl-front-clang — libclang による L2 フロントエンド
//!
//! 役割は cowl-front-ts (L1) と同じくただひとつ: **Cソース → facts（構文事実）** の変換。
//! 所有権の解釈・診断・指標計算は一切しない（それは cowl-core::analysis の仕事）。
//! 公開APIの形も L1 と同一（`extract_file` / `extract_source`）—
//! facts がフロントエンド差し替えの継ぎ目であるという設計（ADR-0002）を体現している。
//!
//! ## なぜ libclang か（L2 の位置づけ）
//! L1 (tree-sitter) は型情報・マクロ展開・制御フローを持たず、これが精度の天井に
//! なっていた。libclang は実際にプリプロセス＋意味解析を行うため、次の2点が
//! 「自然に」解ける。これが W5 のスコープの**全て**であり、それ以外の精度向上
//! （スコープ対応・制御フロー考慮・関数戻り値の所有権推定など）は意図的にやらない
//! （ROADMAP W5 の設計判断。やりたくなったら次のタスクとして起票する。
//! docs/adr/0007-l2-libclang-frontend.md 参照）:
//!
//! - (a) マクロ展開: `#define MY_ALLOC(n) malloc(n)` 越しの `p = MY_ALLOC(4)` は
//!   展開後のAST（callee="malloc"）を見るので Alloc{Heap} になる
//!   （L1 は未展開のまま "MY_ALLOC" という未知関数として AssignOpaque に落ちる）
//! - (b) const ポインタ引数: 呼び出し先の関数宣言が同一TU内に**実際に書かれて**いて
//!   （暗黙宣言・組み込み関数認識だけの合成宣言は対象外。判定方法は後述）、
//!   渡した実引数に対応する仮引数の型が `const T*`（pointee が const 修飾）なら
//!   consumed:Some(false) と断定する。根拠は「const を外して free するには
//!   キャストが要る」という C の慣習であり、その慣習を破る病的コード
//!   （const を外して free する）は検出できない（限界として許容する）
//!
//! ## L2 の割り切り一覧（ワーカーはここを「バグ」として直さないこと）
//! L1 と同一の割り切りを維持する（オーケストレータ固定の設計判断。上記2点以外は
//! すべて L1 と同じ挙動にする）:
//! - スコープ非対応: 同名変数の再宣言は両方とも追跡対象外にし、unknowns に記録。
//!   libclang は本来スコープを解決できるが、L1 とのfacts互換のためあえて使わない
//! - 制御フロー非対応: イベントはソース出現順。if/loop の分岐は考慮しない。
//!   ただし代入・初期化イベントは「右辺を評価してから代入が起きる」という実行意味論
//!   に合わせて右辺終端でソートする（L1 と同一規約。cowl-core/src/facts.rs の
//!   events フィールドのドキュメント参照）
//! - ポインタ演算 `p+1` は単なる使用(Use)として扱う（別名の派生は追わない）
//! - `&p`（ポインタ自身のアドレス取得）は追跡外とし、unknowns に記録
//! - 未知関数呼び出し（宣言が見えない、または見えても仮引数が const でない）は
//!   consumed:None（曖昧）— L1 と同じ。(b)の規則が「効かない」場合の既定動作
//!
//! ## 実装上の注意（libclang 特有の制約。スパイクで実証したうえでの設計）
//! - `clang::Clang` は**プロセス全体で同時に1インスタンスまで**という制約を持つ
//!   （2つ目の生成は Err になる。クレート内部の AVAILABLE という static がその番人）。
//!   `cargo test` はスレッド並列でテストを走らせるため、[`CLANG_GATE`] という
//!   Mutex で extract 呼び出し全体を直列化する
//! - テスト断片は `#include` を省略することが多い。関数呼び出しは libclang の
//!   「未宣言関数は暗黙宣言として復元する」機構により致命傷にならないが、**値**の
//!   未宣言識別子（代表例: `NULL`）は復元されず初期化式ごと AST から消える。
//!   これを避けるため `-DNULL=((void*)0)` をデフォルト引数に含める
//! - `-std=gnu11` を採用する（`-std=c11` より POSIX 関数の組み込み認識が広く、
//!   暗黙宣言からの型推論精度が上がり診断が減る。AST構造自体はどちらでも
//!   壊れないことをスパイクで確認済み）
//! - 診断（コンパイルエラー相当のメッセージ）が出ても `parse()` 自体は失敗しない
//!   （スパイクで実証済み）。これは requirement 7 の「読めた範囲の facts と
//!   unknowns で自己申告する」を libclang 側が自然に満たしてくれることを意味する。
//!   `extract_source` が `Err` を返すのは `parse()` 自体が失敗する場合
//!   （libclang クラッシュ・AST読み込み失敗等の致命的な状況）のみで、
//!   「コード側の診断」とは区別する

use anyhow::{anyhow, Context, Result};
use clang::{Clang, Entity, EntityKind, EvaluationResult, Index, TypeKind, Unsaved};
use cowl_core::facts::*;
use std::collections::HashMap;
use std::sync::Mutex;

/// libclang はプロセス全体で同時に1インスタンスまでという制約を持つ
/// （`clang::Clang::new()` は2つ目の呼び出しを Err にする）。
/// この Mutex で extract 呼び出し全体を直列化し、テストの並列実行でも
/// 安全に順番待ちさせる。
///
/// poison からの回復について: あるテストが Mutex 保持中に panic しても、
/// 巻き戻し（unwind）の過程で `Clang` の `Drop` は通常どおり走り
/// AVAILABLE フラグは正しく解放される。つまり poison は「実際に壊れた」
/// ことを意味しないので、`into_inner()` で回復して以降の呼び出しを
/// 継続させる（1つのテストの失敗で他のテストまで巻き添えにしない）
static CLANG_GATE: Mutex<()> = Mutex::new(());

/// パース時に常に渡す引数。
/// - `-x c`: ファイル名の拡張子に依存せず常にC言語として解釈する
///   （テストは "t.c" 以外の仮名を使わないが、拡張子判定に頼らない方が堅牢）
/// - `-std=gnu11`: C11相当のGNU拡張版。上記モジュールドキュメント参照
/// - `-DNULL=((void*)0)`: 上記モジュールドキュメント参照。実ヘッダ
///   （stdlib.h等）を `#include` している場合も、標準ヘッダは NULL を
///   再定義する前に `#undef` するため衝突しないことを確認済み
///   （確認方法・確認結果は docs/adr/0007-l2-libclang-frontend.md）
const DEFAULT_CLANG_ARGS: &[&str] = &["-x", "c", "-std=gnu11", "-DNULL=((void*)0)"];

// ---------------------------------------------------------------------------
// 公開API — cowl-front-ts と完全に同じ形（フロントエンド差し替えの継ぎ目）
// ---------------------------------------------------------------------------

/// ファイルパスから facts を抽出する
pub fn extract_file(path: &str) -> Result<Facts> {
    let source = std::fs::read_to_string(path).with_context(|| format!("読み込み失敗: {path}"))?;
    extract_source(&source, path)
}

/// ソース文字列から facts を抽出する（テスト・API経由の入力用）。
/// libclang の unsaved file 機構を使うため、`file_name` は実在しなくてよい
pub fn extract_source(source: &str, file_name: &str) -> Result<Facts> {
    // extract呼び出し全体を直列化する（CLANG_GATEのドキュメント参照）
    let _guard = CLANG_GATE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let clang = Clang::new().map_err(|e| anyhow!("libclang の初期化に失敗: {e}"))?;
    // display_diagnostics=false: 診断は意図的に無視する方針（モジュールドキュメント参照）。
    // true だと libclang が stderr に直接書き出してしまい、ライブラリ関数として行儀が悪い
    let index = Index::new(&clang, false, false);
    let unsaved = Unsaved::new(file_name, source);
    let tu = index
        .parser(file_name)
        .arguments(DEFAULT_CLANG_ARGS)
        .unsaved(&[unsaved])
        .parse()
        .map_err(|e| anyhow!("libclang パース失敗（コード側の診断ではなく致命的エラー）: {e}"))?;

    let mut functions = Vec::new();
    for e in tu.get_entity().get_children() {
        if !e.is_in_main_file() {
            continue; // #include で持ち込まれた宣言・定義は対象外
        }
        if e.get_kind() == EntityKind::FunctionDecl && e.is_definition() {
            if let Some(ff) = extract_function(e, source) {
                functions.push(ff);
            }
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

/// 宣言収集の中間表現。cowl-front-ts の同名構造体と同じ役割
/// （イベント化は名前→ID表が完成した後に行うため一旦貯める）
struct PendingDecl<'tu> {
    name: String,
    name_span: Span,
    byte: usize,
    /// 初期化式（あれば）
    init: Option<Entity<'tu>>,
    /// 関数引数由来か
    is_param: bool,
    /// pointee の const 修飾（ADR-0009 / W6-1）。pointee_const_of の結果をそのまま持つ
    pointee_const: Option<bool>,
}

fn extract_function<'tu>(fn_entity: Entity<'tu>, src: &str) -> Option<FunctionFacts> {
    let fn_name = fn_entity.get_name()?;
    let body = fn_entity
        .get_children()
        .into_iter()
        .find(|c| c.get_kind() == EntityKind::CompoundStmt)?;

    // --- Pass A: 追跡対象ポインタ変数の収集 --------------------------------
    let mut pendings: Vec<PendingDecl<'tu>> = Vec::new();

    // (A-1) 引数のポインタ。所有権は呼び出し規約に依存するため、
    //       宣言と同時に AssignOpaque を発行して「追跡不能な値を持つ」状態から始める
    for p in fn_entity.get_arguments().unwrap_or_default() {
        if !is_pointer_type(p) {
            continue; // ポインタでない引数は対象外（宣言型ベースの判定。typedef越しは辿らない＝L1のpointer_declarator検出と同じスコープ）
        }
        let Some(name) = p.get_name() else {
            continue; // 仮引数名が無い宣言（`void f(char*);`）は追跡不能なので諦める
        };
        pendings.push(PendingDecl {
            name,
            name_span: span_of(p),
            byte: start_offset_of(p),
            init: None,
            is_param: true,
            pointee_const: pointee_const_of(p),
        });
    }

    // (A-2) 関数本体内のローカル宣言。スコープ非対応 = ネストしたブロックも
    //       区別せず全部集める（L1 と同じ割り切り。duplicate_name banning で使う）
    let mut decl_nodes = Vec::new();
    collect_kind(body, EntityKind::VarDecl, &mut decl_nodes);
    for d in decl_nodes {
        if !is_pointer_type(d) {
            continue;
        }
        let Some(name) = d.get_name() else {
            continue;
        };
        pendings.push(PendingDecl {
            name,
            name_span: span_of(d),
            byte: start_offset_of(d),
            init: var_init(d),
            is_param: false,
            pointee_const: pointee_const_of(d),
        });
    }

    let mut unknowns: Vec<Unknown> = Vec::new();

    // (A-3) 同名の再宣言はスコープ非対応では区別できないため、
    //       その名前ごと追跡を諦めて unknowns に自己申告する（L1と同一アルゴリズム）
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
                    "同名変数 `{}` の再宣言（スコープ未対応のため追跡対象外。L1と同一挙動）",
                    b
                ),
            });
        }
    }
    pendings.retain(|p| !banned.contains(&p.name));
    // VarId は「出現バイト順の連番」— cowl-core::analysis の debug_assert と
    // 合わせた契約（cowl-front-ts と同一）
    pendings.sort_by_key(|p| p.byte);

    let vars: Vec<VarDecl> = pendings
        .iter()
        .enumerate()
        .map(|(i, p)| VarDecl {
            id: VarId(i as u32),
            name: p.name.clone(),
            decl: p.name_span,
            pointee_const: p.pointee_const,
        })
        .collect();
    let tracked: HashMap<String, VarId> = vars.iter().map(|v| (v.name.clone(), v.id)).collect();

    // --- イベント生成 -------------------------------------------------------
    let mut evs: Vec<(usize, Event)> = Vec::new();

    // (B-0) 宣言由来のイベント（引数の不透明値・初期化式）。
    // tree-sitter版は「宣言の分類」と「本体の識別子走査」が2パスに分かれているが
    // （親ポインタを辿れるため）、libclang はトップダウンでしか辿れないので
    // 「初期化式の分類 → その中身の再帰」を classify_rhs_and_walk に統一している
    for p in &pendings {
        let vid = tracked[&p.name];
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
            let kind = classify_rhs_and_walk(init, &tracked, src, &mut evs, &mut unknowns);
            // ソートキーは init の**終端**（右辺評価→代入の実行意味論。
            // walk内のBinaryOperator("=")ケースのコメント参照）
            evs.push((
                end_offset_of(init),
                Event {
                    var: vid,
                    span: span_of(init),
                    kind,
                },
            ));
        }
    }

    // (B-1) 本体を歩いてイベント化
    walk(body, &tracked, src, &mut evs, &mut unknowns, false);

    // (B-2) goto があると「出現順＝実行順」の前提が崩れるので信頼度低下を申告
    let mut gotos = Vec::new();
    collect_kind(body, EntityKind::GotoStmt, &mut gotos);
    if let Some(g) = gotos.first().copied() {
        unknowns.push(Unknown {
            span: span_of(g),
            reason: "goto による非構造フローあり（線形順序前提の解析は信頼度低下。L1と同一挙動）"
                .into(),
        });
    }

    evs.sort_by_key(|(b, _)| *b);

    Some(FunctionFacts {
        name: fn_name,
        span: span_of(fn_entity),
        vars,
        events: evs.into_iter().map(|(_, e)| e).collect(),
        unknowns,
    })
}

// ---------------------------------------------------------------------------
// 識別子出現の分類 — L2の心臓部
//
// tree-sitter版(cowl-front-ts)は「識別子から親方向へ登る」ボトムアップ探索だが、
// libclang の Entity には汎用の「式レベルの親」を取る API が無い（宣言の
// semantic/lexical parent はあるが、式の入れ子構造には使えない）。
// そのためこちらはトップダウンの再帰下降で設計している。
// 「今どういう文脈にいるか」を write_ctx（Write文脈か否か）という1個の
// bool で引き回すことで、tree-sitter版の is_assign_lhs 相当
// （`*p = x` / `s->x = y` / `arr[i] = z` の左辺を辿って書き込みと判定する）
// を上から下へ伝播する形で再現している。
// ---------------------------------------------------------------------------

fn walk<'tu>(
    e: Entity<'tu>,
    tracked: &HashMap<String, VarId>,
    src: &str,
    evs: &mut Vec<(usize, Event)>,
    unknowns: &mut Vec<Unknown>,
    write_ctx: bool,
) {
    match e.get_kind() {
        // DeclStmtはただの入れ物。中身(VarDecl)は個別に判定する
        EntityKind::DeclStmt => {
            for c in e.get_children() {
                walk(c, tracked, src, evs, unknowns, false);
            }
        }

        EntityKind::VarDecl => {
            // 追跡対象のポインタ変数（Pass Aでpendingsに入り、重複名でも
            // 無かった）なら、初期化式は classify_rhs_and_walk が既に
            // 再帰済み＝二重計上を避けるため何もしない。
            //
            // それ以外（非ポインタ宣言、または重複名で追跡を諦めた宣言）は
            // Pass Aが一切触れていないので、初期化式の**中身**をここで歩く。
            // 例: `char c = *p;` の c は非ポインタなので pendings に入らないが、
            // 初期化式の中の `p` は追跡変数であり、そのままだと見逃す
            // （実測されたバグの回帰: compat_alias_then_free_then_deref）。
            // c 自身への宣言イベントは出さない（そもそも追跡対象外なので）
            let is_tracked_here = e
                .get_name()
                .map(|n| is_pointer_type(e) && tracked.contains_key(&n))
                .unwrap_or(false);
            if !is_tracked_here {
                if let Some(init) = var_init(e) {
                    walk(init, tracked, src, evs, unknowns, false);
                }
            }
        }

        EntityKind::DeclRefExpr => {
            // ここに到達するのは「もっと近い文脈が無かった」場合のフォールバック
            // （tree-sitter版の `_ => cur = par` が最終的に落ちる Use::Read と同じ）
            if let Some(vid) = tracked_var_id(e, tracked) {
                push_use(evs, vid, e, write_ctx);
            }
        }

        EntityKind::BinaryOperator => {
            let children = e.get_children();
            if children.len() != 2 {
                // 見慣れない形（本来2子のはず）。安全側で両方をRead文脈で辿る
                for c in children {
                    walk(c, tracked, src, evs, unknowns, false);
                }
                return;
            }
            let lhs = children[0];
            let rhs = children[1];
            if binary_op_text(lhs, rhs, src) == "=" {
                let lhs_core = strip_transparent(lhs);
                if let Some(vid) = tracked_var_id(lhs_core, tracked) {
                    // `p = <RHS>` : ポインタ変数そのものへの再束縛。
                    // ソートキーは右辺の**終端**（実行意味論は「右辺評価→代入」の順。
                    // 出現順のまま並べると `p = realloc(p, 8)` のような自己代入で
                    // 獲得が消費より前に並んでしまい、偽の overwrite_owned/leak_suspect
                    // を生む。L1(cowl-front-ts)の同種コメント・W3の回帰テスト参照）
                    let kind = classify_rhs_and_walk(rhs, tracked, src, evs, unknowns);
                    evs.push((
                        end_offset_of(rhs),
                        Event {
                            var: vid,
                            span: span_of(lhs_core),
                            kind,
                        },
                    ));
                    return;
                }
                let rhs_core = strip_transparent(rhs);
                if let Some(vid) = tracked_var_id(rhs_core, tracked) {
                    // `<追跡外のLHS> = <追跡変数>` : 追跡範囲外への格納 = 脱出
                    // （左辺が追跡変数自身なら上のブランチで再束縛として処理済みなので
                    // ここに来るのは左辺が追跡外のときだけ）
                    let target = truncate(entity_text(lhs, src), 40);
                    evs.push((
                        start_offset_of(rhs_core),
                        Event {
                            var: vid,
                            span: span_of(rhs_core),
                            kind: EventKind::EscapeStore { target },
                        },
                    ));
                    return;
                }
                // `*p = ...` / `s->x = ...` / `arr[i] = ...` :
                // 左辺は書き込み文脈で辿り、追跡変数が見つかればWriteになる。
                // 右辺は通常のRead文脈
                walk(lhs_core, tracked, src, evs, unknowns, true);
                walk(rhs, tracked, src, evs, unknowns, false);
            } else {
                // 比較・算術等の非代入演算子: 両辺をRead文脈で辿る
                walk(lhs, tracked, src, evs, unknowns, false);
                walk(rhs, tracked, src, evs, unknowns, false);
            }
        }

        EntityKind::CompoundAssignOperator => {
            // `+=` 等は再束縛ではない（L1の割り切りを継承：
            // 複合代入は「新しい所有権の獲得」ではなくただの使用として扱う）。
            // 両辺をRead文脈で辿る
            for c in e.get_children() {
                walk(c, tracked, src, evs, unknowns, false);
            }
        }

        EntityKind::UnaryOperator => {
            let Some(operand) = e.get_children().first().copied() else {
                return;
            };
            match unary_op_text(e, operand, src).as_str() {
                "*" => {
                    // デリファレンス。`*p`。書き込み文脈は呼び出し元から継承する
                    let core = strip_transparent(operand);
                    if let Some(vid) = tracked_var_id(core, tracked) {
                        push_use(evs, vid, core, write_ctx);
                    } else {
                        walk(core, tracked, src, evs, unknowns, write_ctx);
                    }
                }
                "&" => {
                    // ポインタ変数自身のアドレス。`f(&p)` は呼び先が p を
                    // 書き換えうるが L2 でも追わない（L1と同一の割り切り）→
                    // 使用扱い＋自己申告。書き込み文脈は無関係（常にRead）
                    let core = strip_transparent(operand);
                    if let Some(vid) = tracked_var_id(core, tracked) {
                        evs.push((
                            start_offset_of(core),
                            Event {
                                var: vid,
                                span: span_of(core),
                                kind: EventKind::Use {
                                    mode: UseMode::Read,
                                },
                            },
                        ));
                        unknowns.push(Unknown {
                            span: span_of(core),
                            reason: format!(
                                "`&{}`（ポインタ自身のアドレス取得）は追跡外。呼び先で書き換えられる可能性",
                                entity_text(core, src)
                            ),
                        });
                    } else {
                        walk(core, tracked, src, evs, unknowns, false);
                    }
                }
                _ => {
                    // `-` `!` `~` `++` `--`（前置/後置）等はUse{Read}相当
                    // （L1のunary_expression/update_expressionバケツと同じ割り切り）。
                    // 後置演算子は unary_op_text の前提（前置順）が崩れて
                    // 空文字列を返すことがあるが、"*"/"&" 以外はどのみち
                    // このフォールバックに落ちるので実害は無い
                    walk(operand, tracked, src, evs, unknowns, false);
                }
            }
        }

        EntityKind::MemberRefExpr => {
            // `s->x` / `s.x`。子は基点(base)の式1つだけ（フィールド名はspellingで持つ）
            if let Some(base) = e.get_children().first().copied() {
                let core = strip_transparent(base);
                if let Some(vid) = tracked_var_id(core, tracked) {
                    push_use(evs, vid, core, write_ctx);
                } else {
                    walk(core, tracked, src, evs, unknowns, write_ctx);
                }
            }
        }

        EntityKind::ArraySubscriptExpr => {
            // `arr[i]`。子は [base, index]
            let children = e.get_children();
            if let Some(base) = children.first().copied() {
                let core = strip_transparent(base);
                if let Some(vid) = tracked_var_id(core, tracked) {
                    push_use(evs, vid, core, write_ctx);
                } else {
                    walk(core, tracked, src, evs, unknowns, write_ctx);
                }
            }
            // 添字自体は書き込み文脈を継承しない（`arr[p] = x` の p はRead）
            if let Some(index) = children.get(1).copied() {
                walk(index, tracked, src, evs, unknowns, false);
            }
        }

        EntityKind::CallExpr => handle_call(e, tracked, src, evs, unknowns),

        EntityKind::ReturnStmt => {
            if let Some(child) = e.get_children().first().copied() {
                let core = strip_transparent(child);
                if let Some(vid) = tracked_var_id(core, tracked) {
                    evs.push((
                        start_offset_of(core),
                        Event {
                            var: vid,
                            span: span_of(core),
                            kind: EventKind::EscapeReturn,
                        },
                    ));
                } else {
                    walk(core, tracked, src, evs, unknowns, false);
                }
            }
        }

        // --- 未対応の種別・単なる中継ノード: 安全側で子を辿る ---------------
        // CompoundStmt / IfStmt / WhileStmt / ForStmt / ConditionalOperator /
        // ParenExpr / UnexposedExpr 等がここに落ちる。write_ctx は維持する
        // （透過ラッパー越しに書き込み文脈が消えないようにするため）
        _ => {
            for c in e.get_children() {
                walk(c, tracked, src, evs, unknowns, write_ctx);
            }
        }
    }
}

/// 関数呼び出しの引数を分類する。
/// `free` は特別扱いで Free イベントに、それ以外は「表 → constポインタ規則」の
/// 優先順で consumed を決める（表が最優先＝manpage確認済みの知識を上書きしない）
fn handle_call<'tu>(
    call: Entity<'tu>,
    tracked: &HashMap<String, VarId>,
    src: &str,
    evs: &mut Vec<(usize, Event)>,
    unknowns: &mut Vec<Unknown>,
) {
    let callee = callee_name(call);
    let is_free = callee == "free";
    let args = call.get_arguments().unwrap_or_default();

    for (i, arg) in args.into_iter().enumerate() {
        let core = strip_transparent(arg);
        if let Some(vid) = tracked_var_id(core, tracked) {
            let kind = if is_free {
                EventKind::Free
            } else {
                // 優先順: 既知関数テーブル（manpage確認済み） → constポインタ規則
                // （宣言が見える場合のみ）。表に無ければ規則を試し、それも
                // 効かなければ従来通り曖昧(None)にする
                let consumed = cowl_front_ts::consumed_for(&callee, Some(i))
                    .or_else(|| const_pointee_consumed(call, i));
                EventKind::PassedTo {
                    callee: callee.clone(),
                    consumed,
                }
            };
            evs.push((
                start_offset_of(core),
                Event {
                    var: vid,
                    span: span_of(core),
                    kind,
                },
            ));
        } else {
            // 引数が「裸の追跡変数」でない（`free(p->next)` 等）→
            // 通常の走査に委ねる（Read文脈。呼び先が書き換える可能性はL1同様追わない）
            walk(core, tracked, src, evs, unknowns, false);
        }
    }
}

/// 初期化式・代入右辺の解釈。
/// **ここで返すのは構文事実であって所有権判断ではない**点に注意
/// （AssignFromVar がムーブか借用かは analysis が決める。cowl-front-ts と同じ方針）
fn classify_core<'tu>(core: Entity<'tu>, tracked: &HashMap<String, VarId>, src: &str) -> EventKind {
    match core.get_kind() {
        EntityKind::CallExpr => {
            let callee = callee_name(core);
            if cowl_front_ts::ALLOC_FNS.contains(&callee.as_str()) {
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
        EntityKind::UnaryOperator => {
            // "&" だけがAlloc{AddressOf}。それ以外（"*"含む）はAssignOpaqueへ
            // フォールバック（L1のinterp_rhsも "&" 以外のpointer/unary演算子は
            // 特別扱いしていない＝falls through to `_`と同じ挙動）
            let operand = core.get_children().first().copied();
            let is_amp = operand
                .map(|op| unary_op_text(core, op, src) == "&")
                .unwrap_or(false);
            if is_amp {
                EventKind::Alloc {
                    source: AllocSource::AddressOf,
                }
            } else {
                EventKind::AssignOpaque {
                    detail: truncate(entity_text(core, src), 40),
                }
            }
        }
        // NULL判定は evaluate()（意味的な値評価）で行う。理由:
        // `-DNULL=((void*)0)` 経由の展開後は spelling location が
        // マクロ定義側を指すため、テキスト比較（"0"かどうか）はマクロ越しだと
        // 頑健でない。evaluate() ならマクロの綴りに関係なく値0を検出できる。
        // ただし IntegerLiteral 種別に限定する（BinaryOperator等の定数畳み込み
        // 結果まで拾うと `int *a = 1-1;` のような式もNULL扱いになってしまい、
        // L1にもcowl-front-clangにも無い「定数畳み込みによる精度向上」という
        // W5で許可されていない第3の改善になってしまうため、意図的に絞っている
        EntityKind::IntegerLiteral if is_zero_literal(core) => EventKind::AssignNull,
        EntityKind::DeclRefExpr => {
            let name = core.get_name().unwrap_or_default();
            match tracked.get(&name) {
                Some(&vid) => EventKind::AssignFromVar { src: vid },
                None => EventKind::AssignOpaque {
                    detail: format!("追跡外の変数 `{}`", name),
                },
            }
        }
        _ => EventKind::AssignOpaque {
            detail: truncate(entity_text(core, src), 40),
        },
    }
}

/// 初期化式・代入右辺を分類しつつ、ネストした追跡変数の出現（呼び出し引数や
/// アドレス取得の対象など）も再帰的にイベント化する。戻り値は「宣言/代入の
/// 対象変数」自身に対するイベント種別。
///
/// tree-sitter版は「宣言の分類(Pass A)」と「本体の識別子走査(Pass B)」が
/// 2パスに分かれているが（親ポインタを辿れるため、Pass Bで二重計上を
/// Skipで防げる）、libclang はトップダウンでしか辿れないため
/// 「分類→中身の再帰」を1つの関数にまとめている。
/// 例: `p = realloc(p, 8)` は「pへのAlloc{realloc}」(この関数の戻り値)と
/// 「pの引数としての消費」(再帰で辿ったCallExprの中で検出)の2イベントになる
fn classify_rhs_and_walk<'tu>(
    rhs_raw: Entity<'tu>,
    tracked: &HashMap<String, VarId>,
    src: &str,
    evs: &mut Vec<(usize, Event)>,
    unknowns: &mut Vec<Unknown>,
) -> EventKind {
    let core = strip_transparent(rhs_raw);
    let kind = classify_core(core, tracked, src);
    // coreが「ちょうど追跡変数そのもの」なら上のkind(AssignFromVar)で
    // 表現済み・かつ子を持たない葉ノードなので再帰不要。それ以外
    // （呼び出し・アドレス取得・その他複雑な式）は中身を辿る
    if tracked_var_id(core, tracked).is_none() {
        walk(core, tracked, src, evs, unknowns, false);
    }
    kind
}

/// 呼び出し先の宣言が同一TU内に**実際に書かれて**いて、対象引数の仮引数型が
/// `const T*` なら consumed:Some(false) と断定する（W5の精度向上(b)）。
///
/// 「実際に書かれている」の判定: 仮引数に名前が付いているかで見る。
/// libclang は宣言の見えない呼び出しに対して「暗黙宣言」を合成し、
/// `get_reference()` はその合成宣言に解決されてしまう（未知関数呼び出しが
/// 致命傷にならない仕組みそのもの）。組み込み認識されている関数
/// （例: gnu11 での strdup）では戻り値・引数の型まで正しく推論されるが、
/// 合成された仮引数は常に無名になる（スパイクで実証済み）。
/// 一方、ソースに実際に書かれた宣言の仮引数は（多くの場合）名前を持つ。
/// 「宣言位置と呼び出し位置が違うか」より、この判定の方が2回目以降の
/// 呼び出しでも安定する
fn const_pointee_consumed<'tu>(call: Entity<'tu>, arg_pos: usize) -> Option<bool> {
    let callee_decl = call.get_reference()?;
    let params = callee_decl.get_arguments()?;
    let param = *params.get(arg_pos)?;
    param.get_name()?; // 無名 = 合成宣言 → 対象外（Noneへフォールバック）
    let pointee = param.get_type()?.get_pointee_type()?;
    pointee.is_const_qualified().then_some(false)
}

// ---------------------------------------------------------------------------
// 小さな構文ユーティリティ
// ---------------------------------------------------------------------------

/// 部分木を先行順DFSで走査し、指定 kind のノードを出現順に集める
/// （cowl-front-ts の collect_kind と同じ役割）
fn collect_kind<'tu>(e: Entity<'tu>, kind: EntityKind, out: &mut Vec<Entity<'tu>>) {
    if e.get_kind() == kind {
        out.push(e);
    }
    for c in e.get_children() {
        collect_kind(c, kind, out);
    }
}

/// `(expr)` と `(T*)expr` および暗黙変換（libclangは ImplicitCastExpr に
/// 専用のCursorKindを割り当てず UnexposedExpr として1子ラッパーで表す）を
/// 透過して中身に到達する。「キャストや括弧は所有権の意味を変えない」という
/// 判断をコードにしたもの（cowl-front-ts の strip と同じ役割）
fn strip_transparent(mut e: Entity<'_>) -> Entity<'_> {
    loop {
        match e.get_kind() {
            EntityKind::ParenExpr | EntityKind::CStyleCastExpr => {
                match e.get_children().into_iter().next() {
                    Some(c) => e = c,
                    None => return e,
                }
            }
            EntityKind::UnexposedExpr => {
                let children = e.get_children();
                if children.len() == 1 {
                    e = children[0];
                } else {
                    // 子が0個または2個以上は「単なる透過ラッパー」の想定から
                    // 外れるので、安全側でそれ以上は剥がさない
                    return e;
                }
            }
            _ => return e,
        }
    }
}

/// 宣言型が生ポインタか（`get_type()`は宣言時の型そのまま＝typedefは
/// 解決しない。tree-sitter版が構文上の `pointer_declarator` だけを見て
/// typedef越しのポインタ(`my_string_t`等)を追跡しないのと同じスコープに
/// 揃えるため、意図的に `get_canonical_type()` を使わない）
fn is_pointer_type(e: Entity<'_>) -> bool {
    e.get_type()
        .map(|t| t.get_kind() == TypeKind::Pointer)
        .unwrap_or(false)
}

/// 宣言型 pointee の const 修飾（ADR-0009 / W6-1）。呼び出し元は
/// is_pointer_type(e) が真であることを確認済みの Entity（引数 or VarDecl）を渡す前提。
///
/// `get_canonical_type()` を挟むのが要点: 宣言そのままの型（sugared type）に
/// 直接 `is_const_qualified()` を呼ぶと、typedef の内側に const が隠れている
/// ケース（`typedef const char cstr; cstr *p;`）で false を返してしまう
/// （実測: pointee.display="cstr", is_const_qualified=false だが
/// canonical.is_const_qualified=true）。canonicalize して初めて typedef の
/// 定義まで見た判定になる。これが L1(構文のみ・typedefはNoneに倒す)に対する
/// L2 の精度向上の核心（W5のconst規則実装と同系の「型を実際に解決する」設計）。
/// 素の const/非const/ポインタ自身のconst（`char * const p`。pointeeには
/// 効かない）は canonicalize してもしなくても結果が変わらないことも実測済みで、
/// 常に canonical 側を使って一本化して問題ない
fn pointee_const_of(e: Entity<'_>) -> Option<bool> {
    let pointee = e.get_type()?.get_pointee_type()?;
    Some(pointee.get_canonical_type().is_const_qualified())
}

/// VarDeclの初期化式を取り出す。
/// `clang_Cursor_getVarDeclInitializer` は libclang 12.0 以降限定かつ
/// 安全ラッパの `clang` クレートが公開していないため使わず、
/// 「TypeRef（宣言型の参照。`struct S *s = ...`のような場合に子として現れる）
/// を除いた最後の子」を初期化式とみなすヒューリスティックを使う
/// （tree-sitter版が `child_by_field_name("value")` という文法フィールドで
/// 拾うのと同程度の精度で、対象がポインタ変数の単純な初期化式に限られる
/// 今のスコープでは十分頑健）
fn var_init<'tu>(var_decl: Entity<'tu>) -> Option<Entity<'tu>> {
    var_decl
        .get_children()
        .into_iter()
        .rfind(|c| c.get_kind() != EntityKind::TypeRef)
}

/// coreが追跡変数そのものを指す DeclRefExpr なら、その VarId を返す
fn tracked_var_id(e: Entity<'_>, tracked: &HashMap<String, VarId>) -> Option<VarId> {
    if e.get_kind() != EntityKind::DeclRefExpr {
        return None;
    }
    let name = e.get_name()?;
    tracked.get(&name).copied()
}

fn push_use(evs: &mut Vec<(usize, Event)>, vid: VarId, at: Entity<'_>, write_ctx: bool) {
    evs.push((
        start_offset_of(at),
        Event {
            var: vid,
            span: span_of(at),
            kind: EventKind::Use {
                mode: if write_ctx {
                    UseMode::Write
                } else {
                    UseMode::Read
                },
            },
        },
    ));
}

/// call_expression の呼び先名。CallExprエンティティ自身のspellingが
/// そのまま呼び出し先の識別子名になる（libclangの挙動として安定）。
/// 関数ポインタ経由の呼び出し等でspellingが取れない場合のみ
/// フォールバックする（tree-sitter版のcallee_nameのUnknown回避と同じ方針）
fn callee_name(call: Entity<'_>) -> String {
    call.get_name().unwrap_or_else(|| "(不明)".into())
}

/// 2つのエンティティのソース上のテキストが `0` かどうかではなく、
/// **値として0か**を判定する（NULL判定に使用。上のclassify_coreのコメント参照）
fn is_zero_literal(e: Entity<'_>) -> bool {
    matches!(
        e.evaluate(),
        Some(EvaluationResult::SignedInteger(0)) | Some(EvaluationResult::UnsignedInteger(0))
    )
}

/// BinaryOperator の左右の子の間にあるテキストを演算子として取り出す。
/// libclang には演算子を直接返すAPI（`clang_getCursorBinaryOperatorKind`）が
/// あるが libclang 17.0 以降限定かつ安全ラッパ未対応のため使わず、
/// 左辺終端〜右辺始端のソーステキストをそのまま演算子とみなす
/// （`p=malloc(4)` でも `p == 0` でも空白を trim すれば正しく取れることを
/// スパイクで確認済み。コメントや行またぎの珍しい書式までは保証しないが、
/// tree-sitter版もgrammarフィールド頼みという意味では同程度の前提に立っている）
fn binary_op_text(lhs: Entity<'_>, rhs: Entity<'_>, src: &str) -> String {
    let a_end = end_offset_of(lhs);
    let b_start = start_offset_of(rhs);
    src.get(a_end..b_start).unwrap_or("").trim().to_string()
}

/// UnaryOperator の演算子テキストを取り出す（前置演算子を想定）。
/// 後置 `++`/`--` は演算子がオペランドの後ろに来るため、この関数は
/// 意図せず空文字列を返すことがあるが、判定対象は "*" と "&"
/// （どちらも前置専用でCに後置形が無い）だけなので実害はない
/// （呼び出し元 walk の UnaryOperator ケースのコメント参照）
fn unary_op_text(op: Entity<'_>, operand: Entity<'_>, src: &str) -> String {
    let op_start = start_offset_of(op);
    let operand_start = start_offset_of(operand);
    src.get(op_start..operand_start)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// libclang の位置情報を facts の規約（1始まりの行・列）へ変換する。
/// **展開位置(expansion location)** を使う点が重要: マクロ越しの場合
/// spelling location はマクロ定義側を指してしまうが、expansion location は
/// 常に「実際に呼ばれた場所」を指すため、facts の Span はユーザーが
/// エディタで見ている行に一致する（tree-sitterが最初からマクロを知らずに
/// 出現位置をそのまま返すのと結果的に同じ体験になる）
fn span_of(e: Entity<'_>) -> Span {
    match e.get_range() {
        Some(r) => {
            let s = r.get_start().get_expansion_location();
            let en = r.get_end().get_expansion_location();
            Span {
                line_start: s.line,
                line_end: en.line,
                col_start: s.column,
                col_end: en.column,
            }
        }
        // 到達しない想定（実在のASTノードは常にrangeを持つ）だが、
        // panicより「0埋めして処理を続ける」方をfacts firewallの精神に合わせて選ぶ
        None => Span {
            line_start: 0,
            line_end: 0,
            col_start: 0,
            col_end: 0,
        },
    }
}

fn start_offset_of(e: Entity<'_>) -> usize {
    e.get_range()
        .map(|r| r.get_start().get_expansion_location().offset as usize)
        .unwrap_or(0)
}

fn end_offset_of(e: Entity<'_>) -> usize {
    e.get_range()
        .map(|r| r.get_end().get_expansion_location().offset as usize)
        .unwrap_or(0)
}

/// エンティティのソース上のテキストを展開位置ベースで取り出す
/// （AssignOpaqueのdetailやEscapeStoreのtargetの表示用。マクロ越しでも
/// 実際に書かれた呼び出し側のテキストが取れる）
fn entity_text<'a>(e: Entity<'_>, src: &'a str) -> &'a str {
    match e.get_range() {
        Some(r) => {
            let s = r.get_start().get_expansion_location().offset as usize;
            let en = r.get_end().get_expansion_location().offset as usize;
            src.get(s..en).unwrap_or("")
        }
        None => "",
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let t: String = s.chars().take(max).collect();
        format!("{t}…")
    }
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn examples_dir() -> PathBuf {
        // cargo test 実行時のCWDはパッケージのマニフェストディレクトリになる
        // 契約に依存しないよう CARGO_MANIFEST_DIR から絶対パスを組み立てる
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("examples")
    }

    // -----------------------------------------------------------------------
    // 互換ゴールデン: cowl-front-ts (L1) の代表ケースと同じCソースを両方の
    // フロントエンドに通し、イベント列が一致することを固定する。
    // 比較粒度: (VarId, EventKind の形状)。AssignOpaqueのdetailと
    // EscapeStoreのtargetは自由記述なので形状（variant）だけ比較する。
    // unknownsは件数と行の弱い比較（reason文言は比較しない）
    //
    // L1側の19本のうち6本はここでは意図して再現しない（削り忘れではない）:
    // 表内容の網羅テスト4本（alloc_fns_added_are_heap_sources 等）は共有した
    // cowl_front_ts の表・consumed_for を引き直すだけで L2 固有の経路を
    // 通らず冗長なため。analysis 層の回帰テスト2本は facts 形状の一致を
    // 本ゴールデンで固定済みであり、analysis は facts の純関数なので
    // 結果の一致も論理的に従うため
    // -----------------------------------------------------------------------

    /// EventKind を「自由記述フィールドを落とした形状」に正規化する。
    /// PassedTo の callee/consumed と Alloc の source、AssignFromVar の src、
    /// Use の mode は構造的事実なのでそのまま比較する
    #[derive(Debug, Clone, PartialEq)]
    enum Shape {
        Alloc(AllocSource),
        AssignFromVar(VarId),
        AssignNull,
        AssignOpaque,
        Use(UseMode),
        Free,
        PassedTo {
            callee: String,
            consumed: Option<bool>,
        },
        EscapeReturn,
        EscapeStore,
    }

    impl Shape {
        fn of(k: &EventKind) -> Self {
            match k {
                EventKind::Alloc { source } => Shape::Alloc(source.clone()),
                EventKind::AssignFromVar { src } => Shape::AssignFromVar(*src),
                EventKind::AssignNull => Shape::AssignNull,
                EventKind::AssignOpaque { .. } => Shape::AssignOpaque,
                EventKind::Use { mode } => Shape::Use(*mode),
                EventKind::Free => Shape::Free,
                EventKind::PassedTo { callee, consumed } => Shape::PassedTo {
                    callee: callee.clone(),
                    consumed: *consumed,
                },
                EventKind::EscapeReturn => Shape::EscapeReturn,
                EventKind::EscapeStore { .. } => Shape::EscapeStore,
            }
        }
    }

    /// L1/L2 双方に同じソースを通し、(VarId, Shape) 列と unknowns の
    /// 件数/行が一致することを確認する
    fn assert_l1_l2_compatible(src: &str) {
        let f1 = cowl_front_ts::extract_source(src, "t.c").expect("L1 extract");
        let f2 = extract_source(src, "t.c").expect("L2 extract");
        assert_eq!(
            f1.functions.len(),
            f2.functions.len(),
            "関数の数が一致しない"
        );
        for (ff1, ff2) in f1.functions.iter().zip(f2.functions.iter()) {
            let names1: Vec<&str> = ff1.vars.iter().map(|v| v.name.as_str()).collect();
            let names2: Vec<&str> = ff2.vars.iter().map(|v| v.name.as_str()).collect();
            assert_eq!(names1, names2, "追跡変数の並びが一致しない: {}", ff1.name);

            // W6-1 (ADR-0009): pointee_const。この互換ゴールデン群のソースは
            // どれもtypedefを含まない素朴な宣言なので、L1/L2で完全一致するはず
            // （typedef越しの精度向上でL1=None/L2=Someに分かれるケースは対象外。
            // それは pointee_const_resolves_through_typedef で別途固定する）
            let pc1: Vec<Option<bool>> = ff1.vars.iter().map(|v| v.pointee_const).collect();
            let pc2: Vec<Option<bool>> = ff2.vars.iter().map(|v| v.pointee_const).collect();
            assert_eq!(
                pc1, pc2,
                "pointee_constがL1/L2で一致しない: {}\nL1={:?}\nL2={:?}",
                ff1.name, ff1.vars, ff2.vars
            );

            let s1: Vec<(VarId, Shape)> = ff1
                .events
                .iter()
                .map(|e| (e.var, Shape::of(&e.kind)))
                .collect();
            let s2: Vec<(VarId, Shape)> = ff2
                .events
                .iter()
                .map(|e| (e.var, Shape::of(&e.kind)))
                .collect();
            assert_eq!(
                s1, s2,
                "イベント列(VarId,種別)が一致しない: {}\nL1={:?}\nL2={:?}",
                ff1.name, ff1.events, ff2.events
            );

            let mut lines1: Vec<u32> = ff1.unknowns.iter().map(|u| u.span.line_start).collect();
            let mut lines2: Vec<u32> = ff2.unknowns.iter().map(|u| u.span.line_start).collect();
            lines1.sort_unstable();
            lines2.sort_unstable();
            assert_eq!(
                lines1, lines2,
                "unknownsの件数/行が一致しない: {}\nL1={:?}\nL2={:?}",
                ff1.name, ff1.unknowns, ff2.unknowns
            );
        }
    }

    #[test]
    fn compat_basic_malloc_use_free() {
        assert_l1_l2_compatible(
            r#"
#include <stdlib.h>
void f(void) {
    char *p = malloc(4);
    *p = 'a';
    free(p);
}
"#,
        );
    }

    #[test]
    fn compat_alias_then_free_then_deref() {
        assert_l1_l2_compatible(
            r#"
void f(void) {
    char *p = malloc(4);
    char *q = p;
    free(q);
    char c = *p;
}
"#,
        );
    }

    #[test]
    fn compat_return_is_escape() {
        assert_l1_l2_compatible("char *f(void) { char *p = malloc(4); return p; }");
    }

    #[test]
    fn compat_unknown_callee_is_ambiguous() {
        assert_l1_l2_compatible("void f(void) { char *p = malloc(4); mystery(p); }");
    }

    #[test]
    fn compat_benign_callee_is_nonconsuming_use() {
        assert_l1_l2_compatible(
            r#"void f(void) { char *p = malloc(4); printf("%s", p); free(p); }"#,
        );
    }

    #[test]
    fn compat_null_and_address_of_inits() {
        assert_l1_l2_compatible(
            r#"
void f(void) {
    int x = 0;
    int *a = NULL;
    int *b = &x;
}
"#,
        );
    }

    #[test]
    fn compat_param_pointer_starts_opaque() {
        assert_l1_l2_compatible("void f(char *p) { free(p); }");
    }

    #[test]
    fn compat_store_to_untracked_is_escape() {
        assert_l1_l2_compatible("char *g; void f(void) { char *p = malloc(4); g = p; }");
    }

    #[test]
    fn compat_duplicate_name_is_banned_with_unknown() {
        assert_l1_l2_compatible(
            "void f(void) { { char *p = malloc(4); } { char *p = malloc(8); } }",
        );
    }

    #[test]
    fn compat_cast_is_transparent() {
        assert_l1_l2_compatible("void f(void) { char *p = (char *)malloc(4); free((void *)p); }");
    }

    #[test]
    fn compat_realloc_self_assign_event_order_matches_execution_semantics() {
        assert_l1_l2_compatible(
            r#"
void f(void) {
    char *p = malloc(4);
    p = realloc(p, 8);
    free(p);
}
"#,
        );
    }

    #[test]
    fn compat_freopen_consumes_third_arg_only() {
        assert_l1_l2_compatible(
            r#"
void f(void) {
    char *path = malloc(4);
    char *fp = malloc(8);
    freopen(path, "r", fp);
}
"#,
        );
    }

    #[test]
    fn compat_fopen_alloc_and_argument_are_consistent() {
        assert_l1_l2_compatible(
            r#"
void f(void) {
    char *path = malloc(4);
    char *fp = fopen(path, "r");
}
"#,
        );
    }

    // -----------------------------------------------------------------------
    // 精度向上テスト (a) マクロ展開
    // -----------------------------------------------------------------------

    #[test]
    fn macro_wrapped_malloc_resolves_to_alloc() {
        // L1: マクロを展開できないため、"MY_ALLOC" という未知関数の戻り値として
        //     AssignOpaqueに落ちる（曖昧）
        // L2: libclangはASTを構築する前にプリプロセスするため、
        //     "malloc(4)" に展開された後の姿を見る → ALLOC_FNS表と一致し
        //     Alloc{Heap{"malloc"}} になる（正当性: マクロは所有権の意味を
        //     変えない、L1が展開できないのは実装上の制約であってCの意味論
        //     ではないため、これは精度向上であり新しい解釈の混入ではない）
        let src = r#"
#define MY_ALLOC(n) malloc(n)
void f(void) {
    char *p = MY_ALLOC(4);
    free(p);
}
"#;
        let f1 = cowl_front_ts::extract_source(src, "t.c").unwrap();
        assert!(matches!(
            f1.functions[0].events[0].kind,
            EventKind::AssignOpaque { .. }
        ));

        let f2 = extract_source(src, "t.c").unwrap();
        assert_eq!(
            f2.functions[0].events[0].kind,
            EventKind::Alloc {
                source: AllocSource::Heap {
                    func: "malloc".into()
                }
            }
        );
    }

    #[test]
    fn macro_wrapped_fclose_resolves_consumed_via_table_after_expansion() {
        // L1: "MY_CLOSE" という未知関数呼び出しとして consumed:None（曖昧）
        // L2: 展開後は callee="fclose" というASTになり、cowl-front-ts の
        //     CONSUMER_FNS 表（fclose は位置0を消費）がそのまま効いて
        //     consumed:Some(true) に解決する。マクロ展開と既知関数表という
        //     2つの既存の仕組みの組み合わせで曖昧さが解消される例
        let src = r#"
#include <stdio.h>
#define MY_CLOSE(f) fclose(f)
void f(void) {
    FILE *fp = fopen("x", "r");
    MY_CLOSE(fp);
}
"#;
        let f1 = cowl_front_ts::extract_source(src, "t.c").unwrap();
        let ev1 = &f1.functions[0].events[1].kind;
        assert_eq!(
            *ev1,
            EventKind::PassedTo {
                callee: "MY_CLOSE".into(),
                consumed: None
            }
        );

        let f2 = extract_source(src, "t.c").unwrap();
        let ev2 = &f2.functions[0].events[1].kind;
        assert_eq!(
            *ev2,
            EventKind::PassedTo {
                callee: "fclose".into(),
                consumed: Some(true)
            }
        );
    }

    // -----------------------------------------------------------------------
    // 精度向上テスト (b) constポインタ引数
    // -----------------------------------------------------------------------

    #[test]
    fn const_pointee_resolves_previously_ambiguous_call() {
        // L1: "audit_log" はALLOC_FNS/CONSUMER_FNS/BENIGN_FNSのどれにも
        //     載っていない未知関数なので consumed:None（曖昧）
        // L2: 同一TU内に `void audit_log(const char *msg);` という宣言が
        //     実際に書かれており、渡した引数(位置0)に対応する仮引数型が
        //     `const char *`（pointeeがconst修飾）なので consumed:Some(false)
        //     と断定できる（正当性: constを外してfreeするにはキャストが要る、
        //     というCの慣習に基づく。限界: 病的にconstを外すコードは検出外
        //     ＝ ADR-0007 参照）
        let src = r#"
void audit_log(const char *msg);
void f(void) {
    char *p = malloc(4);
    audit_log(p);
    free(p);
}
"#;
        let f1 = cowl_front_ts::extract_source(src, "t.c").unwrap();
        assert_eq!(
            f1.functions[0].events[1].kind,
            EventKind::PassedTo {
                callee: "audit_log".into(),
                consumed: None
            }
        );

        let f2 = extract_source(src, "t.c").unwrap();
        assert_eq!(
            f2.functions[0].events[1].kind,
            EventKind::PassedTo {
                callee: "audit_log".into(),
                consumed: Some(false)
            }
        );
    }

    #[test]
    fn const_pointee_resolves_only_the_const_positioned_argument() {
        // 同じ呼び出しの引数のうち const な位置**だけ**が解決され、
        // 非constな位置は従来通り曖昧のままであることを確認する
        // （規則が過剰に発火しない＝精度であって当てずっぽうではないことの証拠）
        let src = r#"
void copy_into(char *dst, const char *src);
void f(void) {
    char *d = malloc(8);
    char *s = malloc(8);
    copy_into(d, s);
    free(d);
    free(s);
}
"#;
        let f1 = cowl_front_ts::extract_source(src, "t.c").unwrap();
        let ev1: Vec<_> = f1.functions[0]
            .events
            .iter()
            .map(|e| (e.var, e.kind.clone()))
            .collect();
        assert_eq!(
            ev1[2],
            (
                VarId(0),
                EventKind::PassedTo {
                    callee: "copy_into".into(),
                    consumed: None
                }
            )
        );
        assert_eq!(
            ev1[3],
            (
                VarId(1),
                EventKind::PassedTo {
                    callee: "copy_into".into(),
                    consumed: None
                }
            )
        );

        let f2 = extract_source(src, "t.c").unwrap();
        let ev2: Vec<_> = f2.functions[0]
            .events
            .iter()
            .map(|e| (e.var, e.kind.clone()))
            .collect();
        // dst（位置0, 非const）は依然として曖昧
        assert_eq!(
            ev2[2],
            (
                VarId(0),
                EventKind::PassedTo {
                    callee: "copy_into".into(),
                    consumed: None
                }
            )
        );
        // src（位置1, const char*）だけ解決される
        assert_eq!(
            ev2[3],
            (
                VarId(1),
                EventKind::PassedTo {
                    callee: "copy_into".into(),
                    consumed: Some(false)
                }
            )
        );
    }

    // -----------------------------------------------------------------------
    // 精度向上テスト (c) pointee_const の typedef 解決（W6-1 / ADR-0009）
    // -----------------------------------------------------------------------

    #[test]
    fn pointee_const_resolves_through_typedef() {
        // L1: 宣言指定子列に見えるのは type_identifier（typedef名 `cstr`）だけで、
        //     その中身が const かどうかは構文情報だけでは分からないので None
        //     （cowl-front-ts::tests::pointee_const_none_through_typedef と対）
        // L2: libclangは型を実際に解決できる。`get_type()`で得られる宣言そのまま
        //     の型（sugared）に直接 is_const_qualified() を呼ぶと、const が
        //     typedef の中に隠れているケースでは false を返してしまう
        //     （スパイクで実測: pointee.display="cstr" は sugared のまま）。
        //     get_canonical_type() を挟んで初めて `cstr` = `const char` という
        //     定義まで見た判定になり、Some(true) に解決する。
        //     これは const ポインタ引数(W5, (b)節)と同型の「型を実際に解決する」
        //     精度向上であり、L1/L2 どちらにも無かった新しい解釈の混入ではない
        //
        // 注: 元のtypedefは `typedef const char *cstr;`（cstr自体がポインタ型）
        // ではなく `typedef const char cstr;`（ポインタの基底型だけをtypedef）
        // にしている。前者を `cstr p;` のように*無しで使うと、L1(構文上
        // pointer_declaratorが無い)もL2(is_pointer_typeがget_type()のkindを見る
        // だけでtypedefを解決しない設計。モジュール冒頭コメント参照)も
        // そもそもポインタ変数として追跡しない — 両者とも「追跡すらしない」で
        // 一致してしまい、pointee_constの精度差というこのテストの主題を
        // 検証できない。使用箇所に明示的な`*`を残す本形なら両方とも追跡対象になり、
        // 差が pointee_const だけに絞り込める
        let src = "typedef const char cstr; void f(void) { cstr *p = 0; }";

        let f1 = cowl_front_ts::extract_source(src, "t.c").unwrap();
        assert_eq!(f1.functions[0].vars[0].pointee_const, None);

        let f2 = extract_source(src, "t.c").unwrap();
        assert_eq!(f2.functions[0].vars[0].pointee_const, Some(true));
    }

    // -----------------------------------------------------------------------
    // 統合テスト: examples/*.c 全7本が cowl_core::analysis を panic なく通る
    // -----------------------------------------------------------------------

    #[test]
    fn all_examples_survive_analysis_without_panic() {
        let dir = examples_dir();
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("examples/ が読めない: {} ({e})", dir.display()))
            .filter_map(|entry| entry.ok().map(|e| e.path()))
            .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("c"))
            .collect();
        files.sort();
        assert_eq!(files.len(), 7, "examples/*.c の本数が想定と違う: {files:?}");

        for path in files {
            let facts = extract_file(path.to_str().unwrap())
                .unwrap_or_else(|e| panic!("{}: extract失敗: {e}", path.display()));
            let report = cowl_core::analysis::analyze(&facts);
            // panicしないことそのものが受け入れ条件だが、明らかな異常
            // （関数を1つも拾えていない等）が無いことも軽く確認しておく
            assert!(
                !facts.functions.is_empty(),
                "{}: 関数を1つも抽出できていない",
                path.display()
            );
            assert_eq!(
                report.functions.len(),
                facts.functions.len(),
                "{}: analysis結果の関数数がfactsと食い違う",
                path.display()
            );
        }
    }
}
