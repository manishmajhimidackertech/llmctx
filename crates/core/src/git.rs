// <<<LLMCTX
// FILE: crates/core/src/git.rs
// ROLE: Minimal read-only git queries (via the git CLI) used to protect committed content
// EXPORTS: committed_version()
// IMPORTS: NONE
// USED BY: crates/core/src/process.rs
// NOTES: Every failure (no git, not a repo, untracked file) reads as None — callers treat that as "not committed"
// LLMCTX>>>

use std::{path::Path, process::Command};

/// The contents of `path` as committed at `HEAD`, or `None` when git is not
/// installed, the file is not in a repository, or it is not committed.
pub fn committed_version(path: &Path) -> Option<String> {
    let dir = path.parent()?;
    let name = path.file_name()?.to_str()?;
    // `HEAD:./name` resolves relative to the working directory, so there is
    // no need to work out the path from the repository root.
    let output = Command::new("git")
        .arg("-C")
        .arg(if dir.as_os_str().is_empty() {
            Path::new(".")
        } else {
            dir
        })
        .args(["show", &format!("HEAD:./{name}")])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test]
    fn reads_head_version_and_ignores_untracked() {
        let dir = TempDir::new().unwrap();
        if !git(dir.path(), &["init", "-q"]) {
            eprintln!("git not available — skipping");
            return;
        }
        let tracked = dir.path().join("tracked.txt");
        fs::write(&tracked, "committed\n").unwrap();
        assert!(git(dir.path(), &["add", "tracked.txt"]));
        assert!(git(
            dir.path(),
            &[
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "-m",
                "init"
            ]
        ));
        fs::write(&tracked, "edited\n").unwrap();

        assert_eq!(committed_version(&tracked).as_deref(), Some("committed\n"));

        let untracked = dir.path().join("new.txt");
        fs::write(&untracked, "x").unwrap();
        assert_eq!(committed_version(&untracked), None);
    }
}
