// <<<LLMCTX
// FILE: crates/core/src/filter.rs
// ROLE: Decide whether a single file is excluded by .gitignore/.ignore, hidden paths or llmctx_ignore
// EXPORTS: is_ignored()
// IMPORTS: crates/core/src/fsutil.rs
// USED BY: crates/core/src/process.rs
// NOTES: Mirrors what `llmctx index`'s walker skips, so the daemon (one file at a time) and index agree
// LLMCTX>>>

//! One-file version of the rules `llmctx index` applies while walking.
//!
//! The walker prunes whole directories as it goes; the daemon only ever sees
//! single saved files, so it needs the same answer for one path. Both now go
//! through `process_file()`, which calls [`is_ignored`] — a file that `index`
//! would skip is never read, sent to Ollama or stored because it was saved.

use std::path::{Path, PathBuf};

use ignore::{
    gitignore::{Gitignore, GitignoreBuilder},
    overrides::OverrideBuilder,
    Match,
};
use tracing::warn;

use crate::fsutil::absolute;

/// Per-directory ignore files, highest precedence first (ripgrep's order).
const IGNORE_FILES: &[&str] = &[".ignore", ".gitignore"];

/// True when `path` must not be processed:
///
/// - any path component below `root` is hidden (starts with `.`), which also
///   covers `.git/`, `.env` and the `.llmctx/` store itself;
/// - it matches an `llmctx_ignore` pattern (relative to `root`);
/// - it is ignored by a `.ignore`/`.gitignore` between it and the repository
///   root, by `.git/info/exclude`, or by git's global excludes file.
///
/// `.gitignore` files are honoured even outside a git checkout, matching
/// `llmctx index`. Paths outside `root` are never reported as ignored.
pub fn is_ignored(root: &Path, path: &Path, llmctx_ignore: &[String]) -> bool {
    let path = absolute(path);
    let root = absolute(root);
    let Ok(rel) = path.strip_prefix(&root) else {
        return false;
    };

    // ── hidden components ────────────────────────────────────────────────
    if rel
        .components()
        .any(|c| c.as_os_str().to_string_lossy().starts_with('.'))
    {
        return true;
    }

    // ── llmctx_ignore ────────────────────────────────────────────────────
    if !llmctx_ignore.is_empty() {
        let mut builder = OverrideBuilder::new(&root);
        for pattern in llmctx_ignore {
            if let Err(e) = builder.add(&format!("!{pattern}")) {
                warn!("invalid llmctx_ignore pattern {pattern:?}: {e}");
            }
        }
        if let Ok(overrides) = builder.build() {
            // Check every ancestor directory too: `vendor/**` or `vendor`
            // must exclude `vendor/a/b.rs` just as the walker's pruning would.
            let mut prefix = PathBuf::new();
            let parts: Vec<_> = rel.components().collect();
            for (i, part) in parts.iter().enumerate() {
                prefix.push(part);
                let is_dir = i + 1 < parts.len();
                if overrides.matched(&prefix, is_dir).is_ignore() {
                    return true;
                }
            }
        }
    }

    // ── .ignore / .gitignore / exclude / global ──────────────────────────
    let git_root = path
        .ancestors()
        .skip(1)
        .find(|d| d.join(".git").exists())
        .map(Path::to_path_buf);
    // Ignore files apply from the file's directory up to the repository root
    // (which may be above the project root), or up to the project root.
    let top = git_root.clone().unwrap_or_else(|| root.clone());

    for dir in path.ancestors().skip(1) {
        for name in IGNORE_FILES {
            let file = dir.join(name);
            if !file.is_file() {
                continue;
            }
            let mut builder = GitignoreBuilder::new(dir);
            if let Some(e) = builder.add(&file) {
                warn!("could not read {}: {e}", file.display());
            }
            if let Ok(gi) = builder.build() {
                if let Some(ignored) = decide(&gi, dir, &path) {
                    return ignored;
                }
            }
        }
        if dir == top {
            break;
        }
    }

    if let Some(git_root) = git_root {
        let exclude = git_root.join(".git").join("info").join("exclude");
        if exclude.is_file() {
            let mut builder = GitignoreBuilder::new(&git_root);
            builder.add(&exclude);
            if let Ok(gi) = builder.build() {
                if let Some(ignored) = decide(&gi, &git_root, &path) {
                    return ignored;
                }
            }
        }
        let (global, _) = GitignoreBuilder::new(&git_root).build_global();
        if let Some(ignored) = decide(&global, &git_root, &path) {
            return ignored;
        }
    }

    false
}

/// `Some(true)` ignored, `Some(false)` explicitly re-included, `None` no rule.
fn decide(gi: &Gitignore, dir: &Path, path: &Path) -> Option<bool> {
    // Pass a path relative to the matcher's own root: the matcher panics on
    // paths it cannot place under its root.
    let rel = path.strip_prefix(dir).ok()?;
    match gi.matched_path_or_any_parents(rel, false) {
        Match::Ignore(_) => Some(true),
        Match::Whitelist(_) => Some(false),
        Match::None => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(root: &Path, rel: &str) -> PathBuf {
        let p = root.join(rel);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "x").unwrap();
        p
    }

    #[test]
    fn hidden_paths_are_ignored() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        assert!(is_ignored(root, &touch(root, ".env"), &[]));
        assert!(is_ignored(root, &touch(root, ".github/ci.yml"), &[]));
        assert!(is_ignored(root, &touch(root, ".llmctx/context.db"), &[]));
        assert!(!is_ignored(root, &touch(root, "src/main.rs"), &[]));
    }

    #[test]
    fn gitignore_rules_apply_with_nesting_and_negation() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        fs::write(root.join(".gitignore"), "target/\n*.log\n").unwrap();
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("sub/.gitignore"), "!keep.log\n").unwrap();

        assert!(is_ignored(root, &touch(root, "target/debug/app"), &[]));
        assert!(is_ignored(root, &touch(root, "sub/other.log"), &[]));
        assert!(!is_ignored(root, &touch(root, "sub/keep.log"), &[]));
        assert!(!is_ignored(root, &touch(root, "src/lib.rs"), &[]));
    }

    #[test]
    fn gitignore_above_project_root_applies_inside_repo() {
        let dir = TempDir::new().unwrap();
        let repo = dir.path();
        fs::create_dir_all(repo.join(".git")).unwrap();
        fs::write(repo.join(".gitignore"), "generated/\n").unwrap();
        let project = repo.join("app");
        assert!(is_ignored(
            &project,
            &touch(&project, "generated/x.rs"),
            &[]
        ));
        assert!(!is_ignored(&project, &touch(&project, "src/x.rs"), &[]));
    }

    #[test]
    fn llmctx_ignore_patterns_apply_to_files_and_directories() {
        let dir = TempDir::new().unwrap();
        let root = dir.path();
        let patterns = vec![
            "vendor/**".to_string(),
            "*.svg".to_string(),
            "docs".to_string(),
        ];
        assert!(is_ignored(root, &touch(root, "vendor/a/b.rs"), &patterns));
        assert!(is_ignored(root, &touch(root, "img/logo.svg"), &patterns));
        assert!(is_ignored(
            root,
            &touch(root, "docs/guide/intro.md"),
            &patterns
        ));
        assert!(!is_ignored(root, &touch(root, "src/a.rs"), &patterns));
    }

    #[test]
    fn outside_root_is_never_ignored() {
        let a = TempDir::new().unwrap();
        let b = TempDir::new().unwrap();
        assert!(!is_ignored(a.path(), &touch(b.path(), ".hidden"), &[]));
    }
}
