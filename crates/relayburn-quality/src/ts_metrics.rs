//! Counts `any` / `unknown` type usages in the TypeScript surface
//! (`packages/*/src` and hand-written `.d.ts` files).
//!
//! A lightweight token scan rather than a full TS parse: comments and string
//! bodies are stripped, then a word occurrence counts as a *type usage* only
//! when it is not a property name (`unknown: number`) and not a property
//! access (`.unknown`).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct TsTypeCounts {
    pub any_count: usize,
    pub unknown_count: usize,
    /// file -> (any, unknown) for reporting.
    pub per_file: Vec<(String, usize, usize)>,
}

pub fn collect(repo_root: &Path, source_roots: &[String]) -> Result<TsTypeCounts> {
    let mut files = Vec::new();
    for root in source_roots {
        collect_ts_files(&repo_root.join(root), &mut files)?;
    }
    files.sort();

    let mut out = TsTypeCounts::default();
    for path in files {
        let rel = path
            .strip_prefix(repo_root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        let src = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let stripped = strip_comments_and_strings(&src);
        let any = count_type_word(&stripped, "any");
        let unknown = count_type_word(&stripped, "unknown");
        if any > 0 || unknown > 0 {
            out.per_file.push((rel, any, unknown));
        }
        out.any_count += any;
        out.unknown_count += unknown;
    }
    Ok(out)
}

fn collect_ts_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    for entry in std::fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if path.is_dir() {
            if name == "node_modules" || name == "dist" || name.starts_with('.') {
                continue;
            }
            collect_ts_files(&path, out)?;
        } else if name.ends_with(".ts") || name.ends_with(".mts") || name.ends_with(".cts") {
            out.push(path);
        }
    }
    Ok(())
}

/// Replace comment and string-literal bodies with spaces, preserving offsets.
fn strip_comments_and_strings(src: &str) -> String {
    let bytes = src.as_bytes();
    let mut out = bytes.to_vec();
    let mut i = 0;
    while i < bytes.len() {
        i = match bytes[i] {
            b'/' if bytes.get(i + 1) == Some(&b'/') => blank_line_comment(bytes, &mut out, i),
            b'/' if bytes.get(i + 1) == Some(&b'*') => blank_block_comment(bytes, &mut out, i),
            q @ (b'"' | b'\'' | b'`') => blank_quoted(bytes, &mut out, i, q),
            _ => i + 1,
        };
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn blank_line_comment(bytes: &[u8], out: &mut [u8], mut i: usize) -> usize {
    while i < bytes.len() && bytes[i] != b'\n' {
        out[i] = b' ';
        i += 1;
    }
    i
}

fn blank_block_comment(bytes: &[u8], out: &mut [u8], mut i: usize) -> usize {
    while i < bytes.len() && !(bytes[i] == b'*' && bytes.get(i + 1) == Some(&b'/')) {
        out[i] = b' ';
        i += 1;
    }
    if i + 1 < bytes.len() {
        out[i] = b' ';
        out[i + 1] = b' ';
        i += 2;
    }
    i
}

fn blank_quoted(bytes: &[u8], out: &mut [u8], mut i: usize, quote: u8) -> usize {
    out[i] = b' ';
    i += 1;
    while i < bytes.len() && bytes[i] != quote {
        // A backslash escape blanks both bytes so an escaped quote does not
        // terminate the literal.
        let step = if bytes[i] == b'\\' { 2 } else { 1 };
        for _ in 0..step {
            if i < bytes.len() {
                out[i] = b' ';
                i += 1;
            }
        }
    }
    if i < bytes.len() {
        out[i] = b' ';
        i += 1;
    }
    i
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$'
}

/// Count standalone `word` occurrences that read as a type, skipping
/// property names (`word:` / `word?:`) and property accesses (`.word`).
fn count_type_word(stripped: &str, word: &str) -> usize {
    let bytes = stripped.as_bytes();
    let mut count = 0;
    let mut start = 0;
    while let Some(pos) = stripped[start..].find(word) {
        let begin = start + pos;
        let end = begin + word.len();
        start = end;

        let before = begin.checked_sub(1).map(|i| bytes[i]);
        if before.is_some_and(is_word_byte) || bytes.get(end).copied().is_some_and(is_word_byte) {
            continue; // part of a longer identifier
        }
        if before == Some(b'.') {
            continue; // property access
        }
        // Property name: `word:` or `word?:` (a type usage is preceded by
        // `:`/`<`/`,`/`(`/`as`/`|`/`&` instead).
        let mut j = end;
        while bytes.get(j) == Some(&b' ') {
            j += 1;
        }
        if bytes.get(j) == Some(&b'?') {
            j += 1;
        }
        if bytes.get(j) == Some(&b':') {
            continue;
        }
        count += 1;
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_type_positions_only() {
        let src = "const a: unknown = 1; const b = { unknown: 2 }; b.unknown; x as any;";
        let s = strip_comments_and_strings(src);
        assert_eq!(count_type_word(&s, "unknown"), 1);
        assert_eq!(count_type_word(&s, "any"), 1);
    }

    #[test]
    fn ignores_comments_and_strings() {
        let src = "// any unknown\nconst s = 'any unknown';\nlet x: any;";
        let s = strip_comments_and_strings(src);
        assert_eq!(count_type_word(&s, "any"), 1);
        assert_eq!(count_type_word(&s, "unknown"), 0);
    }

    #[test]
    fn generic_and_union_positions_count() {
        let src = "function f(): Promise<unknown> { return g() as unknown | any[]; }";
        let s = strip_comments_and_strings(src);
        assert_eq!(count_type_word(&s, "unknown"), 2);
        assert_eq!(count_type_word(&s, "any"), 1);
    }
}
