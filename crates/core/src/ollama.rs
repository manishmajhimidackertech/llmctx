// <<<LLMCTX
// FILE: crates/core/src/ollama.rs
// ROLE: Send a source file to Ollama and return validated six-field context
// EXPORTS: OllamaClient, OllamaError, GeneratedContext, resolve_ollama_url(), OLLAMA_URL_ENV
// IMPORTS: crates/core/src/config.rs
// USED BY: crates/core/src/process.rs, crates/cli/src/main.rs, crates/daemon/src/main.rs
// NOTES: Repo configs may only point at a local Ollama; LLMCTX_OLLAMA_URL (set by the user) may point anywhere. Output is JSON, validated, retried once
// LLMCTX>>>

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::warn;

use crate::config::ProjectConfig;

/// Environment variable through which the *user* (not a repository) chooses
/// the Ollama server. The VS Code extension sets it from `llmctx.ollamaUrl`.
pub const OLLAMA_URL_ENV: &str = "LLMCTX_OLLAMA_URL";

const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";

#[derive(Debug, Error)]
pub enum OllamaError {
    /// Nothing is listening, or the connection was refused/reset.
    ///
    /// Kept strictly separate from `Timeout`: reqwest renders both as
    /// "error sending request for url (...)", so collapsing them into one
    /// variant makes a slow model indistinguishable from a dead server.
    #[error("Ollama is unreachable at {url} — is `ollama serve` running? ({source})")]
    Unreachable {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error(
        "Ollama did not respond within {secs}s at {url} — the server is up but the model is \
         too slow for this input. Raise `ollama_timeout_secs`, lower `ollama_max_bytes`, or \
         use a smaller model."
    )]
    Timeout { url: String, secs: u64 },

    #[error("Request to {url} failed: {source}")]
    Transport {
        url: String,
        #[source]
        source: reqwest::Error,
    },

    #[error("Ollama returned HTTP {status}: {body}")]
    HttpError { status: u16, body: String },

    #[error("Ollama response parse failed: {source}")]
    ParseError {
        #[source]
        source: reqwest::Error,
    },

    #[error("Ollama response was empty or contained no usable content")]
    EmptyResponse,

    #[error("Ollama's answer was not usable context ({detail}), even after one retry")]
    InvalidOutput { detail: String },

    #[error(
        "refusing to send source to {url}: `ollama_url` in llmcontext.yaml may only point at this \
         machine (localhost, 127.0.0.1, ::1), because a cloned repository must not decide where \
         your code goes. To use a remote Ollama, set the {OLLAMA_URL_ENV} environment variable \
         (or `llmctx.ollamaUrl` in VS Code)."
    )]
    RemoteNotAllowed { url: String },

    #[error("invalid Ollama URL {url:?}: {detail}")]
    InvalidUrl { url: String, detail: String },
}

impl OllamaError {
    /// True when this error means "the server never answered in time" rather
    /// than "the server is not there". Callers use this to decide whether
    /// aborting the whole run makes sense.
    pub fn is_timeout(&self) -> bool {
        matches!(self, OllamaError::Timeout { .. })
    }

    /// True when nothing is listening on the configured address.
    pub fn is_unreachable(&self) -> bool {
        matches!(self, OllamaError::Unreachable { .. })
    }
}

/// Classify a `reqwest::Error` into the right variant.
///
/// `is_timeout()` must be checked first: a timeout that fires during connect
/// also reports `is_connect() == true`, and reporting it as "unreachable"
/// is exactly the misdiagnosis this function exists to prevent.
fn classify(e: reqwest::Error, url: &str, secs: u64) -> OllamaError {
    if e.is_timeout() {
        OllamaError::Timeout {
            url: url.to_string(),
            secs,
        }
    } else if e.is_connect() {
        OllamaError::Unreachable {
            url: url.to_string(),
            source: e,
        }
    } else {
        OllamaError::Transport {
            url: url.to_string(),
            source: e,
        }
    }
}

/// The parsed result of a successful Ollama generation.
#[derive(Debug, Clone)]
pub struct GeneratedContext {
    /// The six field lines, `FILE:` through `NOTES:`, ready to store.
    pub fields: String,
}

// ── URL policy ───────────────────────────────────────────────────────────────

/// The Ollama base URL to use for a project.
///
/// `LLMCTX_OLLAMA_URL` wins and may point anywhere: only the user can set
/// it. `ollama_url` from `llmcontext.yaml` comes from the repository, so it
/// may only name this machine — otherwise cloning a repository and saving a
/// file could ship your source code to a server of the repository's choosing.
pub fn resolve_ollama_url(config: &ProjectConfig) -> Result<String, OllamaError> {
    if let Ok(url) = std::env::var(OLLAMA_URL_ENV) {
        let url = url.trim().trim_end_matches('/').to_string();
        if !url.is_empty() {
            return Ok(url);
        }
    }
    let Some(url) = config.ollama_url.as_deref() else {
        return Ok(DEFAULT_OLLAMA_URL.to_string());
    };
    let url = url.trim().trim_end_matches('/').to_string();
    let parsed = reqwest::Url::parse(&url).map_err(|e| OllamaError::InvalidUrl {
        url: url.clone(),
        detail: e.to_string(),
    })?;
    let local = match parsed.host() {
        Some(url::Host::Domain(d)) => d.eq_ignore_ascii_case("localhost"),
        Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
        Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
        None => false,
    };
    if local {
        Ok(url)
    } else {
        Err(OllamaError::RemoteNotAllowed { url })
    }
}

// ── Ollama API types (generate endpoint) ────────────────────────────────────

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
    /// Constrain the answer to JSON so it can be parsed, not scraped.
    format: &'a str,
    options: GenerateOptions,
}

#[derive(Serialize)]
struct GenerateOptions {
    /// Deterministic answers: the same file should get the same context.
    temperature: f32,
    /// Context window. Ollama's default is small enough that a file near
    /// `ollama_max_bytes` would be truncated silently. Fixed per project,
    /// because changing it between requests makes Ollama reload the model.
    num_ctx: u32,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

#[derive(Deserialize)]
struct TagsResponse {
    #[serde(default)]
    models: Vec<TagModel>,
}

#[derive(Deserialize)]
struct TagModel {
    name: String,
}

// ── Client ───────────────────────────────────────────────────────────────────

/// Thin async wrapper around the Ollama `/api/generate` endpoint.
///
/// The client holds a reqwest client (which internally pools connections)
/// so it should be created once per daemon/CLI run and shared.
#[derive(Debug, Clone)]
pub struct OllamaClient {
    http: reqwest::Client,
}

/// Timeout for the cheap `/api/tags` liveness probe. This one *should* be
/// short — if the server is up it answers instantly.
const HEALTH_TIMEOUT_SECS: u64 = 10;

impl OllamaClient {
    pub fn new() -> Self {
        // No client-wide timeout: generation deadlines are applied per request
        // from `ollama_timeout_secs` so they can be tuned per project, and so
        // the health probe can use its own much shorter budget.
        let http = reqwest::Client::builder()
            .build()
            .expect("Failed to build reqwest client");
        Self { http }
    }

    /// Ask Ollama for its installed model list.
    ///
    /// Used as a pre-flight check by `llmctx index` so a stopped server fails
    /// once, immediately, with an accurate message — instead of once per file
    /// after a full timeout each.
    pub async fn list_models(&self, config: &ProjectConfig) -> Result<Vec<String>, OllamaError> {
        let url = format!("{}/api/tags", resolve_ollama_url(config)?);

        let resp = self
            .http
            .get(&url)
            .timeout(Duration::from_secs(HEALTH_TIMEOUT_SECS))
            .send()
            .await
            .map_err(|e| classify(e, &url, HEALTH_TIMEOUT_SECS))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(OllamaError::HttpError { status, body });
        }

        let parsed: TagsResponse = resp
            .json()
            .await
            .map_err(|e| OllamaError::ParseError { source: e })?;

        Ok(parsed.models.into_iter().map(|m| m.name).collect())
    }

    /// Generate the six fields for `source_code` at `rel_path` (used in the
    /// prompt, and always written as the `FILE:` field).
    ///
    /// The answer is validated; an unusable one (not JSON, no ROLE) is
    /// retried once before giving up, since small models occasionally ramble.
    pub async fn generate(
        &self,
        config: &ProjectConfig,
        rel_path: &str,
        source_code: &str,
    ) -> Result<GeneratedContext, OllamaError> {
        let prompt = build_prompt(config, rel_path, source_code);
        let mut last = String::new();
        for attempt in 1..=2 {
            let raw = self.generate_raw(config, &prompt).await?;
            match parse_generated(&raw, rel_path) {
                Ok(fields) => return Ok(GeneratedContext { fields }),
                Err(detail) => {
                    warn!(rel_path, attempt, "unusable Ollama answer: {detail}");
                    last = detail;
                }
            }
        }
        Err(OllamaError::InvalidOutput { detail: last })
    }

    async fn generate_raw(
        &self,
        config: &ProjectConfig,
        prompt: &str,
    ) -> Result<String, OllamaError> {
        let url = format!("{}/api/generate", resolve_ollama_url(config)?);
        let secs = config.ollama_timeout_secs_or_default();

        let req = GenerateRequest {
            model: config.ollama_model_or_default(),
            prompt,
            stream: false,
            format: "json",
            options: GenerateOptions {
                temperature: 0.0,
                num_ctx: config.ollama_num_ctx_or_default(),
            },
        };

        let resp = self
            .http
            .post(&url)
            .timeout(Duration::from_secs(secs))
            .json(&req)
            .send()
            .await
            .map_err(|e| classify(e, &url, secs))?;

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            return Err(OllamaError::HttpError { status, body });
        }

        // The deadline covers the body too — a stalled read here is still a
        // timeout, not a parse failure.
        let parsed: GenerateResponse = resp.json().await.map_err(|e| {
            if e.is_timeout() {
                OllamaError::Timeout {
                    url: url.clone(),
                    secs,
                }
            } else {
                OllamaError::ParseError { source: e }
            }
        })?;

        let body = parsed.response.trim().to_string();
        if body.is_empty() {
            return Err(OllamaError::EmptyResponse);
        }
        Ok(body)
    }
}

// ── Answer parsing ───────────────────────────────────────────────────────────

/// Turn the model's answer into the six field lines, or explain why not.
///
/// JSON is expected (`format: "json"`), but a `KEY: value` answer — what an
/// older Ollama that ignores `format` tends to produce — is accepted too.
/// `FILE:` is always the real path and `USED BY:` is always `UNKNOWN`: the
/// model sees one file and cannot know either; `USED BY` is computed from the
/// other files' `IMPORTS` when context is packed.
fn parse_generated(raw: &str, rel_path: &str) -> Result<String, String> {
    let cleaned = strip_code_fence(raw);
    let mut values = std::collections::HashMap::<String, String>::new();

    if let Ok(serde_json::Value::Object(map)) = serde_json::from_str::<serde_json::Value>(cleaned) {
        for (key, value) in map {
            values.insert(normalise_key(&key), json_to_text(&value));
        }
    } else {
        for line in cleaned.lines() {
            if let Some((key, value)) = line.split_once(':') {
                values
                    .entry(normalise_key(key))
                    .or_insert_with(|| value.trim().to_string());
            }
        }
    }

    let get = |key: &str, default: &str| -> String {
        values
            .get(key)
            .map(|v| one_line(v))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| default.to_string())
    };
    let role = get("role", "");
    if role.is_empty() {
        return Err(format!(
            "no ROLE in answer: {:?}",
            cleaned.chars().take(120).collect::<String>()
        ));
    }
    Ok(format!(
        "FILE: {rel_path}\nROLE: {role}\nEXPORTS: {}\nIMPORTS: {}\nUSED BY: UNKNOWN\nNOTES: {}",
        get("exports", "NONE"),
        get("imports", "NONE"),
        get("notes", "NONE"),
    ))
}

fn strip_code_fence(raw: &str) -> &str {
    let t = raw.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let rest = rest.split_once('\n').map(|(_, body)| body).unwrap_or(rest);
    rest.trim_end().strip_suffix("```").unwrap_or(rest).trim()
}

/// `"Used By"`, `"USED_BY"`, `"used by"` → `"usedby"`.
fn normalise_key(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn json_to_text(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(json_to_text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

impl Default for OllamaClient {
    fn default() -> Self {
        Self::new()
    }
}

// ── Prompt builder ───────────────────────────────────────────────────────────

fn build_prompt(config: &ProjectConfig, rel_path: &str, source_code: &str) -> String {
    let project = config.project.as_deref().unwrap_or("(unknown)");
    let stack = config.stack.as_deref().unwrap_or("");
    let task = config.task.as_deref().unwrap_or("");
    let conventions = config.conventions.join("\n- ");

    format!(
        r#"You are a code context generator for LLM assistance.

Project: {project}
Stack: {stack}
Conventions:
- {conventions}
Current task: {task}

Analyze the file below and answer with ONLY a JSON object with exactly these keys:

{{
  "role": "one sentence: what this file does",
  "exports": "key functions/classes/constants it exposes, comma separated, or NONE",
  "imports": "other files of this project it depends on, as paths relative to the project root, comma separated, or NONE",
  "notes": "anything unusual an LLM should know, or NONE"
}}

File path: {rel_path}

```
{source_code}
```"#
    )
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_contains_file_path() {
        let cfg = ProjectConfig {
            project: Some("TestProj".into()),
            ..Default::default()
        };
        let prompt = build_prompt(&cfg, "src/foo.rs", "fn foo() {}");
        assert!(prompt.contains("src/foo.rs"));
        assert!(prompt.contains("fn foo()"));
        assert!(prompt.contains("TestProj"));
    }

    #[test]
    fn json_answers_become_six_fields() {
        let raw = r#"{"Role": "Parses config", "exports": ["load", "save"], "IMPORTS": "a.rs", "notes": null, "file": "wrong.rs", "used_by": "x.rs"}"#;
        assert_eq!(
            parse_generated(raw, "src/config.rs").unwrap(),
            "FILE: src/config.rs\nROLE: Parses config\nEXPORTS: load, save\nIMPORTS: a.rs\nUSED BY: UNKNOWN\nNOTES: NONE"
        );
    }

    #[test]
    fn fenced_and_plain_text_answers_are_accepted() {
        let fenced = "```json\n{\"role\": \"Does\\nthings\"}\n```";
        assert!(parse_generated(fenced, "a.rs")
            .unwrap()
            .contains("ROLE: Does things"));

        let text = "Sure! Here it is:\nROLE: Handles auth\nEXPORTS: login()\n";
        let fields = parse_generated(text, "a.rs").unwrap();
        assert!(fields.contains("ROLE: Handles auth"));
        assert!(fields.contains("EXPORTS: login()"));
    }

    #[test]
    fn answers_without_a_role_are_rejected() {
        assert!(parse_generated("I cannot help with that.", "a.rs").is_err());
        assert!(parse_generated("{\"exports\": \"x\"}", "a.rs").is_err());
    }

    #[test]
    fn repo_config_may_only_point_at_this_machine() {
        // Only meaningful when the user has not set the override.
        if std::env::var(OLLAMA_URL_ENV).is_ok() {
            return;
        }
        let with = |url: &str| ProjectConfig {
            ollama_url: Some(url.into()),
            ..Default::default()
        };
        assert_eq!(
            resolve_ollama_url(&ProjectConfig::default()).unwrap(),
            DEFAULT_OLLAMA_URL
        );
        for ok in [
            "http://localhost:11434/",
            "http://127.0.0.1:9",
            "http://[::1]:11434",
        ] {
            assert!(resolve_ollama_url(&with(ok)).is_ok(), "{ok}");
        }
        for bad in [
            "https://attacker.example",
            "http://10.0.0.5:11434",
            "http://localhost.evil.com",
        ] {
            assert!(
                matches!(
                    resolve_ollama_url(&with(bad)),
                    Err(OllamaError::RemoteNotAllowed { .. })
                ),
                "{bad}"
            );
        }
        assert!(matches!(
            resolve_ollama_url(&with("not a url")),
            Err(OllamaError::InvalidUrl { .. })
        ));
    }

    #[test]
    fn prompt_uses_conventions() {
        let cfg = ProjectConfig {
            conventions: vec!["No unwrap".into(), "Use thiserror".into()],
            ..Default::default()
        };
        let prompt = build_prompt(&cfg, "x.rs", "");
        assert!(prompt.contains("No unwrap"));
        assert!(prompt.contains("Use thiserror"));
    }
}
