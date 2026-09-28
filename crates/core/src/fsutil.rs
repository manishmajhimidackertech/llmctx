// <<<LLMCTX
// FILE: crates/core/src/fsutil.rs
// ROLE: Filesystem helpers — safe in-place rewrite of a user's file, lexical absolute paths
// EXPORTS: replace_file(), absolute()
// IMPORTS: NONE
// USED BY: crates/core/src/process.rs
// NOTES: Follows symlinks so the link itself is never replaced; keeps the original permissions
// LLMCTX>>>

use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

/// Make `path` absolute and resolve `.`/`..` lexically. Unlike
/// `canonicalize`, this works for paths that do not exist (yet), never
/// follows symlinks and never produces Windows `\\?\` verbatim paths.
pub fn absolute(path: &Path) -> PathBuf {
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(path),
            Err(_) => path.to_path_buf(),
        }
    };
    let mut out = PathBuf::new();
    for component in joined.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Replace the contents of `path` with `contents` atomically.
///
/// The new contents are written to a temporary file next to the target,
/// flushed to disk, given the target's permissions, and renamed over it. A
/// crash or full disk at any point leaves either the old file or the new one,
/// never a truncated mix — unlike `fs::write`, which truncates first.
///
/// Symlinks are resolved first, so the file they point at is rewritten and
/// the link stays a link.
pub fn replace_file(path: &Path, contents: &[u8]) -> io::Result<()> {
    let target = fs::canonicalize(path)?;
    let dir = target
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file has no parent"))?;
    let name = target
        .file_name()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "path has no file name"))?;
    let permissions = fs::metadata(&target)?.permissions();

    let tmp = dir.join(format!(
        ".{}.llmctx-tmp-{}",
        name.to_string_lossy(),
        std::process::id()
    ));

    let result = (|| {
        let mut file = OpenOptions::new().write(true).create_new(true).open(&tmp)?;
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        fs::set_permissions(&tmp, permissions)?;
        fs::rename(&tmp, &target)
    })();

    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn replaces_contents_and_leaves_no_temp_file() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, "old contents that are longer").unwrap();
        replace_file(&file, b"new").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn keeps_permissions_and_symlinks() {
        use std::os::unix::fs::{symlink, PermissionsExt};

        let dir = TempDir::new().unwrap();
        let real = dir.path().join("real.sh");
        fs::write(&real, "echo old").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o755)).unwrap();
        let link = dir.path().join("link.sh");
        symlink(&real, &link).unwrap();

        replace_file(&link, b"echo new").unwrap();

        assert!(fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "echo new");
        let mode = fs::metadata(&real).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);
    }
}
