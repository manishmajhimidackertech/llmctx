// <<<LLMCTX
// FILE: crates/cpctx/src/main.rs
// ROLE: Context-preserving file/directory copy — clones NTFS ADS streams from source to destination
// EXPORTS: NONE (binary)
// IMPORTS: crates/core/src/ads.rs
// USED BY: UNKNOWN
// NOTES: `cpctx setup` registers the binary on PATH via PowerShell profile and HKCU environment
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

use llmctx_core::ads;

// ── CLI ───────────────────────────────────────────────────────────────────────

#[derive(Parser, Debug)]
#[command(
    name = "cpctx",
    version,
    about = "Context-preserving copy: copies files and re-attaches llmctx ADS streams on Windows/NTFS.",
    long_about = "cpctx copies files or directories the same way `cp` or Explorer would, \
    but also reads the llmctx Alternate Data Stream from every source file and writes it \
    to the destination, so context built up by llmctxd is never lost during a copy.\n\n\
    On non-Windows platforms it falls back to a plain copy (no ADS to preserve).\n\n\
    Run `cpctx setup` once to add the binary to your PATH permanently."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Copy SOURCE to DEST, preserving ADS context streams.
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
    if source.is_dir() {
        copy_dir(source, dest, verbose)
    } else {
        copy_file(source, dest, verbose)
    }
}

/// Recursively copy a directory tree, re-attaching ADS on every file.
fn copy_dir(src_dir: &Path, dst_dir: &Path, verbose: bool) -> Result<()> {
    std::fs::create_dir_all(dst_dir)
        .with_context(|| format!("failed to create {}", dst_dir.display()))?;

    for entry in std::fs::read_dir(src_dir)
        .with_context(|| format!("failed to read dir {}", src_dir.display()))?
    {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst_dir.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir(&src_path, &dst_path, verbose)?;
        } else {
            copy_file(&src_path, &dst_path, verbose)?;
        }
    }
    Ok(())
}

/// Copy a single file then re-attach its ADS to the destination.
fn copy_file(src: &Path, dst: &Path, verbose: bool) -> Result<()> {
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

    // Re-attach ADS.
    match ads::read_ads(src) {
        Ok(context_body) => {
            match ads::write_ads(&dst, &context_body) {
                Ok(()) => {
                    if verbose {
                        println!("context {}", dst.display());
                    } else {
                        info!("restored ADS → {}", dst.display());
                    }
                }
                Err(e) => {
                    warn!("could not write ADS to {}: {e}", dst.display());
                }
            }
        }
        Err(ads::AdsError::Empty) | Err(ads::AdsError::VersionMismatch { .. }) => {
            // Source has no valid context yet — nothing to copy. This is normal.
        }
        Err(ads::AdsError::NotSupported) => {
            // Non-Windows — silently skip, we already copied the file content.
        }
        Err(e) => {
            warn!("could not read ADS from {}: {e}", src.display());
        }
    }

    Ok(())
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
    println!("cpctx is a Windows/NTFS tool (ADS is not available on this platform),");
    println!("but the binary still works as a plain `cp` replacement on other OSes.");
    println!();
    println!("To add cpctx to your PATH, add the following line to your shell profile");
    println!("(~/.bashrc, ~/.zshrc, ~/.profile, etc.):");
    println!();
    println!("  export PATH=\"{bin_dir}:$PATH\"");
    println!();
    println!("Then reload your shell:");
    println!("  source ~/.bashrc   # or ~/.zshrc, etc.");
}
