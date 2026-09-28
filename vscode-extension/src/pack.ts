// <<<LLMCTX
// FILE: vscode-extension/src/pack.ts
// ROLE: Read stored context via the CLI and merge it with source text for clipboard paste
// EXPORTS: packFile(), reindexFile(), verifyCliOnPath()
// IMPORTS: NONE
// USED BY: vscode-extension/src/extension.ts
// NOTES: Shells out to `llmctx pack` which reads the .llmctx store; detects missing binary with clear guidance
// LLMCTX>>>

import * as vscode from "vscode";
import * as fs from "fs";
import * as path from "path";
import { execFile } from "child_process";
import { promisify } from "util";

const execFileAsync = promisify(execFile);

// ── CLI binary resolution ─────────────────────────────────────────────────────

/**
 * Locate the `llmctx` binary.
 *
 * Resolution order:
 *   1. `llmctx.cliPath` VS Code setting (allows the user to override).
 *   2. `llmctx` on the system PATH (standard install via `cpctx setup`).
 *
 * Returns the resolved path string, or throws with a human-readable message
 * that includes setup instructions if the binary cannot be found.
 */
function resolveCliBinary(): string {
  const config = vscode.workspace.getConfiguration("llmctx");
  const override = config.get<string>("cliPath", "").trim();
  if (override) {
    if (!fs.existsSync(override)) {
      throw new Error(
        `llmctx.cliPath is set to "${override}" but no file exists there. ` +
          `Update the setting or remove it to use PATH resolution.`
      );
    }
    return override;
  }
  // Fall back to plain name — execFile will search PATH.
  return "llmctx";
}

/**
 * Check whether the `llmctx` binary is reachable and show a one-time
 * actionable error if not.  Call this from `activate()` so the user
 * finds out immediately, not only when they first try to pack.
 */
export async function verifyCliOnPath(): Promise<void> {
  try {
    const bin = resolveCliBinary();
    await execFileAsync(bin, ["--version"], { timeout: 5_000 });
  } catch (err: unknown) {
    const isNotFound =
      err instanceof Error &&
      (err.message.includes("ENOENT") || err.message.includes("not found"));

    const msg = isNotFound
      ? "llmctx binary not found on PATH. Build it with `cargo build --release -p llmctx` then run `cpctx setup` to register it."
      : `llmctx binary check failed: ${err instanceof Error ? err.message : String(err)}`;

    void vscode.window
      .showErrorMessage(msg, "Open README")
      .then((choice) => {
        if (choice === "Open README") {
          void vscode.env.openExternal(
            vscode.Uri.parse(
              "https://github.com/your-org/llmctx#installation"
            )
          );
        }
      });
  }
}

// ── pack ──────────────────────────────────────────────────────────────────────

/**
 * Pack the file at `filePath` to the system clipboard via `llmctx pack`.
 *
 * The CLI binary owns the context-store read logic so we shell out to it
 * rather than reimplementing SQLite access in TypeScript.
 */
export async function packFile(filePath: string): Promise<void> {
  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Window, title: "llmctx: packing…" },
    async () => {
      try {
        const bin = resolveCliBinary();
        await execFileAsync(bin, ["pack", filePath], {
          env: process.env,
          timeout: 30_000,
        });
        void vscode.window.showInformationMessage(
          "llmctx: packed to clipboard — paste into any LLM"
        );
      } catch (err: unknown) {
        handleCliError(err, "pack");
      }
    }
  );
}

// ── reindex ───────────────────────────────────────────────────────────────────

/**
 * Force Ollama regeneration for the file at `filePath`.
 */
export async function reindexFile(filePath: string): Promise<void> {
  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Window, title: "llmctx: reindexing…" },
    async () => {
      try {
        const bin = resolveCliBinary();
        await execFileAsync(bin, ["reindex", filePath], {
          env: process.env,
          timeout: 120_000,
        });
        void vscode.window.showInformationMessage("llmctx: reindex complete");
      } catch (err: unknown) {
        handleCliError(err, "reindex");
      }
    }
  );
}

// ── Error handling ────────────────────────────────────────────────────────────

function handleCliError(err: unknown, command: string): void {
  if (!(err instanceof Error)) {
    void vscode.window.showErrorMessage(`llmctx ${command} failed: ${String(err)}`);
    return;
  }

  const isNotFound =
    err.message.includes("ENOENT") || err.message.includes("not found");

  if (isNotFound) {
    void vscode.window
      .showErrorMessage(
        "llmctx binary not found. Run `cargo build --release -p llmctx` then `cpctx setup`.",
        "Open README"
      )
      .then((choice) => {
        if (choice === "Open README") {
          void vscode.env.openExternal(
            vscode.Uri.parse("https://github.com/your-org/llmctx#installation")
          );
        }
      });
  } else {
    void vscode.window.showErrorMessage(
      `llmctx ${command} failed: ${err.message}`
    );
  }
}
