// <<<LLMCTX
// FILE: vscode-extension/src/daemon.ts
// ROLE: Client for llmctxd — finds it via the per-user discovery file, authenticates, starts it if needed, sends save/rename/delete events
// EXPORTS: DaemonClient, daemonFilePath(), StatusMessage, StatusState, UpdateAvailableMessage
// IMPORTS: NONE
// USED BY: vscode-extension/src/extension.ts
// NOTES: Discovery path must match crates/core/src/runtime.rs; pack never depends on this connection
// LLMCTX>>>

import * as net from "net";
import * as fs from "fs";
import * as os from "os";
import * as path from "path";
import { spawn } from "child_process";
import { EventEmitter } from "events";

// ── Protocol types (must match crates/daemon/src/main.rs) ────────────────────

type OutboundMessage =
  | { type: "hello"; token: string }
  | { type: "save"; path: string; hash: string }
  | { type: "rename"; from: string; to: string }
  | { type: "delete"; path: string };

export type StatusState = "queued" | "generating" | "ready" | "skipped" | "error";

export interface StatusMessage {
  type: "status";
  path: string;
  state: StatusState;
  message?: string;
}

export interface UpdateAvailableMessage {
  type: "updateAvailable";
  currentVersion: string;
  latestVersion: string;
}

interface WelcomeMessage {
  type: "welcome";
  version: string;
}

type InboundMessage = StatusMessage | UpdateAvailableMessage | WelcomeMessage;

interface DaemonInfo {
  port: number;
  token: string;
  pid: number;
  version: string;
}

// ── Discovery ────────────────────────────────────────────────────────────────

/**
 * Where llmctxd publishes its port and token. Mirrors `runtime_dir()` in
 * crates/core/src/runtime.rs — keep the two in sync.
 */
export function daemonFilePath(): string | undefined {
  const env = (k: string): string | undefined => process.env[k] || undefined;
  let dir: string | undefined;
  if (env("LLMCTX_RUNTIME_DIR")) {
    dir = env("LLMCTX_RUNTIME_DIR");
  } else if (process.platform === "win32") {
    const local = env("LOCALAPPDATA");
    dir = local ? path.join(local, "llmctx") : undefined;
  } else if (env("XDG_RUNTIME_DIR")) {
    dir = path.join(env("XDG_RUNTIME_DIR") as string, "llmctx");
  } else {
    dir = path.join(os.homedir(), ".cache", "llmctx");
  }
  return dir ? path.join(dir, "daemon.json") : undefined;
}

function readDaemonInfo(): DaemonInfo | undefined {
  const file = daemonFilePath();
  if (!file) {
    return undefined;
  }
  try {
    const info = JSON.parse(fs.readFileSync(file, "utf8")) as DaemonInfo;
    return typeof info.port === "number" && typeof info.token === "string"
      ? info
      : undefined;
  } catch {
    return undefined;
  }
}

// ── Client ───────────────────────────────────────────────────────────────────

const RECONNECT_BASE_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;
const RECONNECT_FACTOR = 2;

export interface DaemonOptions {
  /** Start llmctxd when none is running. */
  autoStart: boolean;
  /** Binary to start; "llmctxd" resolves via PATH. */
  daemonPath: string;
  /** Extra environment for a daemon we start (e.g. LLMCTX_OLLAMA_URL). */
  env: NodeJS.ProcessEnv;
}

/**
 * One authenticated connection to llmctxd, re-established as needed.
 *
 * Emits:
 *   "status"          (msg: StatusMessage)
 *   "updateAvailable" (msg: UpdateAvailableMessage)
 *   "connect"         ()  — after the daemon accepted our token
 *   "disconnect"      ()
 *   "startFailed"     (err: Error) — auto-start could not launch llmctxd
 */
export class DaemonClient extends EventEmitter {
  private socket: net.Socket | null = null;
  private connected = false;
  private disposed = false;
  private reconnectDelay = RECONNECT_BASE_MS;
  private reconnectTimer: NodeJS.Timeout | null = null;
  private lineBuffer = "";
  private startAttempted = false;

  constructor(private readonly options: DaemonOptions) {
    super();
  }

  /** Start the connection (and auto-reconnect loop). */
  start(): void {
    this.connect();
  }

  /** Fire-and-forget; silently dropped while disconnected. */
  notifySave(filePath: string, hash: string): void {
    this.send({ type: "save", path: filePath, hash });
  }

  notifyRename(from: string, to: string): void {
    this.send({ type: "rename", from, to });
  }

  notifyDelete(filePath: string): void {
    this.send({ type: "delete", path: filePath });
  }

  /** Tear down the connection and stop reconnecting. */
  dispose(): void {
    this.disposed = true;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
    }
    this.socket?.destroy();
  }

  // ── Private ──────────────────────────────────────────────────────────────

  private connect(): void {
    if (this.disposed) {
      return;
    }

    // Re-read every time: the daemon picks a new port and token per start.
    const info = readDaemonInfo();
    if (!info) {
      this.startDaemonOnce();
      this.scheduleReconnect();
      return;
    }

    const socket = new net.Socket();
    this.socket = socket;
    this.lineBuffer = "";

    socket.connect(info.port, "127.0.0.1", () => {
      this.writeRaw({ type: "hello", token: info.token });
    });

    // Data arrives as a stream; split on newlines (NDJSON).
    socket.on("data", (chunk: Buffer) => {
      this.lineBuffer += chunk.toString("utf8");
      const lines = this.lineBuffer.split("\n");
      // Last element is either empty or a partial line — keep it in the buffer.
      this.lineBuffer = lines.pop() ?? "";
      for (const line of lines) {
        const trimmed = line.trim();
        if (!trimmed) {
          continue;
        }
        try {
          this.dispatch(JSON.parse(trimmed) as InboundMessage);
        } catch {
          // Malformed JSON from the daemon — ignore silently.
        }
      }
    });

    socket.on("error", () => {
      // Error always precedes close; handled there.
    });

    socket.on("close", () => {
      const wasConnected = this.connected;
      this.connected = false;
      this.socket = null;
      if (wasConnected) {
        this.emit("disconnect");
      } else {
        // Never got a welcome: a stale discovery file (daemon gone) or a
        // token mismatch. Starting a daemon fixes the first case; one that
        // is already running just exits.
        this.startDaemonOnce();
      }
      this.scheduleReconnect();
    });
  }

  private dispatch(msg: InboundMessage): void {
    switch (msg.type) {
      case "welcome":
        this.connected = true;
        this.reconnectDelay = RECONNECT_BASE_MS;
        this.emit("connect");
        break;
      case "status":
        this.emit("status", msg);
        break;
      case "updateAvailable":
        this.emit("updateAvailable", msg);
        break;
    }
  }

  /** Launch a detached llmctxd, at most once per session. */
  private startDaemonOnce(): void {
    if (!this.options.autoStart || this.startAttempted) {
      return;
    }
    this.startAttempted = true;
    try {
      const child = spawn(this.options.daemonPath || "llmctxd", [], {
        detached: true,
        stdio: "ignore",
        windowsHide: true,
        env: { ...process.env, ...this.options.env },
      });
      child.on("error", (err) => this.emit("startFailed", err));
      // Outlive this window: other windows and later sessions share it.
      child.unref();
    } catch (err) {
      this.emit("startFailed", err instanceof Error ? err : new Error(String(err)));
    }
  }

  private scheduleReconnect(): void {
    if (this.disposed) {
      return;
    }
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
    }
    this.reconnectTimer = setTimeout(() => {
      this.reconnectDelay = Math.min(
        this.reconnectDelay * RECONNECT_FACTOR,
        RECONNECT_MAX_MS
      );
      this.connect();
    }, this.reconnectDelay);
  }

  private send(msg: OutboundMessage): void {
    if (this.connected && this.socket) {
      this.writeRaw(msg);
    }
  }

  private writeRaw(msg: OutboundMessage): void {
    try {
      this.socket?.write(JSON.stringify(msg) + "\n", "utf8");
    } catch {
      // Socket died between the connected check and the write — harmless.
    }
  }
}
