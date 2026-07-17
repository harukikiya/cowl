// VS Code 側の殻。cowl-cli / cowl-mcp と同じく「入力を cowl-api の
// Request に写して結果を表示する」だけに徹し、解析ロジックは一切持たない。
// stdio の往復は cowlClient.ts に隔離してあるので、このファイルは
// vscode API との接着（コマンド・設定・Webview・イベント購読）のみを扱う。

import * as vscode from "vscode";
import * as path from "node:path";
import { CowlClient } from "./cowlClient";

/**
 * クライアントはシングルトン。cowl serve は逐次処理サーバなので、
 * 1プロセスを使い回して FIFO で並べる方が、リクエストごとの spawn
 * （tree-sitter 初期化を毎回払う）より速く、プロセスリークも起きない。
 */
let client: CowlClient | null = null;

/** レポートパネルも1枚を再利用する。コマンド連打でパネルが増殖しないため */
let panel: vscode.WebviewPanel | undefined;

/** いま帯レポートを追従させている対象。パネル生存中のみ意味を持つ */
let trackedDoc: vscode.TextDocument | undefined;

let debounceTimer: NodeJS.Timeout | undefined;

export function activate(context: vscode.ExtensionContext): void {
  context.subscriptions.push(
    vscode.commands.registerCommand("cowl.showReport", () => {
      void showReport();
    })
  );
}

export function deactivate(): void {
  // パネルの dispose 1発で足りる: 後始末（changeSub の解除・デバウンス
  // タイマーの破棄・trackedDoc のクリア）は onDidDispose ハンドラに
  // 集約してあるので、dispose を呼べばそこへ連鎖する。ここで個別に
  // 片付けると後始末の経路が二重になり、将来の追加漏れの温床になる
  panel?.dispose();
  client?.dispose();
  client = null;
}

/**
 * 生きているクライアントを返す。死んでいたら作り直す。
 * 死んだインスタンスを使い回さないのは cowlClient.ts の設計
 * （FIFO の対応付けずれ防止）で、再 spawn はこちらの責務。
 */
function getClient(): CowlClient {
  if (client === null || !client.alive) {
    client?.dispose();
    const serverPath = vscode.workspace
      .getConfiguration("cowl")
      .get<string>("serverPath", "cowl");
    client = new CowlClient(serverPath);
  }
  return client;
}

async function showReport(): Promise<void> {
  const editor = vscode.window.activeTextEditor;
  if (editor === undefined || editor.document.languageId !== "c") {
    vscode.window.showInformationMessage(
      "cowl: アクティブなエディタが C ファイルではありません（languageId=c のみ対象）"
    );
    return;
  }
  trackedDoc = editor.document;

  if (panel === undefined) {
    panel = vscode.window.createWebviewPanel(
      "cowl.report",
      "cowl: Ownership Report",
      vscode.ViewColumn.Beside,
      {
        // cowl の HTML はホバー強調用の inline script を含む
        // （cowl-core/src/render_html.rs）ので script を許可する。
        // 自己完結 HTML で外部リソース読み込みはゼロなので露出は最小
        // （ADR-0005）
        enableScripts: true,
      }
    );

    // 「保存不要で更新が追従する」の実体: バッファ変更を購読し、
    // 編集中バッファ全文を source で送り直す。300ms のデバウンスは
    // キー入力ごとに解析を走らせると逐次サーバの応答キューが渋滞して
    // 表示が入力から遅れていく一方になるため（タイピング停止後に1回だけ走らせる）
    const changeSub = vscode.workspace.onDidChangeTextDocument((e) => {
      if (trackedDoc === undefined || e.document !== trackedDoc) {
        return;
      }
      if (debounceTimer !== undefined) {
        clearTimeout(debounceTimer);
      }
      const doc = trackedDoc;
      debounceTimer = setTimeout(() => {
        debounceTimer = undefined;
        void renderInto(doc);
      }, 300);
    });

    // パネルを閉じたら購読も止める。パネル無しで解析だけ走り続けるのは無駄
    panel.onDidDispose(() => {
      changeSub.dispose();
      if (debounceTimer !== undefined) {
        clearTimeout(debounceTimer);
        debounceTimer = undefined;
      }
      panel = undefined;
      trackedDoc = undefined;
    });
  } else {
    panel.reveal(vscode.ViewColumn.Beside, true);
  }

  panel.title = `cowl: ${path.basename(trackedDoc.fileName)}`;
  await renderInto(trackedDoc);
}

/**
 * 編集中バッファ全文を render_html に送り、返った自己完結 HTML を
 * そのまま Webview に流し込む。エンベロープの ok / html / error を
 * 見るだけで、中身の解釈はしない（JSON 契約の再実装をしない: CLAUDE.md）
 */
async function renderInto(doc: vscode.TextDocument): Promise<void> {
  try {
    const res = await getClient().request({
      cmd: "render_html",
      source: doc.getText(),
      file_name: path.basename(doc.fileName),
    });
    if (panel === undefined) {
      return; // 応答を待つ間にパネルが閉じられた
    }
    if (res.ok === true && typeof res.html === "string") {
      panel.webview.html = res.html;
    } else {
      const msg = typeof res.error === "string" ? res.error : JSON.stringify(res);
      vscode.window.showErrorMessage(`cowl: ${msg}`);
    }
  } catch (err) {
    // ENOENT = serverPath の先にバイナリが無い。一番踏みやすい失敗なので
    // 直し方（設定名）まで案内する
    if ((err as NodeJS.ErrnoException).code === "ENOENT") {
      vscode.window.showErrorMessage(
        "cowl バイナリが見つかりません。`cargo build -p cowl-cli` を実行し、" +
          "設定 cowl.serverPath に実行ファイルの絶対パス（例: <repo>/target/debug/cowl）を指定してください"
      );
    } else {
      vscode.window.showErrorMessage(`cowl: ${(err as Error).message}`);
    }
  }
}
