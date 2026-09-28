// <<<LLMCTX
// FILE: crates/core/src/extract.rs
// ROLE: Detect, validate, and strip <<<LLMCTX comment blocks from LLM-generated source files
// EXPORTS: ExtractedContext, ExtractError, extract_llmctx_block()
// IMPORTS: NONE
// USED BY: crates/core/src/process.rs
// NOTES: Only a block at the top of a file counts; never panics or guesses on malformed input; preserves CRLF line endings
// LLMCTX>>>

use thiserror::Error;

const OPEN_DELIMITER: &str = "<<<LLMCTX";
const CLOSE_DELIMITER: &str = "LLMCTX>>>";

/// Maximum number of lines to scan after the opening delimiter before
/// giving up looking for the closing delimiter.  Prevents an accidental
/// `<<<LLMCTX` without a matching close from scanning an entire large file.
const MAX_BLOCK_LINES: usize = 50;

/// How many language-mandated lines (shebang, crate attribute, encoding
/// cookie, …) may come before the opening delimiter.
const MAX_PREAMBLE_LINES: usize = 5;

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
    /// first real line of code. Line endings match the original (`\r\n`
    /// stays `\r\n`).
    pub cleaned_source: String,
}

/// Attempt to find and extract an `<<<LLMCTX … LLMCTX>>>` block from `source`.
///
/// Only a block at the top of the file counts: the opening delimiter must be
/// the first non-blank line, or follow nothing but language-mandated preamble
/// (a shebang, a Rust `#![…]` attribute, an encoding cookie, `<?php`, …).
/// Anything further down — an example in documentation, a test fixture — is
/// ordinary file content and is never touched.
///
/// On success the caller receives a clean (context, source) pair.
/// On any error the source is left completely untouched — the caller should
/// fall through to Ollama generation instead.
pub fn extract_llmctx_block(source: &str) -> Result<ExtractedContext, ExtractError> {
    let lines: Vec<&str> = source.lines().collect();

    // ── 1. Find the opening delimiter line at the top of the file ────────────
    let open_idx = find_open_delimiter(&lines).ok_or(ExtractError::NoDelimiter)?;

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

    // Re-join with the file's own line ending. `lines()` drops the `\r` of
    // every `\r\n`, and writing plain `\n` back would turn a one-block edit
    // into a whole-file diff on Windows checkouts.
    let eol = if source.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let mut cleaned_source = remaining.join(eol);
    // Preserve a trailing newline if the original had one.
    if source.ends_with('\n') && !cleaned_source.is_empty() {
        cleaned_source.push_str(eol);
    }

    Ok(ExtractedContext {
        body,
        cleaned_source,
    })
}

/// Index of the opening delimiter if it sits at the top of the file.
fn find_open_delimiter(lines: &[&str]) -> Option<usize> {
    let mut preamble = 0;
    for (idx, line) in lines.iter().enumerate() {
        if strip_comment_prefix(line).trim() == OPEN_DELIMITER {
            return Some(idx);
        }
        if line.trim().is_empty() {
            continue;
        }
        if is_preamble(line) && preamble < MAX_PREAMBLE_LINES {
            preamble += 1;
            continue;
        }
        // Real content came first: any delimiter further down is not ours.
        return None;
    }
    None
}

/// Lines a language requires before anything else, so the block may follow them.
fn is_preamble(line: &str) -> bool {
    let t = line.trim();
    let lower = t.to_ascii_lowercase();
    t.starts_with("#!")                          // shebang, Rust `#![…]`
        || t.starts_with("<?")                   // <?php, <?xml
        || lower.starts_with("<!doctype")
        || lower.starts_with("# -*-")            // Python/Ruby encoding cookie
        || lower.starts_with("# vim:")
        || lower.starts_with("# coding")
        || lower.starts_with("# frozen_string_literal")
        || t == "\"use strict\";"
        || t == "'use strict';"
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
    const PREFIXES: &[&str] = &["<!-- ", "<!---", "// ", "# ", "/* ", " * ", "* ", "//", "#"];
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
    fn block_below_other_content_is_ignored() {
        // Documentation that shows the format (like docs/llmctx.md) must never
        // be rewritten: only a block at the very top of a file is a block.
        let doc = format!(
            "# The format\n\nExample:\n\n```python\n{}```\n",
            python_block()
        );
        assert!(matches!(
            extract_llmctx_block(&doc),
            Err(ExtractError::NoDelimiter)
        ));
    }

    #[test]
    fn block_after_shebang_and_attributes_is_found() {
        let src = format!(
            "#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\n{}",
            python_block()
        );
        let result = extract_llmctx_block(&src).unwrap();
        assert!(result
            .cleaned_source
            .starts_with("#!/usr/bin/env python3\n# -*- coding: utf-8 -*-\ndef verify_token"));

        let rust = format!("#![allow(dead_code)]\n\n{}", js_block());
        assert!(extract_llmctx_block(&rust).is_ok());
    }

    #[test]
    fn crlf_line_endings_are_preserved() {
        let src = python_block().replace('\n', "\r\n");
        let result = extract_llmctx_block(&src).unwrap();
        assert_eq!(result.cleaned_source, "def verify_token():\r\n    pass\r\n");
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
