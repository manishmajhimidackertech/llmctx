// <<<LLMCTX
// FILE: crates/core/src/config.rs
// ROLE: Parse llmcontext.yaml and resolve it per-file by walking up the directory tree
// EXPORTS: ProjectConfig, ConfigError, find_config(), load_config(), load_config_for_file(), CONFIG_FILENAME
// IMPORTS: NONE
// USED BY: crates/core/src/process.rs, crates/cli/src/main.rs
// NOTES: Resolution mirrors .gitignore semantics — nearest ancestor wins; no global state
// LLMCTX>>>

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

pub const CONFIG_FILENAME: &str = "llmcontext.yaml";

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("No {CONFIG_FILENAME} found in {path} or any ancestor")]
    NotFound { path: String },

    #[error("Failed to read {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("Failed to parse {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: serde_yaml::Error,
    },
}

/// Deserialised representation of `llmcontext.yaml`.
///
/// All fields are optional so a minimal config (just `project:`) is valid.
/// The daemon and CLI fill in blanks with empty strings when building
/// prompts — they never fail on a sparse config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProjectConfig {
    /// Human-readable project name.
    pub project: Option<String>,

    /// Technology stack description, e.g. "Python, FastAPI, PostgreSQL".
    pub stack: Option<String>,

    /// Coding conventions the LLM should know about.
    #[serde(default)]
    pub conventions: Vec<String>,

    /// Current task or sprint goal — injected into every Ollama prompt.
    pub task: Option<String>,

    /// Additional glob patterns (relative to the config file) for files
    /// that .gitignore wouldn't already exclude but should be skipped.
    #[serde(default)]
    pub llmctx_ignore: Vec<String>,

    /// Ollama base URL; defaults to http://127.0.0.1:11434 if absent.
    ///
    /// Because this file lives in the repository, only a URL on this machine
    /// is honoured (see `ollama::resolve_ollama_url`). A remote server has to
    /// be chosen by the user through `LLMCTX_OLLAMA_URL`.
    pub ollama_url: Option<String>,

    /// Ollama model to use; defaults to "phi3:mini" if absent.
    pub ollama_model: Option<String>,

    /// Maximum concurrent Ollama jobs (default: 2).
    pub ollama_concurrency: Option<usize>,

    /// Per-request timeout for Ollama generation, in seconds (default: 300).
    ///
    /// This is a *per-request* budget, not a whole-run budget. Small local
    /// models on modest hardware routinely need more than a minute for a
    /// couple of kilobytes of source, so this is deliberately generous —
    /// a timeout should mean "something is wrong", not "the model was busy".
    pub ollama_timeout_secs: Option<u64>,

    /// Largest file, in bytes, that will be sent to Ollama (default: 16384).
    ///
    /// Files above this are skipped rather than submitted. Small local models
    /// have context windows of a few thousand tokens, so a large file is
    /// truncated by the server anyway and produces a useless answer after a
    /// very long wait. Skipping is both faster and more honest.
    pub ollama_max_bytes: Option<usize>,

    /// Context window (tokens) requested from Ollama. Defaults to a size
    /// that fits a file of `ollama_max_bytes` plus the prompt.
    pub ollama_num_ctx: Option<u32>,
}

impl ProjectConfig {
    pub fn ollama_model_or_default(&self) -> &str {
        self.ollama_model.as_deref().unwrap_or("phi3:mini")
    }

    pub fn ollama_concurrency_or_default(&self) -> usize {
        // Ollama serialises requests unless OLLAMA_NUM_PARALLEL is raised, so
        // a concurrency above 1 mostly just multiplies each request's wall
        // time instead of overlapping real work. 1 is the safe default.
        self.ollama_concurrency.unwrap_or(1).max(1)
    }

    pub fn ollama_timeout_secs_or_default(&self) -> u64 {
        self.ollama_timeout_secs.unwrap_or(300).max(1)
    }

    pub fn ollama_max_bytes_or_default(&self) -> usize {
        self.ollama_max_bytes.unwrap_or(16 * 1024)
    }

    /// Enough tokens for the largest file that will be sent (≈3 bytes per
    /// token for source code) plus ~1k for the prompt and answer, rounded up
    /// to a multiple of 1024 and never below 4096.
    pub fn ollama_num_ctx_or_default(&self) -> u32 {
        if let Some(n) = self.ollama_num_ctx {
            return n.max(512);
        }
        let tokens = self.ollama_max_bytes_or_default() / 3 + 1024;
        let rounded = tokens.div_ceil(1024) * 1024;
        u32::try_from(rounded).unwrap_or(u32::MAX).max(4096)
    }

    /// Formats the project header shown above packed context. Rendered from
    /// the current config every time, never stored, so edits take effect
    /// immediately for every file.
    pub fn header_block(&self) -> String {
        let project = self.project.as_deref().unwrap_or("(unknown)");
        let stack = self.stack.as_deref().unwrap_or("");
        let task = self.task.as_deref().unwrap_or("");
        let conventions = self.conventions.join(" | ");

        format!(
            "PROJECT: {project} | {stack}\nTASK: {task}\nCONVENTIONS: {conventions}\n"
        )
    }
}

/// Walk up from `start_dir` (inclusive) looking for `llmcontext.yaml`.
/// Returns the path of the first one found, or `ConfigError::NotFound`.
///
/// This mirrors the way `.gitignore` resolution works — each file is
/// governed by the nearest ancestor config, not a global daemon-session one.
pub fn find_config(start_dir: &Path) -> Result<PathBuf, ConfigError> {
    let mut dir = start_dir.to_path_buf();
    loop {
        let candidate = dir.join(CONFIG_FILENAME);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !dir.pop() {
            break;
        }
    }
    Err(ConfigError::NotFound {
        path: start_dir.display().to_string(),
    })
}

/// Convenience: find and parse config starting from the directory of `file_path`.
pub fn load_config_for_file(file_path: &Path) -> Result<(PathBuf, ProjectConfig), ConfigError> {
    let dir = file_path
        .parent()
        .unwrap_or_else(|| Path::new("."));
    let config_path = find_config(dir)?;
    let config = load_config(&config_path)?;
    Ok((config_path, config))
}

/// Parse `llmcontext.yaml` at the given path.
pub fn load_config(path: &Path) -> Result<ProjectConfig, ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
        path: path.display().to_string(),
        source: e,
    })?;
    serde_yaml::from_str(&text).map_err(|e| ConfigError::Parse {
        path: path.display().to_string(),
        source: e,
    })
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_config(dir: &Path, content: &str) {
        fs::write(dir.join(CONFIG_FILENAME), content).unwrap();
    }

    #[test]
    fn nearest_ancestor_config_wins() {
        // Resolution mirrors .gitignore semantics: walk up until a config is
        // found, and prefer the nearest one. The previous version of this test
        // asserted that a lookup from `src/` with a config at the root was an
        // error, which contradicted both its own comments and find_config().
        let dir = TempDir::new().unwrap();
        write_config(dir.path(), "project: TestProj\n");
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();

        // Only the root has a config — the walk-up finds it.
        let found = find_config(&src).unwrap();
        assert_eq!(found, dir.path().join(CONFIG_FILENAME));

        // Now src/ has its own — the nearer one takes precedence.
        write_config(&src, "project: Sub\n");
        let found2 = find_config(&src).unwrap();
        assert_eq!(found2, src.join(CONFIG_FILENAME));
    }

    #[test]
    fn walks_up_to_ancestor() {
        let root = TempDir::new().unwrap();
        write_config(root.path(), "project: Root\n");
        let deep = root.path().join("a").join("b").join("c");
        fs::create_dir_all(&deep).unwrap();
        let found = find_config(&deep).unwrap();
        assert_eq!(found, root.path().join(CONFIG_FILENAME));
    }

    #[test]
    fn parses_full_config() {
        let root = TempDir::new().unwrap();
        write_config(
            root.path(),
            r#"
project: MyApp
stack: "Rust, tokio"
task: "Build core crate"
conventions:
  - "No unwrap in library code"
  - "Errors via thiserror"
llmctx_ignore:
  - "generated/**"
ollama_model: codellama:7b
ollama_concurrency: 4
"#,
        );
        let cfg = load_config(&root.path().join(CONFIG_FILENAME)).unwrap();
        assert_eq!(cfg.project.as_deref(), Some("MyApp"));
        assert_eq!(cfg.ollama_model_or_default(), "codellama:7b");
        assert_eq!(cfg.ollama_concurrency_or_default(), 4);
        assert_eq!(cfg.conventions.len(), 2);
        assert_eq!(cfg.llmctx_ignore, vec!["generated/**"]);
    }

    #[test]
    fn defaults_are_sensible() {
        let cfg = ProjectConfig::default();
        assert_eq!(cfg.ollama_model_or_default(), "phi3:mini");
        assert_eq!(cfg.ollama_concurrency_or_default(), 1);
        assert_eq!(cfg.ollama_timeout_secs_or_default(), 300);
        assert_eq!(cfg.ollama_max_bytes_or_default(), 16 * 1024);
        // 16 KB ≈ 5.5k tokens + prompt → 7k, more than Ollama's small default.
        assert_eq!(cfg.ollama_num_ctx_or_default(), 7168);
    }

    #[test]
    fn concurrency_never_resolves_to_zero() {
        // A zero here would deadlock `llmctx index` (no workers spawned), so
        // it is clamped rather than trusted.
        let cfg = ProjectConfig {
            ollama_concurrency: Some(0),
            ..Default::default()
        };
        assert_eq!(cfg.ollama_concurrency_or_default(), 1);
    }

    #[test]
    fn header_block_format() {
        let cfg = ProjectConfig {
            project: Some("Acme".into()),
            stack: Some("Python".into()),
            task: Some("Sprint 3".into()),
            conventions: vec!["Use type hints".into()],
            ..Default::default()
        };
        let h = cfg.header_block();
        assert!(h.contains("PROJECT: Acme | Python"));
        assert!(h.contains("TASK: Sprint 3"));
        assert!(h.contains("CONVENTIONS: Use type hints"));
    }
}
