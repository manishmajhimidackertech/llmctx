// <<<LLMCTX
// FILE: crates/core/src/migrate.rs
// ROLE: Import context from llmctx 0.1 NTFS Alternate Data Streams into the project store
// EXPORTS: import_stream(), ImportOutcome, MigrateError
// IMPORTS: crates/core/src/ads.rs, crates/core/src/store.rs
// USED BY: crates/cli/src/main.rs (llmctx migrate)
// NOTES: Idempotent; never overwrites context already in the store; only removes a stream once its context is stored
// LLMCTX>>>

use std::path::Path;

use thiserror::Error;

use crate::{
    ads::{self, AdsError},
    store::{self, StoreError, StoreSet},
};

#[derive(Debug, Error)]
pub enum MigrateError {
    #[error("could not read the NTFS stream: {0}")]
    Ads(#[from] AdsError),
    #[error("{0}")]
    Store(#[from] StoreError),
}

/// What happened to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    /// The file has no (usable) llmctx stream.
    NoStream,
    /// The stream's context was copied into the store.
    Imported,
    /// The store already had context for this file; it was kept.
    AlreadyStored,
}

/// Copy `path`'s llmctx stream into its project's store. With
/// `remove_stream`, the stream is deleted once the store holds context for
/// the file (whether just imported or already there).
///
/// Bodies from before the `HASH:` field get an empty hash, so `pack` serves
/// them but the next `llmctx index` regenerates them.
pub fn import_stream(
    path: &Path,
    stores: &mut StoreSet,
    remove_stream: bool,
) -> Result<ImportOutcome, MigrateError> {
    if !matches!(ads::ads_exists(path), Ok(true)) {
        return Ok(ImportOutcome::NoStream);
    }
    let body = match ads::read_ads(path) {
        Ok(body) => body,
        // An empty or pre-versioning stream has nothing worth keeping.
        Err(AdsError::Empty) | Err(AdsError::VersionMismatch { .. }) => {
            return Ok(ImportOutcome::NoStream)
        }
        Err(e) => return Err(e.into()),
    };

    let (store, rel) = stores.for_file(path)?;
    let outcome = if store.get(&rel)?.is_some() {
        ImportOutcome::AlreadyStored
    } else {
        let legacy = store::parse_legacy_body(&body);
        let fields = store::set_field(&legacy.fields, "FILE", &rel);
        store.put(&rel, &legacy.hash, &legacy.source, &fields)?;
        ImportOutcome::Imported
    };

    if remove_stream {
        ads::clear_ads(path)?;
    }
    Ok(outcome)
}

// These exercise real NTFS streams, so they only run on Windows (CI has a
// Windows runner for exactly this).
#[cfg(all(test, target_os = "windows"))]
mod tests {
    use super::*;
    use crate::{config, process::content_hash};
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn stream_is_imported_once_and_removed_on_request() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join(config::CONFIG_FILENAME), "project: P\n").unwrap();
        let file = dir.path().join("a.py");
        fs::write(&file, "x = 1\n").unwrap();
        let hash = content_hash("x = 1\n");
        let body = format!(
            "PROJECT: P | \nTASK: \nCONVENTIONS: \nSOURCE: ollama\nHASH: {hash}\n\nFILE: old/a.py\nROLE: r"
        );
        ads::write_ads(&file, &body).unwrap();

        let mut stores = StoreSet::new();
        assert_eq!(
            import_stream(&file, &mut stores, false).unwrap(),
            ImportOutcome::Imported
        );
        assert_eq!(
            import_stream(&file, &mut stores, true).unwrap(),
            ImportOutcome::AlreadyStored
        );
        assert_eq!(
            import_stream(&file, &mut stores, false).unwrap(),
            ImportOutcome::NoStream
        );

        let (store, rel) = stores.for_file(&file).unwrap();
        let got = store.get(&rel).unwrap().unwrap();
        assert_eq!(got.content_hash, hash);
        assert_eq!(got.source, "ollama");
        assert_eq!(got.fields, "FILE: a.py\nROLE: r");
    }
}
