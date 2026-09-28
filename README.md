# llmctx

Context for your source files, invisible to your repo.

llmctx attaches a per-file context block to every source file in your project. The context lives in llmctx's own store, a single SQLite database in a hidden `.llmctx/` folder at the project root (much like `.git/`). Your source files are never touched, and the store works the same on any file system: NTFS, ext4, APFS, FAT32, exFAT or a network share. When you paste a file into an LLM, click the status bar button to merge the context in first.

---

## How it works

1. **Save a file in VS Code.** The extension sends a save notification to the background daemon (`llmctxd`).
2. **If the file contains an `<<<LLMCTX` comment block** (written by the LLM that last generated it), the daemon extracts it immediately, strips it from the source file, and writes it to the project's context store. Ollama is never called.
3. **Otherwise**, the daemon waits 30 seconds (in case you keep typing), then sends the file to a local Ollama model, which generates the six-field context block. The result is written to the context store.
4. **When you want to use the context**, click the status bar button (or run `llmctx: Pack current file to clipboard`). The context and source text are merged and placed on the clipboard. Paste into any LLM.

The source files in your repo stay completely clean. No comments, no markers, nothing visible.

---

## Where context is stored

Each project gets one store at `<project root>/.llmctx/context.db`. The project root is the folder holding `llmcontext.yaml`. Without one, llmctx uses the nearest folder that already has a `.llmctx/` store or is a git checkout. Files outside any project are ignored.

- **Invisible to git.** `.llmctx/` contains its own `.gitignore` (`*`), so it never shows up in `git status`, and your own `.gitignore` is never edited. The VS Code extension hides the folder from the Explorer and file watcher. On Windows the folder also gets the hidden attribute.
- **Travels with the project.** Copying, zipping, syncing or backing up the project folder with any tool keeps the context, because it is just a file inside the folder.
- **Survives renames and moves.** Each entry is keyed by the file's path relative to the project root *and* by a hash of its content. When a renamed or moved file is next saved or indexed, llmctx finds its context by content and carries it over, with no Ollama call. A copied file gets its own copy of the context the same way.
- **Safe to share between processes.** The daemon and the CLI can write at the same time. SQLite serialises the writes, and each one is all-or-nothing.
- **Disposable.** Deleting `.llmctx/` loses nothing that `llmctx index` can't rebuild.

---

## Documentation

| Doc | What's in it |
|---|---|
| [`docs/USAGE.md`](docs/USAGE.md) | Walking through llmctx on a real project day-to-day — writing code, packing context for an LLM, reindexing after a clone, etc. |
| [`docs/llmctx.md`](docs/llmctx.md) | The skill file — give this to an LLM so it emits `<<<LLMCTX` blocks itself when writing your source files. |
| [`docs/llmctx_architecture_v5.svg`](docs/llmctx_architecture_v5.svg) | Architecture diagram of the extension/daemon/core/cpctx pieces. |
| [`docs/llmctx_tracker.md`](docs/llmctx_tracker.md) | Internal project tracker/dev log — upload it to a Claude session to resume work on llmctx itself. |

This README covers installation and reference. Start with `docs/USAGE.md` if you just want to know how to use the tool day-to-day.

---

## Installing Rust (step-by-step)

If you have never used Rust before, follow these steps. This takes about five minutes.

### Step 1 — Download the Rust installer

Go to **https://rustup.rs** in your browser. Click the button to download `rustup-init.exe` (on Windows) and run it.

Alternatively, open PowerShell and paste this one command:

```powershell
winget install Rustlang.Rustup
```

Either way, a small program called **rustup** is installed. rustup is the official Rust version manager — it downloads and updates the Rust compiler (`rustc`) and the build tool (`cargo`) for you.

### Step 2 — Run the installer

If you downloaded `rustup-init.exe`, double-click it. You will see a terminal prompt like:

```
1) Proceed with standard installation (default)
2) Customise installation
3) Cancel installation
```

Press **1** and then Enter. This installs:

- `rustc` — the Rust compiler
- `cargo` — the build tool and package manager (used for everything in this project)
- The standard library

The installer adds Rust to your `PATH` automatically.

### Step 3 — Open a new terminal

Close your current PowerShell or Command Prompt window and open a fresh one. Then verify the installation:

```powershell
rustc --version
cargo --version
```

You should see output like:

```
rustc 1.85.0 (4d91de4e4 2025-02-17)
cargo 1.85.0 (d73d2caf9 2025-02-17)
```

The exact version numbers do not matter as long as both commands succeed.

### Step 4 — Install the Visual C++ Build Tools (Windows only)

Rust on Windows needs the Microsoft C++ linker. If you do not already have Visual Studio installed, run:

```powershell
winget install Microsoft.VisualStudio.2022.BuildTools
```

When the installer opens, tick **"Desktop development with C++"** and click Install. This is a one-time step.

The same tools also compile SQLite, which is built into llmctx for its context store. On macOS and Linux the system C compiler is enough (`xcode-select --install` on macOS; `gcc` or `clang` on Linux).

### Step 5 — You are ready

You can now build any Rust project by running `cargo build` inside its folder. For this project specifically, continue with the Installation section below.

> **Keeping Rust up to date:** run `rustup update` at any time to get the latest stable compiler.

---

## Installing Ollama (step-by-step)

llmctx needs a **local Ollama server, with at least one model pulled**, running in the
background at all times. This is what actually generates context for any file an LLM
didn't write for you (see "How it works" above) — without it, the daemon has nothing to
call, and every file just sits in `error` state. If you don't have Ollama yet, follow
these steps. Installing it takes a minute; pulling a model takes a few minutes more
depending on your connection.

### Step 1 — Download and install Ollama

Go to **https://ollama.com/download**, download the Windows installer, and run it.

Alternatively, open PowerShell and paste this one command:

```powershell
winget install Ollama.Ollama
```

The installer registers Ollama to run automatically at login and puts a llama icon in
your system tray. Ollama itself is not a one-shot program you launch when you need it —
once installed, it runs continuously as a background service, serving a local API on
`http://127.0.0.1:11434`.

### Step 2 — Verify Ollama is actually running

Open a **new** terminal window and run:

```powershell
ollama --version
```

If that fails or you don't see the tray icon, log out and back in (or restart) so the
auto-start entry takes effect. You can also check **Task Manager** for an `ollama.exe` (or
`ollama app.exe`) process — this is the process `llmctxd` needs to reach.

### Step 3 — Pull a model

**Installing Ollama does not download any model on its own.** This is the step that's
easy to miss — Ollama can be fully installed and running and llmctx will still fail, because
there's no model to actually call. llmctx's default is `phi3:mini` (small and fast, more
than enough for the six short fields it asks Ollama to generate):

```powershell
ollama pull phi3:mini
```

This downloads a few hundred MB. Let it finish before moving on.

### Step 4 — Confirm the model is there

```powershell
ollama list
```

`phi3:mini` should be in the output. To sanity-check that it actually responds:

```powershell
ollama run phi3:mini "Say hello in one word."
```

### Step 5 — You are ready

`llmctxd` will now be able to reach Ollama on the default URL and generate context for any
file that doesn't already carry an `<<<LLMCTX` block. Continue with the Installation
section below.

> **Testing phase — rebuild after every source change.** There is no auto-update.
> Editing the source does nothing until you run `cargo build --release` *and*
> replace the binaries that are actually on your PATH. A stale binary reads your
> new `llmcontext.yaml` without complaint, so it can look updated while behaving
> exactly like the old build. See [`docs/REBUILDING.md`](docs/REBUILDING.md).

> **Using a different model, port, or a remote Ollama instance:** override `ollama_url`,
> `ollama_model`, `ollama_concurrency`, `ollama_timeout_secs`, or `ollama_max_bytes`
> per-project in that project's `llmcontext.yaml`
> (see the `llmcontext.yaml` section further down) — all three are commented out by
> default, using the values above.

---

## Installation

### Prerequisites

- Windows, macOS or Linux, on any file system (the setup commands below use Windows PowerShell; adapt paths for other platforms)
- Ollama installed, running, and with a model pulled (see "Installing Ollama" above if you haven't done this yet — it's easy to install Ollama and still miss the model-pull step)
- Rust toolchain installed (see "Installing Rust" above if you need to install it)

### 1. Clone or unzip the project

```powershell
# If you have Git:
git clone https://github.com/your-org/llmctx.git
cd llmctx

# Or unzip the downloaded archive and open a terminal in the llmctx folder.
```

### 2. Build all binaries

```powershell
cargo build --release -p llmctxd -p llmctx -p cpctx
```

The three binaries appear at:

```
target\release\llmctxd.exe    ← background daemon
target\release\llmctx.exe     ← CLI tool
target\release\cpctx.exe      ← context-preserving copy tool
```

### 3. Put the binaries on your PATH

Copy all three `.exe` files into a folder that is on your `PATH`, for example `C:\Tools\llmctx\`.

```powershell
# Example: create the folder and copy the binaries
New-Item -ItemType Directory -Force -Path "C:\Tools\llmctx"
Copy-Item target\release\llmctxd.exe, target\release\llmctx.exe, target\release\cpctx.exe `
    -Destination "C:\Tools\llmctx\"
```

Then run the setup command from `cpctx` — this registers `C:\Tools\llmctx\` on your user PATH permanently and patches your PowerShell profile:

```powershell
C:\Tools\llmctx\cpctx.exe setup --bin-dir "C:\Tools\llmctx"
```

Open a **new** terminal window and verify:

```powershell
llmctx --version
llmctxd --version
cpctx --version
```

### 4. Start the daemon

```powershell
llmctxd
```

To start it automatically at login without a terminal window, add it to Task Scheduler:

```powershell
schtasks /create /tn "llmctxd" /tr "llmctxd" /sc onlogon /ru "%USERNAME%" /f
```

### 5. Install the VS Code extension

```powershell
cd vscode-extension
npm install
npm run package       # produces llmctx-0.1.0.vsix
code --install-extension llmctx-0.1.0.vsix
```

### 6. Initialise a project

Open the project folder in VS Code, then open the Command Palette (`Ctrl+Shift+P`) and run:

```
llmctx: Open llmcontext.yaml
```

This creates `llmcontext.yaml` at the project root. Edit it to describe your project, stack, and current task.

---

## Upgrading from llmctx 0.1 (NTFS streams)

llmctx 0.1 kept context in NTFS Alternate Data Streams. To bring that context into the new store without regenerating it, run this once in each project root on Windows:

```powershell
llmctx migrate                   # copy stream context into .llmctx/context.db
llmctx migrate --remove-streams  # ...and delete each stream once it is stored
```

`migrate` is safe to run more than once. It never overwrites context that is already in the store. If you skip it, `llmctx index` regenerates the context instead, which takes longer. Until you migrate, `llmctx pack` prints a hint when a file still has an old stream.

---

## cpctx — Context-preserving copy

Copying a whole project folder keeps its context automatically, since the store lives inside the folder. `cpctx` is for the other case: copying files or folders **into a different project**. It copies the file content, then carries each file's context from the source project's store into the destination project's store, with the `FILE:` field updated to the new path.

If a copied folder lands outside every project, it becomes a project of its own: cpctx creates a `.llmctx/` store at the top of the copy.

### Usage

```powershell
# Copy a single file
cpctx copy src\auth\middleware.py D:\backup\middleware.py

# Copy a file into a directory (filename is preserved)
cpctx copy src\auth\middleware.py D:\backup\

# Copy an entire directory tree recursively
cpctx copy C:\Projects\myapp D:\Projects\myapp

# Verbose — print each file as it is copied
cpctx copy --verbose src\ D:\backup\src\
```

### Setting up cpctx system-wide

Running `cpctx setup` once adds the binary to your user PATH both in the Windows registry (permanent, survives restarts) and in your PowerShell profile (takes effect immediately in new sessions):

```powershell
cpctx setup
```

After that you can use `cpctx copy` from any terminal or script on the machine without specifying the full path.

### When to use cpctx vs llmctx index

| Situation | What to do |
|---|---|
| Copying, zipping, syncing or backing up a whole project folder | Nothing: `.llmctx/` goes along with it |
| Renaming or moving a file inside a project | Nothing: the next save or `llmctx index` carries its context over by content hash |
| Copying files into a different project | `cpctx copy` |
| A fresh `git clone` (the store is not committed) | `llmctx index` to generate context |
| Files deleted or renamed outside VS Code piling up stale entries | `llmctx gc` |

---

## llmcontext.yaml

```yaml
project: "MyApp"
stack: "Python, FastAPI, PostgreSQL"
task: "Build the payments module"

conventions:
  - "No bare except clauses"
  - "All DB calls go through the repository layer"

llmctx_ignore:
  - "generated/**"
  - "migrations/**"

# Optional Ollama overrides (defaults shown)
# ollama_url: "http://127.0.0.1:11434"
# ollama_model: "phi3:mini"
# ollama_concurrency: 1      # Ollama serialises requests unless
#                            # OLLAMA_NUM_PARALLEL is raised
# ollama_timeout_secs: 300   # per-request budget for one generation
# ollama_max_bytes: 16384    # files above this are skipped, not sent
```

---

## CLI reference

```
llmctx init              Write a llmcontext.yaml template here
llmctx index [dir]       Walk all files and generate/extract context
                         (resumable — skips files whose context is current)
llmctx index --force     Regenerate everything, ignoring stored context
llmctx pack <file>       Merge context + source → clipboard
llmctx reindex <file>    Force Ollama regeneration for one file
llmctx extract <file>    Run extraction/generation (same as daemon, once)
llmctx extract --force   ...even if stored context is already current
llmctx gc [dir]          Remove stored context for files that no longer exist
llmctx migrate [dir]     Import context from llmctx 0.1 NTFS streams (Windows)
llmctx migrate --remove-streams   ...and delete the streams afterwards

cpctx copy <src> <dest>  Copy file or directory, carrying context to the destination project
cpctx setup              Register cpctx on the system PATH permanently
```

---

## The `<<<LLMCTX` comment block

When an LLM generates a source file for your project, it includes this block at the top:

```python
# <<<LLMCTX
# FILE: auth/middleware.py
# ROLE: JWT verification middleware for protected routes
# EXPORTS: verify_token(), require_auth
# IMPORTS: models/user.py, config/settings.py
# USED BY: routers/orders.py, routers/profile.py
# NOTES: NONE
# LLMCTX>>>

def verify_token():
    ...
```

On the next save, the daemon detects the block, cuts it out of the source file, and writes it to the context store. The file on disk ends up exactly as if the block was never there.

See [`docs/llmctx.md`](docs/llmctx.md) (the skill file used by the LLM) for the full format specification and per-language examples.

---

## Repository layout

```
llmctx/
├── Cargo.toml                  # Workspace root — shared dependency versions
├── llmcontext.yaml             # llmctx config for the llmctx project itself
├── README.md
├── .gitignore
│
├── docs/
│   ├── USAGE.md                 # Day-to-day usage walkthrough on a real project
│   ├── llmctx.md                 # Skill file — teaches LLMs the <<<LLMCTX comment format
│   ├── llmctx_architecture_v5.svg
│   └── llmctx_tracker.md         # Internal dev tracker for the llmctx project itself
│
├── crates/
│   ├── core/                   # llmctx-core — shared library (no binary)
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs          # Re-exports all public modules
│   │       ├── store.rs        # Context store: .llmctx/context.db (SQLite)
│   │       ├── ads.rs          # Legacy NTFS stream reader, used by `llmctx migrate`
│   │       ├── config.rs       # llmcontext.yaml parsing & walk-up resolution
│   │       ├── extract.rs      # <<<LLMCTX block detection and stripping
│   │       ├── ollama.rs       # Ollama /api/generate HTTP client
│   │       └── process.rs      # process_file() — single decision function
│   │
│   ├── daemon/                 # llmctxd — background daemon binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs         # TCP listener, debounce, bounded worker pool
│   │
│   ├── cli/                    # llmctx — CLI binary
│   │   ├── Cargo.toml
│   │   └── src/
│   │       └── main.rs         # init, index, pack, reindex, extract, migrate, gc
│   │
│   └── cpctx/                  # cpctx — context-preserving copy binary
│       ├── Cargo.toml
│       └── src/
│           └── main.rs         # copy + setup subcommands; PATH registration
│
└── vscode-extension/           # VS Code extension
    ├── package.json            # Extension manifest, commands, config schema
    ├── tsconfig.json
    ├── .eslintrc.json
    ├── .vscodeignore
    └── src/
        ├── extension.ts        # activate/deactivate — wires everything together
        ├── daemon.ts           # TCP client with auto-reconnect & NDJSON framing
        ├── statusBar.ts        # Per-file status bar item (queued/generating/ready/error)
        ├── pack.ts             # Shells out to `llmctx pack` and `llmctx reindex`
        └── hash.ts             # SHA-256 matching process::content_hash() on Rust side
```

---

## Architecture

```
VS Code extension
  │  save event → TCP NDJSON → llmctxd (port 51515)
  │  status push ←
  │  status bar: $(check) / $(sync~spin) / $(warning)
  │  llmctx.pack → shells out to `llmctx pack`
  │
llmctxd (daemon)
  │  extraction path (immediate, no semaphore)
  │      detect <<<LLMCTX → extract → write store → strip source
  │  Ollama path (debounced 30 s, bounded pool)
  │      hash check → acquire semaphore → call Ollama → write store
  │
llmctx-core (library)
  ├── store.rs      <project>/.llmctx/context.db — keyed by path and content hash
  ├── ads.rs        legacy NTFS stream reader (llmctx migrate only)
  ├── config.rs     llmcontext.yaml — per-file walk-up resolution
  ├── extract.rs    <<<LLMCTX block detection and stripping
  ├── ollama.rs     Ollama /api/generate client
  └── process.rs    process_file() — the single decision function

cpctx (standalone binary)
      cpctx copy src dest   →  std::fs::copy, then source store → destination store
      cpctx setup           →  HKCU\Environment\Path + PowerShell profile
```

---

## Caveats

- **Context is per machine unless you share the folder.** `.llmctx/` ignores itself in git, so a fresh clone starts without context. Run `llmctx index` after cloning.
- **Files outside a project get no context.** The daemon ignores saves for files with no `llmcontext.yaml`, `.llmctx/` or `.git` above them, so it never scatters `.llmctx/` folders next to stray files. Run `llmctx init` to make a folder a project.
- **Renames are matched by content.** A file renamed *and* edited before its next save no longer matches its old context by hash, so it is regenerated. The old entry lingers until `llmctx gc`.
- **Ollama must be running, with a model pulled.** If Ollama isn't running, or is running but no model has been pulled, the file is marked `error` in the status bar. Check Task Manager for `ollama.exe`, and run `ollama list` to confirm a model is present — see "Installing Ollama" above. Save the file again once Ollama is up to retry.
- **cpctx setup requires no admin rights.** It writes to `HKCU\Environment` (user-level, not system-level) and your PowerShell profile. No UAC prompt will appear.
