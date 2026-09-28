// <<<LLMCTX
// FILE: crates/core/src/runtime.rs
// ROLE: Per-user daemon discovery file (port + auth token) shared by llmctxd and its clients
// EXPORTS: DaemonInfo, daemon_file(), read_daemon_info(), write_daemon_info(), remove_daemon_info(), new_token(), tokens_match(), RUNTIME_DIR_ENV
// IMPORTS: NONE
// USED BY: crates/daemon/src/main.rs; mirrored by vscode-extension/src/daemon.ts
// NOTES: The file is private to the user (0700 dir / 0600 file on Unix; %LOCALAPPDATA% on Windows), which is what makes the token a secret
// LLMCTX>>>

//! How clients find and authenticate to `llmctxd`.
//!
//! The daemon listens on a port the OS picks (so it never collides with
//! anything), and writes `{port, token, pid, version}` to a file only the
//! current user can read. A client must present the token before the daemon
//! will act on anything it sends, so other users and processes that cannot
//! read the file cannot drive the daemon — and a client never talks to some
//! other program that happens to hold a well-known port.
//!
//! Location (the VS Code extension computes the same path):
//! - `$LLMCTX_RUNTIME_DIR/daemon.json` if set;
//! - Windows: `%LOCALAPPDATA%\llmctx\daemon.json`;
//! - elsewhere: `$XDG_RUNTIME_DIR/llmctx/daemon.json`, else
//!   `~/.cache/llmctx/daemon.json`.

use std::{
    fs,
    io::{self, Write},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

/// Overrides the directory holding `daemon.json` (tests, unusual setups).
pub const RUNTIME_DIR_ENV: &str = "LLMCTX_RUNTIME_DIR";

const FILE_NAME: &str = "daemon.json";

/// What a running daemon publishes about itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInfo {
    pub port: u16,
    pub token: String,
    pub pid: u32,
    pub version: String,
}

/// Directory holding the discovery file, or `None` if no suitable
/// per-user location can be determined.
pub fn runtime_dir() -> Option<PathBuf> {
    let env = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    if let Some(dir) = env(RUNTIME_DIR_ENV) {
        return Some(dir);
    }
    if cfg!(target_os = "windows") {
        return env("LOCALAPPDATA").map(|d| d.join("llmctx"));
    }
    if let Some(dir) = env("XDG_RUNTIME_DIR") {
        return Some(dir.join("llmctx"));
    }
    env("HOME").map(|h| h.join(".cache").join("llmctx"))
}

/// Full path of the discovery file.
pub fn daemon_file() -> Option<PathBuf> {
    runtime_dir().map(|d| d.join(FILE_NAME))
}

/// The published daemon info, if a daemon has written one.
pub fn read_daemon_info() -> Option<DaemonInfo> {
    let text = fs::read_to_string(daemon_file()?).ok()?;
    serde_json::from_str(&text).ok()
}

/// Publish `info`, readable by the current user only. Written to a temp
/// file and renamed so a client never reads half a file.
pub fn write_daemon_info(info: &DaemonInfo) -> io::Result<PathBuf> {
    let dir = runtime_dir().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no per-user runtime directory (set LLMCTX_RUNTIME_DIR)",
        )
    })?;
    create_private_dir(&dir)?;
    let path = dir.join(FILE_NAME);
    let tmp = dir.join(format!("{FILE_NAME}.{}.tmp", std::process::id()));
    {
        let mut file = open_private(&tmp)?;
        file.write_all(serde_json::to_string_pretty(info)?.as_bytes())?;
        file.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Remove the discovery file if it still describes the daemon `pid` (a newer
/// daemon may have replaced it).
pub fn remove_daemon_info(pid: u32) {
    if read_daemon_info().is_some_and(|i| i.pid == pid) {
        if let Some(path) = daemon_file() {
            let _ = fs::remove_file(path);
        }
    }
}

/// 256 random bits from the OS, hex-encoded.
pub fn new_token() -> io::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).map_err(|e| io::Error::other(e.to_string()))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Constant-time comparison, so response timing leaks nothing about the token.
pub fn tokens_match(expected: &str, given: &str) -> bool {
    let (a, b) = (expected.as_bytes(), given.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(unix)]
fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // Tighten an existing directory too.
    fs::set_permissions(dir, fs::Permissions::from_mode(0o700))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &std::path::Path) -> io::Result<()> {
    // %LOCALAPPDATA% is already private to the user.
    fs::create_dir_all(dir)
}

#[cfg(unix)]
fn open_private(path: &std::path::Path) -> io::Result<fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)
}

#[cfg(not(unix))]
fn open_private(path: &std::path::Path) -> io::Result<fs::File> {
    fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_random_hex_and_compare_exactly() {
        let a = new_token().unwrap();
        let b = new_token().unwrap();
        assert_eq!(a.len(), 64);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
        assert!(tokens_match(&a, &a.clone()));
        assert!(!tokens_match(&a, &b));
        assert!(!tokens_match(&a, &a[..63]));
    }

    // One test touches the process-wide env var, so the round trip and the
    // removal check share it rather than racing each other.
    #[test]
    fn discovery_file_round_trip_is_private() {
        let dir = tempfile::TempDir::new().unwrap();
        std::env::set_var(RUNTIME_DIR_ENV, dir.path());

        let info = DaemonInfo {
            port: 4242,
            token: new_token().unwrap(),
            pid: 7,
            version: "0.2.0".into(),
        };
        let path = write_daemon_info(&info).unwrap();
        assert_eq!(read_daemon_info(), Some(info.clone()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        remove_daemon_info(8); // someone else's pid: left alone
        assert!(path.exists());
        remove_daemon_info(7);
        assert!(!path.exists());

        std::env::remove_var(RUNTIME_DIR_ENV);
    }
}
