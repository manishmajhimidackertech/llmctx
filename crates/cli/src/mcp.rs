// <<<LLMCTX
// FILE: crates/cli/src/mcp.rs
// ROLE: `llmctx mcp` — Model Context Protocol server over stdio exposing the project's stored context as tools
// EXPORTS: serve(), Server
// IMPORTS: crates/core/src/pack.rs, crates/core/src/store.rs, crates/core/src/config.rs
// USED BY: crates/cli/src/main.rs
// NOTES: Newline-delimited JSON-RPC 2.0; read-only; paths are confined to the project root
// LLMCTX>>>

//! Lets MCP clients (Claude Code, Claude Desktop, …) read llmctx context
//! directly instead of going through the clipboard.
//!
//! Tools:
//! - `get_file_context` — context for one file (project header + six fields);
//! - `project_map` — one line per file: path and ROLE;
//! - `search_context` — files whose path or context mentions a query.

use std::{
    io::{self, BufRead, Write},
    path::{Path, PathBuf},
};

use serde_json::{json, Value};

use llmctx_core::{config, fsutil, pack};

/// Protocol versions this server can speak, newest last.
const SUPPORTED_VERSIONS: &[&str] = &["2024-11-05", "2025-03-26", "2025-06-18"];

/// Serve MCP on stdin/stdout until stdin closes.
pub fn serve(root: PathBuf) -> anyhow::Result<()> {
    let server = Server { root };
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(&line) {
            Ok(request) => server.handle(&request),
            Err(e) => Some(error(Value::Null, -32700, &format!("parse error: {e}"))),
        };
        if let Some(response) = response {
            writeln!(stdout, "{response}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

pub struct Server {
    root: PathBuf,
}

impl Server {
    /// Answer one JSON-RPC message; `None` for notifications.
    pub fn handle(&self, request: &Value) -> Option<Value> {
        let id = request.get("id").cloned();
        let method = request["method"].as_str().unwrap_or("");
        let params = &request["params"];

        // Notifications carry no id and get no reply.
        let id = id?;

        let result = match method {
            "initialize" => Ok(self.initialize(params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tools() })),
            "tools/call" => Ok(self.call_tool(params)),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => error(id, code, &message),
        })
    }

    fn initialize(&self, params: &Value) -> Value {
        let requested = params["protocolVersion"].as_str().unwrap_or("");
        let version = if SUPPORTED_VERSIONS.contains(&requested) {
            requested
        } else {
            SUPPORTED_VERSIONS[SUPPORTED_VERSIONS.len() - 1]
        };
        json!({
            "protocolVersion": version,
            "capabilities": { "tools": {} },
            "serverInfo": { "name": "llmctx", "version": env!("CARGO_PKG_VERSION") },
            "instructions": format!(
                "llmctx keeps a short context summary (role, exports, imports, importers, notes) \
                 for each file of the project at {}. Call project_map for an overview, \
                 get_file_context before reading or editing an unfamiliar file, and \
                 search_context to find where something lives.",
                self.root.display()
            ),
        })
    }

    fn call_tool(&self, params: &Value) -> Value {
        let name = params["name"].as_str().unwrap_or("");
        let args = &params["arguments"];
        let outcome = match name {
            "get_file_context" => match args["path"].as_str() {
                Some(path) => self.file_context(path),
                None => Err("`path` is required".to_string()),
            },
            "project_map" => pack::project_map(&self.root).map_err(|e| e.to_string()),
            "search_context" => match args["query"].as_str() {
                Some(query) => self.search(query),
                None => Err("`query` is required".to_string()),
            },
            _ => Err(format!("unknown tool: {name}")),
        };
        match outcome {
            Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": false }),
            Err(text) => json!({ "content": [{ "type": "text", "text": text }], "isError": true }),
        }
    }

    fn file_context(&self, path: &str) -> Result<String, String> {
        let file = self.resolve(path)?;
        let config = config::load_config_for_file(&file)
            .map(|(_, c)| c)
            .unwrap_or_default();
        match pack::file_context(&file).map_err(|e| e.to_string())? {
            Some(ctx) => {
                let mut text = format!("{}\n{}", config.header_block(), ctx.fields);
                if ctx.stale {
                    text.push_str(
                        "\n(This context was generated from an earlier version of the file.)",
                    );
                }
                Ok(text)
            }
            None => Ok(format!(
                "No stored context for {path} yet. Read the file directly, or run `llmctx index`."
            )),
        }
    }

    fn search(&self, query: &str) -> Result<String, String> {
        let hits = pack::search(&self.root, query).map_err(|e| e.to_string())?;
        if hits.is_empty() {
            return Ok(format!("No stored context mentions {query:?}."));
        }
        Ok(hits
            .iter()
            .map(|h| h.fields.clone())
            .collect::<Vec<_>>()
            .join("\n\n"))
    }

    /// A tool's `path` argument as a file inside the project root.
    fn resolve(&self, path: &str) -> Result<PathBuf, String> {
        let candidate = Path::new(path);
        let full = if candidate.is_absolute() {
            fsutil::absolute(candidate)
        } else {
            fsutil::absolute(&self.root.join(candidate))
        };
        if !full.starts_with(fsutil::absolute(&self.root)) {
            return Err(format!("{path} is outside the project root"));
        }
        Ok(full)
    }
}

fn tools() -> Value {
    json!([
        {
            "name": "get_file_context",
            "description": "Context summary for one project file: what it does, what it exports \
                            and imports, which files use it, and notes. Cheaper than reading \
                            the whole file to find out what it is for.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "File path relative to the project root (absolute paths inside the root also work)"
                    }
                },
                "required": ["path"]
            }
        },
        {
            "name": "project_map",
            "description": "One line per file of the project with its role, under the project's \
                            name, stack, current task and conventions. Use it to orient yourself.",
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "search_context",
            "description": "Find files whose path or context summary mentions a word or phrase \
                            (case-insensitive).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Text to look for" }
                },
                "required": ["query"]
            }
        }
    ])
}

fn error(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

#[cfg(test)]
mod tests {
    use super::*;
    use llmctx_core::{process::content_hash, store::ContextStore};
    use std::fs;
    use tempfile::TempDir;

    fn server() -> (TempDir, Server) {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(config::CONFIG_FILENAME),
            "project: Demo\ntask: ship it\n",
        )
        .unwrap();
        fs::write(dir.path().join("a.py"), "x = 1\n").unwrap();
        let store = ContextStore::open(dir.path()).unwrap();
        store
            .put(
                "a.py",
                &content_hash("x = 1\n"),
                "llm",
                "FILE: a.py\nROLE: Parses tokens\nIMPORTS: NONE",
            )
            .unwrap();
        let server = Server {
            root: dir.path().to_path_buf(),
        };
        (dir, server)
    }

    fn call(server: &Server, name: &str, args: Value) -> Value {
        server
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":name,"arguments":args}}))
            .unwrap()["result"]
            .clone()
    }

    #[test]
    fn initialize_negotiates_version_and_notifications_are_silent() {
        let (_dir, server) = server();
        let reply = server
            .handle(&json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":"2025-03-26"}}))
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(reply["result"]["serverInfo"]["name"], "llmctx");
        assert!(server
            .handle(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}))
            .is_none());
        let unknown = server
            .handle(&json!({"jsonrpc":"2.0","id":2,"method":"resources/list"}))
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[test]
    fn tools_serve_stored_context() {
        let (_dir, server) = server();
        let listed = server
            .handle(&json!({"jsonrpc":"2.0","id":1,"method":"tools/list"}))
            .unwrap();
        assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 3);

        let ctx = call(&server, "get_file_context", json!({"path":"a.py"}));
        assert_eq!(ctx["isError"], false);
        let text = ctx["content"][0]["text"].as_str().unwrap();
        assert!(text.contains("PROJECT: Demo"));
        assert!(text.contains("ROLE: Parses tokens"));

        let map = call(&server, "project_map", json!({}));
        assert!(map["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("a.py — Parses tokens"));

        let hits = call(&server, "search_context", json!({"query":"TOKENS"}));
        assert!(hits["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("FILE: a.py"));
    }

    #[test]
    fn paths_outside_the_root_are_refused() {
        let (_dir, server) = server();
        let r = call(&server, "get_file_context", json!({"path":"../../etc/passwd"}));
        assert_eq!(r["isError"], true);
    }
}
