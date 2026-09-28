# Rebuilding after a source update

**llmctx is in the testing phase, and there is no auto-update.** Editing the
source does nothing on its own — `llmctx`, `llmctxd`, and `cpctx` are compiled
binaries, and the ones already on your PATH keep running the old code until you
rebuild *and* replace them.

This is easy to miss because a stale binary does not announce itself. It reads
your **new** `llmcontext.yaml` quite happily — `ProjectConfig` does not use
`deny_unknown_fields`, so unknown keys like `ollama_timeout_secs` are silently
ignored while known ones like `ollama_concurrency` still apply. The result is a
run that looks partly updated, behaves entirely like the old build, and gives
you no error to go on.

Work through the steps below in order after every source change.

---

## 1. Stop anything currently running

Windows will not overwrite a binary that is in use, and the failure mode is a
confusing "Access is denied" or "The process cannot access the file" during the
copy in step 4.

```powershell
# The daemon the VS Code extension starts keeps running after VS Code closes,
# so stop it explicitly:
taskkill /IM llmctxd.exe /F 2>$null
taskkill /IM llmctx.exe  /F 2>$null
```

On Linux or macOS: `pkill llmctxd`. The next VS Code window starts the new build
automatically. Its per-user discovery file (port and token) is rewritten on every
start, so a stale one from the killed daemon does no harm.

A non-zero exit here just means the process was not running. That is fine.

---

## 2. Build

```powershell
cd C:\Users\Admin\Downloads\temp\llmctx
cargo build --release
```

Use `--release` for anything you actually intend to use. A `debug` build of the
Ollama path is markedly slower, which matters when you are already close to the
generation timeout.

**The build must end in `Finished`.** If it ends in `error:`, stop here — step 4
would otherwise copy a stale binary from a previous successful build and you
would be debugging code that is not running.

### Platform-specific compile errors

Parts of this codebase are `#[cfg(target_os = "windows")]`. Code inside those
blocks is invisible to a Linux or macOS build, so a warning-free build on
another platform does **not** guarantee a clean Windows build — and vice versa.
The `bail!` import in `crates/cpctx/src/main.rs` is exactly this: used only by
the Windows PATH-setup code, so it reads as an unused import elsewhere.

CI builds, lints and tests on Windows, macOS and Linux for every pull request
(`.github/workflows/ci.yml`), which is the practical way to catch this before a
user does.

If a cross-platform build reports an unused import or dead code, check whether
the only use site sits inside a `cfg` block for a different target before
deleting anything. Silence it with a scoped attribute instead:

```rust
#[cfg_attr(not(target_os = "windows"), allow(unused_imports))]
use anyhow::{bail, Context, Result};
```

---

## 3. Run the tests

```powershell
cargo test --workspace
```

Expect **0 failed** (the exact count differs a little by platform: Windows also
runs the NTFS-stream migration tests). They are fast (a second or two), need no
Ollama, and catch config, extraction, store and ignore-rule regressions before
you spend forty minutes discovering them through Ollama.

---

## 4. Replace the binaries that are actually on your PATH

This is the step that is easiest to skip and the one that causes the
"my fix did nothing" symptom.

First find out which binary wins:

```powershell
where.exe llmctx
```

`where.exe` lists **every** match in PATH order. The first line is what runs
when you type `llmctx`.

- **If the first line is `...\llmctx\target\release\llmctx.exe`** — you are done,
  the fresh build is already what runs.
- **If it is anywhere else** — typically `%USERPROFILE%\.cargo\bin\` from a
  `cargo install`, or a copy placed by `cpctx setup` — that stale binary is
  shadowing your build. Overwrite it:

```powershell
$dest = Split-Path (where.exe llmctx | Select-Object -First 1)
Copy-Item .\target\release\llmctx.exe   $dest -Force
Copy-Item .\target\release\llmctxd.exe  $dest -Force
Copy-Item .\target\release\cpctx.exe    $dest -Force
```

If `where.exe` returns nothing at all, run `cpctx setup` once to register the
directory on your PATH permanently, then open a **new** terminal.

> A terminal captures PATH when it starts. After any PATH change, an already-open
> terminal keeps the old value — open a fresh one.

---

## 5. Confirm the new binary is the one running

Do not take it on trust. Check for a string that only exists in the new build:

```powershell
llmctx index --help
```

If `--force` is listed, you are on a current build. Then:

```powershell
llmctx index
```

The first line should read:

```
INFO llmctx: indexing . with up to 1 concurrent Ollama jobs (timeout 300s, max 16384 bytes per file)
```

If the `(timeout ...)` suffix is missing, you are still running an old binary —
go back to step 4.

A second, faster check: stop Ollama and run `llmctx index`. A current build
pre-flights the server and fails in about ten seconds with a single clear
message. An old build starts generating and produces one timeout per file.

---

## 6. Reload the VS Code extension

The extension talks to `llmctxd` over a socket, and VS Code holds the old daemon
process until it is restarted.

1. Close every VS Code window.
2. Reopen the project.
3. Confirm the status bar shows the llmctx indicator.

If you changed anything under `vscode-extension/`, that is a separate build:

```powershell
cd vscode-extension
npm install
npm run compile
```

---

## Quick reference

```powershell
taskkill /IM llmctxd.exe /F 2>$null      # 1. stop
cargo build --release                     # 2. build  (must say Finished)
cargo test --workspace                    # 3. test   (0 failed)
where.exe llmctx                          # 4. find the live binary, copy over it
llmctx index --help                       # 5. verify (--force present)
```

---

## Symptom → cause

| What you see | What it means |
|---|---|
| Log line lacks `(timeout Ns, max N bytes)` | Old binary still on PATH — step 4 |
| `Ollama is unreachable ... error sending request` with no `is \`ollama serve\` running?` | Old binary — the new message names the timeout explicitly |
| Summary lacks `already current` / `too large` counts | Old binary |
| One error per file instead of one for the whole run | Old binary — pre-flight is missing |
| Every file regenerates despite no edits | Context written by a pre-hash binary has no `HASH:` field and is treated as unknown. Expect one full pass, then resume works |
| `Access is denied` copying the exe | A process still holds it — step 1 |
| `llmctx` not found in a new terminal | `cpctx setup` not run, or terminal predates the PATH change |
