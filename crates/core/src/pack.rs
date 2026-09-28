// <<<LLMCTX
// FILE: crates/core/src/pack.rs
// ROLE: Read-only views of stored context — packed text for an LLM, rendered per-file context, project map, search
// EXPORTS: pack(), file_context(), project_map(), search(), Packed, FileContext
// IMPORTS: crates/core/src/store.rs, crates/core/src/config.rs, crates/core/src/process.rs
// USED BY: crates/cli/src/main.rs (pack, map, mcp)
// NOTES: Never writes to the store; the project header and USED BY are computed here, at read time
// LLMCTX>>>

//! Everything that turns stored context into text for an LLM.
//!
//! Two things are computed here rather than stored, so they are always
//! current: the project header (from today's `llmcontext.yaml`) and `USED BY`
//! (from the other files' `IMPORTS`).

use std::path::Path;

use crate::{
    config::{self, ProjectConfig},
    process::{content_hash, ProcessError},
    store::{self, ContextStore, Lookup, StoredContext},
};

/// Context for one file, rendered for display.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileContext {
    pub rel_path: String,
    /// The six fields, with `USED BY` computed from other files when known.
    pub fields: String,
    /// True when the file changed after this context was generated.
    pub stale: bool,
}

/// The result of [`pack`].
#[derive(Debug, Clone)]
pub struct Packed {
    pub text: String,
    /// False when the file itself had no context yet.
    pub has_context: bool,
}

/// Context for `path`, read-only: what is stored under its path (marked
/// stale if the file changed since), or context carried by identical content
/// elsewhere (a rename that has not been processed yet). `Ok(None)` when
/// there is none or the file is outside every project; no store is created.
pub fn file_context(path: &Path) -> Result<Option<FileContext>, ProcessError> {
    let Some((store, rel_path)) = ContextStore::open_existing_for_file(path)? else {
        return Ok(None);
    };
    let found = match std::fs::read_to_string(path) {
        Ok(source) => match store.resolve(&rel_path, &content_hash(&source))? {
            Lookup::Current(c) => Some((c.fields, false)),
            Lookup::Stale(c) => Some((c.fields, true)),
            Lookup::Found(donor) => Some((
                store::set_field(&donor.context.fields, "FILE", &rel_path),
                false,
            )),
            Lookup::Missing => None,
        },
        // Unreadable now, but whatever was stored for it is still the answer.
        Err(_) => store.get(&rel_path)?.map(|c| (c.fields, true)),
    };
    Ok(found.map(|(fields, stale)| FileContext {
        fields: with_used_by(&store, &rel_path, &fields),
        rel_path,
        stale,
    }))
}

/// Merge the project header, the file's context and its source into one
/// block of text to paste into an LLM. With `with_imports`, the context of
/// every project file listed in its `IMPORTS` is included too (context only,
/// not their source).
pub fn pack(path: &Path, with_imports: bool) -> Result<Packed, ProcessError> {
    let source = std::fs::read_to_string(path).map_err(|e| ProcessError::ReadSource {
        path: path.display().to_string(),
        source: e,
    })?;
    let config = load_config(path);
    let context = file_context(path)?;
    let label = match &context {
        Some(c) => c.rel_path.clone(),
        None => display_name(path),
    };

    let mut out = format!("=== PROJECT ===\n{}\n", config.header_block());

    out.push_str(&format!("=== CONTEXT: {label} ===\n"));
    match &context {
        Some(c) => {
            out.push_str(&c.fields);
            out.push('\n');
            if c.stale {
                out.push_str("(This context was generated from an earlier version of the file.)\n");
            }
        }
        None => {
            out.push_str("[no context yet — daemon may still be generating, try again shortly]\n")
        }
    }

    if with_imports {
        if let Some(c) = &context {
            for (rel, fields) in imported_contexts(path, &c.fields)? {
                out.push_str(&format!("\n=== CONTEXT: {rel} (imported) ===\n{fields}\n"));
            }
        }
    }

    out.push_str(&format!("\n=== SOURCE: {label} ===\n{source}"));
    Ok(Packed {
        text: out,
        has_context: context.is_some(),
    })
}

/// One line per file with stored context — path and ROLE — under the
/// project header. A compact overview to start an LLM session with.
pub fn project_map(root: &Path) -> Result<String, ProcessError> {
    let config = config::load_config(&root.join(config::CONFIG_FILENAME)).unwrap_or_default();
    let mut out = format!(
        "=== PROJECT ===\n{}\n=== FILES ===\n",
        config.header_block()
    );
    let Some(store) = ContextStore::open_existing(root)? else {
        out.push_str("(no stored context yet — run `llmctx index`)\n");
        return Ok(out);
    };
    let all = store.all()?;
    if all.is_empty() {
        out.push_str("(no stored context yet — run `llmctx index`)\n");
    }
    for c in all {
        let role = store::field_value(&c.fields, "ROLE").unwrap_or("?");
        out.push_str(&format!("{} — {role}\n", c.rel_path));
    }
    Ok(out)
}

/// Stored context whose path or fields contain `query` (case-insensitive).
pub fn search(root: &Path, query: &str) -> Result<Vec<FileContext>, ProcessError> {
    let Some(store) = ContextStore::open_existing(root)? else {
        return Ok(Vec::new());
    };
    let needle = query.to_lowercase();
    Ok(store
        .all()?
        .into_iter()
        .filter(|c| {
            c.rel_path.to_lowercase().contains(&needle) || c.fields.to_lowercase().contains(&needle)
        })
        .map(|c| FileContext {
            fields: with_used_by(&store, &c.rel_path, &c.fields),
            rel_path: c.rel_path,
            stale: false,
        })
        .collect())
}

// ── helpers ──────────────────────────────────────────────────────────────────

/// Replace `USED BY` with the files that actually list this one in their
/// `IMPORTS`, when there are any. Otherwise the stored value stands.
fn with_used_by(store: &ContextStore, rel_path: &str, fields: &str) -> String {
    match store.importers_of(rel_path) {
        Ok(importers) if !importers.is_empty() => {
            store::set_field(fields, "USED BY", &importers.join(", "))
        }
        _ => fields.to_string(),
    }
}

/// Stored context of each project file named in `fields`' IMPORTS.
fn imported_contexts(path: &Path, fields: &str) -> Result<Vec<(String, String)>, ProcessError> {
    let Some((store, _)) = ContextStore::open_existing_for_file(path)? else {
        return Ok(Vec::new());
    };
    let imports = store::field_value(fields, "IMPORTS").unwrap_or("");
    let mut out = Vec::new();
    for rel in imports.split(',').map(|p| {
        p.trim()
            .trim_matches(|c| c == '`' || c == '"' || c == '\'')
            .replace('\\', "/")
            .trim_start_matches("./")
            .to_string()
    }) {
        if rel.is_empty() {
            continue;
        }
        if let Some(StoredContext { fields, .. }) = store.get(&rel)? {
            out.push((rel.clone(), with_used_by(&store, &rel, &fields)));
        }
    }
    Ok(out)
}

fn load_config(path: &Path) -> ProjectConfig {
    config::load_config_for_file(path)
        .map(|(_, c)| c)
        .unwrap_or_default()
}

fn display_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn project() -> (TempDir, ContextStore) {
        let dir = TempDir::new().unwrap();
        fs::write(
            dir.path().join(config::CONFIG_FILENAME),
            "project: P\ntask: first task\n",
        )
        .unwrap();
        let store = ContextStore::open(dir.path()).unwrap();
        (dir, store)
    }

    fn write(dir: &TempDir, rel: &str, content: &str) -> std::path::PathBuf {
        let p = dir.path().join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn header_comes_from_the_current_config() {
        let (dir, store) = project();
        let file = write(&dir, "a.py", "x = 1\n");
        store
            .put(
                "a.py",
                &content_hash("x = 1\n"),
                "llm",
                "FILE: a.py\nROLE: r",
            )
            .unwrap();
        assert!(pack(&file, false)
            .unwrap()
            .text
            .contains("TASK: first task"));

        // Editing the config changes every pack immediately — nothing stale.
        fs::write(
            dir.path().join(config::CONFIG_FILENAME),
            "project: P\ntask: second task\n",
        )
        .unwrap();
        let text = pack(&file, false).unwrap().text;
        assert!(text.contains("TASK: second task"));
        assert!(!text.contains("first task"));
        assert!(!text.contains("HASH:"));
        assert!(text.contains("=== SOURCE: a.py ===\nx = 1\n"));
    }

    #[test]
    fn used_by_is_computed_and_imports_can_be_included() {
        let (dir, store) = project();
        let util = write(&dir, "lib/util.py", "u = 1\n");
        let app = write(&dir, "app.py", "a = 1\n");
        store
            .put(
                "lib/util.py",
                &content_hash("u = 1\n"),
                "ollama",
                "FILE: lib/util.py\nROLE: helpers\nIMPORTS: NONE\nUSED BY: UNKNOWN",
            )
            .unwrap();
        store
            .put(
                "app.py",
                &content_hash("a = 1\n"),
                "llm",
                "FILE: app.py\nROLE: entry\nIMPORTS: lib/util.py\nUSED BY: NONE",
            )
            .unwrap();

        let util_ctx = file_context(&util).unwrap().unwrap();
        assert!(util_ctx.fields.contains("USED BY: app.py"));

        let packed = pack(&app, true).unwrap().text;
        assert!(packed
            .contains("=== CONTEXT: lib/util.py (imported) ===\nFILE: lib/util.py\nROLE: helpers"));
    }

    #[test]
    fn stale_context_is_labelled_and_missing_context_is_reported() {
        let (dir, store) = project();
        let file = write(&dir, "a.py", "changed\n");
        store
            .put(
                "a.py",
                &content_hash("original\n"),
                "llm",
                "FILE: a.py\nROLE: r",
            )
            .unwrap();
        let packed = pack(&file, false).unwrap();
        assert!(packed.has_context);
        assert!(packed.text.contains("earlier version of the file"));

        let other = write(&dir, "b.py", "b\n");
        let packed = pack(&other, false).unwrap();
        assert!(!packed.has_context);
        assert!(packed.text.contains("[no context yet"));
    }

    #[test]
    fn map_and_search() {
        let (dir, store) = project();
        store
            .put("a.py", "1", "llm", "FILE: a.py\nROLE: Parses tokens")
            .unwrap();
        store
            .put("b.py", "2", "llm", "FILE: b.py\nROLE: Renders HTML")
            .unwrap();
        let map = project_map(dir.path()).unwrap();
        assert!(map.contains("a.py — Parses tokens\nb.py — Renders HTML\n"));
        let hits = search(dir.path(), "html").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].rel_path, "b.py");
    }
}
