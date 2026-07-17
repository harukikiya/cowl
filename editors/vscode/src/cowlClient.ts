// cowl serve --stdio の行指向 JSON クライアント。
//
// 【なぜ vscode API 非依存か】このモジュールは node:test から実バイナリを
// 相手に直接回す（test/cowlClient.test.mjs）。VS Code 本体を起動する E2E は
// コンテナに X サーバが無く自動化できないため、W2 受け入れ条件のうち
// 「編集中バッファを source で送るパスが通っている」はこの層のテストで
// 固定する（ADR-0005）。したがってここに `import "vscode"` を書いてはならない。
// vscode 由来の都合（設定・通知など）はすべて extension.ts 側に置く。

import { spawn, type ChildProcessWithoutNullStreams } from "node:child_process";
import * as readline from "node:readline";

/**
 * cowl-api のレスポンスエンベロープ。クライアントが依存してよい契約は
 * 「必ず ok を持つ JSON が1行返る」ことだけ。html / error / report 等の
 * 中身はコマンドごとに異なるので、ここでは解釈せず呼び出し側へ素通しする
 * （JSON 契約をクライアント側で再実装・再解釈しない: CLAUDE.md）。
 */
export interface CowlResponse {
  ok: boolean;
  api_version?: string;
  [key: string]: unknown;
}

/** 応答待ちの1件。timer は per-request タイムアウト（下記の注参照） */
interface Pending {
  resolve: (res: CowlResponse) => void;
  reject: (err: Error) => void;
  timer: NodeJS.Timeout;
}

/**
 * spawn した `cowl serve --stdio` と 1行1JSON で往復するクライアント。
 *
 * 【FIFO 対応付けの前提】cowl の serve ループはリクエストを**逐次**処理する
 * （cowl-cli/src/main.rs serve_stdio: 1行読む→1行返す、並行多重化なし）。
 * だからリクエスト ID は不要で、「送った順に応答が返る」ことを前提に
 * キュー先頭の Promise を resolve すればよい。サーバが並行処理に変わる日が
 * 来たら、この対応付けは ID ベースに作り直すこと。
 *
 * 【死んだら使い回さない】プロセスの exit / spawn 失敗 / タイムアウト /
 * JSON でない応答行、のいずれかが起きたら fail() で死亡が固定され、
 * このインスタンスは以後すべての request を即 reject する。
 * 中途半端に生き返らせると FIFO の対応付けがずれて「別のリクエストの
 * 応答」を返す事故になるため。再 spawn は呼び出し側（extension.ts）の責務。
 */
export class CowlClient {
  private readonly proc: ChildProcessWithoutNullStreams;
  private queue: Pending[] = [];
  /** 一度 fail したら理由をここに固定し、以後の request は即 reject */
  private dead: Error | null = null;
  /** stderr は捨てない。失敗時の reject メッセージに添えてデバッグ可能にする */
  private stderrBuf = "";

  constructor(command: string) {
    // 引数はサーバ起動の固定形。オプションを増やすときも「cowl 側の
    // CLI 契約」をここ1箇所に閉じ込める
    this.proc = spawn(command, ["serve", "--stdio"]);

    // stdout の行分割は readline に任せる。チャンク境界と JSON の境界は
    // 一致しない（大きな HTML は複数チャンクで届く）ので自前 split は危険
    const rl = readline.createInterface({ input: this.proc.stdout });
    rl.on("line", (line) => this.onLine(line));

    this.proc.stderr.on("data", (chunk: Buffer) => {
      this.stderrBuf += chunk.toString();
      // 暴走したサーバがメモリを食い潰さないよう末尾だけ保持
      if (this.stderrBuf.length > 64 * 1024) {
        this.stderrBuf = this.stderrBuf.slice(-64 * 1024);
      }
    });

    // 死んだ子プロセスの stdin への書き込み失敗（EPIPE 等）は、write の
    // コールバックとは**別に** stream の 'error' イベントとしても届く。
    // リスナが無いと Node はこれを未処理例外にしてホストプロセスごと
    // 落とすため、必ずここで受けて fail() に回収する
    this.proc.stdin.on("error", (err: NodeJS.ErrnoException) => {
      this.fail(
        new Error(`cowl への書き込みに失敗: ${err.message}${this.stderrSuffix()}`)
      );
    });

    // spawn 失敗（バイナリ不在 = ENOENT など）はここに来る。呼び出し側が
    // 「serverPath を設定せよ」と案内できるよう errno コードを引き継ぐ
    this.proc.on("error", (err: NodeJS.ErrnoException) => {
      const e: NodeJS.ErrnoException = new Error(
        `cowl プロセスを起動できません: ${err.message}`
      );
      e.code = err.code;
      this.fail(e);
    });

    // serve は stdin EOF まで生き続ける設計なので、exit は
    // 「dispose した」か「異常死」のどちらか。前者なら fail 済みで no-op
    this.proc.on("exit", (code, signal) => {
      this.fail(
        new Error(
          `cowl プロセスが終了しました (code=${code}, signal=${signal})${this.stderrSuffix()}`
        )
      );
    });
  }

  /** 呼び出し側が「作り直すべきか」を判断するためのフラグ */
  get alive(): boolean {
    return this.dead === null;
  }

  /**
   * 1リクエスト送って応答 JSON を返す。
   * タイムアウトはリクエスト単位だが、発火したら**クライアントごと殺す**。
   * FIFO 対応付けのため、1件だけキューから抜くと以後の応答が全部1つずつ
   * ずれてしまい、静かに誤対応するよりプロセスを落とす方が安全だから。
   */
  request(req: object, timeoutMs = 10000): Promise<CowlResponse> {
    if (this.dead !== null) {
      return Promise.reject(this.dead);
    }
    return new Promise<CowlResponse>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.fail(
          new Error(`cowl の応答がタイムアウト (${timeoutMs}ms)${this.stderrSuffix()}`)
        );
        this.proc.kill();
      }, timeoutMs);
      this.queue.push({ resolve, reject, timer });
      try {
        // JSON.stringify は文字列中の改行を必ず \n にエスケープするので、
        // 「1行=1リクエスト」の行プロトコルをソース本文が壊すことはない
        this.proc.stdin.write(JSON.stringify(req) + "\n", (err) => {
          if (err) {
            this.fail(
              new Error(`cowl への書き込みに失敗: ${err.message}${this.stderrSuffix()}`)
            );
          }
        });
      } catch (err) {
        this.fail(
          new Error(`cowl への書き込みに失敗: ${(err as Error).message}${this.stderrSuffix()}`)
        );
      }
    });
  }

  /** プロセスを止める。以後の request は「破棄済み」で即 reject */
  dispose(): void {
    this.fail(new Error("CowlClient は破棄済みです"));
    this.proc.kill();
  }

  private onLine(line: string): void {
    if (line.trim() === "") {
      return; // サーバは空行を出さない契約だが、行プロトコルの作法として無視
    }
    const pending = this.queue.shift();
    if (pending === undefined) {
      // 対応するリクエストが無い行。逐次サーバの契約上起きないはずだが、
      // 起きても落とすほどではない（次の対応付けはずれない: 余剰行なので）
      return;
    }
    clearTimeout(pending.timer);
    let res: CowlResponse;
    try {
      res = JSON.parse(line) as CowlResponse;
    } catch {
      // 行が JSON として壊れているなら「1行=1JSON」という枠組みそのものが
      // 信用できない（途中で割れた行かもしれず、以後どの行がどのリクエストの
      // 応答かを保証できない）。該当リクエストを reject するだけでなく、
      // タイムアウト・write失敗・exit と同じ fail() 経路に合流させて
      // クライアントごと殺す。誤った対応付けで応答を返すより安全
      const err = new Error(
        `cowl の応答を JSON として解釈できません: ${line.slice(0, 200)}`
      );
      pending.reject(err);
      this.fail(err);
      this.proc.kill();
      return;
    }
    pending.resolve(res);
  }

  /** 死亡処理は一度だけ。待機中の全 Promise を同じ理由で reject する */
  private fail(err: Error): void {
    if (this.dead !== null) {
      return;
    }
    this.dead = err;
    const pending = this.queue;
    this.queue = [];
    for (const p of pending) {
      clearTimeout(p.timer);
      p.reject(err);
    }
  }

  private stderrSuffix(): string {
    const s = this.stderrBuf.trim();
    return s === "" ? "" : `\n--- cowl stderr ---\n${s}`;
  }
}
