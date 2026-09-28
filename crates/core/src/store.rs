// <<<LLMCTX
// FILE: crates/core/src/store.rs
// ROLE: Filesystem-independent context store — one SQLite database per project at <root>/.llmctx/context.db
// EXPORTS: ContextStore, StoreSet, StoredContext, Lookup, Donor, StoreError, LegacyBody, project_root(), project_root_for_file(), move_context(), forget_context(), field_value(), set_field(), parse_legacy_body(), STORE_DIR, CONTEXT_VERSION
// IMPORTS: crates/core/src/config.rs, crates/core/src/fsutil.rs
// USED BY: crates/core/src/process.rs, crates/core/src/pack.rs, crates/core/src/migrate.rs, crates/cli/src/main.rs, crates/cpctx/src/main.rs, crates/daemon/src/main.rs
// NOTES: Stores only the six per-file fields; the project header is rendered at pack time. Keys are root-relative, `/`-separated, in on-disk case
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
//!
//! Each row holds only the six per-file fields (`FILE:` … `NOTES:`). The
//! project-level header (PROJECT/TASK/CONVENTIONS) is deliberately *not*
//! stored: it is rendered from the current `llmcontext.yaml` whenever context
//! is packed, so editing the config takes effect everywhere immediately.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use thiserror::Error;

use crate::{config, fsutil::absolute};

/// Directory at the project root that holds everything llmctx stores.
pub const STORE_DIR: &str = ".llmctx";

const DB_FILENAME: &str = "context.db";

/// Written into `.llmctx/.gitignore` so the store ignores itself.
const GITIGNORE_BODY: &str =
    "# Created by llmctx. Keeps this directory out of version control.\n*\n";

/// Format version of stored field text. Rows stamped with any other version
/// read as missing, so a format change regenerates silently.
pub const CONTEXT_VERSION: u32 = 1;

/// Version of the table layout, kept in SQLite's `user_version` pragma.
///
/// 1: `body` held the whole rendered block (header, SOURCE, HASH, fields).
/// 2: `fields` holds only the six fields; `source` has its own column; nested
///    project stores are linked for cross-project renames.
const SCHEMA_VERSION: i64 = 2;

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
    /// SHA-256 of the source the context was generated from. Empty when
    /// unknown (imported from a pre-hash llmctx), which never matches.
    pub content_hash: String,
    /// Who wrote it: `llm` (an `<<<LLMCTX` block) or `ollama`.
    pub source: String,
    /// The six field lines, `FILE:` through `NOTES:`.
    pub fields: String,
}

/// Context for identical content found under another path, possibly in a
/// neighbouring (enclosing or nested) project's store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Donor {
    pub context: StoredContext,
    /// Root of the store that holds it.
    pub store_root: PathBuf,
}

/// Result of [`ContextStore::resolve`]. Resolving never writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Lookup {
    /// Context stored under this path was generated from this exact content.
    Current(StoredContext),
    /// Nothing current under this path, but identical content has context
    /// elsewhere: the file was renamed, moved or copied.
    Found(Donor),
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
    /// `root` with symlinks resolved and, on Windows and macOS, in on-disk
    /// case. Keys are computed against this so a path typed in the wrong
    /// case still maps to the one existing key.
    canonical_root: Option<PathBuf>,
    db_path: PathBuf,
    conn: Connection,
}

impl ContextStore {
    /// Open the store at `root`, creating `.llmctx/` and the database if
    /// they do not exist yet.
    pub fn open(root: &Path) -> Result<Self, StoreError> {
        let root = absolute(root);
        let dir = root.join(STORE_DIR);
        let created = prepare_store_dir(&dir)?;
        let store = Self::connect(root, dir.join(DB_FILENAME))?;
        if created {
            // A new store inside another project: let the enclosing store
            // know, so files moved between the two keep their context.
            store.register_with_enclosing();
        }
        Ok(store)
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
        let canonical_root = std::fs::canonicalize(&root).ok();
        let store = Self {
            root,
            canonical_root,
            db_path,
            conn,
        };
        store.init_schema()?;
        Ok(store)
    }

    fn user_version(&self) -> Result<i64, StoreError> {
        self.conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .map_err(self.err())
    }

    fn init_schema(&self) -> Result<(), StoreError> {
        let found = self.user_version()?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::NewerSchema {
                path: self.db_path.display().to_string(),
                found,
                expected: SCHEMA_VERSION,
            });
        }
        if found == SCHEMA_VERSION {
            return Ok(());
        }

        // IMMEDIATE takes the write lock up front, so two processes creating
        // or upgrading the same store at once simply queue on busy_timeout.
        let tx = Transaction::new_unchecked(&self.conn, TransactionBehavior::Immediate)
            .map_err(self.err())?;
        // Re-read under the lock: another process may have done the work.
        match self.user_version()? {
            0 => self.create_tables()?,
            1 => self.upgrade_from_v1()?,
            _ => {}
        }
        self.conn
            .execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION};"))
            .map_err(self.err())?;
        tx.commit().map_err(self.err())
    }

    fn create_tables(&self) -> Result<(), StoreError> {
        self.conn
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS context (
                     rel_path     TEXT PRIMARY KEY,
                     content_hash TEXT NOT NULL,
                     source       TEXT NOT NULL,
                     fields       TEXT NOT NULL,
                     version      INTEGER NOT NULL,
                     updated_at   INTEGER NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS context_by_hash ON context(content_hash);
                 CREATE TABLE IF NOT EXISTS linked_stores (rel_root TEXT PRIMARY KEY);",
            )
            .map_err(self.err())
    }

    /// v1 rows kept the whole rendered block in `body`; split it.
    fn upgrade_from_v1(&self) -> Result<(), StoreError> {
        self.conn
            .execute_batch(
                "ALTER TABLE context RENAME TO context_v1;
                 DROP INDEX IF EXISTS context_by_hash;",
            )
            .map_err(self.err())?;
        self.create_tables()?;
        let old: Vec<(String, String, String, i64, i64)> = {
            let mut stmt = self
                .conn
                .prepare("SELECT rel_path, content_hash, body, version, updated_at FROM context_v1")
                .map_err(self.err())?;
            let rows = stmt
                .query_map([], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
                })
                .map_err(self.err())?;
            rows.collect::<Result<_, _>>().map_err(self.err())?
        };
        for (rel_path, hash, body, version, updated_at) in old {
            let legacy = parse_legacy_body(&body);
            self.conn
                .execute(
                    "INSERT OR REPLACE INTO context VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                    params![
                        rel_path,
                        hash,
                        legacy.source,
                        legacy.fields,
                        version,
                        updated_at
                    ],
                )
                .map_err(self.err())?;
        }
        self.conn
            .execute_batch("DROP TABLE context_v1;")
            .map_err(self.err())
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
    ///
    /// When the file exists, the key uses its on-disk spelling (symlinks
    /// resolved; on Windows and macOS, the real letter case), so `SRC\Main.rs`
    /// and `src\main.rs` are one key there. Otherwise the path is used as
    /// written.
    pub fn rel_key(&self, file: &Path) -> Result<String, StoreError> {
        if let (Some(root), Ok(real)) = (&self.canonical_root, std::fs::canonicalize(file)) {
            if let Ok(rel) = real.strip_prefix(root) {
                return Ok(join_components(rel));
            }
        }
        let file = absolute(file);
        let rel = file
            .strip_prefix(&self.root)
            .map_err(|_| StoreError::OutsideRoot {
                path: file.display().to_string(),
                root: self.root.display().to_string(),
            })?;
        Ok(join_components(rel))
    }

    /// Context stored under `rel_path`, whatever content it was made from.
    pub fn get(&self, rel_path: &str) -> Result<Option<StoredContext>, StoreError> {
        self.conn
            .query_row(
                "SELECT rel_path, content_hash, source, fields FROM context
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
        if content_hash.is_empty() {
            return Ok(None);
        }
        self.conn
            .query_row(
                "SELECT rel_path, content_hash, source, fields FROM context
                 WHERE content_hash = ?1 AND rel_path <> ?2 AND version = ?3
                 ORDER BY updated_at DESC LIMIT 1",
                params![content_hash, excluding, CONTEXT_VERSION],
                row_to_context,
            )
            .optional()
            .map_err(self.err())
    }

    /// Store the six `fields` for `rel_path`, replacing whatever was there.
    pub fn put(
        &self,
        rel_path: &str,
        content_hash: &str,
        source: &str,
        fields: &str,
    ) -> Result<(), StoreError> {
        self.conn
            .execute(
                "INSERT INTO context (rel_path, content_hash, source, fields, version, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(rel_path) DO UPDATE SET
                     content_hash = excluded.content_hash,
                     source       = excluded.source,
                     fields       = excluded.fields,
                     version      = excluded.version,
                     updated_at   = excluded.updated_at",
                params![
                    rel_path,
                    content_hash,
                    source,
                    fields,
                    CONTEXT_VERSION,
                    now_secs()
                ],
            )
            .map(|_| ())
            .map_err(self.err())
    }

    /// Delete the entry for exactly `rel_path`. Returns whether one existed.
    pub fn remove(&self, rel_path: &str) -> Result<bool, StoreError> {
        self.conn
            .execute("DELETE FROM context WHERE rel_path = ?1", params![rel_path])
            .map(|n| n > 0)
            .map_err(self.err())
    }

    /// Delete the entry for `rel_path`, or every entry under it if it names
    /// a directory. Returns how many were removed.
    pub fn remove_path(&self, rel_path: &str) -> Result<usize, StoreError> {
        // substr() rather than LIKE: `_` and `%` are legal in file names.
        self.conn
            .execute(
                "DELETE FROM context WHERE rel_path = ?1
                 OR substr(rel_path, 1, length(?1) + 1) = ?1 || '/'",
                params![rel_path],
            )
            .map_err(self.err())
    }

    /// Re-key the entry for `from` (a file) or every entry under it (a
    /// directory) to `to`, pointing each `FILE:` field at the new path.
    /// Existing entries at the destination are replaced. Returns how many
    /// entries moved.
    pub fn rename_path(&self, from: &str, to: &str) -> Result<usize, StoreError> {
        let moved = self.entries_under(from)?;
        if moved.is_empty() {
            return Ok(0);
        }
        let tx = self.conn.unchecked_transaction().map_err(self.err())?;
        for ctx in &moved {
            let new_rel = format!("{to}{}", &ctx.rel_path[from.len()..]);
            self.remove(&ctx.rel_path)?;
            self.put(
                &new_rel,
                &ctx.content_hash,
                &ctx.source,
                &set_field(&ctx.fields, "FILE", &new_rel),
            )?;
        }
        tx.commit().map_err(self.err())?;
        Ok(moved.len())
    }

    /// The entry for `rel_path` and every entry below it (if a directory).
    pub fn entries_under(&self, rel_path: &str) -> Result<Vec<StoredContext>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT rel_path, content_hash, source, fields FROM context
                 WHERE rel_path = ?1 OR substr(rel_path, 1, length(?1) + 1) = ?1 || '/'",
            )
            .map_err(self.err())?;
        let rows = stmt
            .query_map(params![rel_path], row_to_context)
            .map_err(self.err())?;
        rows.collect::<Result<_, _>>().map_err(self.err())
    }

    /// Every entry, sorted by path.
    pub fn all(&self) -> Result<Vec<StoredContext>, StoreError> {
        let mut stmt = self
            .conn
            .prepare(
                "SELECT rel_path, content_hash, source, fields FROM context
                 WHERE version = ?1 ORDER BY rel_path",
            )
            .map_err(self.err())?;
        let rows = stmt
            .query_map(params![CONTEXT_VERSION], row_to_context)
            .map_err(self.err())?;
        rows.collect::<Result<Vec<_>, _>>().map_err(self.err())
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

    /// Files whose `IMPORTS:` field names `rel_path`. This is what `USED BY`
    /// really is, computed across the project instead of guessed from one
    /// file.
    pub fn importers_of(&self, rel_path: &str) -> Result<Vec<String>, StoreError> {
        Ok(self
            .all()?
            .into_iter()
            .filter(|c| c.rel_path != rel_path)
            .filter(|c| {
                field_value(&c.fields, "IMPORTS")
                    .map(|v| split_paths(v).iter().any(|p| p == rel_path))
                    .unwrap_or(false)
            })
            .map(|c| c.rel_path)
            .collect())
    }

    /// Resolve the context for `rel_path` whose current content hashes to
    /// `content_hash`. Never writes; see [`adopt`](Self::adopt).
    ///
    /// When the path has nothing current, a match on the content hash is
    /// looked for in this store and then in the stores of the enclosing and
    /// nested projects: that is how a renamed, moved or copied file keeps its
    /// context without regenerating it.
    pub fn resolve(&self, rel_path: &str, content_hash: &str) -> Result<Lookup, StoreError> {
        let existing = self.get(rel_path)?;
        if let Some(ctx) = &existing {
            if !ctx.content_hash.is_empty() && ctx.content_hash == content_hash {
                return Ok(Lookup::Current(ctx.clone()));
            }
        }

        if let Some(context) = self.find_by_hash(content_hash, rel_path)? {
            return Ok(Lookup::Found(Donor {
                context,
                store_root: self.root.clone(),
            }));
        }
        if let Some(donor) = self.find_in_neighbours(content_hash)? {
            return Ok(Lookup::Found(donor));
        }

        Ok(match existing {
            Some(ctx) => Lookup::Stale(ctx),
            None => Lookup::Missing,
        })
    }

    /// Store `donor`'s context under `rel_path` (with `FILE:` retargeted).
    /// If the donor's file no longer exists, this was a rename or move, and
    /// the donor's entry is removed from whichever store held it.
    pub fn adopt(
        &self,
        rel_path: &str,
        content_hash: &str,
        donor: &Donor,
    ) -> Result<StoredContext, StoreError> {
        let fields = set_field(&donor.context.fields, "FILE", rel_path);
        let donor_gone = !donor.store_root.join(&donor.context.rel_path).exists();
        let same_store = absolute(&donor.store_root) == self.root;

        let tx = self.conn.unchecked_transaction().map_err(self.err())?;
        self.put(rel_path, content_hash, &donor.context.source, &fields)?;
        if donor_gone && same_store {
            self.remove(&donor.context.rel_path)?;
        }
        tx.commit().map_err(self.err())?;

        if donor_gone && !same_store {
            if let Some(other) = ContextStore::open_existing(&donor.store_root)? {
                other.remove(&donor.context.rel_path)?;
            }
        }
        Ok(StoredContext {
            rel_path: rel_path.to_string(),
            content_hash: content_hash.to_string(),
            source: donor.context.source.clone(),
            fields,
        })
    }

    /// Delete entries whose file no longer exists under the root — all of
    /// them, or only those under the directory `under` (root-relative).
    /// Also forgets linked stores that have disappeared. Returns the keys
    /// removed.
    pub fn prune_missing(&self, under: Option<&str>) -> Result<Vec<String>, StoreError> {
        let gone: Vec<String> = self
            .rel_paths()?
            .into_iter()
            .filter(|rel| match under {
                Some(prefix) if !prefix.is_empty() => {
                    rel == prefix || rel.starts_with(&format!("{prefix}/"))
                }
                _ => true,
            })
            .filter(|rel| !self.root.join(rel).is_file())
            .collect();
        let tx = self.conn.unchecked_transaction().map_err(self.err())?;
        for rel in &gone {
            self.remove(rel)?;
        }
        for linked in self.linked_roots()? {
            if !linked.join(STORE_DIR).join(DB_FILENAME).is_file() {
                if let Ok(rel) = linked.strip_prefix(&self.root) {
                    self.conn
                        .execute(
                            "DELETE FROM linked_stores WHERE rel_root = ?1",
                            params![join_components(rel)],
                        )
                        .map_err(self.err())?;
                }
            }
        }
        tx.commit().map_err(self.err())?;
        Ok(gone)
    }

    // ── neighbouring stores ──────────────────────────────────────────────

    /// Roots of nested project stores registered with this one.
    fn linked_roots(&self) -> Result<Vec<PathBuf>, StoreError> {
        let mut stmt = self
            .conn
            .prepare("SELECT rel_root FROM linked_stores")
            .map_err(self.err())?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .map_err(self.err())?;
        let rels = rows.collect::<Result<Vec<_>, _>>().map_err(self.err())?;
        Ok(rels.into_iter().map(|r| self.root.join(r)).collect())
    }

    /// The enclosing project's store, if there is one.
    fn enclosing(&self) -> Option<ContextStore> {
        let parent_root = project_root(self.root.parent()?)?;
        ContextStore::open_existing(&parent_root).ok().flatten()
    }

    /// Record this store in the enclosing project's store (best effort).
    fn register_with_enclosing(&self) {
        if let Some(outer) = self.enclosing() {
            outer.link(&self.root);
        }
    }

    fn link(&self, nested_root: &Path) {
        if let Ok(rel) = nested_root.strip_prefix(&self.root) {
            let _ = self.conn.execute(
                "INSERT OR IGNORE INTO linked_stores (rel_root) VALUES (?1)",
                params![join_components(rel)],
            );
        }
    }

    fn find_in_neighbours(&self, content_hash: &str) -> Result<Option<Donor>, StoreError> {
        if content_hash.is_empty() {
            return Ok(None);
        }
        if let Some(outer) = self.enclosing() {
            // Linking here too covers stores created before their enclosing
            // project had one.
            outer.link(&self.root);
            if let Some(context) = outer.find_by_hash(content_hash, "")? {
                return Ok(Some(Donor {
                    context,
                    store_root: outer.root.clone(),
                }));
            }
        }
        for linked in self.linked_roots()? {
            if let Some(inner) = ContextStore::open_existing(&linked)? {
                if let Some(context) = inner.find_by_hash(content_hash, "")? {
                    return Ok(Some(Donor {
                        context,
                        store_root: inner.root.clone(),
                    }));
                }
            }
        }
        Ok(None)
    }

    fn err(&self) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
        sqlite_err(&self.db_path)
    }
}

// ── Path-level operations (VS Code rename/delete events) ─────────────────────

/// Move the context of a renamed or moved file or directory from `from` to
/// `to` — within one project, or between projects. Context for something
/// moved out of every project is dropped. Returns how many entries moved.
pub fn move_context(from: &Path, to: &Path) -> Result<usize, StoreError> {
    let Some(from_root) = project_root_for_file(from) else {
        return Ok(0);
    };
    let Some(src) = ContextStore::open_existing(&from_root)? else {
        return Ok(0);
    };
    let from_rel = src.rel_key(from)?;
    let Some(to_root) = project_root_for_file(to) else {
        return src.remove_path(&from_rel);
    };
    if absolute(&to_root) == src.root {
        let to_rel = src.rel_key(to)?;
        return src.rename_path(&from_rel, &to_rel);
    }

    let entries = src.entries_under(&from_rel)?;
    if entries.is_empty() {
        return Ok(0);
    }
    let dst = ContextStore::open(&to_root)?;
    let to_rel = dst.rel_key(to)?;
    for ctx in &entries {
        let new_rel = format!("{to_rel}{}", &ctx.rel_path[from_rel.len()..]);
        dst.put(
            &new_rel,
            &ctx.content_hash,
            &ctx.source,
            &set_field(&ctx.fields, "FILE", &new_rel),
        )?;
    }
    src.remove_path(&from_rel)?;
    Ok(entries.len())
}

/// Drop the context of a deleted file or directory. Returns how many
/// entries were removed.
pub fn forget_context(path: &Path) -> Result<usize, StoreError> {
    let Some(root) = project_root_for_file(path) else {
        return Ok(0);
    };
    let Some(store) = ContextStore::open_existing(&root)? else {
        return Ok(0);
    };
    let rel = store.rel_key(path)?;
    store.remove_path(&rel)
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

// ── Field helpers ────────────────────────────────────────────────────────────

/// Value of the `NAME:` line in a six-field text, if present.
pub fn field_value<'a>(fields: &'a str, name: &str) -> Option<&'a str> {
    let prefix = format!("{name}:");
    fields
        .lines()
        .find_map(|l| l.trim_start().strip_prefix(&prefix))
        .map(str::trim)
}

/// Replace (or prepend) the `NAME:` line in a six-field text.
pub fn set_field(fields: &str, name: &str, value: &str) -> String {
    let prefix = format!("{name}:");
    let mut replaced = false;
    let mut out: Vec<String> = fields
        .lines()
        .map(|line| {
            if !replaced && line.trim_start().starts_with(&prefix) {
                replaced = true;
                format!("{prefix} {value}")
            } else {
                line.to_string()
            }
        })
        .collect();
    if !replaced {
        out.insert(0, format!("{prefix} {value}"));
    }
    out.join("\n")
}

/// Normalise a comma-separated path list (`IMPORTS:`) to store keys.
fn split_paths(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(|p| {
            p.trim()
                .trim_matches(|c| c == '`' || c == '"' || c == '\'')
                .replace('\\', "/")
                .trim_start_matches("./")
                .to_string()
        })
        .filter(|p| !p.is_empty() && p != "NONE" && p != "UNKNOWN")
        .collect()
}

/// A context body in the llmctx 0.1 format (NTFS streams, and store schema
/// v1): project header, `SOURCE:`, `HASH:`, a blank line, then the fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LegacyBody {
    pub source: String,
    /// Empty when the body predates the `HASH:` field.
    pub hash: String,
    pub fields: String,
}

/// Split a legacy body into its parts; the header is discarded because it is
/// now rendered from the current config.
pub fn parse_legacy_body(body: &str) -> LegacyBody {
    let source = field_value(body, "SOURCE").unwrap_or("llm").to_string();
    let hash = field_value(body, "HASH").unwrap_or("").to_string();
    let lines: Vec<&str> = body.lines().collect();
    let fields = match lines
        .iter()
        .position(|l| l.trim_start().starts_with("FILE:"))
    {
        Some(start) => lines[start..].join("\n"),
        None => lines
            .iter()
            .filter(|l| {
                let t = l.trim_start();
                ![
                    "LLMCTX_VERSION:",
                    "PROJECT:",
                    "TASK:",
                    "CONVENTIONS:",
                    "SOURCE:",
                    "HASH:",
                ]
                .iter()
                .any(|p| t.starts_with(p))
            })
            .copied()
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
    };
    LegacyBody {
        source,
        hash,
        fields,
    }
}

// ── Private helpers ──────────────────────────────────────────────────────────

/// Create `.llmctx/` if needed and make sure it still ignores itself.
/// Returns whether the directory was created.
fn prepare_store_dir(dir: &Path) -> Result<bool, StoreError> {
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
    Ok(created)
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

fn join_components(rel: &Path) -> String {
    let parts: Vec<_> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect();
    parts.join("/")
}

fn row_to_context(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredContext> {
    Ok(StoredContext {
        rel_path: row.get(0)?,
        content_hash: row.get(1)?,
        source: row.get(2)?,
        fields: row.get(3)?,
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

    fn fields(file: &str) -> String {
        format!(
            "FILE: {file}\nROLE: r\nEXPORTS: NONE\nIMPORTS: NONE\nUSED BY: UNKNOWN\nNOTES: NONE"
        )
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
        // A file that does not exist yet still gets a key.
        assert_eq!(
            store.rel_key(&dir.path().join("new/file.rs")).unwrap(),
            "new/file.rs"
        );
    }

    /// On case-insensitive file systems a path typed in the wrong case must
    /// map to the existing key, not create a second one.
    #[cfg(any(target_os = "windows", target_os = "macos"))]
    #[test]
    fn keys_use_on_disk_case() {
        let (dir, _) = project();
        fs::write(dir.path().join("src").join("Widget.rs"), "x").unwrap();
        let store = ContextStore::open(dir.path()).unwrap();
        let typed = dir.path().join("SRC").join("widget.RS");
        assert_eq!(store.rel_key(&typed).unwrap(), "src/Widget.rs");
    }

    #[test]
    fn put_get_remove_round_trip() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        assert_eq!(store.get("a.rs").unwrap(), None);
        store.put("a.rs", "h1", "llm", "one").unwrap();
        store.put("a.rs", "h2", "ollama", "two").unwrap();
        let got = store.get("a.rs").unwrap().unwrap();
        assert_eq!(got.content_hash, "h2");
        assert_eq!(got.source, "ollama");
        assert_eq!(got.fields, "two");
        assert!(store.remove("a.rs").unwrap());
        assert!(!store.remove("a.rs").unwrap());
        assert_eq!(store.get("a.rs").unwrap(), None);
    }

    #[test]
    fn data_persists_across_connections() {
        let (dir, _) = project();
        ContextStore::open(dir.path())
            .unwrap()
            .put("a.rs", "h", "llm", "b")
            .unwrap();
        let reopened = ContextStore::open_existing(dir.path()).unwrap().unwrap();
        assert_eq!(reopened.get("a.rs").unwrap().unwrap().fields, "b");
    }

    #[test]
    fn resolve_current_stale_missing() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        assert_eq!(store.resolve("a.rs", "h1").unwrap(), Lookup::Missing);
        store.put("a.rs", "h1", "llm", &fields("a.rs")).unwrap();
        assert!(matches!(
            store.resolve("a.rs", "h1").unwrap(),
            Lookup::Current(_)
        ));
        assert!(matches!(
            store.resolve("a.rs", "h2").unwrap(),
            Lookup::Stale(_)
        ));
    }

    #[test]
    fn empty_hash_never_matches() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        store.put("a.rs", "", "llm", &fields("a.rs")).unwrap();
        assert!(matches!(
            store.resolve("a.rs", "").unwrap(),
            Lookup::Stale(_)
        ));
        assert_eq!(store.resolve("b.rs", "").unwrap(), Lookup::Missing);
    }

    #[test]
    fn resolve_is_read_only_and_adopt_moves_renamed_file() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        // old.rs no longer exists on disk: this is a rename.
        store.put("old.rs", "h", "llm", &fields("old.rs")).unwrap();

        let Lookup::Found(donor) = store.resolve("new.rs", "h").unwrap() else {
            panic!("expected Found");
        };
        assert_eq!(donor.context.rel_path, "old.rs");
        // Resolving wrote nothing.
        assert_eq!(store.get("new.rs").unwrap(), None);

        let adopted = store.adopt("new.rs", "h", &donor).unwrap();
        assert_eq!(field_value(&adopted.fields, "FILE"), Some("new.rs"));
        assert_eq!(store.get("old.rs").unwrap(), None);
        assert!(matches!(
            store.resolve("new.rs", "h").unwrap(),
            Lookup::Current(_)
        ));
    }

    #[test]
    fn adopt_keeps_donor_of_copied_file() {
        let (_dir, file) = project();
        let (store, rel) = ContextStore::open_for_file(&file).unwrap();
        store.put(&rel, "h", "llm", &fields(&rel)).unwrap();

        let Lookup::Found(donor) = store.resolve("src/copy.rs", "h").unwrap() else {
            panic!("expected Found");
        };
        store.adopt("src/copy.rs", "h", &donor).unwrap();
        // The donor file still exists, so this is a copy — both keep context.
        assert!(store.get(&rel).unwrap().is_some());
        assert_eq!(
            field_value(&store.get("src/copy.rs").unwrap().unwrap().fields, "FILE"),
            Some("src/copy.rs")
        );
    }

    #[test]
    fn rename_and_remove_whole_directories() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        store.put("a/x.rs", "h1", "llm", &fields("a/x.rs")).unwrap();
        store
            .put("a/b/y.rs", "h2", "llm", &fields("a/b/y.rs"))
            .unwrap();
        store.put("ab.rs", "h3", "llm", &fields("ab.rs")).unwrap();

        assert_eq!(store.rename_path("a", "c").unwrap(), 2);
        assert_eq!(
            store.rel_paths().unwrap(),
            vec!["ab.rs", "c/b/y.rs", "c/x.rs"]
        );
        assert_eq!(
            field_value(&store.get("c/b/y.rs").unwrap().unwrap().fields, "FILE"),
            Some("c/b/y.rs")
        );

        assert_eq!(store.rename_path("ab.rs", "z.rs").unwrap(), 1);
        assert_eq!(store.remove_path("c").unwrap(), 2);
        assert_eq!(store.rel_paths().unwrap(), vec!["z.rs"]);
    }

    #[test]
    fn move_and_forget_follow_files_across_projects() {
        let (outer, _) = project();
        let other = TempDir::new().unwrap();
        fs::write(other.path().join(config::CONFIG_FILENAME), "project: O\n").unwrap();
        let store = ContextStore::open(outer.path()).unwrap();
        store
            .put("dir/a.rs", "h", "llm", &fields("dir/a.rs"))
            .unwrap();
        store
            .put("dir/b.rs", "h2", "llm", &fields("dir/b.rs"))
            .unwrap();

        // Rename inside the project (paths as an editor reports them).
        let moved = move_context(&outer.path().join("dir"), &outer.path().join("lib")).unwrap();
        assert_eq!(moved, 2);
        assert_eq!(store.rel_paths().unwrap(), vec!["lib/a.rs", "lib/b.rs"]);

        // Move one file into another project.
        fs::create_dir_all(other.path().join("x")).unwrap();
        let n = move_context(&outer.path().join("lib/a.rs"), &other.path().join("x/a.rs")).unwrap();
        assert_eq!(n, 1);
        let dst = ContextStore::open_existing(other.path()).unwrap().unwrap();
        assert_eq!(
            field_value(&dst.get("x/a.rs").unwrap().unwrap().fields, "FILE"),
            Some("x/a.rs")
        );
        assert_eq!(store.rel_paths().unwrap(), vec!["lib/b.rs"]);

        assert_eq!(forget_context(&outer.path().join("lib")).unwrap(), 1);
        assert!(store.rel_paths().unwrap().is_empty());
    }

    #[test]
    fn rows_from_other_versions_read_as_missing() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        store
            .conn
            .execute(
                "INSERT INTO context VALUES ('a.rs', 'h', 'llm', 'f', ?1, 0)",
                params![CONTEXT_VERSION + 1],
            )
            .unwrap();
        assert_eq!(store.get("a.rs").unwrap(), None);
        assert_eq!(store.resolve("b.rs", "h").unwrap(), Lookup::Missing);
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
    fn v1_store_is_upgraded_in_place() {
        let (dir, _) = project();
        let store_dir = dir.path().join(STORE_DIR);
        fs::create_dir_all(&store_dir).unwrap();
        {
            let conn = Connection::open(store_dir.join(DB_FILENAME)).unwrap();
            conn.execute_batch(
                "CREATE TABLE context (rel_path TEXT PRIMARY KEY, content_hash TEXT NOT NULL,
                     body TEXT NOT NULL, version INTEGER NOT NULL, updated_at INTEGER NOT NULL);
                 CREATE INDEX context_by_hash ON context(content_hash);
                 INSERT INTO context VALUES ('a.rs', 'h',
                     'PROJECT: P | \nTASK: t\nCONVENTIONS: \nSOURCE: ollama\nHASH: h\n\nFILE: a.rs\nROLE: r', 1, 5);
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }
        let store = ContextStore::open(dir.path()).unwrap();
        let got = store.get("a.rs").unwrap().unwrap();
        assert_eq!(got.source, "ollama");
        assert_eq!(got.content_hash, "h");
        assert_eq!(got.fields, "FILE: a.rs\nROLE: r");
        assert!(matches!(
            store.resolve("b.rs", "h").unwrap(),
            Lookup::Found(_)
        ));
    }

    #[test]
    fn prune_removes_entries_for_deleted_files_optionally_under_a_dir() {
        let (_dir, file) = project();
        let (store, rel) = ContextStore::open_for_file(&file).unwrap();
        store.put(&rel, "h", "llm", "b").unwrap();
        store.put("src/deleted.rs", "h2", "llm", "b").unwrap();
        store.put("other/deleted.rs", "h3", "llm", "b").unwrap();
        assert_eq!(
            store.prune_missing(Some("src")).unwrap(),
            vec!["src/deleted.rs"]
        );
        assert_eq!(store.prune_missing(None).unwrap(), vec!["other/deleted.rs"]);
        assert_eq!(store.rel_paths().unwrap(), vec![rel]);
    }

    #[test]
    fn importers_are_computed_from_imports_fields() {
        let (dir, _) = project();
        let store = ContextStore::open(dir.path()).unwrap();
        store
            .put(
                "a.rs",
                "1",
                "llm",
                "FILE: a.rs\nIMPORTS: ./lib/util.rs, `b.rs`",
            )
            .unwrap();
        store
            .put("c.rs", "2", "llm", "FILE: c.rs\nIMPORTS: lib\\util.rs")
            .unwrap();
        store
            .put("d.rs", "3", "llm", "FILE: d.rs\nIMPORTS: NONE")
            .unwrap();
        assert_eq!(
            store.importers_of("lib/util.rs").unwrap(),
            vec!["a.rs", "c.rs"]
        );
        assert!(store.importers_of("d.rs").unwrap().is_empty());
    }

    #[test]
    fn file_moved_between_nested_projects_keeps_context() {
        let (outer, _) = project();
        let inner_root = outer.path().join("tools");
        fs::create_dir_all(&inner_root).unwrap();
        fs::write(inner_root.join(config::CONFIG_FILENAME), "project: Inner\n").unwrap();

        let outer_store = ContextStore::open(outer.path()).unwrap();
        let inner_store = ContextStore::open(&inner_root).unwrap();

        // Outer → inner: the file left the outer project.
        outer_store
            .put("gone.rs", "h1", "llm", &fields("gone.rs"))
            .unwrap();
        let Lookup::Found(donor) = inner_store.resolve("moved.rs", "h1").unwrap() else {
            panic!("expected context from the enclosing project");
        };
        inner_store.adopt("moved.rs", "h1", &donor).unwrap();
        assert_eq!(outer_store.get("gone.rs").unwrap(), None);

        // Inner → outer: found through the registered nested store.
        inner_store
            .put("old.rs", "h2", "llm", &fields("old.rs"))
            .unwrap();
        let Lookup::Found(donor) = outer_store.resolve("back.rs", "h2").unwrap() else {
            panic!("expected context from the nested project");
        };
        outer_store.adopt("back.rs", "h2", &donor).unwrap();
        assert_eq!(inner_store.get("old.rs").unwrap(), None);
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
        let (_dir, file) = project();
        let mut set = StoreSet::new();
        assert!(set.existing_for_file(&file).unwrap().is_none());
        {
            let (store, rel) = set.for_file(&file).unwrap();
            store.put(&rel, "h", "llm", "b").unwrap();
        }
        let (store, rel) = set.existing_for_file(&file).unwrap().unwrap();
        assert_eq!(store.get(&rel).unwrap().unwrap().fields, "b");
        assert_eq!(set.stores.len(), 1);
    }

    #[test]
    fn field_helpers() {
        let f = "FILE: a.rs\nROLE: mentions FILE: a.rs\nIMPORTS: x";
        assert_eq!(field_value(f, "IMPORTS"), Some("x"));
        assert_eq!(field_value(f, "NOTES"), None);
        assert_eq!(
            set_field(f, "FILE", "b/c.rs"),
            "FILE: b/c.rs\nROLE: mentions FILE: a.rs\nIMPORTS: x"
        );
        assert_eq!(set_field("ROLE: r", "FILE", "x"), "FILE: x\nROLE: r");
    }

    #[test]
    fn legacy_bodies_are_split() {
        let body = "LLMCTX_VERSION: 1\nPROJECT: P | S\nTASK: t\nCONVENTIONS: c\nSOURCE: ollama\nHASH: abc\n\nFILE: x.rs\nROLE: r";
        let parsed = parse_legacy_body(body);
        assert_eq!(parsed.source, "ollama");
        assert_eq!(parsed.hash, "abc");
        assert_eq!(parsed.fields, "FILE: x.rs\nROLE: r");

        let pre_hash = parse_legacy_body("PROJECT: p\nSOURCE: llm\n\nFILE: y.rs");
        assert_eq!(pre_hash.hash, "");
        assert_eq!(pre_hash.fields, "FILE: y.rs");
    }
}
