// <<<LLMCTX
// FILE: vscode-extension/src/daemon.ts
// ROLE: TCP socket client for llmctxd — sends save notifications, receives status updates
// EXPORTS: DaemonClient
// IMPORTS: NONE
// USED BY: vscode-extension/src/extension.ts
// NOTES: Auto-reconnects with exponential backoff; pack never depends on this connection
// LLMCTX>>>

import * as net from "net";
import { EventEmitter } from "events";

// ── Protocol types (must match crates/daemon/src/main.rs) ────────────────────

export interface SaveMessage {
  type: "save";
  path: string;
  hash: string;
}

export type StatusState = "queued" | "generating" | "ready" | "error";

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

type OutboundMessage = SaveMessage;
type InboundMessage = StatusMessage | UpdateAvailableMessage;

// ── Client ───────────────────────────────────────────────────────────────────

const RECONNECT_BASE_MS = 1_000;
const RECONNECT_MAX_MS = 30_000;
const RECONNECT_FACTOR = 2;

/**
 * Thin wrapper around a single TCP connection to llmctxd.
 *
 * Emits:
 *   "status"  (msg: StatusMessage) — on every inbound status push from the daemon
 *   "connect" ()                   — when (re)connected
 *   "disconnect" ()                — when the socket drops
 */
export class DaemonClient extends EventEmitter {
  private socket: net.Socket | null = null;
  private port: number;
  private connected = false;
  private disposed = false;
  private reconnectDelay = RECONNECT_BASE_MS;
  private reconnectTimer: NodeJS.Timeout | null = null;
  private lineBuffer = "";

  constructor(port: number) {
    super();
    this.port = port;
  }

  /** Start the connection (and auto-reconnect loop). */
  start(): void {
    this.connect();
  }

  /** Send a save notification. Fire-and-forget; silently drops if disconnected. */
  notifySave(filePath: string, hash: string): void {
    if (!this.connected || !this.socket) {
      return;
    }
    const msg: SaveMessage = { type: "save", path: filePath, hash };
    this.sendRaw(msg);
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

    const socket = new net.Socket();
    this.socket = socket;

    socket.connect(this.port, "127.0.0.1", () => {
      this.connected = true;
      this.reconnectDelay = RECONNECT_BASE_MS;
      this.lineBuffer = "";
      this.emit("connect");
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
          const msg = JSON.parse(trimmed) as InboundMessage;
          if (msg.type === "status") {
            this.emit("status", msg);
          } else if (msg.type === "updateAvailable") {
            this.emit("updateAvailable", msg);
          }
        } catch {
          // Malformed JSON from the daemon — ignore silently.
        }
      }
    });

    socket.on("error", () => {
      // Error always precedes close; handled there.
    });

    socket.on("close", () => {
      this.connected = false;
      this.socket = null;
      this.emit("disconnect");
      this.scheduleReconnect();
    });
  }

  private scheduleReconnect(): void {
    if (this.disposed) {
      return;
    }
    this.reconnectTimer = setTimeout(() => {
      this.reconnectDelay = Math.min(
        this.reconnectDelay * RECONNECT_FACTOR,
        RECONNECT_MAX_MS
      );
      this.connect();
    }, this.reconnectDelay);
  }

  private sendRaw(msg: OutboundMessage): void {
    try {
      this.socket?.write(JSON.stringify(msg) + "\n", "utf8");
    } catch {
      // Socket died between the connected check and the write — harmless.
    }
  }
}
