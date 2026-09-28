// <<<LLMCTX
// FILE: vscode-extension/src/statusBar.ts
// ROLE: Manage the status bar item that shows per-file llmctx state and triggers pack on click
// EXPORTS: StatusBarManager
// IMPORTS: NONE
// USED BY: vscode-extension/src/extension.ts
// NOTES: One shared item; re-rendered on every active-editor change and every daemon status push
// LLMCTX>>>

import * as vscode from "vscode";
import { StatusState } from "./daemon";

interface FileState {
  state: StatusState;
  message?: string;
}

const ICON: Record<StatusState, string> = {
  queued: "$(clock)",
  generating: "$(sync~spin)",
  ready: "$(check)",
  error: "$(warning)",
};

const TOOLTIP: Record<StatusState, string> = {
  queued: "llmctx: queued for context generation",
  generating: "llmctx: generating context via Ollama…",
  ready: "llmctx: context ready — click to pack to clipboard",
  error: "llmctx: context generation failed — click to pack anyway",
};

/**
 * Owns the single status bar item shown at the right of the bar.
 *
 * Callers update it by calling `setFileState()` whenever a daemon status
 * message arrives for a file, and `refresh()` whenever the active editor
 * changes.
 */
export class StatusBarManager implements vscode.Disposable {
  private item: vscode.StatusBarItem;
  private fileStates = new Map<string, FileState>();
  /** True once the daemon has connected at least once. */
  private daemonSeen = false;

  constructor(alignment: vscode.StatusBarAlignment, priority: number) {
    this.item = vscode.window.createStatusBarItem(alignment, priority);
    this.item.command = "llmctx.pack";
    this.item.show();
    this.render(undefined);
  }

  /** Called when the daemon connects for the first time. */
  onDaemonConnect(): void {
    this.daemonSeen = true;
    this.render(vscode.window.activeTextEditor?.document.uri.fsPath);
  }

  /** Called when the daemon disconnects. */
  onDaemonDisconnect(): void {
    // Keep showing last known file state — daemon may reconnect momentarily.
    // Only show the disconnected badge if we've never connected this session.
    if (!this.daemonSeen) {
      this.item.text = "$(plug) llmctx";
      this.item.tooltip = "llmctx: daemon not running — start llmctxd";
      this.item.backgroundColor = new vscode.ThemeColor(
        "statusBarItem.warningBackground"
      );
    }
  }

  /** Record a state update for a specific file path. */
  setFileState(filePath: string, state: StatusState, message?: string): void {
    this.fileStates.set(filePath, { state, message });
    // Only repaint if this file is the active one.
    const active = vscode.window.activeTextEditor?.document.uri.fsPath;
    if (active === filePath) {
      this.render(filePath);
    }
  }

  /** Repaint for the currently active editor. Call on editor focus changes. */
  refresh(filePath: string | undefined): void {
    this.render(filePath);
  }

  dispose(): void {
    this.item.dispose();
  }

  // ── Private ──────────────────────────────────────────────────────────────

  private render(filePath: string | undefined): void {
    if (!filePath) {
      // No active text editor.
      this.item.text = "$(database) llmctx";
      this.item.tooltip = "llmctx: open a file to see its context state";
      this.item.backgroundColor = undefined;
      return;
    }

    const fileState = this.fileStates.get(filePath);

    if (!fileState) {
      // File hasn't been seen by the daemon yet (e.g. never saved this session,
      // or context already exists from a previous session so no notification was sent).
      this.item.text = "$(database) llmctx";
      this.item.tooltip =
        "llmctx: save this file to update its context, or click to pack";
      this.item.backgroundColor = undefined;
      return;
    }

    const { state, message } = fileState;
    this.item.text = `${ICON[state]} llmctx`;
    this.item.tooltip =
      message != null
        ? `${TOOLTIP[state]}\n${message}`
        : TOOLTIP[state];

    this.item.backgroundColor =
      state === "error"
        ? new vscode.ThemeColor("statusBarItem.warningBackground")
        : undefined;
  }
}
