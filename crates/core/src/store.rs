// <<<LLMCTX
// FILE: crates/core/src/store.rs
// ROLE: Filesystem-independent context store — one SQLite database per project at <root>/.llmctx/context.db
// EXPORTS: ContextStore, StoreSet, StoredContext, Lookup, StoreError, project_root(), project_root_for_file(), retarget_body(), STORE_DIR, CONTEXT_VERSION
// IMPORTS: crates/core/src/config.rs
// USED BY: crates/core/src/process.rs, crates/cli/src/main.rs, crates/cpctx/src/main.rs, crates/daemon/src/main.rs
// NOTES: Keys are root-relative paths with `/` separators; the content hash is a secondary key so renamed files keep their context
// LLMCTX>>>

//! llmctx's own storage layer.
//!
//! Context used to live in an NTFS Alternate Data Stream on each file, which
//! only exists on Windows/NTFS and is silently dropped by most copy, sync and
//! archive tools. Instead, every project now owns a single SQLite database in
//! a hidden `.llmctx/` directory at its root — the same idea as `.git/`. It
//! behaves identically on NTFS, ext4, APFS, FAT32, exFAT and network shares,
//! and travels with the project whenever the project folder is copied.
//!
//! The directory ignores itself (`.llmctx/.gitignore` contains `*`), so it
//! never shows up in version control and the user's own `.gitignore` is never
//! touched.

use std::{
    collections::HashMap,
    path::{Component, Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension};
use thiserror::Error;

use crate::config;

/// Directory at the project root that holds everything llmctx stores.
pub const STORE_DIR: &str = ".llmctx";

const DB_FILENAME: &str = "context.db";

/// Written into `.llmctx/.gitignore` so the store ignores itself.
const GITIGNORE_BODY: &str =
    "# Created by llmctx. Keeps this directory out of version control.\n*\n";

/// Format version of stored context bodies. Rows stamped with any other
/// version read as missing, so a format change regenerates silently.
pub const CONTEXT_VERSION: u32 = 1;

/// Version of the table layout, kept in SQLite's `user_version` pragma.
const SCHEMA_VERSION: i64 = 1;

/// How long a connection waits for another process (daemon vs. CLI) to
/// finish a write before giving up.
const BUSY_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Error)]
pub enum StoreError {
    #[error(
        "{path} is not inside an llmctx project (no llmcontext.yaml, {STORE_DIR}/ or .git above it) \
         — run `llmctx init` in the project root"
    )]
    NoProject { path: String },

    #[error("{path} is outside the project root {root}")]
    OutsideRoot { path: String, root: String },

    #[error("I/O error on {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("context store {path}: {source}")]
    Sqlite {
        path: String,
        #[source]
        source: rusqlite::Error,
    },

    #[error(
        "context store {path} uses schema {found}, but this llmctx only knows {expected} \
         — upgrade llmctx"
    )]
    NewerSchema {
        path: String,
        found: i64,
        expected: i64,
    },
}

/// One stored context entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredContext {
    /// Path relative to the project root, `/`-separated on every platform.
    pub rel_path: String,
    /// SHA-256 of the source the context was generated from.
    pub content_hash: String,
    /// The full context body (header, SOURCE, HASH and the six fields).
    pub body: String,
}

/// Result of [`ContextStore::lookup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Context stored under this path was generated from this exact content.
    Current(StoredContext),
    /// Another path held context for identical content (the file was
    /// renamed, moved or copied). It has been stored under this path too,
    /// and the old entry dropped if its file no longer exists.
    Adopted {
        context: StoredContext,
        from: String,
    },
    /// Context exists under this path but for different content.
    Stale(StoredContext),
    /// Nothing usable stored.
    Missing,
}

// ── Project root resolution ──────────────────────────────────────────────────

/// Find the project root governing `start_dir`.
///
/// The nearest `llmcontext.yaml` wins, since that file is llmctx's own
/// project marker. Without one, the nearest directory that already has a
/// store or is a git checkout is used. `None` means the path is not inside
/// any project, and nothing should be written for it — the store must never
/// scatter `.llmctx/` directories next to arbitrary files.
pub fn project_root(start_dir: &Path) -> Option<PathBuf> {
    let start = absolute(start_dir);
    if let Ok(config_path) = config::find_config(&start) {
        return config_path.parent().map(Path::to_path_buf);
    }
    start
        .ancestors()
        .find(|dir| dir.join(STORE_DIR).is_dir() || dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// [`project_root`] for the directory containing `file`.
pub fn project_root_for_file(file: &Path) -> Option<PathBuf> {
    let file = absolute(file);
    project_root(file.parent()?)
}

// ── Store ────────────────────────────────────────────────────────────────────

/// An open connection to one project's context database.
pub struct ContextStore {
    root: PathBuf,
    db_path: PathBuf,
    conn: Connection,
}

impl ContextStore {
    /// Open the store at `root`, creating `.llmctx/` and the database if
    /// they do not exist yet.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let root = absolute(root);
        let dir = root.join(STORE_DIR);
        prepare_store_dir(&dir)?;
        Self::connect(root, dir.join(DB_FILENAME))
    }

    /// Open the store at `root` only if one already exists. Used by read-only
    /// commands so that looking for context never creates a store.
    pub fn open_existing(root: &Path) -> Result<Option<Self>, StoreError> {
        let root = absolute(root);
        let db_path = root.join(STORE_DIR).join(DB_FILENAME);
        if !db_path.is_file() {
            return Ok(None);
        }
        Self::connect(root, db_path).map(Some)
    }

    /// Open (creating if needed) the store governing `file`, and return it
    /// together with the file's key inside it.
    pub fn open_for_file(file: &Path) -> Result<(Self, String), StoreError> {
        let root = project_root_for_file(file).ok_or_else(|| StoreError::NoProject {
            path: file.display().to_string(),
        })?;
        let store = Self::open(&root)?;
        let rel = store.rel_key(file)?;
        Ok((store, rel))
    }

    /// Like [`open_for_file`](Self::open_for_file), but `Ok(None)` when the
    /// file is outside any project or its project has no store yet.
    pub fn open_existing_for_file(file: &Path) -> Result<Option<(Self, String)>, StoreError> {
        let Some(root) = project_root_for_file(file) else {
            return Ok(None);
        };
        let Some(store) = Self::open_existing(&root)? else {
            return Ok(None);
        };
        let rel = store.rel_key(file)?;
        Ok(Some((store, rel)))
    }

    fn connect(root: PathBuf, db_path: PathBuf) -> Result<Self, StoreError> {
        let conn = Connection::open(&db_path).map_err(sqlite_err(&db_path))?;
        conn.busy_timeout(BUSY_TIMEOUT)
            .map_err(sqlite_err(&db_path))?;
        // The journal mode is left at SQLite's default (rollback journal)
        // on purpose: WAL needs shared memory, which does not work on
        // network file systems, and the writes here are tiny and rare.
        let store = Self {
            root,
            db_path,
            conn,
        };
        store.init_schema()?;
        Ok(store)
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        let found: i64 = self
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(self.err())?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema {
                path: self.db_path.display().to_string(),
                found,
                expected: SCHEMA_VERSION,
            });
        }
        if found < SCHEMA_VERSION {
            // IMMEDIATE takes the write lock up front, so two processes
            // creating the same store at once simply queue on busy_timeout.
            self.conn
                .execute_batch(&format!(
                    "BEGIN IMMEDIATE;
                     CREATE TABLE IF NOT EXISTS context (
                         rel_path     TEXT PRIMARY KEY,
                         content_hash TEXT NOT NULL,
                         body         TEXT NOT NULL,
                         version      INTEGER NOT NULL,
                         updated_at   INTEGER NOT NULL
                     );
                     CREATE INDEX IF NOT EXISTS context_by_hash ON context(content_hash);
                     PRAGMA user_version = {SCHEMA_VERSION};
                     COMMIT;"
                ))
                .map_err(self.err())?;
        }
        Ok(())
    }

    /// The project root this store belongs to.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Location of the database file.
    pub fn db_path(&self) -> &Path {
        &self.db_path
    }

    /// Key for `file`: its path relative to the root, `/`-separated.
    pub fn rel_key(&self, file: &Path) -> Result<String, StoreError> {
        let file = absolute(file);
        let rel = file
            .strip_prefix(&self.root)
            .map_err(|_| StoreError::OutsideRoot {
                path: file.display().to_string(),
                root: self.root.display().to_string(),
            })?;
        let parts: Vec<_> = rel
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect();
        Ok(parts.join("/"))
    }

    /// Context stored under `rel_path`, whatever content it was made from.
    pub fn get(&self, rel_path: &str) -> Result<Option<StoredContext>, StoreError> {
        self.conn
            .query_row(
                "SELECT rel_path, content_hash, body FROM context
                 WHERE rel_path = ?1 AND version = ?2",
                params![rel_path, CONTEXT_VERSION],
                row_to_context,
            )
            .optional()
            .map_err(self.err())
    }

    /// Most recent context for `content_hash` stored under any path other
    /// than `excluding`.
    pub fn find_by_hash(
        &self,
        content_hash: &str,
        excluding: &str,
    ) -> Result<Option<StoredContext>, StoreError> {
        self.conn
            .query_row(
                "SELECT rel_path, content_hash, body FROM context
                 WHERE content_hash = ?1 AND rel_path <> ?2 AND version = ?3
                 ORDER BY updated_at DESC LIMIT 1",
                params![content_hash, excluding, CONTEXT_VERSION],
                row_to_context,
            )
            .optional()
            .map_err(self.err())
    }

    /// Store `body` for `rel_path`, replacing whatever was there.
    pub fn put(&self, rel_path: &str, content_hash: &str, body: &str) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO context (rel_path, content_hash, body, version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(rel_path) DO UPDATE SET
                     content_hash = excluded.content_hash,
                     body         = excluded.body,
                     version      = excluded.version,
                     updated_at   = excluded.updated_at",
                params![rel_path, content_hash, body, CONTEXT_VERSION, now_secs()],
            )
            .map(|_| ())
            .map_err(self.err())
    }

    /// Delete the entry for `rel_path`. Returns whether one existed.
    pub fn remove(&self, rel_path: &str) -> Result<bool, StoreError> {
        self.conn
            .execute("DELETE FROM context WHERE rel_path = ?1", params![rel_path])
            .map(|n| n > 0)
            .map_err(self.err())
    }

    /// Every stored key, sorted.
    pub fn rel_paths(&self) -> Result<Vec<String>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel_path FROM context ORDER BY rel_path")
            .map_err(self.err())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(self.err())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(self.err())
    }

    /// Resolve the context for `rel_path` whose current content hashes to
    /// `content_hash`.
    ///
    /// When the path itself has nothing current, a match on the content hash
    /// elsewhere is adopted: that is how a renamed, moved or copied file keeps
    /// its context without regenerating it. An NTFS stream got this for free
    /// by travelling with the file; here the hash plays that role.
    pub fn lookup(&self, rel_path: &str, content_hash: &str) -> Result<Lookup, StoreError> {
        let existing = self.get(rel_path)?;
        if let Some(ctx) = &existing {
            if ctx.content_hash == content_hash {
                return Ok(Lookup::Current(ctx.clone()));
            }
        }

        if let Some(donor) = self.find_by_hash(content_hash, rel_path)? {
            let body = retarget_body(&donor.body, rel_path);
            // A donor whose file is gone was renamed or moved, not copied —
            // its entry would only be an orphan now.
            let donor_gone = !self.root.join(&donor.rel_path).exists();

            let tx = self.conn.unchecked_transaction().map_err(self.err())?;
            self.put(rel_path, content_hash, &body)?;
            if donor_gone {
                self.remove(&donor.rel_path)?;
            }
            tx.commit().map_err(self.err())?;

            return Ok(Lookup::Adopted {
                context: StoredContext {
                    rel_path: rel_path.to_string(),
                    content_hash: content_hash.to_string(),
                    body,
                },
                from: donor.rel_path,
            });
        }

        Ok(match existing {
            Some(ctx) => Lookup::Stale(ctx),
            None => Lookup::Missing,
        })
    }

    /// Delete entries whose file no longer exists under the root. Returns
    /// the keys removed.
    pub fn prune_missing(&self) -> Result<Vec<String>, StoreError> {
        let gone: Vec<String> = self
            .rel_paths()?
            .into_iter()
            .filter(|rel| !self.root.join(rel).is_file())
            .collect();
        let tx = self.conn.unchecked_transaction().map_err(self.err())?;
        for rel in &gone {
            self.remove(rel)?;
        }
        tx.commit().map_err(self.err())?;
        Ok(gone)
    }

    fn err(&self) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
        sqlite_err(&self.db_path)
    }
}

// ── StoreSet ─────────────────────────────────────────────────────────────────

/// Open stores keyed by project root, for commands that touch many files
/// (`cpctx copy`, `llmctx migrate`) and should not reopen a database per file.
#[derive(Default)]
pub struct StoreSet {
    stores: HashMap<PathBuf, ContextStore>,
}

impl StoreSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// The store governing `file` (created if needed) and the file's key.
    pub fn for_file(&mut self, file: &Path) -> Result<(&ContextStore, String), StoreError> {
        let root = project_root_for_file(file).ok_or_else(|| StoreError::NoProject {
            path: file.display().to_string(),
        })?;
        let store = match self.stores.entry(root) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                let store = ContextStore::open(e.key())?;
                e.insert(store)
            }
        };
        let rel = store.rel_key(file)?;
        Ok((store, rel))
    }

    /// The store governing `file` only if it already exists.
    pub fn existing_for_file(
        &mut self,
        file: &Path,
    ) -> Result<Option<(&ContextStore, String)>, StoreError> {
        let Some(root) = project_root_for_file(file) else {
            return Ok(None);
        };
        let store = match self.stores.entry(root) {
            std::collections::hash_map::Entry::Occupied(e) => e.into_mut(),
            std::collections::hash_map::Entry::Vacant(e) => {
                match ContextStore::open_existing(e.key())? {
                    Some(store) => e.insert(store),
                    None => return Ok(None),
                }
            }
        };
        let rel = store.rel_key(file)?;
        Ok(Some((store, rel)))
    }
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Point the `FILE:` field of a context body at `rel_path`. Used when context
/// is carried to a new path, so the body never names the file's old location.
pub fn retarget_body(body: &str, rel_path: &str) -> String {
    let mut replaced = false;
    let mut out: Vec<String> = body
        .lines()
        .map(|line| {
            if !replaced && line.trim_start().starts_with("FILE:") {
                replaced = true;
                format!("FILE: {rel_path}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if body.ends_with('\n') {
        out.push(String::new());
    }
    out.join("\n")
}

/// Create `.llmctx/` if needed and make sure it still ignores itself.
fn prepare_store_dir(dir: &Path) -> Result<(), StoreError> {
    let io_err = |path: &Path| {
        let path = path.display().to_string();
        move |source| StoreError::Io { path, source }
    };

    let created = !dir.is_dir();
    if created {
        std::fs::create_dir_all(dir).map_err(io_err(dir))?;
    }
    let gitignore = dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, GITIGNORE_BODY).map_err(io_err(&gitignore))?;
    }
    if created {
        hide_dir(dir);
    }
    Ok(())
}

/// Windows does not treat dot-directories as hidden, so set the attribute.
/// Best effort: a visible `.llmctx` is cosmetic, not an error.
#[cfg(target_os = "windows")]
fn hide_dir(dir: &Path) {
    use windows::{
        core::HSTRING,
        Win32::Storage::FileSystem::{SetFileAttributesW, FILE_ATTRIBUTE_HIDDEN},
    };
    let name = HSTRING::from(&*dir.to_string_lossy());
    unsafe {
        let _ = SetFileAttributesW(&name, FILE_ATTRIBUTE_HIDDEN);
    }
}

/// Elsewhere the leading dot already hides it.
#[cfg(not(target_os = "windows"))]
fn hide_dir(_dir: &Path) {}

/// Make `path` absolute and resolve `.`/`..` lexically. Unlike
/// `canonicalize`, this works for paths that do not exist (yet) and never
/// produces Windows `\\?\` verbatim paths, so keys stay stable.
fn absolute(path: &Path) -> PathBuf {
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

fn row_to_context(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredContext> {
    Ok(StoredContext {
        rel_path: row.get(0)?,
        content_hash: row.get(1)?,
        body: row.get(2)?,
    })
}

fn sqlite_err(db_path: &Path) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |source| StoreError::Sqlite {
        path: db_path.display().to_string(),
        source,
    }
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// A temp project with a config at its root and one source file.
    fn project() -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(config::CONFIG_FILENAME), "project: T\n").unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        let file = src.join("main.rs");
        fs::write(&file, "fn main() {}\n").unwrap();
        (dir, file)
    }

    #[test]
    fn open_creates_self_ignoring_store() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        assert!(store.db_path().is_file());
        let ignore = fs::read_to_string(dir.path().join(STORE_DIR).join(".gitignore")).unwrap();
        assert!(ignore.lines().any(|l| l == "*"));
    }

    #[test]
    fn open_existing_never_creates() {
        let (dir, _) = project();
        assert!(ContextStore::open_existing(dir.path()).unwrap().is_none());
        assert!(!dir.path().join(STORE_DIR).exists());
    }

    #[test]
    fn keys_are_root_relative_with_forward_slashes() {
        let (dir, file) = project();
        let (store, rel) = ContextStore::open_for_file(&file).unwrap();
        assert_eq!(rel, "src/main.rs");
        assert_eq!(store.root(), absolute(dir.path()));
        // `.` and `..` segments resolve to the same key.
        let odd = dir
            .path()
            .join("src")
            .join(".")
            .join("..")
            .join("src")
            .join("main.rs");
        assert_eq!(store.rel_key(&odd).unwrap(), "src/main.rs");
    }

    #[test]
    fn put_get_remove_round_trip() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        assert_eq!(store.get("a.rs").unwrap(), None);
        store.put("a.rs", "h1", "body one").unwrap();
        store.put("a.rs", "h2", "body two").unwrap();
        let got = store.get("a.rs").unwrap().unwrap();
        assert_eq!(got.content_hash, "h2");
        assert_eq!(got.body, "body two");
        assert!(store.remove("a.rs").unwrap());
        assert!(!store.remove("a.rs").unwrap());
        assert_eq!(store.get("a.rs").unwrap(), None);
    }

    #[test]
    fn data_persists_across_connections() {
        let (dir, _) = project();
        ContextStore::open(dir.path())
            .unwrap()
            .put("a.rs", "h", "b")
            .unwrap();
        let reopened = ContextStore::open_existing(dir.path()).unwrap().unwrap();
        assert_eq!(reopened.get("a.rs").unwrap().unwrap().body, "b");
    }

    #[test]
    fn lookup_current_stale_missing() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        assert_eq!(store.lookup("a.rs", "h1").unwrap(), Lookup::Missing);
        store.put("a.rs", "h1", "FILE: a.rs").unwrap();
        assert!(matches!(
            store.lookup("a.rs", "h1").unwrap(),
            Lookup::Current(_)
        ));
        assert!(matches!(
            store.lookup("a.rs", "h2").unwrap(),
            Lookup::Stale(_)
        ));
    }

    #[test]
    fn lookup_adopts_context_of_renamed_file() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        // old.rs no longer exists on disk: this is a rename.
        store
            .put("old.rs", "h", "SOURCE: llm\nFILE: old.rs\nROLE: r")
            .unwrap();

        match store.lookup("new.rs", "h").unwrap() {
            Lookup::Adopted { context, from } => {
                assert_eq!(from, "old.rs");
                assert_eq!(context.body, "SOURCE: llm\nFILE: new.rs\nROLE: r");
            }
            other => panic!("expected Adopted, got {other:?}"),
        }
        assert_eq!(store.get("old.rs").unwrap(), None);
        assert!(matches!(
            store.lookup("new.rs", "h").unwrap(),
            Lookup::Current(_)
        ));
    }

    #[test]
    fn lookup_keeps_donor_of_copied_file() {
        let (_dir, file) = project();
        let (store, rel) = ContextStore::open_for_file(&file).unwrap();
        store.put(&rel, "h", "FILE: src/main.rs").unwrap();

        // The donor file still exists, so this is a copy — both keep context.
        assert!(matches!(
            store.lookup("src/copy.rs", "h").unwrap(),
            Lookup::Adopted { .. }
        ));
        assert!(store.get(&rel).unwrap().is_some());
        assert_eq!(
            store.get("src/copy.rs").unwrap().unwrap().body,
            "FILE: src/copy.rs"
        );
    }

    #[test]
    fn rows_from_other_versions_read_as_missing() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO context VALUES ('a.rs', 'h', 'b', ?1, 0)",
                params![CONTEXT_VERSION + 1],
            )
            .unwrap();
        assert_eq!(store.get("a.rs").unwrap(), None);
        assert_eq!(store.lookup("b.rs", "h").unwrap(), Lookup::Missing);
    }

    #[test]
    fn newer_schema_is_refused() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        store
            .conn
            .execute_batch(&format!("PRAGMA user_version = {}", SCHEMA_VERSION + 1))
            .unwrap();
        drop(store);
        assert!(matches!(
            ContextStore::open(dir.path()),
            Err(StoreError::NewerSchema { .. })
        ));
    }

    #[test]
    fn prune_removes_entries_for_deleted_files() {
        let (dir, file) = project();
        let (store, rel) = ContextStore::open_for_file(&file).unwrap();
        store.put(&rel, "h", "b").unwrap();
        store.put("src/deleted.rs", "h2", "b").unwrap();
        assert_eq!(store.prune_missing().unwrap(), vec!["src/deleted.rs"]);
        assert_eq!(store.rel_paths().unwrap(), vec![rel]);
        drop(dir);
    }

    #[test]
    fn config_beats_nearer_git_checkout() {
        let (dir, _) = project();
        let nested = dir.path().join("vendor").join("lib");
        fs::create_dir_all(nested.join(".git")).unwrap();
        assert_eq!(project_root(&nested), Some(absolute(dir.path())));
    }

    #[test]
    fn git_checkout_is_a_root_without_config() {
        let dir = TempDir::new().unwrap();
        fs::create_dir_all(dir.path().join(".git")).unwrap();
        let deep = dir.path().join("a").join("b");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(project_root(&deep), Some(absolute(dir.path())));
    }

    #[test]
    fn store_set_reuses_one_store_per_root() {
        let (dir, file) = project();
        let mut set = StoreSet::new();
        assert!(set.existing_for_file(&file).unwrap().is_none());
        {
            let (store, rel) = set.for_file(&file).unwrap();
            store.put(&rel, "h", "b").unwrap();
        }
        let (store, rel) = set.existing_for_file(&file).unwrap().unwrap();
        assert_eq!(store.get(&rel).unwrap().unwrap().body, "b");
        assert_eq!(set.stores.len(), 1);
        drop(dir);
    }

    #[test]
    fn retarget_rewrites_only_the_file_field() {
        let body = "PROJECT: p\nFILE: a.rs\nROLE: mentions FILE: a.rs\n";
        assert_eq!(
            retarget_body(body, "b/c.rs"),
            "PROJECT: p\nFILE: b/c.rs\nROLE: mentions FILE: a.rs\n"
        );
        assert_eq!(retarget_body("ROLE: none", "x"), "ROLE: none");
    }
}
