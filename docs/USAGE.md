# Using llmctx on a real project

This walks through everything from "I just built llmctx" to "I use this every day without
thinking about it," using an actual project — **CodeA4** — as the example throughout.
Swap in your own paths and file names as you go.

CodeA4's layout, for reference:

```
CodeA4/
│   CodeA4_Backend_Plan.md
│   CodeA4_Spec.md
│   codea4_ui_prototype.html
│   codea4_ui_prototype_v2.html
│   llmcontext.yaml
│   tracker.md
│
└───backend/
    │   app.py, config.py, extensions.py, models.py, storage.py, worker.py
    │   docker-compose.yml, Dockerfile, requirements.txt, .env.example
    │
    ├───api/          jobs.py, push.py, render.py, sessions.py, style.py
    ├───data/
    ├───jobs/         capture_jobs.py, clustering_jobs.py, render_jobs.py
    └───pipeline/      clustering.py, code_parser.py, glyph_assets.py,
                        pdf_export.py, render_svg.py, segmentation.py
```

This assumes you've already followed the [Installation](../README.md#installation) steps
in the main README — `llmctxd`, `llmctx`, and `cpctx` are on your PATH, and the VS Code
extension is installed.

---

## 1. Initialize the project

From the CodeA4 root:

```powershell
cd C:\Users\Admin\Downloads\temp\CodeA4
llmctx init
```

This writes an empty `llmcontext.yaml` template (you can also trigger this from VS Code
via `llmctx: Open llmcontext.yaml` in the Command Palette). Fill in the top three fields —
these are the only ones that get read on every single context generation, so it's worth
getting them right:

```yaml
project: "CodeA4"
stack: "Python, Flask-style app.py/api/jobs, Docker Compose, Pillow/cairosvg-style SVG→PDF pipeline"
task: "Building the code-clustering render pipeline"

conventions:
  - "API route handlers live under backend/api/, never touch the DB directly — go through models.py"
  - "Background work goes through backend/jobs/, dispatched via worker.py"

llmctx_ignore:
  - "backend/data/**"      # generated output, not source
  - "**/__pycache__/**"
  - "**/*.pyc"
```

Adjust `stack` and `conventions` to what's actually true of your codebase — the daemon
sends this verbatim to Ollama on every generation, so accuracy here directly improves the
quality of every context block it writes. `backend/data/` is a good `llmctx_ignore`
candidate since it looks like a runtime/output directory rather than source.

---

## 2. Check Ollama (the daemon starts itself)

Make sure Ollama is actually installed, running, **and** has a model pulled. Installing
Ollama alone isn't enough: the model download is a separate step
(`ollama pull phi3:mini`). See
[Installing Ollama](../README.md#installing-ollama-step-by-step) in the README if you
haven't done this yet. A quick check:

```powershell
ollama list
```

If that doesn't show a model, everything else still works, but files that need Ollama end
up in `error` state until you pull one.

You don't need to start `llmctxd` yourself: when you open CodeA4 in VS Code, the extension
starts it in the background if it isn't already running, and connects to it using the
port and token the daemon publishes in your per-user discovery file. The status bar item
shows `$(plug) llmctx` until it's connected. If you turned `llmctx.autoStartDaemon` off,
run `llmctxd` in a terminal instead.

---

## 3. Backfill context for existing files

CodeA4 already has real code in it — `backend/pipeline/clustering.py`,
`backend/api/render.py`, and so on — none of which has ever seen llmctx. Rather than
waiting for each file to be saved individually, backfill everything in one pass:

```powershell
llmctx index
```

This walks every file under the project root (respecting `.gitignore` and your
`llmctx_ignore` patterns, and skipping hidden files like `.env`), and for each one:

- If it starts with an `<<<LLMCTX` block, extracts it immediately.
- Otherwise, sends it to your local Ollama model to generate one.

For a project the size of CodeA4's `backend/`, expect this to take a minute or two the
first time, since every `.py` file goes through Ollama once. Run it again any time and
it's fast — files that haven't changed are skipped via content hash.

---

## 4. The everyday loop

Once step 3 is done, you mostly forget llmctx exists. Two paths, depending on how a file
gets written:

### You hand-edit a file

Open `backend/pipeline/render_svg.py`, make a change, save. The status bar shows
`$(clock)` for 30 seconds (the daemon debounces in case you're still typing), then
`$(sync~spin)` while Ollama works, and `$(check)` once the new context is stored. Saving
a file without changing it goes straight to `$(check)`.

Files llmctx deliberately leaves alone (ignored, hidden, too small, too large, binary)
show `$(circle-slash)`; hover it to see why.

Renaming or moving a file in VS Code's Explorer carries its context along, even if you
also edit it.

### An LLM writes a file for you

If you're asking Claude, ChatGPT, or another LLM to write or extend a file — say, adding
a new step to `backend/pipeline/glyph_assets.py` — paste the contents of
[`docs/llmctx.md`](llmctx.md) into that conversation first (or add it as a project/system
instruction if your tool supports one). The LLM will emit an `<<<LLMCTX` block at the top
of the file it writes. When you save that file, the daemon recognizes the block and
extracts it **instantly** — no Ollama call, no 30-second wait, and no comment left behind
in the file on disk.

---

## 5. Getting context back out, for an LLM

This is the actual payoff. Say you're in a brand-new chat with no history, and you want
to ask an LLM to explain or modify `backend/jobs/render_jobs.py` without re-explaining
what it does, what it imports, or what depends on it.

**From VS Code:** open the file, click the llmctx status bar item. Source + context are
merged and placed on your clipboard. Paste directly into the chat.

**From a terminal**, same result:

```powershell
llmctx pack backend\jobs\render_jobs.py
```

Either way, what lands on the clipboard looks like:

```
=== PROJECT ===
PROJECT: CodeA4 | Python, Flask-style app.py/api/jobs, Docker Compose, ...
TASK: Building the code-clustering render pipeline
CONVENTIONS: API route handlers live under backend/api/, ... | Background work goes through backend/jobs/, ...

=== CONTEXT: backend/jobs/render_jobs.py ===
FILE: backend/jobs/render_jobs.py
ROLE: Dispatches SVG/PDF render jobs to the pipeline and updates job status
EXPORTS: enqueue_render_job(), render_worker()
IMPORTS: backend/pipeline/render_svg.py, backend/pipeline/pdf_export.py, backend/models.py
USED BY: backend/api/jobs.py
NOTES: Runs inside the Celery-style worker.py process, not the web process

=== SOURCE: backend/jobs/render_jobs.py ===
<the actual file contents>
```

The LLM gets the full picture in one paste — no separate "let me explain the codebase"
message needed. The PROJECT/TASK lines always come from today's `llmcontext.yaml`, so
changing `task:` shows up in the very next pack. `USED BY` lists the files whose own
context names this one in `IMPORTS`.

Two more ways to hand context over:

- **Pack current file with the context of its imports** (Command Palette, or
  `llmctx pack <file> --with-imports`) adds the context — not the source — of every
  project file it imports, so the LLM knows what `render_svg.py` and `pdf_export.py` do
  without you pasting them.
- **Copy project map to clipboard** (or `llmctx map`) gives one line per file with its
  role: a compact way to open a new chat about the whole project.

If you use Claude Code or another MCP client, skip the clipboard entirely: register
`llmctx mcp` once (`claude mcp add llmctx -- llmctx mcp` from the CodeA4 root) and the
assistant can look up `project_map`, `get_file_context` and `search_context` itself.

---

## 6. Forcing a refresh

If you significantly rewrite a file by hand and want fresh context immediately, rather
than waiting for the next save's debounce:

```powershell
llmctx reindex backend\pipeline\segmentation.py
```

---

## 7. Moving or backing up the project

All of CodeA4's context lives in one file, `CodeA4\.llmctx\context.db`. Copying the
project folder by any means keeps it: Explorer drag-copy, `robocopy`, a ZIP, an external
or FAT32/exFAT drive, a network share, or a OneDrive/Drive/Dropbox sync. Nothing special
is needed.

Renaming or moving a file *inside* the project is fine too. On its next save (or the next
`llmctx index`), llmctx recognises the unchanged content and carries the context over
without calling Ollama.

Two cases do need a step:

- **`git clone`**: `.llmctx\` ignores itself in git, so a fresh clone has no context. Run
  `llmctx index` at the new location. It's cheap and safe to re-run; only files without
  current context get sent to Ollama.
- **Copying files into a *different* project**: use `cpctx`, which carries each file's
  context into the destination project's store:

  ```powershell
  cpctx copy CodeA4\backend\pipeline D:\OtherProject\pipeline
  ```

If you used llmctx 0.1, which kept context in NTFS streams, run `llmctx migrate` once in
the project root to move that context into the store instead of regenerating it.

---

## 8. Quick troubleshooting

- **Status bar stuck on `$(warning)`** — Ollama isn't running, or is running but no model
  has been pulled. Check Task Manager for `ollama.exe`, then run `ollama list` — if
  `phi3:mini` (or whatever `ollama_model` is set to in `llmcontext.yaml`) isn't listed,
  run `ollama pull phi3:mini` and save the file again. See "Installing Ollama" in the
  main [README](../README.md#installing-ollama-step-by-step) if Ollama isn't installed at
  all — installing Ollama and pulling a model are two separate steps, and it's easy to do
  the first without the second.
- **`llmctx index` reports timeouts** — Ollama is running (index pre-flights the server
  before doing any work and would have refused to start otherwise), it's just slower than
  `ollama_timeout_secs` allows for those files. Raise that value, lower `ollama_max_bytes`,
  or switch to a smaller model. Re-running `llmctx index` retries only the files that
  failed, since everything already done is skipped by content hash.
- **Files reported as "too large"** — they exceed `ollama_max_bytes` (16 KB by default).
  A small model's context window is only a few thousand tokens, so a bigger file gets
  truncated server-side and produces a useless answer after a long wait. Add such files to
  `llmctx_ignore`, or raise the limit if your model can handle it.
- **Context missing entirely after a copy** — the `.llmctx\` folder didn't come along.
  Either the files were copied without the project root (use `cpctx copy` for that), or
  the project came from `git clone`. Run `llmctx index` at the destination.
- **Stale entries for deleted or renamed files** — the next `llmctx index` removes them;
  `llmctx gc` does just that step.
- **`refusing to send source to …`** — `ollama_url` in `llmcontext.yaml` points at another
  machine, which a repository's config isn't allowed to do. Set `LLMCTX_OLLAMA_URL` (or
  `llmctx.ollamaUrl` in your VS Code user settings) instead.
- **Status bar stuck on `$(plug) llmctx`** — the extension can't reach the daemon. If it
  said it couldn't start `llmctxd`, put the binary on your PATH or set
  `llmctx.daemonPath`.
- **A block stayed in the file** — it's committed in git, so llmctx stored it but left it
  in place rather than change everyone's working tree. Remove it in a commit if you'd
  rather it lived only in the store.
- **A source fix seems to have had no effect** — you almost certainly did not
  rebuild, or rebuilt but left an older binary earlier on your PATH. Confirm with
  `where.exe llmctx`, and check that `llmctx index`'s first log line ends with
  `(timeout Ns, max N bytes per file)`. Full steps in
  [`REBUILDING.md`](REBUILDING.md).
- **`llmctx` not found in a new terminal** — you built the binaries but never ran
  `cpctx setup`, or you're in a terminal that predates it. Open a fresh terminal after
  running setup.

See the main [README](../README.md) for installation and full CLI reference, and
[`llmctx.md`](llmctx.md) for the exact `<<<LLMCTX` block format if you're feeding it to
an LLM directly instead of pasting it into chat.
