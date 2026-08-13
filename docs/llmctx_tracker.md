# llmctx — Project Tracker

> Upload this file to any Claude session to continue exactly where we left off.

---

## What This Project Is

A developer tool that eliminates the need to re-explain your codebase every time you start a new LLM chat session.

**Core idea:** Attach LLM context invisibly to each source file using NTFS Alternate Data Streams (ADS) as local storage. Context is generated automatically on every save by a local Ollama model running in the background. When you want to send a file to any LLM, click the llmctx button in the VS Code status bar — it reads the ADS and source, merges them into one plain text block, and copies to clipboard. You paste into any LLM. Zero terminal, zero commands, zero re-explaining.

> **Important:** LLMs (including Claude) cannot read ADS directly. ADS is purely local invisible storage. The VS Code status bar button is the only bridge between ADS and any LLM. Never upload raw files directly.

---

## Current Status

**Built and verified on a real Windows machine.** Three real bugs surfaced during that build/package process (not sandbox-only issues) — all fixed and reverified. See "Bugs Found & Fixed" below. The sandbox note in "Known Build Issue" still applies to *this Claude session's own* verification environment, but is no longer the whole story — the fixes below were needed regardless of Rust version.

---

## Key Decisions Made

| Decision | Choice | Reason |
|---|---|---|
| Context storage | NTFS ADS (`file.py:llmctx`) | Zero clutter, invisible, git-ignored by design. Local only — LLMs cannot read ADS directly |
| LLM delivery | VS Code status bar button | User clicks, merged text lands in clipboard. Zero terminal needed |
| Platform scope | Windows / NTFS only (v1) | ADS is NTFS-native, cross-platform later |
| LLM for generation | Ollama (local) | Free, private, no API cost, runs in background |
| Architecture | Daemon + CLI + VS Code extension + cpctx | Decoupled, editor-agnostic daemon |
| Context generation | Ollama auto-generates on save | Zero manual work for the user |
| Git behaviour | ADS not tracked by git | Feature, not a bug. Devs run `llmctx index` after clone |
| Trigger | VS Code extension notifies daemon on file save | Extension stays thin, daemon does all work |
| Daemon + CLI language | **Rust** | Single `.exe`, no runtime deps, tiny memory footprint, great for background daemon |
| VS Code extension language | **TypeScript** | Non-negotiable — VS Code extension API requirement |
| Distribution | **GitHub Releases** | Daemon `.exe` + CLI `.exe` + cpctx `.exe` published as release artifacts; VS Code extension via Marketplace. **Backlog: Windows code-signing cert before public launch** |
| Project name | **llmctx** | Final |
| Context for LLM-generated files | **Comment-block extraction**, not Ollama | LLM writes context as a delimited comment at the top of the file it generates (taught via `llmctx.md` skill file). Daemon/CLI detects it, cuts it into ADS, strips it from source. Ollama is never called for these files |
| Concurrent saves | **Bounded worker pool for Ollama** (default 2 concurrent jobs) | Mass save must never spawn unbounded local-model inference |
| `llmctx init` | **Non-interactive template drop** | Writes `llmcontext.yaml` with placeholder comments and exits immediately |
| ADS format versioning | **`LLMCTX_VERSION: 1` as the first line of every write** | Prevents silent incompatibility later. Mismatch on read → silently regenerate |
| Uninstall behaviour | **Leave ADS streams in place, no cleanup pass** | Orphaned ADS is invisible and harmless |
| `llmctx.md` for non-Claude LLMs | **Out of scope for v1** | Targets Claude/Claude Code specifically for now |
| Auto-update | **Passive check on daemon startup only** | One-time GitHub Releases check, never auto-replaces running `.exe` |
| Copying files | **`cpctx` — context-preserving copy tool** | Solves the "normal copy strips ADS" problem. `cpctx copy` reads ADS from source and writes it to destination after every file copy. `cpctx setup` registers itself on the user PATH permanently (no admin required) |

---

## Completed Work

### crates/core (shared library) ✅
- **`lib.rs`** — re-exports all public modules
- **`ads.rs`** — ADS read/write via `windows-rs` with version stamp (`LLMCTX_VERSION: 1`). Non-Windows stub returns `AdsError::NotSupported` gracefully. Functions: `read_ads()`, `write_ads()`, `ads_exists()`, `clear_ads()`
- **`config.rs`** — `llmcontext.yaml` parsing with per-file walk-up resolution (mirrors `.gitignore` semantics). `find_config()`, `load_config()`, `load_config_for_file()`. `ProjectConfig` struct with `header_block()` builder
- **`extract.rs`** — `<<<LLMCTX … LLMCTX>>>` comment-block detection and stripping. Handles all comment prefix styles (`#`, `//`, `/* */`, `<!-- -->`). Validates all six required fields. Malformed blocks fall through to Ollama — never a silent failure. `extract_llmctx_block()` → `Result<ExtractedContext, ExtractError>`
- **`ollama.rs`** — `OllamaClient` wrapping `reqwest`. `generate()` builds the six-field prompt and parses the response. Shared `reqwest::Client` for connection pooling
- **`process.rs`** — **the keystone**. `process_file(path, client, force_ollama)`: extraction-first → Ollama fallback. Injects project header and version stamp either way. `content_hash()` for daemon debounce. `ProcessSource` enum (`Extracted | Ollama | SkippedTooSmall | SkippedUnreadable`). Both daemon and CLI call this — never reimplement the branch

### crates/daemon ✅
- **`main.rs`** — TCP listener with port fallback (tries 51515–51524, warns if non-default port used). NDJSON framing. Inbound: `{"type":"save"}`. Outbound: `{"type":"status"}` and `{"type":"updateAvailable"}`. Client registry (`Vec<ClientTx>`) so the update-check task can broadcast to all connected VS Code windows. Extraction fast-path (immediate, no semaphore). Ollama branch (30s debounce + bounded `Semaphore`). Real update-check via `reqwest` GET to GitHub Releases API: parses `tag_name`, compares semver, broadcasts `UpdateAvailable` to all clients. `is_newer()` + `parse_semver()` with unit tests

### crates/cli ✅
- **`main.rs`** — five clap subcommands: `init` (non-interactive template), `index` (`.gitignore` + `llmctx_ignore` via `ignore` crate; streaming walker via `async-channel` bounded channel with `concurrency` consumer tasks — never holds all file paths in memory, safe on 100k+ file repos), `pack` (ADS + source → clipboard via `arboard`), `reindex` (force Ollama, `force_ollama=true`), `extract` (calls `process_file`)

### crates/cpctx ✅ (new)
- **`main.rs`** — two clap subcommands:
  - `cpctx copy <src> <dest>` — copies file or directory tree, then reads ADS from each source and writes it to the corresponding destination. Gracefully handles: no ADS on source (normal), `AdsError::NotSupported` (non-Windows), write failures (warns but doesn't abort the copy)
  - `cpctx setup` — on Windows: writes binary dir to `HKCU\Environment\Path` registry key (permanent, no admin), patches PowerShell profile with a guarded `$env:PATH` prepend, idempotent. On non-Windows: prints manual `export PATH=` instructions

### vscode-extension ✅
- **`extension.ts`** — `activate()` wires save listener → `daemon.notifySave()`, active-editor change → `statusBar.refresh()`, three commands (`llmctx.pack`, `llmctx.reindex`, `llmctx.openConfig`)
- **`daemon.ts`** — `DaemonClient` extends `EventEmitter`. TCP socket with NDJSON line splitting. Exponential backoff reconnect (`1s → 30s`). `notifySave()` is fire-and-forget. Emits `"status"`, `"connect"`, `"disconnect"`
- **`statusBar.ts`** — `StatusBarManager` owns one `StatusBarItem`. Per-file state map. Icons: `$(clock)` queued, `$(sync~spin)` generating, `$(check)` ready, `$(warning)` error. Click always triggers `llmctx.pack`
- **`pack.ts`** — shells out to `llmctx pack` and `llmctx reindex`. Detects missing `llmctx` binary with a clear error and "Open README" action button. Respects `llmctx.cliPath` setting override. `verifyCliOnPath()` called at activation
- **`hash.ts`** — `contentHash()` via Node `crypto.createHash('sha256')`, matches `process::content_hash()` byte-for-byte

### project files ✅
- `Cargo.toml` — workspace root, all four crates, shared deps, `zeroize = "=1.8.1"` pin for Rust <1.85 compat, `async-channel = "2"` for streaming index walker
- `llmcontext.yaml` — dogfooding: llmctx config for the llmctx project itself
- `docs/llmctx.md` — skill file (moved from repo root into `docs/`); teaches any LLM the `<<<LLMCTX` comment format
- `docs/USAGE.md` — day-to-day usage walkthrough on a real project (CodeA4), added after the initial build
- `docs/llmctx_architecture_v5.svg`, `docs/llmctx_tracker.md` — also moved from repo root into `docs/`; only `README.md` stays at the project root now
- `.gitignore`
- `vscode-extension/package.json` — includes `llmctx.cliPath` setting for binary path override, plus `vscode:prepublish` script (see Bugs Found & Fixed)
- `vscode-extension/tsconfig.json`, `.eslintrc.json`, `.vscodeignore`
- `README.md` — full documentation including: step-by-step Rust install guide (for users new to Rust), cpctx usage and setup, CLI reference, repo layout with annotated tree, architecture diagram, caveats, and a Documentation table linking into `docs/`

### TypeScript type-check ✅
`npx tsc --noEmit` in `vscode-extension/` passes with zero errors.

---

## Bugs Found & Fixed (real Windows build)

Three real bugs surfaced running the actual build/package commands on a real Windows
machine — none of these were sandbox-only artifacts, all three would have failed on any
machine regardless of Rust version.

1. **`crates/core/src/ads.rs` — wouldn't compile.**
   - `ReadFile`/`WriteFile` are gated behind the `Win32_System_IO` feature of the
     `windows` crate, which `crates/core/Cargo.toml` never enabled. Added it.
   - `CreateFileW`'s `dwdesiredaccess` parameter wants a raw `u32`, but
     `FILE_GENERIC_READ`/`FILE_GENERIC_WRITE` are `FILE_ACCESS_RIGHTS` (a newtype).
     Needed `.0` at all three call sites (`ads_exists`, `read_ads`, `write_ads`).

2. **`crates/cli/src/main.rs` — wouldn't compile, looked like a Unicode/encoding bug but
   wasn't.** The `cmd_init()` config template is a raw string `r#"..."#`. Its own content
   (`project: "# your project name here`) contains the literal two-character sequence
   `"#` — which is *exactly* the raw-string terminator. The string closed there instead of
   at the real end, so everything after got re-parsed as ordinary Rust code, and the
   compiler surfaced confusing "unknown start of token" errors on em dashes/backticks much
   further down the file that had nothing to do with the actual cause. Fixed by widening
   the delimiter to `r##"..."##` (robust regardless of content) and fixing the broken YAML
   underneath it (`project: ""  # your project name here`, etc. — the original was an
   unterminated quoted scalar and wouldn't have parsed as YAML either).

3. **`vscode-extension/package.json` — `npm run package` failed with "Extension
   entrypoint(s) missing."** `"package": "vsce package"` never ran `tsc` first, so
   `out/extension.js` (the `"main"` entry point) never existed. Fixed by adding
   `"vscode:prepublish": "npm run compile"` — `vsce` runs this hook automatically before
   packaging. Verified end-to-end: `npm install && npm run package` now produces a working
   `.vsix` with `out/*.js` correctly bundled.

---

## Documentation Gap Found & Fixed

Everything above got the code building, but a real-world walkthrough surfaced a gap in the
docs themselves, not the code: the README's Prerequisites section listed Ollama as a bare
bullet point (a link + "with at least one model pulled"), with no actual install steps —
unlike Rust, which had a full step-by-step section. Result: it's entirely possible to get
through the whole build, install the VS Code extension, and only discover Ollama was never
installed once files start sitting in `error` state in the status bar. Worse, even after
installing Ollama, pulling a model is a separate step that's easy to miss since Ollama
"looks" fully working (tray icon present, `ollama --version` succeeds) with zero models
pulled.

Fixed by adding a full **"Installing Ollama (step-by-step)"** section to `README.md`
(mirroring the existing Rust one — install, verify the process is running via Task
Manager, `ollama pull phi3:mini`, `ollama list` to confirm, a sanity-check `ollama run`),
and threading pointers to it through `docs/USAGE.md`'s daemon-start step and its
troubleshooting section, plus tightening the README Caveats bullet on Ollama to name the
actual diagnostic commands instead of just "make sure Ollama is running."

---

## Known Build Issue (environment only)

The sandbox Rust is 1.75 (from apt). Several transitive deps (`zeroize 1.9`, `clap_builder 4.6`) require Rust 1.85+ (edition 2024). The `Cargo.toml` has `zeroize = "=1.8.1"` and `clap = "=4.4.18"` pins to work around this, but the sandbox network blocks `crates.io` (though `static.crates.io` and `index.crates.io` are reachable — Cargo's actual download CDN).

**On a developer machine with Rust ≥ 1.85:**
```powershell
cargo check --workspace     # should pass cleanly
cargo test -p llmctx-core   # unit tests in extract.rs, config.rs, process.rs, ollama.rs
```

The `zeroize` pin and `clap` pin in `Cargo.toml` can be removed once the workspace is pinned to Rust ≥ 1.85 in `rust-version`.

---

## What Remains

| Item | Notes |
|---|---|
| Replace `your-org/llmctx` placeholder | Two places: `RELEASES_URL` in `crates/daemon/src/main.rs` and the two `openExternal` calls in `vscode-extension/src/pack.ts` and `extension.ts`. Set to the real GitHub repo URL before publishing |
| Replace `"publisher": "llmctx"` | `vscode-extension/package.json` — update to the real VS Code Marketplace publisher ID before Marketplace submission |
| Windows ADS integration test | Still needs a live test on the real Windows machine: save a file with a `<<<LLMCTX` block, verify the block is stripped and `llmctx pack` includes the context. The code now compiles cleanly (see Bugs Found & Fixed), but read/write against a real NTFS stream hasn't been exercised yet |
| `cpctx copy` integration test | Copy a file with existing ADS, verify `llmctx pack` on the destination produces the same context |
| End-to-end daemon test | Start `llmctxd`, open VS Code, save a file, watch status bar cycle queued → generating → ready |
| ~~VS Code extension packaging~~ | ✅ Done — `npm install && npm run package` verified producing a working `.vsix` after the `vscode:prepublish` fix |
| Minor cosmetic warning | `DebounceEntry.last_save` field in `crates/daemon/src/main.rs` triggers a harmless `dead_code` warning (never read, only written). Not blocking, cheap to silence with `#[allow(dead_code)]` or by actually using it for debounce-window logic later |
| Code signing | Windows code-signing cert for all three `.exe` files before public launch (SmartScreen / Defender false positives on unsigned daemons that listen on a socket) |

---

## Rust Workspace Structure (as built)

```
llmctx/
├── Cargo.toml                  ← workspace root, four crates
├── llmcontext.yaml
├── README.md
├── .gitignore
├── docs/
│   ├── USAGE.md                ← real-project walkthrough (added post-build)
│   ├── llmctx.md                ← skill file for LLMs (teaches <<<LLMCTX format)
│   ├── llmctx_architecture_v5.svg
│   └── llmctx_tracker.md        ← this file
└── crates/
    ├── core/                   ← shared library (llmctx-core)
    │   └── src/
    │       ├── lib.rs
    │       ├── ads.rs          ← NTFS ADS, windows-rs, version stamp
    │       ├── config.rs       ← llmcontext.yaml, walk-up resolution
    │       ├── extract.rs      ← <<<LLMCTX block detection + stripping
    │       ├── ollama.rs       ← Ollama /api/generate client
    │       └── process.rs      ← process_file() — the keystone
    ├── daemon/                 ← llmctxd binary
    │   └── src/main.rs
    ├── cli/                    ← llmctx binary
    │   └── src/main.rs
    └── cpctx/                  ← cpctx binary (NEW)
        └── src/main.rs

vscode-extension/
└── src/
    ├── extension.ts
    ├── daemon.ts
    ├── statusBar.ts
    ├── pack.ts
    └── hash.ts
```

---

## Rust Crates

| Need | Crate |
|---|---|
| Async runtime | `tokio` |
| HTTP — Ollama calls | `reqwest` |
| CLI argument parsing | `clap` (pinned `=4.4.18` for Rust <1.85 compat) |
| Windows ADS / API | `windows` (windows-rs, target_os = "windows" guard) |
| Clipboard | `arboard` |
| YAML parsing | `serde` + `serde_yaml` |
| JSON — daemon protocol + Ollama response | `serde_json` |
| File hashing for debounce | `sha2` |
| Logging | `tracing` + `tracing-subscriber` |
| Error types | `thiserror` + `anyhow` |
| Gitignore-aware walker | `ignore` (used by `llmctx index`) |
| Zeroize (transitive, pinned) | `zeroize =1.8.1` (pre edition-2024) |

---

## Socket Protocol

**Framing:** NDJSON over local TCP (port 51515). One JSON object per line, UTF-8, `\n`-terminated.

```jsonc
// extension → daemon (on every file save)
{"type":"save","path":"C:\\proj\\auth\\middleware.py","hash":"a1b2c3..."}

// daemon → extension (whenever a file's context state changes)
{"type":"status","path":"C:\\proj\\auth\\middleware.py","state":"generating"}
// state: "queued" | "generating" | "ready" | "error"
{"type":"status","path":"...","state":"error","message":"ollama unreachable"}
```

Pack does NOT go over the socket — handled entirely in the extension by shelling out to `llmctx pack`.

---

## ADS Content Format

```
LLMCTX_VERSION: 1
PROJECT: My Project Name | Python, FastAPI, PostgreSQL
TASK: Current task description
CONVENTIONS: All DB models in /models | Routes return {data, error}
SOURCE: llm | ollama

FILE: auth/middleware.py
ROLE: Middleware for JWT verification on protected routes
EXPORTS: verify_token(), require_auth decorator
IMPORTS: models/user.py, config/settings.py
USED BY: routers/orders.py, routers/profile.py
NOTES: Any unusual patterns or gotchas
```

---

## cpctx — Context-Preserving Copy (new tool)

**Problem solved:** Windows Explorer, `cp`, `robocopy` (without `/COPYALL`), most ZIP tools, and most cloud sync clients silently discard NTFS ADS when copying files. All the context built up by `llmctxd` is lost.

**Solution:** `cpctx copy <src> <dest>` copies file content then immediately calls `ads::read_ads(src)` + `ads::write_ads(dest, body)` for each file. Works on single files and directory trees.

**PATH registration:** `cpctx setup` (run once, no admin required):
1. Reads `HKCU\Environment\Path` via `reg query`
2. Prepends the binary directory if not already present, writes back via `reg add /t REG_EXPAND_SZ`
3. Detects the PowerShell profile path via `$PROFILE`, appends a guarded `$env:PATH` block
4. Both steps are idempotent — running setup twice is safe

**Fallback on non-NTFS / non-Windows:** file content is copied normally; ADS steps return `AdsError::NotSupported` and are silently skipped.

---

## Context for Next Session

Build verified on a real Windows machine; three real bugs found and fixed along the way
(see "Bugs Found & Fixed"). VS Code extension packaging is also verified working. To
continue:

1. **Exercise the actual ADS read/write path** on the real machine — the code compiles
   now, but a live save-a-file-with-a-`<<<LLMCTX>>>`-block test hasn't been run yet:
   ```powershell
   cargo build --release -p llmctxd -p llmctx -p cpctx
   cpctx setup
   llmctxd    # in one terminal
   # then, in the project you're testing: save a file with a <<<LLMCTX block, confirm
   # it gets stripped from the source and `llmctx pack` includes the context
   ```

2. **Fill two string placeholders** (search for `your-org/llmctx`):
   - `crates/daemon/src/main.rs` → `RELEASES_URL` constant
   - `vscode-extension/src/pack.ts` → two `openExternal` URLs
   - `vscode-extension/src/extension.ts` → one `openExternal` URL
   - `vscode-extension/package.json` → `"publisher"` field

3. **Run the remaining integration tests** — `cpctx copy` round-trip, end-to-end daemon
   status-bar cycle (see "What Remains").

4. **Commission code signing** before public release.
