// <<<LLMCTX
// FILE: crates/core/src/ollama.rs
// ROLE: Send a source file to Ollama and return the generated six-field context body
// EXPORTS: OllamaClient, OllamaError, GeneratedContext
// IMPORTS: crates/core/src/config.rs
// USED BY: crates/core/src/process.rs
// NOTES: Returns raw body text only — caller (process.rs) prepends header and version stamp
// LLMCTX>>>

use std::time::Duration;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::ProjectConfig;

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
    /// Six-field body text, ready to be written to ADS after header injection.
    pub body: String,
}

// ── Ollama API types (generate endpoint) ────────────────────────────────────

#[derive(Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
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
        let url = format!("{}/api/tags", config.ollama_url_or_default());

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

    /// Generate context for `source_code` using the project config and the
    /// relative file path `rel_path` (used in the prompt for file identity).
    pub async fn generate(
        &self,
        config: &ProjectConfig,
        rel_path: &str,
        source_code: &str,
    ) -> Result<GeneratedContext, OllamaError> {
        let url = format!("{}/api/generate", config.ollama_url_or_default());
        let secs = config.ollama_timeout_secs_or_default();
        let prompt = build_prompt(config, rel_path, source_code);

        let req = GenerateRequest {
            model: config.ollama_model_or_default(),
            prompt: &prompt,
            stream: false,
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

        Ok(GeneratedContext { body })
    }
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

Analyze this file and return ONLY a context block in this exact format (no extra text, no markdown):

FILE: <filename>
ROLE: <one sentence — what this file does>
EXPORTS: <key functions/classes/constants, comma separated, or NONE>
IMPORTS: <other project files this depends on, comma separated, or NONE>
USED BY: <files likely to import this, comma separated, or UNKNOWN>
NOTES: <anything unusual an LLM should know, or NONE>

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
