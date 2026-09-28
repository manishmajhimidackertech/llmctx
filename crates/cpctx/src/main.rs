// <<<LLMCTX
// FILE: crates/cpctx/src/main.rs
// ROLE: Context-preserving file/directory copy — carries stored context from the source project's store to the destination's
// EXPORTS: NONE (binary)
// IMPORTS: crates/core/src/store.rs
// USED BY: UNKNOWN
// NOTES: Copies files first, then context, so the destination's llmcontext.yaml is in place before roots are resolved; `cpctx setup` registers binaries on PATH
// LLMCTX>>>

use std::{
    ffi::OsStr,
    path::{Path, PathBuf},
};

// `bail!` is used only inside the `#[cfg(target_os = "windows")]` PATH-setup
// code below, so a non-Windows build reports it as an unused import. Do not
// "fix" that warning by removing it — Windows builds need it.
#[cfg_attr(not(target_os = "windows"), allow(unused_imports))]
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use tracing::{info, warn};

use llmctx_core::store::{self, set_field, ContextStore, StoreSet, STORE_DIR};

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "cpctx",
    version,
    about = "Context-preserving copy: copies files and carries their llmctx context along.",
    long_about = "cpctx copies files or directories the same way `cp` or Explorer would, \
    and also carries each file's llmctx context from the source project's .llmctx store \
    into the destination project's store, so context is never lost when files move \
    between projects.\n\n\
    Copying a whole project folder with any tool already keeps its context (it lives in \
    .llmctx/ inside the folder); cpctx is for copying files into a different project.\n\n\
    Run `cpctx setup` once to add the binary to your PATH permanently."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Copy SOURCE to DEST, carrying llmctx context along.
    ///
    /// If SOURCE is a directory, copies recursively (like `cp -r`).
    /// Existing files at DEST are overwritten.
    Copy {
        /// Source file or directory
        source: PathBuf,
        /// Destination file or directory
        dest: PathBuf,

        /// Print each file copied (default: silent)
        #[arg(short, long)]
        verbose: bool,
    },

    /// Register cpctx on the system PATH so it is available everywhere.
    ///
    /// On Windows this writes to both the PowerShell profile and the user-level
    /// HKCU\Environment PATH registry key (no admin rights required).
    /// On other platforms it prints instructions for adding the binary to PATH manually.
    Setup {
        /// Override the directory that contains the cpctx binary.
        /// Defaults to the directory of the currently-running executable.
        #[arg(long)]
        bin_dir: Option<PathBuf>,
    },
}

// ── Entry point ───────────────────────────────────────────────────────────────

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("cpctx=info".parse()?)
                .add_directive("llmctx_core=warn".parse()?),
        )
        .without_time()
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Copy { source, dest, verbose } => cmd_copy(&source, &dest, verbose),
        Command::Setup { bin_dir } => cmd_setup(bin_dir),
    }
}

// ── copy ─────────────────────────────────────────────────────────────────────

fn cmd_copy(source: &Path, dest: &Path, verbose: bool) -> Result<()> {
    // Phase 1: copy the bytes, remembering every (source, destination) pair.
    let mut copied = Vec::new();
    if source.is_dir() {
        copy_dir(source, dest, verbose, &mut copied)?;

        // A copied folder that lands outside every project becomes a project
        // of its own, so its files have somewhere to keep their context.
        if store::project_root(dest).is_none() {
            ContextStore::open(dest)
                .with_context(|| format!("failed to create context store in {}", dest.display()))?;
        }
    } else {
        copy_file(source, dest, verbose, &mut copied)?;
    }

    // Phase 2: carry context across. This runs only after every file is in
    // place, so a copied llmcontext.yaml already marks the destination root.
    let carried = carry_context(&copied, verbose);
    info!("copied {} file(s), carried context for {carried}", copied.len());
    Ok(())
}

/// Recursively copy a directory tree.
///
/// `.llmctx/` directories are not copied byte-for-byte: the destination gets
/// its own store, filled per file in phase 2, so a database that happens to
/// be mid-write is never duplicated.
fn copy_dir(
    src_dir: &Path,
    dst_dir: &Path,
    verbose: bool,
    copied: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    std::fs::create_dir_all(dst_dir)
        .with_context(|| format!("failed to create {}", dst_dir.display()))?;

    for entry in std::fs::read_dir(src_dir)
        .with_context(|| format!("failed to read dir {}", src_dir.display()))?
    {
        let entry = entry?;
        if entry.file_name() == STORE_DIR {
            continue;
        }
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path, verbose, copied)?;
        } else {
            copy_file(&src_path, &dst_path, verbose, copied)?;
        }
    }
    Ok(())
}

/// Copy a single file's bytes.
fn copy_file(
    src: &Path,
    dst: &Path,
    verbose: bool,
    copied: &mut Vec<(PathBuf, PathBuf)>,
) -> Result<()> {
    // If dest is a directory, copy into it with the same filename.
    let dst = if dst.is_dir() {
        dst.join(src.file_name().unwrap_or(OsStr::new("file")))
    } else {
        dst.to_path_buf()
    };

    // Ensure parent directory exists.
    if let Some(parent) = dst.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create parent dir {}", parent.display()))?;
        }
    }

    // Plain file copy.
    std::fs::copy(src, &dst)
        .with_context(|| format!("failed to copy {} → {}", src.display(), dst.display()))?;

    if verbose {
        println!("copied  {}", dst.display());
    }
    copied.push((src.to_path_buf(), dst));
    Ok(())
}

/// Copy each source file's stored context to its destination. Failures are
/// warnings, never errors: the bytes are already copied, and missing context
/// is regenerated by the next `llmctx index`. Returns how many were carried.
fn carry_context(copied: &[(PathBuf, PathBuf)], verbose: bool) -> usize {
    let mut stores = StoreSet::new();
    let mut carried = 0;

    for (src, dst) in copied {
        let context = match stores.existing_for_file(src) {
            Ok(Some((store, rel))) => match store.get(&rel) {
                Ok(Some(context)) => context,
                // No context for this file yet — nothing to carry. Normal.
                Ok(None) => continue,
                Err(e) => {
                    warn!("could not read context for {}: {e}", src.display());
                    continue;
                }
            },
            // Source is outside any project or has no store yet.
            Ok(None) => continue,
            Err(e) => {
                warn!("could not open context store for {}: {e}", src.display());
                continue;
            }
        };

        let written = stores.for_file(dst).and_then(|(store, rel)| {
            let fields = set_field(&context.fields, "FILE", &rel);
            store.put(&rel, &context.content_hash, &context.source, &fields)
        });
        match written {
            Ok(()) => {
                carried += 1;
                if verbose {
                    println!("context {}", dst.display());
                }
            }
            Err(e) => warn!("could not store context for {}: {e}", dst.display()),
        }
    }
    carried
}

// ── setup ─────────────────────────────────────────────────────────────────────

fn cmd_setup(bin_dir_override: Option<PathBuf>) -> Result<()> {
    let bin_dir = match bin_dir_override {
        Some(d) => d,
        None => {
            let exe = std::env::current_exe()
                .context("could not determine path of current executable")?;
            exe.parent()
                .context("executable has no parent directory")?
                .to_path_buf()
        }
    };

    let bin_dir_str = bin_dir
        .to_str()
        .context("binary directory path contains non-UTF-8 characters")?;

    println!("cpctx binary directory: {}", bin_dir_str);

    #[cfg(target_os = "windows")]
    {
        windows_setup(bin_dir_str)?;
    }
    #[cfg(not(target_os = "windows"))]
    {
        posix_setup_instructions(bin_dir_str);
    }

    Ok(())
}

// ── Windows setup ─────────────────────────────────────────────────────────────

#[cfg(target_os = "windows")]
fn windows_setup(bin_dir: &str) -> Result<()> {
    // 1. Add to HKCU\Environment\Path (persistent for the user, no admin needed).
    registry_add_to_path(bin_dir)?;

    // 2. Append to PowerShell profile so `$env:PATH` reflects it in new sessions
    //    without a full logoff/logon cycle.
    powershell_profile_add(bin_dir)?;

    println!();
    println!("Done. Open a new PowerShell window and run:");
    println!("  cpctx --version");
    println!();
    println!("To make it available in the CURRENT window without restarting:");
    println!("  $env:PATH = \"{bin_dir};\" + $env:PATH");
    Ok(())
}

#[cfg(target_os = "windows")]
fn registry_add_to_path(bin_dir: &str) -> Result<()> {
    use std::process::Command;

    // Read the current user PATH from the registry.
    let output = Command::new("reg")
        .args(["query", r"HKCU\Environment", "/v", "Path"])
        .output()
        .context("failed to run reg query")?;

    let current = if output.status.success() {
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Parse: last word on the "Path" line is the value.
        stdout
            .lines()
            .find(|l| l.trim_start().starts_with("Path"))
            .and_then(|l| l.splitn(4, "    ").nth(3))
            .unwrap_or("")
            .trim()
            .to_string()
    } else {
        String::new()
    };

    // Only add if not already present (case-insensitive).
    let lower_dir = bin_dir.to_lowercase();
    if current.to_lowercase().contains(&lower_dir) {
        println!("PATH already contains the binary directory — skipping registry update.");
        return Ok(());
    }

    let new_path = if current.is_empty() {
        bin_dir.to_string()
    } else {
        format!("{};{}", bin_dir, current)
    };

    let status = Command::new("reg")
        .args([
            "add",
            r"HKCU\Environment",
            "/v",
            "Path",
            "/t",
            "REG_EXPAND_SZ",
            "/d",
            &new_path,
            "/f",
        ])
        .status()
        .context("failed to run reg add")?;

    if !status.success() {
        bail!("reg add failed with exit code {status}");
    }

    println!("Added to HKCU\\Environment\\Path (permanent user PATH).");
    Ok(())
}

#[cfg(target_os = "windows")]
fn powershell_profile_add(bin_dir: &str) -> Result<()> {
    use std::process::Command;

    // Ask PowerShell for its profile path.
    let output = Command::new("powershell")
        .args(["-NoProfile", "-Command", "$PROFILE"])
        .output()
        .context("failed to query PowerShell profile path")?;

    if !output.status.success() {
        warn!("could not determine PowerShell profile path — skipping profile update");
        return Ok(());
    }

    let profile_path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if profile_path.is_empty() {
        warn!("empty PowerShell profile path — skipping");
        return Ok(());
    }

    let profile = std::path::Path::new(&profile_path);

    // Read existing profile if it exists.
    let existing = if profile.exists() {
        std::fs::read_to_string(profile).unwrap_or_default()
    } else {
        String::new()
    };

    let sentinel = format!("# cpctx PATH: {bin_dir}");
    if existing.contains(&sentinel) {
        println!("PowerShell profile already patched — skipping.");
        return Ok(());
    }

    // Append the PATH addition.
    let addition = format!(
        "\n{sentinel}\n\
        if ($env:PATH -notlike \"*{bin_dir}*\") {{\n\
            $env:PATH = \"{bin_dir};\" + $env:PATH\n\
        }}\n"
    );

    // Create parent directory if it doesn't exist.
    if let Some(parent) = profile.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create profile dir {}", parent.display()))?;
    }

    let mut content = existing;
    content.push_str(&addition);
    std::fs::write(profile, content)
        .with_context(|| format!("failed to write PowerShell profile {}", profile.display()))?;

    println!("Patched PowerShell profile: {profile_path}");
    Ok(())
}

// ── Non-Windows setup (instructions only) ─────────────────────────────────────

#[cfg(not(target_os = "windows"))]
fn posix_setup_instructions(bin_dir: &str) {
    println!();
    println!("To add cpctx to your PATH, add the following line to your shell profile");
    println!("(~/.bashrc, ~/.zshrc, ~/.profile, etc.):");
    println!();
    println!("  export PATH=\"{bin_dir}:$PATH\"");
    println!();
    println!("Then reload your shell:");
    println!("  source ~/.bashrc   # or ~/.zshrc, etc.");
}
