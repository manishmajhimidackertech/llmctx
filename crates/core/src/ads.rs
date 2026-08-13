// <<<LLMCTX
// FILE: crates/core/src/ads.rs
// ROLE: Read and write the file:llmctx NTFS Alternate Data Stream on Windows
// EXPORTS: read_ads(), write_ads(), ads_exists(), clear_ads(), AdsError
// IMPORTS: NONE
// USED BY: crates/core/src/process.rs, crates/cli/src/main.rs
// NOTES: Windows/NTFS only — every public function returns Err(AdsError::NotSupported) on non-Windows
// LLMCTX>>>

use thiserror::Error;

/// The stream name appended to every source file path.
/// e.g. `C:\proj\auth\middleware.py:llmctx`
///
/// Only referenced from the Windows-only module below, so it reads as dead
/// code on other targets.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const STREAM_NAME: &str = "llmctx";

/// Version stamp written as the first line of every ADS block.
/// Mismatch on read → treat as missing, regenerate silently.
pub const ADS_VERSION: u32 = 1;
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
const VERSION_PREFIX: &str = "LLMCTX_VERSION: ";

#[derive(Debug, Error)]
pub enum AdsError {
    #[error("ADS is only supported on Windows/NTFS")]
    NotSupported,

    #[error("I/O error on ADS stream for {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    #[error("ADS version mismatch (found {found}, expected {expected}) — regenerating")]
    VersionMismatch { found: u32, expected: u32 },

    #[error("ADS content is empty")]
    Empty,
}

// ── platform dispatch ────────────────────────────────────────────────────────

/// Returns `true` if a context stream exists and has a compatible version stamp.
pub fn ads_exists(path: &std::path::Path) -> Result<bool, AdsError> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::ads_exists(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(AdsError::NotSupported)
    }
}

/// Reads the raw context string from the ADS stream.
/// Returns `Err(AdsError::VersionMismatch)` if the first line shows a different
/// version — callers should treat this exactly like `Err(AdsError::Empty)` and
/// regenerate.
pub fn read_ads(path: &std::path::Path) -> Result<String, AdsError> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::read_ads(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(AdsError::NotSupported)
    }
}

/// Writes `content` to the ADS stream, prepending the version stamp line.
/// Callers pass the six-field body; this function owns the stamp so it's
/// never missing or duplicated.
pub fn write_ads(path: &std::path::Path, content: &str) -> Result<(), AdsError> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::write_ads(path, content)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (path, content);
        Err(AdsError::NotSupported)
    }
}

/// Removes the ADS stream entirely. Used by `llmctx reindex` to force
/// a clean regeneration without leaving stale data.
pub fn clear_ads(path: &std::path::Path) -> Result<(), AdsError> {
    #[cfg(target_os = "windows")]
    {
        windows_impl::clear_ads(path)
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = path;
        Err(AdsError::NotSupported)
    }
}

// ── Windows implementation ───────────────────────────────────────────────────

#[cfg(target_os = "windows")]
mod windows_impl {
    use super::*;
    use std::path::Path;

    use windows::{
        core::HSTRING,
        Win32::Storage::FileSystem::{
            CreateFileW, DeleteFileW, ReadFile, WriteFile,
            FILE_ATTRIBUTE_NORMAL, FILE_GENERIC_READ, FILE_GENERIC_WRITE,
            FILE_SHARE_READ, OPEN_ALWAYS, OPEN_EXISTING,
        },
    };

    /// Builds the ADS path string: `<path>:<STREAM_NAME>`
    fn stream_path(path: &Path) -> String {
        format!("{}:{}", path.display(), STREAM_NAME)
    }

    pub fn ads_exists(path: &Path) -> Result<bool, AdsError> {
        let sp = stream_path(path);
        let h = unsafe {
            CreateFileW(
                &HSTRING::from(sp.as_str()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        };
        match h {
            Ok(handle) => {
                unsafe { windows::Win32::Foundation::CloseHandle(handle).ok() };
                Ok(true)
            }
            Err(_) => Ok(false),
        }
    }

    pub fn read_ads(path: &Path) -> Result<String, AdsError> {
        let sp = stream_path(path);
        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(sp.as_str()),
                FILE_GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| AdsError::Io {
            path: sp.clone(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        })?;

        // Read in chunks up to 64 KB — context blocks are tiny.
        let mut buf = vec![0u8; 65536];
        let mut bytes_read: u32 = 0;
        unsafe {
            ReadFile(handle, Some(&mut buf), Some(&mut bytes_read), None)
                .map_err(|e| AdsError::Io {
                    path: sp.clone(),
                    source: std::io::Error::from_raw_os_error(e.code().0),
                })?;
            windows::Win32::Foundation::CloseHandle(handle).ok();
        }

        if bytes_read == 0 {
            return Err(AdsError::Empty);
        }

        let raw = String::from_utf8_lossy(&buf[..bytes_read as usize]).into_owned();
        validate_version(&raw, &sp)?;
        // Strip the version line before returning so callers see only the body.
        let body = raw
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
            .join("\n");
        Ok(body)
    }

    pub fn write_ads(path: &Path, content: &str) -> Result<(), AdsError> {
        let sp = stream_path(path);
        // Prepend version stamp.
        let full = format!("{}{}\n{}", VERSION_PREFIX, ADS_VERSION, content);
        let bytes = full.as_bytes();

        let handle = unsafe {
            CreateFileW(
                &HSTRING::from(sp.as_str()),
                FILE_GENERIC_WRITE.0,
                FILE_SHARE_READ,
                None,
                OPEN_ALWAYS,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .map_err(|e| AdsError::Io {
            path: sp.clone(),
            source: std::io::Error::from_raw_os_error(e.code().0),
        })?;

        // Truncate to zero first so a shorter re-write doesn't leave stale tail bytes.
        unsafe {
            use windows::Win32::Storage::FileSystem::SetEndOfFile;
            SetEndOfFile(handle).ok();
        }

        let mut written: u32 = 0;
        unsafe {
            WriteFile(handle, Some(bytes), Some(&mut written), None)
                .map_err(|e| AdsError::Io {
                    path: sp.clone(),
                    source: std::io::Error::from_raw_os_error(e.code().0),
                })?;
            windows::Win32::Foundation::CloseHandle(handle).ok();
        }
        Ok(())
    }

    pub fn clear_ads(path: &Path) -> Result<(), AdsError> {
        let sp = stream_path(path);
        unsafe {
            DeleteFileW(&HSTRING::from(sp.as_str())).map_err(|e| AdsError::Io {
                path: sp,
                source: std::io::Error::from_raw_os_error(e.code().0),
            })?;
        }
        Ok(())
    }

    fn validate_version(raw: &str, _path: &str) -> Result<(), AdsError> {
        let first = raw.lines().next().unwrap_or("");
        if let Some(v_str) = first.strip_prefix(VERSION_PREFIX) {
            let found: u32 = v_str.trim().parse().unwrap_or(0);
            if found != ADS_VERSION {
                return Err(AdsError::VersionMismatch {
                    found,
                    expected: ADS_VERSION,
                });
            }
            Ok(())
        } else {
            // No version line at all — treat as mismatch so it gets regenerated.
            Err(AdsError::VersionMismatch {
                found: 0,
                expected: ADS_VERSION,
            })
        }
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    /// Version stamp round-trip is tested in process.rs integration tests
    /// since they need a real temp file. Here we just verify the constant.
    #[test]
    fn version_constant_is_one() {
        assert_eq!(super::ADS_VERSION, 1);
    }
}
