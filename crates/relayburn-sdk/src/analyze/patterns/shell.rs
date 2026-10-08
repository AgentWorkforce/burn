//! Shell-command file-read tokenizer for the edit-heavy detector. A small,
//! self-contained POSIX-ish shell parser: it splits a command into segments,
//! tokenizes each segment (honoring quotes), and decides whether a
//! file-printing command (`cat`, `head`, `tail`, `sed -n`, `nl`, `bat`,
//! `less`, `more`) has a file operand (vs. reading stdin via a pipe or
//! heredoc).
//!
//! Only `shell_command_has_file_read` is used by the parent module.

/// Commands that print a file's contents. Search commands (`grep`, `rg`)
/// are excluded for the same reason the `Grep` tool is: they show matched
/// lines, not the surrounding code an edit needs.
fn is_shell_read_command(name: &str) -> bool {
    matches!(
        name,
        "cat" | "head" | "tail" | "sed" | "nl" | "bat" | "less" | "more"
    )
}

pub(super) fn shell_command_has_file_read(command: &str) -> bool {
    for segment in split_shell_segments(command) {
        if shell_segment_starts_with_file_read(segment) {
            return true;
        }
    }
    false
}

// Mirrors `command.split(/(?:&&|\|\||;|\n)/)` from patterns.ts:1318.
fn split_shell_segments(command: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    let bytes = command.as_bytes();
    let mut start = 0_usize;
    let mut i = 0_usize;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' || b == b';' {
            out.push(&command[start..i]);
            start = i + 1;
            i += 1;
            continue;
        }
        if i + 1 < bytes.len()
            && ((b == b'&' && bytes[i + 1] == b'&') || (b == b'|' && bytes[i + 1] == b'|'))
        {
            out.push(&command[start..i]);
            start = i + 2;
            i += 2;
            continue;
        }
        i += 1;
    }
    out.push(&command[start..]);
    out
}

fn shell_segment_starts_with_file_read(segment: &str) -> bool {
    let tokens = shell_words(segment);
    let mut i = 0_usize;
    while i < tokens.len() && is_shell_env_assignment(&tokens[i]) {
        i += 1;
    }
    if i >= tokens.len() {
        return false;
    }
    let cmd = command_basename(&tokens[i]);
    if !is_shell_read_command(&cmd) {
        return false;
    }
    let rest: Vec<String> = tokens[i + 1..].to_vec();
    if cmd == "sed" {
        return is_sed_print(&rest);
    }
    has_shell_file_operand(&cmd, &rest)
}

/// `sed -n <script> <file>…`: a print-only sed over a file. In-place edits
/// (`-i`) and stream filters without `-n` are not reads.
fn is_sed_print(tokens: &[String]) -> bool {
    let flags: Vec<String> = tokens
        .iter()
        .map(|t| strip_shell_quotes(t))
        .take_while(|t| !matches!(t.as_str(), "|" | "&&" | "||" | ";"))
        .collect();
    let quiet = flags
        .iter()
        .any(|t| t == "--quiet" || (t.starts_with('-') && !t.starts_with("--") && t.contains('n')));
    let in_place = flags.iter().any(|t| {
        t.starts_with("--in-place")
            || (t.starts_with('-') && !t.starts_with("--") && t.contains('i'))
    });
    let operands = flags
        .iter()
        .filter(|t| !t.starts_with('-') && !t.starts_with('>') && !t.starts_with('<'))
        .count();
    quiet && !in_place && operands >= 2
}

// Mirrors the JS regex `/"[^"]*"|'[^']*'|\S+/g` from patterns.ts:1336.
fn shell_words(segment: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let chars: Vec<char> = segment.chars().collect();
    let mut i = 0_usize;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
            continue;
        }
        if c == '"' || c == '\'' {
            let quote = c;
            let start = i;
            i += 1;
            while i < chars.len() && chars[i] != quote {
                i += 1;
            }
            // Include closing quote if present, mirroring `"[^"]*"` regex
            // semantics — match consumes the closing quote.
            if i < chars.len() {
                i += 1;
                out.push(chars[start..i].iter().collect());
            } else {
                // Unterminated quote — JS regex would not match. Fall back
                // to a `\S+` style read of the remainder so we don't drop
                // the token entirely.
                let mut j = start;
                while j < chars.len() && !chars[j].is_whitespace() {
                    j += 1;
                }
                out.push(chars[start..j].iter().collect());
                i = j;
            }
            continue;
        }
        let start = i;
        while i < chars.len() && !chars[i].is_whitespace() {
            i += 1;
        }
        out.push(chars[start..i].iter().collect());
    }
    out
}

fn is_shell_env_assignment(token: &str) -> bool {
    let mut chars = token.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        return false;
    }
    let mut saw_eq = false;
    for c in chars {
        if c == '=' {
            saw_eq = true;
            break;
        }
        if !(c.is_ascii_alphanumeric() || c == '_') {
            return false;
        }
    }
    saw_eq
}

fn command_basename(token: &str) -> String {
    let unquoted = strip_shell_quotes(token);
    match unquoted.rfind('/') {
        Some(i) => unquoted[i + 1..].to_string(),
        None => unquoted,
    }
}

fn strip_shell_quotes(token: &str) -> String {
    let chars: Vec<char> = token.chars().collect();
    if chars.len() >= 2 {
        let first = chars[0];
        let last = chars[chars.len() - 1];
        if (first == '"' && last == '"') || (first == '\'' && last == '\'') {
            return chars[1..chars.len() - 1].iter().collect();
        }
    }
    token.to_string()
}

fn has_shell_file_operand(command: &str, tokens: &[String]) -> bool {
    let mut skip_next = false;
    for raw in tokens {
        let token = strip_shell_quotes(raw);
        if skip_next {
            skip_next = false;
            continue;
        }
        if token == "|" || token == "&&" || token == "||" || token == ";" {
            break;
        }
        // `/^\d*>/.test(token) || token.startsWith('>')`
        if is_redirect_open(&token) {
            // `/^\d*>+$/.test(token) || /^>+$/.test(token)`
            if is_pure_redirect(&token) {
                skip_next = true;
            }
            continue;
        }
        if token.starts_with('<') {
            continue;
        }
        if token == "-" {
            continue;
        }
        if (command == "head" || command == "tail")
            && (token == "-n" || token == "-c" || token == "--lines" || token == "--bytes")
        {
            skip_next = true;
            continue;
        }
        if (command == "head" || command == "tail") && is_signed_integer(&token) {
            continue;
        }
        if token.starts_with('-') {
            continue;
        }
        return true;
    }
    false
}

fn is_redirect_open(token: &str) -> bool {
    // matches `^\d*>` (zero or more digits followed by '>')
    for c in token.chars() {
        if c.is_ascii_digit() {
            continue;
        }
        return c == '>';
    }
    false
}

fn is_pure_redirect(token: &str) -> bool {
    // matches `/^\d*>+$/` or `/^>+$/`
    let mut i = 0_usize;
    let bytes = token.as_bytes();
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == bytes.len() {
        return false;
    }
    // At least one byte remains; it's a pure redirect iff all remaining
    // bytes are '>'.
    while i < bytes.len() {
        if bytes[i] != b'>' {
            return false;
        }
        i += 1;
    }
    true
}

fn is_signed_integer(token: &str) -> bool {
    // matches `/^[+-]?\d+$/`
    let digits = match token.strip_prefix(['+', '-']) {
        Some(rest) => rest,
        None => token,
    };
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::shell_command_has_file_read as reads;

    #[test]
    fn file_printing_commands_with_a_file_operand_are_reads() {
        for command in [
            "cat src/lib.rs",
            "head -n 20 src/main.rs",
            "sed -n '1,80p' src/main.rs",
            "sed -ne 5p Cargo.toml",
            "git status && nl -ba src/lib.rs | head",
            "bat README.md",
        ] {
            assert!(reads(command), "{command}");
        }
    }

    #[test]
    fn edits_filters_and_searches_are_not_reads() {
        for command in [
            "sed -i 's/a/b/' src/main.rs",
            "sed -n -i 1p src/main.rs",
            "sed 's/a/b/' src/main.rs",
            "sed -n 1p",
            "git log | sed -n 1,5p",
            "grep -n foo src/main.rs",
            "cat <<EOF",
        ] {
            assert!(!reads(command), "{command}");
        }
    }
}
