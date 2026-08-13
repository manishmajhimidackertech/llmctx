// <<<LLMCTX
// FILE: crates/core/src/extract.rs
// ROLE: Detect, validate, and strip <<<LLMCTX comment blocks from LLM-generated source files
// EXPORTS: ExtractedContext, ExtractError, extract_llmctx_block()
// IMPORTS: NONE
// USED BY: crates/core/src/process.rs
// NOTES: Never panics or guesses on malformed input — returns Err and leaves source untouched
// LLMCTX>>>

use thiserror::Error;

const OPEN_DELIMITER: &str = "<<<LLMCTX";
const CLOSE_DELIMITER: &str = "LLMCTX>>>";

/// Maximum number of lines to scan after the opening delimiter before
/// giving up looking for the closing delimiter.  Prevents an accidental
/// `<<<LLMCTX` without a matching close from scanning an entire large file.
const MAX_BLOCK_LINES: usize = 50;

/// The six required field names, in order.
const REQUIRED_FIELDS: &[&str] = &[
    "FILE:", "ROLE:", "EXPORTS:", "IMPORTS:", "USED BY:", "NOTES:",
];

#[derive(Debug, Error)]
pub enum ExtractError {
    #[error("No <<<LLMCTX delimiter found in source")]
    NoDelimiter,

    #[error("<<<LLMCTX found but no matching LLMCTX>>> within {MAX_BLOCK_LINES} lines")]
    UnclosedBlock,

    #[error("Block is missing required field: {field}")]
    MissingField { field: String },

    #[error("Block has fewer than the required {REQUIRED_FIELDS_LEN} fields")]
    TooFewFields,
}

const REQUIRED_FIELDS_LEN: usize = REQUIRED_FIELDS.len();

/// Result of a successful extraction.
#[derive(Debug, Clone)]
pub struct ExtractedContext {
    /// The six-field body, with comment prefixes stripped.
    /// Does NOT include the version stamp or project header — process.rs adds those.
    pub body: String,

    /// The source file content with the entire comment block removed,
    /// including any leading blank line left between the block and the
    /// first real line of code.
    pub cleaned_source: String,
}

/// Attempt to find and extract an `<<<LLMCTX … LLMCTX>>>` block from `source`.
///
/// On success the caller receives a clean (context, source) pair.
/// On any error the source is left completely untouched — the caller should
/// fall through to Ollama generation instead.
pub fn extract_llmctx_block(source: &str) -> Result<ExtractedContext, ExtractError> {
    let lines: Vec<&str> = source.lines().collect();

    // ── 1. Find the opening delimiter line ───────────────────────────────────
    let open_idx = lines
        .iter()
        .position(|l| strip_comment_prefix(l).trim() == OPEN_DELIMITER)
        .ok_or(ExtractError::NoDelimiter)?;

    // ── 2. Find the closing delimiter within the search window ───────────────
    let search_end = (open_idx + 1 + MAX_BLOCK_LINES).min(lines.len());
    let close_idx = lines[open_idx + 1..search_end]
        .iter()
        .position(|l| strip_comment_prefix(l).trim() == CLOSE_DELIMITER)
        .map(|rel| open_idx + 1 + rel)
        .ok_or(ExtractError::UnclosedBlock)?;

    // ── 3. Extract and strip comment prefixes from the inner lines ───────────
    let inner_lines: Vec<&str> = lines[open_idx + 1..close_idx].to_vec();
    let body_lines: Vec<String> = inner_lines
        .iter()
        .map(|l| strip_comment_prefix(l).to_string())
        .collect();

    // ── 4. Validate that all six fields are present ──────────────────────────
    //
    // The per-field loop runs first so the error names the field that is
    // actually missing. It used to run *after* a `body_lines.len()` count
    // check, which meant a block missing one field reported the generic
    // `TooFewFields` and the caller never learned which one.
    for field in REQUIRED_FIELDS {
        if !body_lines.iter().any(|l| l.trim_start().starts_with(field)) {
            return Err(ExtractError::MissingField {
                field: field.to_string(),
            });
        }
    }

    // Backstop: every required field matched, so this can only trip if two
    // fields shared a line. Kept as a guard rather than removed.
    if body_lines.len() < REQUIRED_FIELDS.len() {
        return Err(ExtractError::TooFewFields);
    }

    let body = body_lines.join("\n");

    // ── 5. Remove the block from the source ─────────────────────────────────
    //
    // The block occupies lines [open_idx..=close_idx].
    // Also consume one blank line immediately after the closing delimiter
    // if present — that's the blank line the skill file mandates between
    // the closing delimiter and the first real code line.
    let mut remaining: Vec<&str> = Vec::with_capacity(lines.len());
    let after_close = close_idx + 1;
    let skip_blank = lines
        .get(after_close)
        .map(|l| l.trim().is_empty())
        .unwrap_or(false);
    let code_start = if skip_blank {
        after_close + 1
    } else {
        after_close
    };

    // Everything before the block (if any — normally nothing comes before it).
    remaining.extend_from_slice(&lines[..open_idx]);
    // Everything after the blank separator.
    remaining.extend_from_slice(&lines[code_start..]);

    // Re-join preserving the original line ending style (assume \n; \r\n is
    // normalised to \n on read and re-written as \n — acceptable for source files).
    let cleaned_source = remaining.join("\n");
    // Preserve a trailing newline if the original had one.
    let cleaned_source = if source.ends_with('\n') {
        format!("{cleaned_source}\n")
    } else {
        cleaned_source
    };

    Ok(ExtractedContext {
        body,
        cleaned_source,
    })
}

/// Strip the comment prefix from a single line, returning the remainder.
///
/// Handles the comment styles used in every example in llmctx.md:
///   `# `, `// `, `/* ` (CSS/C block open), ` * ` (C block continuation),
///   `<!-- ` (HTML open), and their variants without a trailing space.
///
/// The delimiter strings (`<<<LLMCTX`, `LLMCTX>>>`) and field names are
/// ASCII-only and don't contain comment characters, so this stripping is
/// purely cosmetic — even an imperfect strip leaves valid field lines.
pub fn strip_comment_prefix(line: &str) -> &str {
    let trimmed = line.trim_start();

    // Try each prefix in longest-first order so `<!-- ` beats `<`.
    const PREFIXES: &[&str] = &[
        "<!-- ", "<!---", "// ", "# ", "/* ", " * ", "* ", "//", "#",
    ];
    for prefix in PREFIXES {
        if let Some(rest) = trimmed.strip_prefix(prefix) {
            return rest;
        }
    }
    // Also handle the HTML close `-->` that may appear on its own line.
    if trimmed == "-->" || trimmed == "*/" {
        return "";
    }
    trimmed
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn python_block() -> &'static str {
        r#"# <<<LLMCTX
# FILE: auth/middleware.py
# ROLE: JWT verification middleware for protected routes
# EXPORTS: verify_token(), require_auth
# IMPORTS: models/user.py, config/settings.py
# USED BY: routers/orders.py, routers/profile.py
# NOTES: NONE
# LLMCTX>>>

def verify_token():
    pass
"#
    }

    fn js_block() -> &'static str {
        r#"// <<<LLMCTX
// FILE: utils/formatDate.js
// ROLE: Formats ISO timestamps for display
// EXPORTS: formatDate(), formatRelativeTime()
// IMPORTS: NONE
// USED BY: components/Timeline.jsx
// NOTES: Assumes UTC input
// LLMCTX>>>

export function formatDate(iso) {}
"#
    }

    #[test]
    fn extracts_python_block() {
        let result = extract_llmctx_block(python_block()).unwrap();
        assert!(result.body.contains("FILE: auth/middleware.py"));
        assert!(result.body.contains("ROLE: JWT verification"));
        assert!(result.cleaned_source.contains("def verify_token():"));
        assert!(!result.cleaned_source.contains("<<<LLMCTX"));
        assert!(!result.cleaned_source.contains("LLMCTX>>>"));
    }

    #[test]
    fn extracts_js_block() {
        let result = extract_llmctx_block(js_block()).unwrap();
        assert!(result.body.contains("FILE: utils/formatDate.js"));
        assert!(result.cleaned_source.contains("export function formatDate"));
    }

    #[test]
    fn cleaned_source_has_no_leading_blank() {
        let result = extract_llmctx_block(python_block()).unwrap();
        // The blank line between LLMCTX>>> and def verify_token should be consumed.
        assert!(!result.cleaned_source.starts_with('\n'));
    }

    #[test]
    fn trailing_newline_preserved() {
        let result = extract_llmctx_block(python_block()).unwrap();
        assert!(result.cleaned_source.ends_with('\n'));
    }

    #[test]
    fn no_delimiter_returns_err() {
        let source = "def foo():\n    pass\n";
        assert!(matches!(
            extract_llmctx_block(source),
            Err(ExtractError::NoDelimiter)
        ));
    }

    #[test]
    fn unclosed_block_returns_err() {
        let source = "# <<<LLMCTX\n# FILE: x.py\n# no closing delimiter\n";
        assert!(matches!(
            extract_llmctx_block(source),
            Err(ExtractError::UnclosedBlock)
        ));
    }

    #[test]
    fn missing_field_returns_err() {
        // Missing NOTES:
        let source = r#"# <<<LLMCTX
# FILE: x.py
# ROLE: does stuff
# EXPORTS: foo
# IMPORTS: NONE
# USED BY: UNKNOWN
# LLMCTX>>>

pass
"#;
        assert!(matches!(
            extract_llmctx_block(source),
            Err(ExtractError::MissingField { .. })
        ));
    }

    #[test]
    fn block_more_than_max_lines_away_from_open() {
        // Closing delimiter is beyond the MAX_BLOCK_LINES window.
        let mut src = String::from("// <<<LLMCTX\n");
        for i in 0..=MAX_BLOCK_LINES {
            src.push_str(&format!("// line {i}\n"));
        }
        src.push_str("// LLMCTX>>>\n");
        assert!(matches!(
            extract_llmctx_block(&src),
            Err(ExtractError::UnclosedBlock)
        ));
    }

    #[test]
    fn strip_comment_prefix_variants() {
        assert_eq!(strip_comment_prefix("# FILE: foo"), "FILE: foo");
        assert_eq!(strip_comment_prefix("// FILE: foo"), "FILE: foo");
        assert_eq!(strip_comment_prefix("/* <<<LLMCTX"), "<<<LLMCTX");
        assert_eq!(strip_comment_prefix("<!-- <<<LLMCTX"), "<<<LLMCTX");
        assert_eq!(strip_comment_prefix("  # FILE: bar"), "FILE: bar");
    }
}
