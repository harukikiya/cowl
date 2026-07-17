// CowlClient の統合テスト。モックではなく**実バイナリ**の cowl serve を
// spawn して回す。examples/ とテストが事実上の eval セット（CLAUDE.md）で
// あるのと同じ発想で、「編集中バッファを source で送るパスが通っている」
// という W2 受け入れ条件をここで自動テストとして固定する（ADR-0005）。
// cowlClient.ts が vscode API 非依存なのは、まさにこのテストのため。
//
// 実行: npm test（tsc でビルドしてから node --test）
// バイナリ位置は COWL_BIN で上書き可能。既定はリポジトリの target/debug/cowl。

import { test, describe, before, beforeEach, afterEach } from "node:test";
import assert from "node:assert/strict";
import { existsSync, chmodSync } from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";

import { CowlClient } from "../out/cowlClient.js";

const here = path.dirname(fileURLToPath(import.meta.url));
const BIN =
  process.env.COWL_BIN ?? path.resolve(here, "../../../target/debug/cowl");

// 受け入れ条件の固定に使う最小の C ソース（cowl-api のテストと同じ題材）
const SRC = "void f(void){ char *p = malloc(4); free(p); }";

describe("CowlClient と実バイナリ cowl serve --stdio の統合", () => {
  before(() => {
    assert.ok(
      existsSync(BIN),
      `cowl バイナリが見つかりません: ${BIN}\n` +
        "先に `cargo build -p cowl-cli` を実行してください（または環境変数 COWL_BIN でパスを指定）"
    );
  });

  /** @type {CowlClient} */
  let client;

  // プロセスリーク禁止: 各テストで作った子プロセスは必ず殺す
  beforeEach(() => {
    client = new CowlClient(BIN);
  });
  afterEach(() => {
    client.dispose();
  });

  test("version: ok と api_version が返る", async () => {
    const res = await client.request({ cmd: "version" });
    assert.equal(res.ok, true);
    assert.equal(typeof res.api_version, "string");
    assert.ok(res.api_version.length > 0);
  });

  test("analyze: source 指定で report が返る", async () => {
    const res = await client.request({
      cmd: "analyze",
      source: SRC,
      file_name: "mem.c",
    });
    assert.equal(res.ok, true);
    assert.ok(res.report, "report キーが存在すること");
  });

  // W2 受け入れ条件の固定: 編集中バッファ（source）を送って
  // 自己完結 HTML が返るパスが通っていること
  test("render_html: source 指定で html に <!doctype html> を含む本文が返る", async () => {
    const res = await client.request({
      cmd: "render_html",
      source: SRC,
      file_name: "buf.c",
    });
    assert.equal(res.ok, true);
    assert.equal(typeof res.html, "string");
    assert.ok(
      res.html.includes("<!doctype html>"),
      "自己完結 HTML の doctype を含むこと"
    );
  });

  // サーバの「エラーでも JSON を1行返してプロセスは落ちない」契約の確認。
  // request() は JSON.stringify を通すため文法的に壊れた JSON は送れない。
  // 代わりに Request として解釈不能な cmd を送り、サーバ側の
  // 「リクエストのJSONを解釈できません」パス（serde の解釈失敗）を踏む
  test("解釈不能なリクエスト: ok:false が返り、続く正常リクエストが通る", async () => {
    const bad = await client.request({ cmd: "no_such_cmd" });
    assert.equal(bad.ok, false);
    assert.equal(typeof bad.error, "string");

    // プロセスが生きていれば次のリクエストは普通に成功する
    const good = await client.request({ cmd: "version" });
    assert.equal(good.ok, true);
  });

  test("dispose 後の request は reject される", async () => {
    client.dispose();
    await assert.rejects(
      client.request({ cmd: "version" }),
      /破棄済み/,
      "破棄済みクライアントは即 reject すること"
    );
  });
});

// FIFO 安全設計の中核: 「異常時は fail() で待機中を一斉 reject し、以後は
// dead 固定で即 reject」の3分岐（タイムアウト・異常死・JSONでない応答）。
// CowlClient は任意の command を spawn できるので、実バイナリなしで
// test/fixtures/ のダミーサーバを相手に**決定的に**再現する
describe("CowlClient の fail() 経路（実バイナリ不要のダミーサーバ）", () => {
  const FIXTURES = path.resolve(here, "fixtures");
  const clients = [];

  // git は実行ビットを保存するのでフィクスチャはコミット時点で実行可能だが、
  // core.fileMode=false な環境や Windows 経由のチェックアウトではビットが
  // 落ちることがある。テスト側でも毎回付与して環境非依存にする（二重化は
  // 安価で、欠けたときの症状 = spawn EACCES が分かりにくいため）
  before(() => {
    for (const f of ["silent.sh", "garbage.sh"]) {
      chmodSync(path.join(FIXTURES, f), 0o755);
    }
  });

  // プロセスリーク禁止: 各テストが作ったクライアントは必ず殺す
  afterEach(() => {
    while (clients.length > 0) {
      clients.pop().dispose();
    }
  });

  /** @param {string} command */
  const make = (command) => {
    const c = new CowlClient(command);
    clients.push(c);
    return c;
  };

  test("タイムアウト: fail で reject され、以後の request も即 reject される", async () => {
    // silent.sh は**絶対に**応答しないので、200ms は「遅い CI で偽陽性に
    // なる」揺れ方をしない（応答が遅れて来る実サーバ相手ではないため）
    const c = make(path.join(FIXTURES, "silent.sh"));
    await assert.rejects(c.request({ cmd: "version" }, 200), /タイムアウト/);
    // dead 固定の確認: 2発目はプロセスに触れず、同じ理由で即 reject
    await assert.rejects(c.request({ cmd: "version" }, 200), /タイムアウト/);
  });

  test("異常死: exit で fail し、以後の request は即 reject される", async () => {
    // `true` は引数 (serve --stdio) を無視して即 exit 0 する標準コマンド
    // （フィクスチャ不要）。書き込みと exit のレースで reject 理由が
    // 揺れないよう、exit イベントが dead を固定する（alive が落ちる）のを
    // 待ってから request し、「死後は必ず /終了/ で即 reject」を決定的に見る。
    // in-flight のリクエストが fail() で一斉 reject されること自体は
    // タイムアウト・JSONでない応答の2本が既にカバーしている
    const c = make("true");
    for (let i = 0; c.alive && i < 1000; i++) {
      await sleep(5);
    }
    assert.equal(c.alive, false, "exit イベントで dead が固定されること");
    await assert.rejects(c.request({ cmd: "version" }), /終了/);
  });

  test("JSON でない応答: fail し、以後の request も即 reject される (P1-1)", async () => {
    const c = make(path.join(FIXTURES, "garbage.sh"));
    await assert.rejects(c.request({ cmd: "version" }), /解釈できません/);
    // 枠組み不信でクライアントごと死ぬこと（1件の reject で済ませない）の固定
    await assert.rejects(c.request({ cmd: "version" }), /解釈できません/);
  });
});
