// <<<LLMCTX
// FILE: vscode-extension/src/pack.ts
// ROLE: Shell out to the llmctx CLI for pack/map/reindex and put results on the clipboard via VS Code
// EXPORTS: packFile(), copyProjectMap(), reindexFile(), verifyCliOnPath(), cliEnv()
// IMPORTS: NONE
// USED BY: vscode-extension/src/extension.ts
// NOTES: The CLI prints (`--stdout`); the extension writes the clipboard, which works on every OS
// LLMCTX>>>

import * as vscode from "vscode";
import * as fs from "fs";
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
              "https://github.com/manishmajhimidackertech/llmctx#installation"
            )
          );
        }
      });
  }
}

// ── pack ──────────────────────────────────────────────────────────────────────

/** Large projects produce large packs; execFile's 1 MB default is too small. */
const MAX_OUTPUT_BYTES = 64 * 1024 * 1024;

/**
 * Environment for CLI calls: the user's `llmctx.ollamaUrl`, if set, is passed
 * as LLMCTX_OLLAMA_URL — the only way to point llmctx at a remote Ollama.
 */
export function cliEnv(): NodeJS.ProcessEnv {
  const url = vscode.workspace.getConfiguration("llmctx").get<string>("ollamaUrl", "").trim();
  return url ? { ...process.env, LLMCTX_OLLAMA_URL: url } : process.env;
}

/**
 * Pack the file at `filePath` (optionally with the context of the files it
 * imports) and put the result on the clipboard.
 *
 * The CLI owns the store and the formatting; it prints the result and the
 * extension copies it with VS Code's own clipboard API, which works the same
 * on every platform (a CLI process that sets the clipboard and exits loses
 * the contents on Linux).
 */
export async function packFile(filePath: string, withImports = false): Promise<void> {
  await vscode.window.withProgress(
    { location: vscode.ProgressLocation.Window, title: "llmctx: packing…" },
    async () => {
      try {
        const bin = resolveCliBinary();
        const args = ["pack", filePath, "--stdout"];
        if (withImports) {
          args.push("--with-imports");
        }
        const { stdout } = await execFileAsync(bin, args, {
          env: cliEnv(),
          timeout: 30_000,
          maxBuffer: MAX_OUTPUT_BYTES,
        });
        await vscode.env.clipboard.writeText(stdout);
        if (stdout.includes("[no context yet")) {
          void vscode.window.showWarningMessage(
            "llmctx: packed to clipboard, but this file has no context yet — save it, or run `llmctx index`"
          );
        } else {
          void vscode.window.showInformationMessage(
            "llmctx: packed to clipboard — paste into any LLM"
          );
        }
      } catch (err: unknown) {
        handleCliError(err, "pack");
      }
    }
  );
}

/** Copy the one-line-per-file project map for `folder` to the clipboard. */
export async function copyProjectMap(folder: string): Promise<void> {
  try {
    const bin = resolveCliBinary();
    const { stdout } = await execFileAsync(bin, ["map", folder], {
      env: cliEnv(),
      timeout: 30_000,
      maxBuffer: MAX_OUTPUT_BYTES,
    });
    await vscode.env.clipboard.writeText(stdout);
    void vscode.window.showInformationMessage("llmctx: project map copied to clipboard");
  } catch (err: unknown) {
    handleCliError(err, "map");
  }
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
        const { stdout } = await execFileAsync(bin, ["reindex", filePath], {
          env: cliEnv(),
          timeout: 600_000,
        });
        void vscode.window.showInformationMessage(`llmctx: ${stdout.trim()}`);
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
            vscode.Uri.parse("https://github.com/manishmajhimidackertech/llmctx#installation")
          );
        }
      });
  } else {
    void vscode.window.showErrorMessage(
      `llmctx ${command} failed: ${err.message}`
    );
  }
}
