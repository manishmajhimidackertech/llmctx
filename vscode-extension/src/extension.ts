// <<<LLMCTX
// FILE: vscode-extension/src/extension.ts
// ROLE: VS Code extension entry point — wires save/rename/delete listeners, daemon client, status bar, and commands
// EXPORTS: activate(), deactivate()
// IMPORTS: vscode-extension/src/daemon.ts, vscode-extension/src/statusBar.ts, vscode-extension/src/pack.ts, vscode-extension/src/hash.ts
// USED BY: VS Code extension host
// NOTES: All disposables are registered on context.subscriptions; deactivate() is a no-op; the daemon is started on demand
// LLMCTX>>>

import * as vscode from "vscode";
import * as path from "path";
import * as fs from "fs";

import { DaemonClient, StatusMessage, UpdateAvailableMessage } from "./daemon";
import { StatusBarManager } from "./statusBar";
import { cliEnv, copyProjectMap, packFile, reindexFile, verifyCliOnPath } from "./pack";
import { contentHash } from "./hash";

// ── Extension lifecycle ───────────────────────────────────────────────────────

export function activate(context: vscode.ExtensionContext): void {
  const config = vscode.workspace.getConfiguration("llmctx");
  const barSide = config.get<string>("statusBarAlignment", "right");

  const alignment =
    barSide === "left"
      ? vscode.StatusBarAlignment.Left
      : vscode.StatusBarAlignment.Right;

  // ── Status bar ────────────────────────────────────────────────────────────
  const statusBar = new StatusBarManager(alignment, 100);
  context.subscriptions.push(statusBar);

  // ── Daemon client ─────────────────────────────────────────────────────────
  // The daemon publishes its port and a token in a per-user file; the client
  // finds it there, and starts llmctxd itself when none is running.
  const ollamaUrl = cliEnv().LLMCTX_OLLAMA_URL;
  const daemon = new DaemonClient({
    autoStart: config.get<boolean>("autoStartDaemon", true),
    daemonPath: config.get<string>("daemonPath", "").trim(),
    env: ollamaUrl ? { LLMCTX_OLLAMA_URL: ollamaUrl } : {},
  });

  daemon.on("connect", () => {
    statusBar.onDaemonConnect();
  });

  daemon.on("disconnect", () => {
    statusBar.onDaemonDisconnect();
  });

  daemon.on("status", (msg: StatusMessage) => {
    statusBar.setFileState(msg.path, msg.state, msg.message);
  });

  daemon.on("startFailed", (err: Error) => {
    const notFound = err.message.includes("ENOENT");
    void vscode.window.showWarningMessage(
      notFound
        ? "llmctx: could not start the daemon — `llmctxd` is not on your PATH. Install it, or set `llmctx.daemonPath`."
        : `llmctx: could not start the daemon: ${err.message}`
    );
  });

  daemon.on("updateAvailable", (msg: UpdateAvailableMessage) => {
    void vscode.window
      .showInformationMessage(
        `llmctx update available: ${msg.currentVersion} -> ${msg.latestVersion}`,
        "Download"
      )
      .then((choice) => {
        if (choice === "Download") {
          void vscode.env.openExternal(
            vscode.Uri.parse("https://github.com/manishmajhimidackertech/llmctx/releases/latest")
          );
        }
      });
  });

  daemon.start();

  context.subscriptions.push({
    dispose() {
      daemon.dispose();
    },
  });

  // Verify the CLI binary is reachable and warn immediately if not.
  void verifyCliOnPath();

  // ── Save listener ─────────────────────────────────────────────────────────
  //
  // Fired after VS Code has flushed the file to disk, so the daemon can read
  // it immediately without a race.
  context.subscriptions.push(
    vscode.workspace.onDidSaveTextDocument((doc) => {
      if (doc.uri.scheme !== "file") {
        return;
      }
      const filePath = doc.uri.fsPath;
      const text = doc.getText();
      const hash = contentHash(text);
      daemon.notifySave(filePath, hash);
    })
  );

  // ── Rename / delete listeners ─────────────────────────────────────────────
  //
  // Only operations made through VS Code (Explorer, refactorings) are seen
  // here. They keep stored context attached to the right paths, even when a
  // renamed file is also edited; anything done outside VS Code is caught by
  // content-hash matching and `llmctx index`/`gc` instead.
  context.subscriptions.push(
    vscode.workspace.onDidRenameFiles((e) => {
      for (const { oldUri, newUri } of e.files) {
        if (oldUri.scheme === "file" && newUri.scheme === "file") {
          daemon.notifyRename(oldUri.fsPath, newUri.fsPath);
        }
      }
    }),
    vscode.workspace.onDidDeleteFiles((e) => {
      for (const uri of e.files) {
        if (uri.scheme === "file") {
          daemon.notifyDelete(uri.fsPath);
        }
      }
    })
  );

  // ── Active editor listener (status bar refresh) ───────────────────────────
  context.subscriptions.push(
    vscode.window.onDidChangeActiveTextEditor((editor) => {
      statusBar.refresh(editor?.document.uri.fsPath);
    })
  );

  // Render immediately for whatever file is open at activation.
  statusBar.refresh(
    vscode.window.activeTextEditor?.document.uri.fsPath
  );

  // ── Commands ──────────────────────────────────────────────────────────────

  context.subscriptions.push(
    vscode.commands.registerCommand("llmctx.pack", async () => {
      const filePath = activeFilePath();
      if (!filePath) {
        void vscode.window.showWarningMessage(
          "llmctx: no active file to pack"
        );
        return;
      }
      await packFile(filePath);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("llmctx.packWithImports", async () => {
      const filePath = activeFilePath();
      if (!filePath) {
        void vscode.window.showWarningMessage(
          "llmctx: no active file to pack"
        );
        return;
      }
      await packFile(filePath, true);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("llmctx.copyProjectMap", async () => {
      const folder =
        (vscode.window.activeTextEditor &&
          vscode.workspace.getWorkspaceFolder(vscode.window.activeTextEditor.document.uri)
            ?.uri.fsPath) ??
        vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
      if (!folder) {
        void vscode.window.showWarningMessage("llmctx: open a folder first");
        return;
      }
      await copyProjectMap(folder);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("llmctx.reindex", async () => {
      const filePath = activeFilePath();
      if (!filePath) {
        void vscode.window.showWarningMessage(
          "llmctx: no active file to reindex"
        );
        return;
      }
      await reindexFile(filePath);
    })
  );

  context.subscriptions.push(
    vscode.commands.registerCommand("llmctx.openConfig", async () => {
      const configPath = findConfigInWorkspace();
      if (configPath) {
        const uri = vscode.Uri.file(configPath);
        await vscode.window.showTextDocument(uri);
      } else {
        const choice = await vscode.window.showInformationMessage(
          "No llmcontext.yaml found in the workspace. Create one?",
          "Create",
          "Cancel"
        );
        if (choice === "Create") {
          await runCliInit();
        }
      }
    })
  );
}

// deactivate() is intentionally empty — DaemonClient.dispose() is called via
// context.subscriptions, which VS Code drains on deactivation.
export function deactivate(): void {}

// ── Helpers ───────────────────────────────────────────────────────────────────

function activeFilePath(): string | undefined {
  const editor = vscode.window.activeTextEditor;
  if (!editor || editor.document.uri.scheme !== "file") {
    return undefined;
  }
  return editor.document.uri.fsPath;
}

/** Walk workspace folders looking for the nearest llmcontext.yaml. */
function findConfigInWorkspace(): string | undefined {
  for (const folder of vscode.workspace.workspaceFolders ?? []) {
    const candidate = path.join(folder.uri.fsPath, "llmcontext.yaml");
    if (fs.existsSync(candidate)) {
      return candidate;
    }
  }
  return undefined;
}

/** Run `llmctx init` in the first workspace folder root. */
async function runCliInit(): Promise<void> {
  const root = vscode.workspace.workspaceFolders?.[0]?.uri.fsPath;
  if (!root) {
    void vscode.window.showErrorMessage(
      "llmctx: no workspace folder open — cannot run init"
    );
    return;
  }

  const terminal = vscode.window.createTerminal({
    name: "llmctx init",
    cwd: root,
  });
  terminal.sendText("llmctx init", /* addNewLine */ true);
  terminal.show();

  // Give the terminal a moment then open the file if it appeared.
  await new Promise<void>((resolve) => setTimeout(resolve, 1_500));
  const created = path.join(root, "llmcontext.yaml");
  if (fs.existsSync(created)) {
    const uri = vscode.Uri.file(created);
    await vscode.window.showTextDocument(uri);
  }
}
