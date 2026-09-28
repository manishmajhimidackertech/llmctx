# llmctx — Context Comment Skill

Use this skill whenever you generate or rewrite a source code file for a project that
uses llmctx. Triggers: the project contains an `llmcontext.yaml` file, the user mentions
"llmctx", or you are writing a new source file as part of an ongoing coding task where
previous files in the conversation already use this format.

## What this is for

llmctx is a local tool that stores per-file context invisibly (in a hidden `.llmctx/`
store at the project root, outside the source files) so any LLM in a future session can instantly understand a file
without re-explanation. A background daemon normally generates this context after the
fact using a local model — but when you (the LLM) are the one writing the file, you
already know everything that derived context would contain. Writing it yourself, once,
as part of the file you're already producing, is free. Making the daemon re-derive it
later from scratch is wasted work for a worse result.

Your job: emit one small comment block at the top of every source file you write,
using the exact format below. A local tool will detect it, move it into invisible
storage, and strip it back out of the file — so the user's actual source file stays
completely clean. You don't need to do anything else; just include the block.

## The format

Wrap this exact block in whatever comment syntax the file's language uses. The content
between the delimiters is identical across every language — only the comment prefix
changes.

```
<<<LLMCTX
FILE: <path relative to project root>
ROLE: <one sentence — what this file does>
EXPORTS: <key functions/classes/constants this file exposes, comma separated, or NONE>
IMPORTS: <other project files this depends on, as paths relative to the project root, comma separated, or NONE>
USED BY: <files likely to import this, comma separated, or UNKNOWN>
NOTES: <anything unusual a future LLM should know, or NONE>
LLMCTX>>>
```

**Only these six fields — nothing about the project as a whole, and no version marker.** You
may see a `PROJECT:`, `TASK:`, `CONVENTIONS:`, or `LLMCTX_VERSION:` line in context blocks
elsewhere in this conversation or in files you read. Do not include any of those fields
yourself. The local tool adds them
automatically from the project's own configuration after extracting your block — it
knows that information more reliably than you do mid-conversation, and duplicating it
here would risk the two going out of sync.

**Rules:**
- The delimiters `<<<LLMCTX` and `LLMCTX>>>` must appear literally, each on their own line, with no other text on that line besides the comment prefix.
- Every line between the delimiters must use the same comment prefix as the delimiter lines themselves.
- Keep ROLE to one sentence. Keep NOTES short — a phrase or two, not a paragraph. This is a lookup aid, not documentation.
- Always include all six fields, in this order. Use `NONE` or `UNKNOWN` rather than omitting a field.
- **`USED BY` is a guess, not a fact — treat it that way.** You cannot actually know what will import a file you're writing right now, especially a brand-new one with no current importers. Give your best inference from naming conventions and the surrounding code you can see, but don't present it with false confidence, and use `UNKNOWN` freely rather than inventing plausible-sounding file paths. Once other files' context lists this file in their `IMPORTS`, the tool shows those real importers instead of your guess.
- Place the block as the very first thing in the file, before any other comments, imports, or code — except where a language requires something to come first (e.g. `#!/usr/bin/env python3` shebang lines, or a Rust `#![...]` crate attribute) — in which case the block comes immediately after that. **A block anywhere else is ignored** (that is what keeps documentation that merely shows the format from being rewritten), so it will not be picked up.
- Write `IMPORTS` as paths relative to the project root (`models/user.py`, not `from models import User` or `../models/user.py`). The tool builds every file's `USED BY` from the other files' `IMPORTS`, so accurate paths here are worth more than a guessed `USED BY`.
- Leave exactly one blank line between the closing delimiter and the start of real code.
- Do not add extra commentary inside the block, and do not explain to the user that you added it — it's invisible infrastructure, not a feature to narrate.

## Examples by language

**Python** (`#` prefix):
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

**JavaScript / TypeScript** (`//` prefix):
```javascript
// <<<LLMCTX
// FILE: utils/formatDate.js
// ROLE: Formats ISO timestamps for display in the UI
// EXPORTS: formatDate(), formatRelativeTime()
// IMPORTS: NONE
// USED BY: components/Timeline.jsx, components/Comment.jsx
// NOTES: Assumes UTC input, converts to user's local timezone
// LLMCTX>>>

export function formatDate(iso) { ... }
```

**Rust** (`//` prefix):
```rust
// <<<LLMCTX
// FILE: crates/core/src/store.rs
// ROLE: Per-project context store — one SQLite database at <root>/.llmctx/context.db
// EXPORTS: ContextStore, StoreError, project_root()
// IMPORTS: crates/core/src/config.rs
// USED BY: crates/core/src/process.rs, crates/cli/src/main.rs
// NOTES: Keys are root-relative paths; content hash is a secondary key for renames
// LLMCTX>>>

pub struct ContextStore { ... }
```

**C-style block comment** (for languages where `//` line comments aren't idiomatic at file
top, e.g. CSS):
```css
/* <<<LLMCTX
FILE: styles/buttons.css
ROLE: Button variants and hover states for the design system
EXPORTS: .btn, .btn-primary, .btn-secondary, .btn-danger
IMPORTS: NONE
USED BY: every component using a button
NOTES: NONE
LLMCTX>>> */

.btn { ... }
```

**HTML** (`<!-- -->`):
```html
<!-- <<<LLMCTX
FILE: templates/email/welcome.html
ROLE: Welcome email template sent on signup
EXPORTS: NONE
IMPORTS: NONE
USED BY: email/sender.py
NOTES: Variables use {{ }} Jinja2 syntax, rendered server-side
LLMCTX>>> -->

<!DOCTYPE html>
...
```

## What happens after you write the block

This is informational only — you don't need to do anything beyond emitting the block:

1. The user saves the file in VS Code (or the file already exists on disk if generated outside the editor).
2. The llmctx daemon detects the `<<<LLMCTX` delimiter before considering any other context generation step. This check happens immediately on save — unlike its local-model generation path, extraction isn't delayed or debounced, since it's just a string cut rather than a model call.
3. It cuts the block out of the source file entirely — including the comment markers — and writes its contents into the project's hidden context store. Project-level information (name, stack, task, conventions) is added from the project's own configuration file whenever the context is handed to an LLM, so you don't need to know or guess it. The `FILE` field is always set to the file's real path.
4. The source file that remains on disk is exactly what you'd have written without this skill: no leftover comment, no marker, nothing visible.
5. The local context-generation step (which would otherwise run a small local model over the file) is skipped for this file, since your context already covers it.

If the file lives somewhere the project has marked as ignored (build output, vendored
dependencies, etc.), none of this matters — those files aren't processed either way.
You don't need to check for this; just write the block as normal and let the tool decide.

If a human later hand-edits the file significantly, the tool will eventually notice the
drift and fall back to its normal local generation for that file — you don't need to
account for this, it's handled automatically.

**If you get the format wrong** — mismatched delimiters, a missing field — the tool
does not guess or partially apply your block. It leaves your comment exactly as written
in the source file and falls back to generating context locally instead, the same as if
you'd written no block at all. This means getting the delimiters exactly right matters:
a malformed block doesn't just fail quietly, it also leaves a stray comment sitting in
the user's source file that a human will eventually need to notice and remove. Double-check
the opening `<<<LLMCTX` and closing `LLMCTX>>>` lines match exactly before moving on.
