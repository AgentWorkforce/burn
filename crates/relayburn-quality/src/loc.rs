//! Logical line counting: which 1-based lines of a Rust source file carry
//! code (not blank, not comment-only). String literals are tracked so `//`
//! inside a string does not start a comment.

#[derive(PartialEq, Clone, Copy)]
enum State {
    Code,
    LineComment,
    BlockComment(u32),
    Str,
    RawStr(usize),
}

/// Returns the 1-based line numbers that contain at least one code token.
pub fn code_lines(src: &str) -> Vec<usize> {
    let mut scan = Scanner {
        bytes: src.as_bytes(),
        state: State::Code,
        i: 0,
        line: 1,
        line_has_code: false,
        lines: Vec::new(),
    };
    while scan.i < scan.bytes.len() {
        scan.step();
    }
    if scan.line_has_code {
        scan.lines.push(scan.line);
    }
    scan.lines
}

struct Scanner<'a> {
    bytes: &'a [u8],
    state: State,
    i: usize,
    line: usize,
    line_has_code: bool,
    lines: Vec<usize>,
}

impl Scanner<'_> {
    fn step(&mut self) {
        if self.bytes[self.i] == b'\n' {
            self.newline();
            return;
        }
        match self.state {
            State::Code => self.step_code(),
            State::LineComment => self.i += 1,
            State::BlockComment(depth) => self.step_block_comment(depth),
            State::Str => self.step_str(),
            State::RawStr(hashes) => self.step_raw_str(hashes),
        }
    }

    fn newline(&mut self) {
        if self.line_has_code {
            self.lines.push(self.line);
        }
        self.line += 1;
        self.line_has_code = false;
        if self.state == State::LineComment {
            self.state = State::Code;
        }
        self.i += 1;
    }

    fn peek(&self, offset: usize) -> Option<u8> {
        self.bytes.get(self.i + offset).copied()
    }

    fn step_code(&mut self) {
        let b = self.bytes[self.i];
        match b {
            b'/' if self.peek(1) == Some(b'/') => {
                self.state = State::LineComment;
                self.i += 2;
            }
            b'/' if self.peek(1) == Some(b'*') => {
                self.state = State::BlockComment(1);
                self.i += 2;
            }
            b'"' => {
                self.state = State::Str;
                self.line_has_code = true;
                self.i += 1;
            }
            b'r' if self.try_raw_string_open() => {}
            b'\'' if self.try_char_literal() => {}
            _ => {
                if !b.is_ascii_whitespace() {
                    self.line_has_code = true;
                }
                self.i += 1;
            }
        }
    }

    /// Raw string: `r"..."` or `r#"..."#` with any number of `#`s.
    fn try_raw_string_open(&mut self) -> bool {
        let mut hashes = 0;
        let mut j = self.i + 1;
        while self.bytes.get(j) == Some(&b'#') {
            hashes += 1;
            j += 1;
        }
        if self.bytes.get(j) == Some(&b'"') {
            self.state = State::RawStr(hashes);
            self.line_has_code = true;
            self.i = j + 1;
            return true;
        }
        false
    }

    /// Consume a char literal (`'a'`, `'\n'`, `'\u{...}'`); lifetimes like
    /// `'a` are left for the default path.
    fn try_char_literal(&mut self) -> bool {
        if let Some(close) = char_literal_end(self.bytes, self.i) {
            self.line_has_code = true;
            self.i = close + 1;
            return true;
        }
        false
    }

    fn step_block_comment(&mut self, depth: u32) {
        let b = self.bytes[self.i];
        if b == b'/' && self.peek(1) == Some(b'*') {
            self.state = State::BlockComment(depth + 1);
            self.i += 2;
        } else if b == b'*' && self.peek(1) == Some(b'/') {
            self.state = if depth == 1 {
                State::Code
            } else {
                State::BlockComment(depth - 1)
            };
            self.i += 2;
        } else {
            self.i += 1;
        }
    }

    fn step_str(&mut self) {
        match self.bytes[self.i] {
            b'\\' => self.i += 2,
            b'"' => {
                self.state = State::Code;
                self.i += 1;
            }
            _ => self.i += 1,
        }
    }

    fn step_raw_str(&mut self, hashes: usize) {
        if self.bytes[self.i] == b'"' {
            let mut j = self.i + 1;
            let mut seen = 0;
            while seen < hashes && self.bytes.get(j) == Some(&b'#') {
                seen += 1;
                j += 1;
            }
            if seen == hashes {
                self.state = State::Code;
                self.i = j;
                return;
            }
        }
        self.i += 1;
    }
}

/// If `bytes[start]` opens a char literal, return the index of its closing
/// quote; `None` for lifetimes like `'a`.
fn char_literal_end(bytes: &[u8], start: usize) -> Option<usize> {
    let mut j = start + 1;
    if bytes.get(j) == Some(&b'\\') {
        j += 2;
        // \u{...}
        while j < bytes.len() && bytes[j] != b'\'' && j - start < 12 {
            j += 1;
        }
        return (bytes.get(j) == Some(&b'\'')).then_some(j);
    }
    // Plain char: one UTF-8 scalar then a quote.
    let mut k = j + 1;
    while k < bytes.len() && bytes[k] & 0xC0 == 0x80 {
        k += 1;
    }
    (bytes.get(k) == Some(&b'\'')).then_some(k)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_blank_and_comment_lines() {
        let src = "fn main() {\n\n    // comment\n    let x = 1;\n}\n";
        assert_eq!(code_lines(src), vec![1, 4, 5]);
    }

    #[test]
    fn slashes_inside_strings_are_code() {
        let src = "let url = \"https://example.com\";\n";
        assert_eq!(code_lines(src), vec![1]);
    }

    #[test]
    fn block_comments_span_lines() {
        let src = "/*\n block\n*/\nlet y = 2;\n";
        assert_eq!(code_lines(src), vec![4]);
    }

    #[test]
    fn nested_block_comments() {
        let src = "/* a /* b */ still comment */\nlet z = 1;\n";
        assert_eq!(code_lines(src), vec![2]);
    }

    #[test]
    fn lifetimes_do_not_eat_code() {
        let src = "fn f<'a>(x: &'a str) -> &'a str { x }\n";
        assert_eq!(code_lines(src), vec![1]);
    }

    #[test]
    fn raw_strings_with_comment_markers() {
        let src = "let re = r#\"// not a comment\"#;\nlet z = 3;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn escaped_quote_in_string() {
        let src = "let s = \"a\\\"b // c\";\nlet t = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn block_comment_marker_inside_string_is_inert() {
        // If the string opener were mis-scanned, `/*` would swallow line 2.
        let src = "let s = \"/*\";\nlet t = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn division_is_not_a_comment() {
        let src = "let x = a / b;\nlet y = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn raw_string_with_inner_quote_and_block_marker() {
        // Without raw-string handling, the inner quote would close a plain
        // string early and `/*` would swallow line 2.
        let src = "let re = r#\"a\" /* b\"#;\nlet z = 3;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn quote_char_literal_does_not_open_string() {
        // If '"' were not consumed as a char literal, the quote would open a
        // string swallowing line 2.
        let src = "let c = '\"';\nlet d = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn comment_at_file_start() {
        let src = "//x\nlet a = 1;\n";
        assert_eq!(code_lines(src), vec![2]);
    }

    #[test]
    fn block_comment_at_file_start() {
        let src = "/* x */ let a = 1;\nlet b = 2;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn identifier_starting_with_r_is_not_a_raw_string() {
        // If `r` + non-quote opened a raw string, line 2 would sit inside it
        // and lose its code marking.
        let src = "let rx = radius;\nlet y2 = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn zero_hash_raw_string_with_block_marker() {
        // If the scanner re-entered at the opening quote, the raw string
        // would close immediately and `/*` would swallow line 2.
        let src = "let s = r\"/* x\";\nlet t = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn adjacent_char_literals_with_quote_char() {
        // If the first literal's closing quote were re-scanned, `','` would
        // parse as a char literal and the `"` inside the second literal
        // would open a string swallowing line 2.
        let src = "let p = ('a','\"');\nlet q = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn char_close_rescan_cannot_open_string() {
        // 'x' followed by a string containing a quote: if the literal's
        // closing quote were re-scanned, the string opener would be
        // swallowed and the real terminator would open an unterminated
        // string, losing line 2.
        let src = "let a = 'x'\"'\";\nlet b = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn escaped_quote_then_block_marker() {
        // If the escape were dropped, the string would close early and `/*`
        // would swallow line 2.
        let src = "let s = \"a\\\"/* b\";\nlet t = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn raw_string_close_consumes_exactly_its_hashes() {
        // `r#"x"#` closes at one hash; the following `#` and string must be
        // ordinary code. Over-consuming hashes would leave the raw string
        // open and swallow line 2.
        let src = "let s = r#\"x\"## ; \"/*\";\nlet t = 1;\n";
        assert_eq!(code_lines(src), vec![1, 2]);
    }

    #[test]
    fn char_literal_end_exact_indices() {
        assert_eq!(char_literal_end(b"'a'", 0), Some(2));
        assert_eq!(char_literal_end(b"x'b'", 1), Some(3));
        assert_eq!(char_literal_end(b"'\\n'", 0), Some(3));
        assert_eq!(char_literal_end(b"'\\''", 0), Some(3));
        assert_eq!(char_literal_end(b"'\\u{41}'", 0), Some(7));
        // Two-byte scalar: continuation bytes then the closing quote.
        assert_eq!(char_literal_end("'é'".as_bytes(), 0), Some(3));
    }

    #[test]
    fn char_literal_end_rejects_lifetimes_and_truncation() {
        assert_eq!(char_literal_end(b"'a>", 0), None);
        assert_eq!(char_literal_end(b"'static ", 0), None);
        assert_eq!(char_literal_end(b"'a", 0), None);
        assert_eq!(char_literal_end(b"'", 0), None);
        // Escape whose closing quote sits past the 12-byte scan window.
        assert_eq!(char_literal_end(b"'\\uAAAAAAAAAA'", 0), None);
        // Truncated escape.
        assert_eq!(char_literal_end(b"'\\u{4", 0), None);
    }

    #[test]
    fn nested_block_comment_far_from_file_start() {
        // Catches scanning-offset faults in the nested-comment opener: a
        // wrong jump lands past both closers and line 3 disappears.
        let src = "let x = 1;\n/* aaaaaaaaaaaaaaaaaaaaaa /* b */ */\nlet y = 2;\n";
        assert_eq!(code_lines(src), vec![1, 3]);
    }
}
