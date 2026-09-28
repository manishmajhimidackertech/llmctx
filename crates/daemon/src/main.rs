// <<<LLMCTX
// FILE: crates/daemon/src/main.rs
// ROLE: Background daemon — authenticated local socket for editor saves/renames/deletes; extracts immediately, debounces Ollama
// EXPORTS: NONE (binary)
// IMPORTS: crates/core/src/process.rs, crates/core/src/store.rs, crates/core/src/runtime.rs, crates/core/src/ollama.rs, crates/core/src/config.rs
// USED BY: UNKNOWN
// NOTES: One process per user; OS-assigned port published with a token in the per-user discovery file; concurrency per Ollama server from config
// LLMCTX>>>

use std::{collections::HashMap, path::PathBuf, sync::Arc, time::Duration};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{mpsc, Mutex, Semaphore},
    time::{sleep, timeout},
};
use tracing::{debug, error, info, warn};

use llmctx_core::{
    config::{self, ProjectConfig},
    ollama::{self, OllamaClient},
    process::{self, Plan, ProcessOptions, ProcessSource},
    runtime::{self, DaemonInfo},
    store,
};

// ── Configuration ─────────────────────────────────────────────────────────────

/// Debounce window for the Ollama branch only.
/// Extraction runs immediately and is never gated behind this timer.
const OLLAMA_DEBOUNCE_SECS: u64 = 30;

/// A client must authenticate within this long after connecting.
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);

/// Set to a port number to listen on a fixed port instead of one the OS
/// picks. Clients never need it: they read the port from the discovery file.
const PORT_ENV: &str = "LLMCTX_DAEMON_PORT";

/// Set to any value to skip the startup update check.
const NO_UPDATE_CHECK_ENV: &str = "LLMCTX_NO_UPDATE_CHECK";

/// GitHub Releases API endpoint for the update check.
const RELEASES_URL: &str =
    "https://api.github.com/repos/manishmajhimidackertech/llmctx/releases/latest";

// ── Socket protocol types (NDJSON) ────────────────────────────────────────────

/// Message received from a client (the VS Code extension).
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
enum InboundMessage {
    /// Must be the first message on every connection.
    Hello { token: String },
    /// Sent on every file save.
    Save { path: String, hash: String },
    /// A file or directory was renamed or moved in the editor.
    Rename { from: String, to: String },
    /// A file or directory was deleted in the editor.
    Delete { path: String },
}

/// Message sent back to the client.
#[derive(Debug, Serialize, Clone)]
#[serde(tag = "type", rename_all = "camelCase")]
enum OutboundMessage {
    /// Reply to a valid `hello`.
    Welcome { version: String },
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

#[derive(Debug, Serialize, Clone, Copy)]
#[serde(rename_all = "camelCase")]
enum StatusState {
    Queued,
    Generating,
    /// The file has stored context matching its content.
    Ready,
    /// Nothing will be generated for this file (message says why).
    Skipped,
    Error,
}

// ── Shared state ──────────────────────────────────────────────────────────────

type ClientTx = mpsc::UnboundedSender<OutboundMessage>;

struct Daemon {
    client: OllamaClient,
    token: String,
    /// Latest saved hash per path: a queued generation only proceeds if no
    /// newer save arrived during its debounce window.
    latest_save: Mutex<HashMap<PathBuf, String>>,
    limits: Limits,
    /// All authenticated clients, for broadcasts (update notices).
    clients: Mutex<Vec<ClientTx>>,
}

/// One concurrency limit per Ollama server, sized by the projects' own
/// `ollama_concurrency` (the largest seen wins: someone raised
/// OLLAMA_NUM_PARALLEL on that server).
#[derive(Default)]
struct Limits {
    by_url: Mutex<HashMap<String, (Arc<Semaphore>, usize)>>,
}

impl Limits {
    async fn semaphore_for(&self, config: &ProjectConfig) -> Arc<Semaphore> {
        let key = ollama::resolve_ollama_url(config).unwrap_or_else(|_| "(invalid)".into());
        let want = config.ollama_concurrency_or_default();
        let mut map = self.by_url.lock().await;
        let (sem, size) = map
            .entry(key)
            .or_insert_with(|| (Arc::new(Semaphore::new(want)), want));
        if want > *size {
            sem.add_permits(want - *size);
            *size = want;
        }
        Arc::clone(sem)
    }
}

// ── Main ──────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    // No subcommands, so no clap: just the two flags people reach for (the
    // install docs verify with `llmctxd --version`). Anything else would
    // otherwise silently start a daemon.
    if let Some(arg) = std::env::args().nth(1) {
        match arg.as_str() {
            "-V" | "--version" => {
                println!("llmctxd {}", env!("CARGO_PKG_VERSION"));
                return Ok(());
            }
            "-h" | "--help" => {
                println!(
                    "llmctxd {} — llmctx background daemon\n\n\
                     Usage: llmctxd\n\n\
                     Usually started by the VS Code extension. Listens on 127.0.0.1 and\n\
                     publishes its port and token in a per-user discovery file.\n\n\
                     Environment:\n  \
                       {PORT_ENV}=<port>     listen on a fixed port\n  \
                       {NO_UPDATE_CHECK_ENV}=1  skip the startup update check\n  \
                       LLMCTX_RUNTIME_DIR=<dir>    where to write daemon.json\n  \
                       LLMCTX_OLLAMA_URL=<url>     Ollama server to use for every project",
                    env!("CARGO_PKG_VERSION")
                );
                return Ok(());
            }
            other => anyhow::bail!("unknown argument {other:?} (try --help)"),
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("llmctxd=info".parse()?)
                .add_directive("llmctx_core=info".parse()?),
        )
        .init();

    // ── Single instance ───────────────────────────────────────────────────────
    if let Some(existing) = runtime::read_daemon_info() {
        if is_alive(&existing).await {
            info!(
                "llmctxd is already running (pid {}, port {}) — nothing to do",
                existing.pid, existing.port
            );
            return Ok(());
        }
    }

    // ── Bind and publish ──────────────────────────────────────────────────────
    let port: u16 = std::env::var(PORT_ENV)
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .with_context(|| format!("could not listen on 127.0.0.1:{port}"))?;
    let port = listener.local_addr()?.port();

    let info = DaemonInfo {
        port,
        token: runtime::new_token().context("could not generate an auth token")?,
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let file = runtime::write_daemon_info(&info).context("could not write the discovery file")?;
    info!(
        "llmctxd listening on 127.0.0.1:{port} (discovery file: {})",
        file.display()
    );

    let daemon = Arc::new(Daemon {
        client: OllamaClient::new(),
        token: info.token.clone(),
        latest_save: Mutex::new(HashMap::new()),
        limits: Limits::default(),
        clients: Mutex::new(Vec::new()),
    });

    if std::env::var_os(NO_UPDATE_CHECK_ENV).is_none() {
        tokio::spawn(check_for_update(Arc::clone(&daemon)));
    }

    tokio::select! {
        _ = accept_loop(listener, Arc::clone(&daemon)) => {}
        _ = shutdown_signal() => info!("shutting down"),
    }
    runtime::remove_daemon_info(info.pid);
    Ok(())
}

async fn accept_loop(listener: TcpListener, daemon: Arc<Daemon>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                debug!("connection from {peer}");
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, daemon).await {
                        error!("connection error: {e}");
                    }
                });
            }
            Err(e) => error!("accept error: {e}"),
        }
    }
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut term) => {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => {}
                    _ = term.recv() => {}
                }
            }
            Err(_) => {
                let _ = tokio::signal::ctrl_c().await;
            }
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

/// True if a daemon answering with `info`'s token is listening on its port.
/// A stale discovery file whose port now belongs to some other program
/// fails the handshake and reads as "not running".
async fn is_alive(info: &DaemonInfo) -> bool {
    let probe = async {
        let stream = TcpStream::connect(("127.0.0.1", info.port)).await.ok()?;
        let (read, mut write) = stream.into_split();
        let hello = serde_json::json!({"type": "hello", "token": info.token});
        write
            .write_all(format!("{hello}\n").as_bytes())
            .await
            .ok()?;
        let line = BufReader::new(read).lines().next_line().await.ok()??;
        let reply: serde_json::Value = serde_json::from_str(&line).ok()?;
        Some(reply["type"] == "welcome")
    };
    matches!(timeout(Duration::from_secs(2), probe).await, Ok(Some(true)))
}

// ── Connection handler ────────────────────────────────────────────────────────

async fn handle_connection(stream: TcpStream, daemon: Arc<Daemon>) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // ── Authenticate: the first line must carry the token ─────────────────────
    let first = match timeout(HELLO_TIMEOUT, lines.next_line()).await {
        Ok(Ok(Some(line))) => line,
        _ => return Ok(()),
    };
    match serde_json::from_str::<InboundMessage>(first.trim()) {
        Ok(InboundMessage::Hello { token }) if runtime::tokens_match(&daemon.token, &token) => {}
        _ => {
            warn!("rejected a connection that did not present the daemon token");
            return Ok(());
        }
    }

    let (tx, mut rx) = mpsc::unbounded_channel::<OutboundMessage>();
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
    let _ = tx.send(OutboundMessage::Welcome {
        version: env!("CARGO_PKG_VERSION").to_string(),
    });
    daemon.clients.lock().await.push(tx.clone());

    while let Some(line) = lines.next_line().await? {
        let line = line.trim().to_string();
        if line.is_empty() {
            continue;
        }

        match serde_json::from_str::<InboundMessage>(&line) {
            Ok(InboundMessage::Save { path, hash }) => {
                let tx = tx.clone();
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move {
                    handle_save(PathBuf::from(path), hash, tx, daemon).await;
                });
            }
            Ok(InboundMessage::Rename { from, to }) => {
                match store::move_context(&PathBuf::from(&from), &PathBuf::from(&to)) {
                    Ok(0) => {}
                    Ok(n) => info!("{from} → {to}: moved context for {n} file(s)"),
                    Err(e) => warn!("{from} → {to}: could not move context: {e}"),
                }
            }
            Ok(InboundMessage::Delete { path }) => {
                match store::forget_context(&PathBuf::from(&path)) {
                    Ok(0) => {}
                    Ok(n) => info!("{path}: deleted — dropped context for {n} file(s)"),
                    Err(e) => warn!("{path}: could not drop context: {e}"),
                }
            }
            Ok(InboundMessage::Hello { .. }) => {}
            Err(e) => warn!("unrecognised message: {e} — raw: {line}"),
        }
    }

    Ok(())
}

// ── Save handler ──────────────────────────────────────────────────────────────

async fn handle_save(
    path: PathBuf,
    hash: String,
    tx: mpsc::UnboundedSender<OutboundMessage>,
    daemon: Arc<Daemon>,
) {
    let send = |state: StatusState, message: Option<String>| {
        let _ = tx.send(OutboundMessage::Status {
            path: path.display().to_string(),
            state,
            message,
        });
    };

    daemon
        .latest_save
        .lock()
        .await
        .insert(path.clone(), hash.clone());

    // ── Decide first: most saves need no model at all ─────────────────────────
    // Up-to-date, renamed, block-carrying, ignored and too-small files are all
    // answered now; only real generation work is debounced and queued. This
    // is also why a save of an unchanged file never shows "queued".
    let plan = match process::plan(&path, ProcessOptions::new()) {
        Ok(plan) => plan,
        Err(e) => {
            send(StatusState::Error, Some(e.to_string()));
            return;
        }
    };
    match plan {
        // Not ours to report on: settings files, scratch files, …
        Plan::Skip(ProcessSource::SkippedNoProject) => {
            forget_save(&daemon, &path, &hash).await;
            return;
        }
        Plan::Skip(reason) => {
            send(StatusState::Skipped, Some(reason.describe().to_string()));
            forget_save(&daemon, &path, &hash).await;
            return;
        }
        Plan::UpToDate => {
            send(StatusState::Ready, None);
            forget_save(&daemon, &path, &hash).await;
            return;
        }
        Plan::Reuse | Plan::Extract => {
            info!(?path, ?plan, "no model needed — processing immediately");
            run(&path, &daemon, &send).await;
            forget_save(&daemon, &path, &hash).await;
            return;
        }
        Plan::Generate => {}
    }

    // ── Ollama branch — debounce + bounded concurrency ────────────────────────
    send(StatusState::Queued, None);
    sleep(Duration::from_secs(OLLAMA_DEBOUNCE_SECS)).await;

    if daemon.latest_save.lock().await.get(&path) != Some(&hash) {
        debug!(?path, "debounce: newer save detected, skipping");
        return;
    }
    match std::fs::read_to_string(&path) {
        Ok(current) if process::content_hash(&current) != hash => {
            debug!(?path, "debounce: file changed on disk, skipping");
            return;
        }
        Err(_) => return,
        Ok(_) => {}
    }

    let config = config::load_config_for_file(&path)
        .map(|(_, c)| c)
        .unwrap_or_default();
    let semaphore = daemon.limits.semaphore_for(&config).await;
    let Ok(_permit) = semaphore.acquire_owned().await else {
        return;
    };

    send(StatusState::Generating, None);
    run(&path, &daemon, &send).await;
    forget_save(&daemon, &path, &hash).await;
}

/// Run `process_file` and report the outcome honestly: `ready` only when
/// context was actually stored.
async fn run(path: &PathBuf, daemon: &Daemon, send: &impl Fn(StatusState, Option<String>)) {
    match process::process_file(path, &daemon.client, ProcessOptions::new()).await {
        Ok(r) if r.source.has_context() => {
            info!(?path, "{}", r.source.describe());
            send(StatusState::Ready, None);
        }
        Ok(r) => send(StatusState::Skipped, Some(r.source.describe().to_string())),
        Err(e) => {
            error!(?path, "context generation failed: {e}");
            send(StatusState::Error, Some(e.to_string()));
        }
    }
}

/// Drop the debounce entry once its save has been handled — unless a newer
/// save for the same path has arrived meanwhile.
async fn forget_save(daemon: &Daemon, path: &PathBuf, hash: &str) {
    let mut map = daemon.latest_save.lock().await;
    if map.get(path).map(String::as_str) == Some(hash) {
        map.remove(path);
    }
}

// ── Update check ──────────────────────────────────────────────────────────────

/// Check GitHub Releases once on startup.  If a newer version exists, broadcasts
/// an `UpdateAvailable` message to every currently-connected client.
/// Never self-replaces the running binary. Skipped when LLMCTX_NO_UPDATE_CHECK
/// is set.
async fn check_for_update(daemon: Arc<Daemon>) {
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
        // 404 simply means no release has been published yet.
        debug!("update check: GitHub returned HTTP {}", resp.status());
        return;
    }

    let json: serde_json::Value = match resp.json().await {
        Ok(v) => v,
        Err(e) => {
            warn!("update check: could not parse response: {e}");
            return;
        }
    };

    let Some(latest_raw) = json["tag_name"].as_str() else {
        warn!("update check: no tag_name in response");
        return;
    };

    // Strip leading `v` from tag (e.g. "v0.2.0" → "0.2.0").
    let latest = latest_raw.trim_start_matches('v');

    if is_newer(latest, current) {
        info!("update available: {current} → {latest}");
        let msg = OutboundMessage::UpdateAvailable {
            current_version: current.to_string(),
            latest_version: latest.to_string(),
        };
        for tx in daemon.clients.lock().await.iter() {
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

    #[test]
    fn protocol_messages_round_trip() {
        let hello: InboundMessage =
            serde_json::from_str(r#"{"type":"hello","token":"t"}"#).unwrap();
        assert!(matches!(hello, InboundMessage::Hello { token } if token == "t"));
        let rename: InboundMessage =
            serde_json::from_str(r#"{"type":"rename","from":"a","to":"b"}"#).unwrap();
        assert!(matches!(rename, InboundMessage::Rename { .. }));
        let status = serde_json::to_string(&OutboundMessage::Status {
            path: "p".into(),
            state: StatusState::Skipped,
            message: Some("why".into()),
        })
        .unwrap();
        assert_eq!(
            status,
            r#"{"type":"status","path":"p","state":"skipped","message":"why"}"#
        );
    }

    #[tokio::test]
    async fn concurrency_limit_follows_the_largest_config() {
        let limits = Limits::default();
        let one = ProjectConfig::default();
        let three = ProjectConfig {
            ollama_concurrency: Some(3),
            ..Default::default()
        };
        assert_eq!(limits.semaphore_for(&one).await.available_permits(), 1);
        assert_eq!(limits.semaphore_for(&three).await.available_permits(), 3);
        // Never shrinks below what another project asked for.
        assert_eq!(limits.semaphore_for(&one).await.available_permits(), 3);
    }
}
