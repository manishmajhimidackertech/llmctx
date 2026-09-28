// <<<LLMCTX
// FILE: crates/daemon/src/main.rs
// ROLE: Background daemon — listens for VS Code save events, runs extraction immediately, debounces Ollama
// EXPORTS: NONE (binary)
// IMPORTS: crates/core/src/process.rs, crates/core/src/store.rs
// USED BY: UNKNOWN
// NOTES: One process per machine; bounded Ollama worker pool (default 2); extraction never debounced; saves outside any project are ignored
// LLMCTX>>>

use std::{
    collections::HashMap,
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex, Semaphore},
    time::sleep,
};
use tracing::{debug, error, info, warn};

use llmctx_core::{
    ollama::OllamaClient,
    process::{self, ProcessOptions, ProcessSource},
    store,
};

// ── Configuration ─────────────────────────────────────────────────────────────

/// Primary port the daemon listens on.
const DAEMON_PORT: u16 = 51515;

/// If the primary port is already taken, try up to this many sequential ports
/// before giving up (51515, 51516, … 51524).
const PORT_RETRY_COUNT: u16 = 10;

/// Debounce window for the Ollama branch only.
/// Extraction runs immediately and is never gated behind this timer.
const OLLAMA_DEBOUNCE_SECS: u64 = 30;

/// Default concurrency cap for Ollama jobs.
const DEFAULT_OLLAMA_CONCURRENCY: usize = 2;

/// GitHub Releases API endpoint for the update check.
/// Replace `your-org/llmctx` with the real repository path before publishing.
const RELEASES_URL: &str =
    "https://api.github.com/repos/your-org/llmctx/releases/latest";

// ── Socket protocol types (NDJSON) ────────────────────────────────────────────

/// Message received from the VS Code extension.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InboundMessage {
    /// Sent on every file save.
    Save { path: String, hash: String },
}

/// Message sent back to the VS Code extension.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum OutboundMessage {
    /// Pushed whenever a file's context state changes.
    Status {
        path: String,
        state: StatusState,
        #[serde(skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Pushed once on startup if a newer version is available.
    UpdateAvailable {
        current_version: String,
        latest_version: String,
    },
}

#[derive(Debug, Serialize, Clone)]
#[serde(rename_all = "camelCase")]
enum StatusState {
    Queued,
    Generating,
    Ready,
    Error,
}

// ── Debounce tracker ──────────────────────────────────────────────────────────

#[derive(Clone)]
struct DebounceEntry {
    /// When the save that created this entry was observed. Not currently read
    /// — debouncing is decided by `hash` plus a fixed sleep — but kept because
    /// it is what any adaptive-delay change would need.
    #[allow(dead_code)]
    last_save: Instant,
    hash: String,
}

type DebounceMap = Arc<Mutex<HashMap<PathBuf, DebounceEntry>>>;

// ── Client registry — used to broadcast update notices ───────────────────────

/// A sender handle for one connected VS Code extension instance.
type ClientTx = mpsc::UnboundedSender<OutboundMessage>;

/// All currently-connected clients.  Protected by a Mutex so the update-check
/// task can broadcast to them without knowing who connected when.
type ClientRegistry = Arc<Mutex<Vec<ClientTx>>>;

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("llmctxd=info".parse()?)
                .add_directive("llmctx_core=info".parse()?),
        )
        .init();

    // ── Bind with port fallback ───────────────────────────────────────────────
    let (listener, bound_port) = bind_with_fallback(DAEMON_PORT, PORT_RETRY_COUNT)
        .context("failed to bind on any port in range {DAEMON_PORT}–{DAEMON_PORT+PORT_RETRY_COUNT}")?;
    info!("llmctxd listening on 127.0.0.1:{bound_port}");
    if bound_port != DAEMON_PORT {
        warn!(
            "default port {DAEMON_PORT} was in use — bound to {bound_port} instead. \
             Set \"llmctx.daemonPort\": {bound_port} in VS Code settings."
        );
    }

    let client = Arc::new(OllamaClient::new());
    let ollama_sem = Arc::new(Semaphore::new(DEFAULT_OLLAMA_CONCURRENCY));
    let debounce: DebounceMap = Arc::new(Mutex::new(HashMap::new()));
    let registry: ClientRegistry = Arc::new(Mutex::new(Vec::new()));

    // One-time update check — broadcasts to all connected clients if a newer
    // version exists.  Runs concurrently; never blocks the accept loop.
    tokio::spawn(check_for_update(Arc::clone(&registry)));

    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                info!("connection from {peer}");
                let client = Arc::clone(&client);
                let sem = Arc::clone(&ollama_sem);
                let debounce = Arc::clone(&debounce);
                let registry = Arc::clone(&registry);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, client, sem, debounce, registry).await
                    {
                        error!("connection error: {e}");
                    }
                });
            }
            Err(e) => {
                error!("accept error: {e}");
            }
        }
    }
}

/// Try to bind on `start_port`, then `start_port+1`, … up to `retries` more.
/// Returns the listener and the port it actually bound on.
fn bind_with_fallback(start_port: u16, retries: u16) -> Result<(TcpListener, u16)> {
    // TcpListener::bind is sync, so use std::net and convert.
    for offset in 0..=retries {
        let port = start_port.saturating_add(offset);
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        match std::net::TcpListener::bind(addr) {
            Ok(std_listener) => {
                std_listener
                    .set_nonblocking(true)
                    .context("set_nonblocking failed")?;
                let listener = TcpListener::from_std(std_listener)
                    .context("tokio TcpListener::from_std failed")?;
                return Ok((listener, port));
            }
            Err(e) if offset < retries => {
                warn!("port {port} unavailable ({e}), trying next…");
            }
            Err(e) => {
                return Err(e).context(format!("could not bind port {port}"));
            }
        }
    }
    unreachable!()
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection(
    stream: TcpStream,
    client: Arc<OllamaClient>,
    ollama_sem: Arc<Semaphore>,
    debounce: DebounceMap,
    registry: ClientRegistry,
) -> Result<()> {
    let (read_half, write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    let (tx, mut rx) = mpsc::unbounded_channel::<OutboundMessage>();

    // Register this client so the update-check task can reach it.
    registry.lock().await.push(tx.clone());

    let mut write_half = write_half;
    tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if let Ok(mut json) = serde_json::to_string(&msg) {
                json.push('\n');
                if write_half.write_all(json.as_bytes()).await.is_err() {
                    break;
                }
            }
        }
    });

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<InboundMessage>(&line) {
            Ok(InboundMessage::Save { path, hash }) => {
                let path = PathBuf::from(&path);
                let tx = tx.clone();
                let client = Arc::clone(&client);
                let sem = Arc::clone(&ollama_sem);
                let debounce = Arc::clone(&debounce);
                tokio::spawn(async move {
                    handle_save(path, hash, tx, client, sem, debounce).await;
                });
            }
            Err(e) => {
                warn!("unrecognised message: {e} — raw: {line}");
            }
        }
    }

    Ok(())
}

// ── Save handler ──────────────────────────────────────────────────────────────

async fn handle_save(
    path: PathBuf,
    hash: String,
    tx: mpsc::UnboundedSender<OutboundMessage>,
    client: Arc<OllamaClient>,
    ollama_sem: Arc<Semaphore>,
    debounce: DebounceMap,
) {
    // A save outside every project (a settings file, a scratch file) has
    // nowhere to store context. Ignore it before it is queued or debounced,
    // so the status bar never shows work that will not happen.
    if store::project_root_for_file(&path).is_none() {
        debug!(?path, "not inside an llmctx project — ignoring save");
        return;
    }

    {
        let mut map = debounce.lock().await;
        map.insert(
            path.clone(),
            DebounceEntry {
                last_save: Instant::now(),
                hash: hash.clone(),
            },
        );
    }

    // ── Extraction fast-path (immediate, no debounce, no semaphore) ───────────
    let source = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            warn!(?path, "could not read file for save handler: {e}");
            return;
        }
    };

    let has_block = llmctx_core::extract::extract_llmctx_block(&source).is_ok();

    if has_block {
        info!(?path, "save has <<<LLMCTX block — extracting immediately");
        let send = |state, message: Option<String>| {
            let _ = tx.send(OutboundMessage::Status {
                path: path.display().to_string(),
                state,
                message,
            });
        };
        match process::process_file(&path, &client, ProcessOptions::new()).await {
            Ok(r)
                if matches!(
                    r.source,
                    ProcessSource::Extracted | ProcessSource::UpToDate | ProcessSource::Reused
                ) =>
            {
                send(StatusState::Ready, None)
            }
            Ok(_) => {}
            Err(e) => {
                error!(?path, "extraction failed: {e}");
                send(StatusState::Error, Some(e.to_string()));
            }
        }
        return;
    }

    // ── Ollama branch — debounce + bounded concurrency ────────────────────────
    let _ = tx.send(OutboundMessage::Status {
        path: path.display().to_string(),
        state: StatusState::Queued,
        message: None,
    });

    sleep(Duration::from_secs(OLLAMA_DEBOUNCE_SECS)).await;

    {
        let map = debounce.lock().await;
        if let Some(entry) = map.get(&path) {
            if entry.hash != hash {
                info!(?path, "debounce: newer save detected, skipping");
                return;
            }
            if let Ok(current) = std::fs::read_to_string(&path) {
                if process::content_hash(&current) != hash {
                    info!(?path, "debounce: file changed on disk, skipping");
                    return;
                }
            }
        }
    }

    let _permit = ollama_sem.acquire().await;

    let _ = tx.send(OutboundMessage::Status {
        path: path.display().to_string(),
        state: StatusState::Generating,
        message: None,
    });

    match process::process_file(&path, &client, ProcessOptions::new()).await {
        Ok(_) => {
            info!(?path, "Ollama context generated");
            let _ = tx.send(OutboundMessage::Status {
                path: path.display().to_string(),
                state: StatusState::Ready,
                message: None,
            });
        }
        Err(e) => {
            error!(?path, "Ollama generation failed: {e}");
            let _ = tx.send(OutboundMessage::Status {
                path: path.display().to_string(),
                state: StatusState::Error,
                message: Some(e.to_string()),
            });
        }
    }
}

// ── Update check ──────────────────────────────────────────────────────────────

/// Check GitHub Releases once on startup.  If a newer version exists, broadcasts
/// an `UpdateAvailable` message to every currently-connected VS Code client.
/// Never self-replaces the running binary.
async fn check_for_update(registry: ClientRegistry) {
    // Small delay so the first client has time to connect and receive the notice.
    sleep(Duration::from_secs(5)).await;

    let current = env!("CARGO_PKG_VERSION");

    let http = match reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .user_agent(format!("llmctxd/{current}"))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!("update check: could not build HTTP client: {e}");
            return;
        }
    };

    let resp = match http.get(RELEASES_URL).send().await {
        Ok(r) => r,
        Err(e) => {
            // Network unavailable or GitHub unreachable — not an error worth logging
            // loudly, since many dev machines are air-gapped or behind strict firewalls.
            info!("update check: skipped ({e})");
            return;
        }
    };

    if !resp.status().is_success() {
        info!("update check: GitHub returned HTTP {}", resp.status());
        return;
    }

    // Parse tag_name from the JSON response.
    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("update check: could not parse response: {e}");
            return;
        }
    };

    let latest_raw = match json["tag_name"].as_str() {
        Some(t) => t,
        None => {
            warn!("update check: no tag_name in response");
            return;
        }
    };

    // Strip leading `v` from tag (e.g. "v0.2.0" → "0.2.0").
    let latest = latest_raw.trim_start_matches('v');

    if is_newer(latest, current) {
        info!("update available: {current} → {latest}");
        let msg = OutboundMessage::UpdateAvailable {
            current_version: current.to_string(),
            latest_version: latest.to_string(),
        };
        let clients = registry.lock().await;
        for tx in clients.iter() {
            let _ = tx.send(msg.clone());
        }
    } else {
        info!("update check: up to date ({current})");
    }
}

/// Returns true if `candidate` is strictly newer than `current`.
/// Simple numeric comparison on `major.minor.patch` components;
/// ignores pre-release suffixes (conservative — treats them as equal).
fn is_newer(candidate: &str, current: &str) -> bool {
    parse_semver(candidate) > parse_semver(current)
}

fn parse_semver(v: &str) -> (u32, u32, u32) {
    let mut parts = v.split('.').map(|p| {
        // Strip any pre-release suffix (e.g. "1-beta" → 1).
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
            .parse::<u32>()
            .unwrap_or(0)
    });
    (
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
        parts.next().unwrap_or(0),
    )
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_comparison() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
    }

    #[test]
    fn semver_strips_v_prefix() {
        assert_eq!(parse_semver("v0.2.0"), parse_semver("0.2.0"));
    }

    #[test]
    fn semver_ignores_prerelease_suffix() {
        // "0.2.0-beta" should parse as (0,2,0) — not crash
        assert_eq!(parse_semver("0.2.0-beta"), (0, 2, 0));
    }
}
