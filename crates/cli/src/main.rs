// <<<LLMCTX
// FILE: crates/cli/src/main.rs
// ROLE: CLI binary — init, index, pack, map, reindex, extract, migrate, gc, mcp commands via clap
// EXPORTS: NONE (binary)
// IMPORTS: crates/core/src/process.rs, crates/core/src/pack.rs, crates/core/src/store.rs, crates/core/src/migrate.rs, crates/core/src/config.rs, crates/cli/src/mcp.rs
// USED BY: vscode-extension/src/pack.ts (shells out to pack/map/reindex)
// NOTES: index respects .gitignore + llmctx_ignore and never descends into .llmctx/; on Linux the clipboard is served by a detached helper so it survives this process
// LLMCTX>>>

mod mcp;

use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::Arc,
};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use ignore::WalkBuilder;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use llmctx_core::{
    config::{self, ProjectConfig, CONFIG_FILENAME},
    migrate::{self, ImportOutcome},
    ollama::{OllamaClient, OllamaError},
    pack,
    process::{self, ProcessError, ProcessOptions, ProcessSource},
    store::{self, ContextStore, StoreSet, STORE_DIR},
};

// ── CLI definition ────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "llmctx",
    version,
    about = "llmctx — attach LLM context to your source files"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Write a llmcontext.yaml template in the current directory (non-interactive).
    Init,

    /// Walk all project files and generate/extract context for each one.
    ///
    /// Resumable: files whose stored context already matches their current
    /// content are skipped, so re-running after an interrupted or partly
    /// failed run only does the outstanding work. Context for files that no
    /// longer exist under DIR is removed at the end.
    Index {
        /// Directory to index (default: current working directory).
        #[arg(default_value = ".")]
        dir: PathBuf,

        /// Regenerate every file even if its stored context is already current.
        #[arg(long)]
        force: bool,
    },

    /// Merge the project header, the file's context and its source, and copy
    /// the result to the clipboard (same as clicking the status bar button).
    Pack {
        /// Source file to pack.
        file: PathBuf,

        /// Print the result to stdout instead of copying it.
        #[arg(long)]
        stdout: bool,

        /// Also include the context of the project files it imports.
        #[arg(long)]
        with_imports: bool,
    },

    /// Print a one-line-per-file overview of the project (path and role).
    Map {
        /// Any directory inside the project (default: current working directory).
        #[arg(default_value = ".")]
        dir: PathBuf,

        /// Copy to the clipboard instead of printing.
        #[arg(long)]
        copy: bool,
    },

    /// Force Ollama regeneration for a single file, bypassing the extraction check.
    Reindex {
        /// Source file to reindex.
        file: PathBuf,
    },

    /// Run process_file() on a single file (extraction or Ollama, same as daemon).
    Extract {
        /// Source file to process.
        file: PathBuf,

        /// Process even if stored context is already current for this content.
        #[arg(long)]
        force: bool,
    },

    /// Copy context from NTFS Alternate Data Streams (llmctx 0.1) into the
    /// project's .llmctx store. Windows only; safe to run more than once.
    Migrate {
        /// Directory to migrate (default: current working directory).
        #[arg(default_value = ".")]
        dir: PathBuf,

        /// Delete each stream once its context is safely in the store.
        #[arg(long)]
        remove_streams: bool,
    },

    /// Remove stored context for files that no longer exist.
    Gc {
        /// Any directory inside the project (default: current working directory).
        #[arg(default_value = ".")]
        dir: PathBuf,
    },

    /// Serve the project's context to MCP clients (Claude Code, Claude
    /// Desktop, …) over stdio.
    Mcp {
        /// Project directory (default: the project containing the current
        /// working directory).
        #[arg(long)]
        root: Option<PathBuf>,
    },

    /// Internal: hold clipboard contents read from stdin until replaced.
    #[command(name = "__serve-clipboard", hide = true)]
    ServeClipboard,
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // stdout carries the MCP protocol, so logs must stay on stderr (they do)
    // and be quieter there.
    let default_level = if matches!(cli.command, Command::Mcp { .. }) {
        "warn"
    } else {
        "info"
    };
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(format!("llmctx={default_level}").parse()?)
                .add_directive(format!("llmctx_core={default_level}").parse()?),
        )
        .init();

    let client = Arc::new(OllamaClient::new());

    match cli.command {
        Command::Init => cmd_init()?,
        Command::Index { dir, force } => cmd_index(&dir, Arc::clone(&client), force).await?,
        Command::Pack {
            file,
            stdout,
            with_imports,
        } => cmd_pack(&file, stdout, with_imports)?,
        Command::Map { dir, copy } => cmd_map(&dir, copy)?,
        Command::Reindex { file } => cmd_reindex(&file, Arc::clone(&client)).await?,
        Command::Extract { file, force } => cmd_extract(&file, Arc::clone(&client), force).await?,
        Command::Migrate {
            dir,
            remove_streams,
        } => cmd_migrate(&dir, remove_streams)?,
        Command::Gc { dir } => cmd_gc(&dir)?,
        Command::Mcp { root } => cmd_mcp(root)?,
        Command::ServeClipboard => serve_clipboard()?,
    }

    Ok(())
}

// ── init ──────────────────────────────────────────────────────────────────────

fn cmd_init() -> Result<()> {
    let dest = Path::new(CONFIG_FILENAME);
    if dest.exists() {
        eprintln!("{CONFIG_FILENAME} already exists — not overwriting.");
        return Ok(());
    }

    let template = r##"# llmctx project configuration
# Edit this file, then run `llmctx index` to generate context for all files.

project: ""  # your project name here
stack: ""    # e.g. Rust, tokio, serde
task: ""     # current task or sprint goal

conventions:
  # - "e.g. No unwrap() in library code"
  # - "e.g. All DB models live in /models"

# Files to skip in addition to those already excluded by .gitignore.
# Supports glob patterns relative to this file.
llmctx_ignore:
  # - "generated/**"
  # - "vendor/**"

# Ollama settings (defaults shown — remove to use defaults)
#
# Only a server on this machine is accepted here, because this file is part
# of the repository. To use a remote server, set LLMCTX_OLLAMA_URL (or
# `llmctx.ollamaUrl` in VS Code) instead.
# ollama_url: "http://127.0.0.1:11434"
# ollama_model: "phi3:mini"
#
# Ollama serialises requests unless OLLAMA_NUM_PARALLEL is raised, so a value
# above 1 usually just multiplies each request's wall time instead of
# overlapping real work.
# ollama_concurrency: 1
#
# Per-request budget for one generation. Small models on modest hardware can
# take minutes for a few kilobytes of source — raise this if you see timeouts.
# ollama_timeout_secs: 300
#
# Files larger than this are skipped instead of sent. A small model's context
# window is only a few thousand tokens, so a big file is truncated server-side
# and answers slowly with nothing useful.
# ollama_max_bytes: 16384
#
# Context window requested from Ollama, in tokens. The default fits a file of
# ollama_max_bytes plus the prompt.
# ollama_num_ctx: 7168
"##;

    std::fs::write(dest, template).with_context(|| format!("failed to write {CONFIG_FILENAME}"))?;
    println!("Created {CONFIG_FILENAME} — edit it, then run `llmctx index`.");
    Ok(())
}

// ── index ─────────────────────────────────────────────────────────────────────

/// Config governing `dir`, or defaults (with a warning) if there is none.
fn load_dir_config(dir: &Path) -> (PathBuf, ProjectConfig) {
    match config::find_config(dir).and_then(|p| config::load_config(&p).map(|c| (p, c))) {
        Ok(pair) => pair,
        Err(e) => {
            warn!("no config found in {}: {e} — using defaults", dir.display());
            (dir.join(CONFIG_FILENAME), ProjectConfig::default())
        }
    }
}

/// Walker over the project files under `dir`: honours .gitignore (even
/// outside a git checkout, like the daemon) and the config's `llmctx_ignore`,
/// and never descends into the context store.
fn project_walker(dir: &Path, config_path: &Path, config: &ProjectConfig) -> Result<WalkBuilder> {
    let mut walker = WalkBuilder::new(dir);
    walker.standard_filters(true); // honours .gitignore, .git/, hidden files
    walker.require_git(false);

    // Add llmctx_ignore patterns as overrides.
    // `ignore` crate supports adding override globs directly.
    let mut overrides =
        ignore::overrides::OverrideBuilder::new(config_path.parent().unwrap_or(dir));
    for pattern in &config.llmctx_ignore {
        // Prefix with `!` to turn them into ignore patterns (OverrideBuilder
        // treats un-prefixed patterns as whitelist; `!` means exclude).
        overrides
            .add(&format!("!{pattern}"))
            .with_context(|| format!("invalid llmctx_ignore pattern: {pattern}"))?;
    }
    walker.overrides(overrides.build()?);

    // Hidden-file filtering already skips `.llmctx/` on most platforms, but
    // Windows decides "hidden" by attribute — so exclude it explicitly.
    walker.filter_entry(|e| e.file_name() != STORE_DIR);
    Ok(walker)
}

async fn cmd_index(dir: &Path, client: Arc<OllamaClient>, force: bool) -> Result<()> {
    // Load config to get llmctx_ignore and concurrency setting.
    let (config_path, config) = load_dir_config(dir);
    let concurrency = config.ollama_concurrency_or_default();

    // ── Pre-flight ────────────────────────────────────────────────────────────
    // Probe Ollama once before doing any work. Without this, a stopped server
    // is only discovered one file at a time, each after a full generation
    // timeout — turning a five-second diagnosis into a multi-minute one.
    match client.list_models(&config).await {
        Ok(models) => {
            let want = config.ollama_model_or_default();
            // Ollama reports tags as `name:tag`; a bare `name` means `:latest`.
            let have = models
                .iter()
                .any(|m| m == want || m.strip_suffix(":latest").is_some_and(|base| base == want));
            if !have {
                warn!(
                    "model '{want}' is not installed (found: {}). Run `ollama pull {want}` — \
                     otherwise every file will fail or stall while Ollama tries to fetch it.",
                    if models.is_empty() {
                        "none".to_string()
                    } else {
                        models.join(", ")
                    }
                );
            }
        }
        Err(e) if e.is_unreachable() || e.is_timeout() => {
            anyhow::bail!(
                "{e}\n\nNothing was indexed. Start Ollama (`ollama serve`) and try again."
            );
        }
        Err(e @ (OllamaError::RemoteNotAllowed { .. } | OllamaError::InvalidUrl { .. })) => {
            anyhow::bail!("{e}\n\nNothing was indexed.");
        }
        Err(e) => {
            // Reachable but odd (unexpected HTTP status, unparseable body).
            // Not worth aborting the run over.
            warn!("could not list Ollama models: {e} — continuing anyway");
        }
    }

    info!(
        "indexing {} with up to {concurrency} concurrent Ollama jobs \
         (timeout {}s, max {} bytes per file){}",
        dir.display(),
        config.ollama_timeout_secs_or_default(),
        config.ollama_max_bytes_or_default(),
        if force { ", --force" } else { "" }
    );

    let walker = project_walker(dir, &config_path, &config)?;

    // Stream files through a bounded channel so we never hold all paths in
    // memory at once.  This is important for large repos (100k+ files): the
    // old collect-all-then-spawn approach would create one tokio task per file
    // before any of them ran, which is wasteful and could OOM on huge trees.
    //
    // Pattern: one producer task walks the directory and sends paths into a
    // bounded channel; `concurrency` consumer tasks pull from it and call
    // process_file.
    let (path_tx, path_rx) = async_channel::bounded::<PathBuf>(concurrency * 4);

    // Producer: walks the directory tree and feeds paths into the channel.
    let producer = tokio::task::spawn_blocking(move || {
        for entry in walker.build().flatten() {
            if entry.file_type().is_some_and(|t| t.is_file()) {
                // send() blocks when channel is full — that's intentional
                // backpressure so the walker doesn't outrun the workers.
                if path_tx.send_blocking(entry.into_path()).is_err() {
                    break; // receivers dropped (shouldn't happen)
                }
            }
        }
        // path_tx is dropped here, which closes the channel and signals workers
        // to drain and exit.
    });

    // Consumers: `concurrency` tasks, each pulling one path at a time.
    //
    // Outcomes are logged inside the worker, as they happen, rather than
    // collected and printed after every worker has finished, so a slow file
    // is distinguishable from a hung one while it is happening.
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<Outcome>();

    let opts = if force {
        ProcessOptions::forced()
    } else {
        ProcessOptions::new()
    };

    let mut worker_handles = Vec::with_capacity(concurrency);
    for _ in 0..concurrency {
        let rx = path_rx.clone();
        let client = Arc::clone(&client);
        let result_tx = result_tx.clone();
        worker_handles.push(tokio::spawn(async move {
            while let Ok(path) = rx.recv().await {
                let outcome = match process::process_file(&path, &client, opts).await {
                    Ok(r) => {
                        if !matches!(
                            r.source,
                            ProcessSource::SkippedTooSmall
                                | ProcessSource::SkippedUnreadable
                                | ProcessSource::SkippedIgnored
                                | ProcessSource::SkippedNoProject
                        ) {
                            info!("{}: {}", path.display(), r.source.describe());
                        }
                        match r.source {
                            ProcessSource::Extracted
                            | ProcessSource::ExtractedKept
                            | ProcessSource::Ollama => Outcome::Processed,
                            ProcessSource::UpToDate => Outcome::UpToDate,
                            ProcessSource::Reused => Outcome::Reused,
                            ProcessSource::SkippedTooLarge => Outcome::TooLarge,
                            ProcessSource::SkippedTooSmall
                            | ProcessSource::SkippedUnreadable
                            | ProcessSource::SkippedIgnored
                            | ProcessSource::SkippedNoProject => Outcome::Skipped,
                        }
                    }
                    Err(e) => {
                        error!("{}: {e}", path.display());
                        Outcome::Error(is_ollama_timeout(&e))
                    }
                };
                let _ = result_tx.send(outcome);
            }
        }));
    }
    // Drop our copy so result_rx drains once all workers finish.
    drop(result_tx);
    drop(path_rx);

    // Wait for the producer, then all workers.
    let _ = producer.await;
    for h in worker_handles {
        let _ = h.await;
    }

    // Tally results (each was already logged by the worker that produced it).
    let mut ok = 0usize;
    let mut up_to_date = 0usize;
    let mut reused = 0usize;
    let mut skipped = 0usize;
    let mut too_large = 0usize;
    let mut errors = 0usize;
    let mut timeouts = 0usize;

    while let Some(outcome) = result_rx.recv().await {
        match outcome {
            Outcome::Processed => ok += 1,
            Outcome::UpToDate => up_to_date += 1,
            Outcome::Reused => reused += 1,
            Outcome::Skipped => skipped += 1,
            Outcome::TooLarge => too_large += 1,
            Outcome::Error(was_timeout) => {
                errors += 1;
                if was_timeout {
                    timeouts += 1;
                }
            }
        }
    }

    // Context for files deleted or renamed outside the editor would otherwise
    // linger until `llmctx gc`; the walk just proved which files exist.
    let pruned = prune_under(dir).unwrap_or_else(|e| {
        warn!("could not prune stale context: {e}");
        0
    });

    println!(
        "index complete: {ok} processed, {up_to_date} already current, {reused} carried over, \
         {skipped} skipped, {too_large} too large, {errors} errors, {pruned} stale entries removed"
    );

    if too_large > 0 {
        println!(
            "  {too_large} file(s) exceeded ollama_max_bytes ({} bytes). Add them to \
             llmctx_ignore, or raise the limit if your model can handle it.",
            config.ollama_max_bytes_or_default()
        );
    }
    if timeouts > 0 {
        println!(
            "  {timeouts} file(s) timed out after {}s. Ollama was reachable, just slow — \
             raise ollama_timeout_secs, lower ollama_max_bytes, or use a smaller model. \
             Re-run `llmctx index` to retry only these.",
            config.ollama_timeout_secs_or_default()
        );
    }

    Ok(())
}

/// Remove stored context for missing files under `dir` (the whole store if
/// `dir` is the project root).
fn prune_under(dir: &Path) -> Result<usize> {
    let Some(root) = store::project_root(dir) else {
        return Ok(0);
    };
    let Some(store) = ContextStore::open_existing(&root)? else {
        return Ok(0);
    };
    let rel = store.rel_key(dir)?;
    let removed = store.prune_missing(Some(&rel))?;
    for rel in &removed {
        info!("{rel}: file no longer exists — context removed");
    }
    Ok(removed.len())
}

/// What happened to one file. Kept separate from `ProcessSource` because the
/// summary only needs counts, and errors carry one extra bit: whether the
/// cause was a timeout (retryable, server was up) or something else.
enum Outcome {
    Processed,
    UpToDate,
    Reused,
    Skipped,
    TooLarge,
    Error(bool),
}

/// True if this failure was an Ollama timeout — meaning the server answered
/// the pre-flight probe and is simply slow, so a re-run is worth trying.
fn is_ollama_timeout(e: &ProcessError) -> bool {
    matches!(e, ProcessError::Ollama(o) if o.is_timeout())
}

// ── pack / map ────────────────────────────────────────────────────────────────

fn cmd_pack(file: &Path, stdout: bool, with_imports: bool) -> Result<()> {
    let packed = pack::pack(file, with_imports)
        .with_context(|| format!("failed to pack {}", file.display()))?;

    if !packed.has_context {
        eprintln!(
            "warning: no context yet for {} — daemon may still be generating",
            file.display()
        );
        if matches!(llmctx_core::ads::ads_exists(file), Ok(true)) {
            eprintln!(
                "hint: this file has context from an older llmctx in an NTFS stream — \
                 run `llmctx migrate` in the project root to bring it over"
            );
        }
    }

    deliver(&packed.text, !stdout, &format!("packed {}", file.display()))
}

fn cmd_map(dir: &Path, copy: bool) -> Result<()> {
    let root = store::project_root(dir).unwrap_or_else(|| dir.to_path_buf());
    let map = pack::project_map(&root)?;
    deliver(&map, copy, "project map")
}

/// Copy `text` to the clipboard (falling back to stdout if there is none),
/// or print it.
fn deliver(text: &str, to_clipboard: bool, what: &str) -> Result<()> {
    if !to_clipboard {
        print!("{text}");
        return Ok(());
    }
    match copy_to_clipboard(text) {
        Ok(()) => {
            eprintln!("{what} ({} bytes) → clipboard", text.len());
            Ok(())
        }
        Err(e) => {
            eprintln!("clipboard unavailable ({e}), printing to stdout instead:\n");
            print!("{text}");
            Ok(())
        }
    }
}

/// On Windows and macOS the clipboard is a system service, so setting it and
/// exiting is fine. On Linux (X11 and Wayland) the *copying process* serves
/// the contents and they vanish when it exits — so a detached helper process
/// holds them until something else is copied.
fn copy_to_clipboard(text: &str) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        use std::{io::Write, os::unix::process::CommandExt, process::Stdio, time::Duration};

        let mut child = std::process::Command::new(std::env::current_exe()?)
            .arg("__serve-clipboard")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0) // outlive the terminal's job control
            .spawn()
            .context("could not start the clipboard helper")?;
        child
            .stdin
            .take()
            .context("clipboard helper has no stdin")?
            .write_all(text.as_bytes())?;
        // Give it a moment to claim the clipboard, so a missing display
        // server is reported here instead of failing silently.
        std::thread::sleep(Duration::from_millis(300));
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                anyhow::bail!("no clipboard available (no X11/Wayland display?)");
            }
        }
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        arboard::Clipboard::new()?.set_text(text.to_string())?;
        Ok(())
    }
}

/// The detached helper: set the clipboard from stdin and keep serving it
/// until another application replaces it.
fn serve_clipboard() -> Result<()> {
    let mut text = String::new();
    std::io::stdin().read_to_string(&mut text)?;
    let mut clipboard = arboard::Clipboard::new()?;
    #[cfg(target_os = "linux")]
    {
        use arboard::SetExtLinux;
        clipboard.set().wait().text(text)?;
    }
    #[cfg(not(target_os = "linux"))]
    clipboard.set_text(text)?;
    Ok(())
}

// ── reindex ───────────────────────────────────────────────────────────────────

async fn cmd_reindex(file: &Path, client: Arc<OllamaClient>) -> Result<()> {
    // Clear existing context first so stale data can't bleed through.
    if let Err(e) = store::forget_context(file) {
        warn!("could not clear stored context before reindex: {e}");
    }

    let result = process::process_file(file, &client, ProcessOptions::force_ollama())
        .await
        .with_context(|| format!("reindex failed for {}", file.display()))?;
    println!("{}: {}", file.display(), result.source.describe());
    Ok(())
}

// ── extract ───────────────────────────────────────────────────────────────────

async fn cmd_extract(file: &Path, client: Arc<OllamaClient>, force: bool) -> Result<()> {
    let opts = if force {
        ProcessOptions::forced()
    } else {
        ProcessOptions::new()
    };
    let result = process::process_file(file, &client, opts)
        .await
        .with_context(|| format!("extract failed for {}", file.display()))?;
    let hint = if result.source == ProcessSource::UpToDate {
        " (use --force to regenerate)"
    } else {
        ""
    };
    println!("{}: {}{hint}", file.display(), result.source.describe());
    Ok(())
}

// ── migrate ───────────────────────────────────────────────────────────────────

/// Copy every NTFS-stream context under `dir` into its project's store.
///
/// Idempotent: a file that already has context in the store keeps it (it is
/// at least as new as the stream). Streams are only deleted on request, and
/// only after their context is in the store.
fn cmd_migrate(dir: &Path, remove_streams: bool) -> Result<()> {
    if !cfg!(target_os = "windows") {
        println!("nothing to migrate: NTFS streams only exist on Windows");
        return Ok(());
    }

    let (config_path, config) = load_dir_config(dir);
    let walker = project_walker(dir, &config_path, &config)?;
    let mut stores = StoreSet::new();
    let (mut imported, mut already, mut failed) = (0usize, 0usize, 0usize);

    for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        match migrate::import_stream(path, &mut stores, remove_streams) {
            Ok(ImportOutcome::Imported) => {
                info!("{}: migrated", path.display());
                imported += 1;
            }
            Ok(ImportOutcome::AlreadyStored) => already += 1,
            Ok(ImportOutcome::NoStream) => {}
            Err(e) => {
                warn!("{}: {e}", path.display());
                failed += 1;
            }
        }
    }

    println!(
        "migrate complete: {imported} imported, {already} already in the store, {failed} failed{}",
        if remove_streams {
            " (streams of stored files removed)"
        } else {
            ""
        }
    );
    Ok(())
}

// ── gc ────────────────────────────────────────────────────────────────────────

fn cmd_gc(dir: &Path) -> Result<()> {
    let root = store::project_root(dir).with_context(|| {
        format!(
            "{} is not inside an llmctx project (no {CONFIG_FILENAME}, {STORE_DIR}/ or .git above it)",
            dir.display()
        )
    })?;
    let Some(store) = ContextStore::open_existing(&root)? else {
        println!("no context store under {} — nothing to do", root.display());
        return Ok(());
    };
    let removed = store.prune_missing(None)?;
    for rel in &removed {
        info!("{rel}: file no longer exists — context removed");
    }
    println!(
        "gc complete: removed {} entr{} from {}",
        removed.len(),
        if removed.len() == 1 { "y" } else { "ies" },
        store.db_path().display()
    );
    Ok(())
}

// ── mcp ───────────────────────────────────────────────────────────────────────

fn cmd_mcp(root: Option<PathBuf>) -> Result<()> {
    let start = match root {
        Some(r) => r,
        None => std::env::current_dir()?,
    };
    let root = store::project_root(&start).unwrap_or(start);
    mcp::serve(llmctx_core::fsutil::absolute(&root))
}
