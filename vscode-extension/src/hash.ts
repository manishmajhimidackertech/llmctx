// <<<LLMCTX
// FILE: vscode-extension/src/hash.ts
// ROLE: Compute SHA-256 hex digest of a string, matching process::content_hash() on the Rust side
// EXPORTS: contentHash()
// IMPORTS: NONE
// USED BY: vscode-extension/src/extension.ts
// NOTES: Uses Node's built-in crypto — no external deps; must produce identical output to the Rust sha2 impl
// LLMCTX>>>

import { createHash } from "crypto";

/**
 * Returns the lowercase SHA-256 hex digest of `content`.
 *
 * Must produce byte-for-byte identical output to `process::content_hash()` in
 * crates/core/src/process.rs so the daemon can detect unchanged files and skip
 * redundant Ollama calls.
 */
export function contentHash(content: string): string {
  return createHash("sha256").update(content, "utf8").digest("hex");
}
