//! Bash-command parser — extracted from the classifier.

use std::sync::LazyLock;

use phf::{phf_map, phf_set};
use regex::Regex;

use super::build_re;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashParse {
    pub binary: String,
    pub subcommand: Option<String>,
    pub normalized: String,
}

// ---------------------------------------------------------------------------
// Bash binary tables.
// ---------------------------------------------------------------------------

static MULTIWORD_BINARIES: phf::Set<&'static str> = phf_set! {
    "git", "gh", "npm", "pnpm", "yarn", "bun", "pip", "pip3", "uv", "poetry",
    "cargo", "make", "docker", "kubectl", "helm", "terraform", "brew", "apt",
    "apt-get", "gem", "bundle", "go",
};

static PACKAGE_RUNNERS: phf::Set<&'static str> = phf_set! { "npm", "pnpm", "yarn", "bun" };
static SHELL_BINARIES: phf::Set<&'static str> = phf_set! { "bash", "sh", "zsh" };
static PYTHON_BINARIES: phf::Set<&'static str> = phf_set! { "python", "python3" };

// `binary -> nested first-token subcommands`. Slices instead of a nested set
// because counts are tiny (1–6 entries) and linear scan is faster than a hash
// probe at that size.
static TWO_PART_SUBCOMMANDS: phf::Map<&'static str, &'static [&'static str]> = phf_map! {
    "docker" => &["compose"],
    "gh"     => &["pr", "run", "issue", "repo", "workflow", "release"],
    "go"     => &["mod"],
    "uv"     => &["pip"],
};

static OPTION_TAKES_VALUE: phf::Set<&'static str> = phf_set! {
    "-C", "-c", "-F", "--config", "--filter", "--git-dir", "--namespace",
    "--prefix", "--repo", "--repository", "--work-tree",
};

static PYTHON_OPTION_TAKES_VALUE: phf::Set<&'static str> = phf_set! {
    "-c", "-m", "-W", "-X", "--check-hash-based-pycs",
};

/// Matches a leading `KEY=` shell env-assignment token. Shared between
/// `skip_env_assignments` and `env_command_args` so the same compiled regex
/// is reused.
static ENV_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| build_re(r"^[A-Za-z_][A-Za-z0-9_]*="));

// ---------------------------------------------------------------------------
// Bash command parsing.
// ---------------------------------------------------------------------------

pub fn parse_bash_command(command: &str) -> Option<BashParse> {
    parse_bash_command_inner(command, 0)
}

fn parse_bash_command_inner(command: &str, depth: u32) -> Option<BashParse> {
    if depth > 5 {
        return Some(shell_parse());
    }
    let cmd = command.trim();
    if cmd.is_empty() {
        return None;
    }
    if has_heredoc(cmd) || starts_with_compound_shell_syntax(cmd) {
        return Some(shell_parse());
    }
    if !has_balanced_shell_delimiters(cmd) {
        return Some(shell_parse());
    }

    if let Some(unwrapped) = unwrap_subshell(cmd) {
        return parse_bash_command_inner(&unwrapped, depth + 1);
    }
    if let Some(rest) = strip_leading_cd_prefix(cmd) {
        return parse_bash_command_inner(&rest, depth + 1);
    }

    let first = first_top_level_segment(cmd)?;
    let cmd = first.trim();
    if cmd.is_empty() {
        return None;
    }
    if has_heredoc(cmd) || starts_with_compound_shell_syntax(cmd) {
        return Some(shell_parse());
    }
    if let Some(unwrapped) = unwrap_subshell(cmd) {
        return parse_bash_command_inner(&unwrapped, depth + 1);
    }

    let tokens = match shell_words(cmd) {
        Some(t) => t,
        None => return Some(shell_parse()),
    };
    let i = skip_env_assignments(&tokens, 0);
    if i >= tokens.len() {
        return None;
    }

    let raw_binary = &tokens[i];
    let binary = normalize_binary(raw_binary);

    if SHELL_BINARIES.contains(binary.as_str()) {
        if let Some(shell_cmd) = shell_command_arg(&tokens, i + 1) {
            return parse_bash_command_inner(&shell_cmd, depth + 1);
        }
        return Some(verb(&binary, None));
    }

    if binary == "env" {
        let env_args = env_command_args(&tokens, i + 1);
        if env_args.is_empty() {
            return None;
        }
        return parse_bash_command_inner(&env_args.join(" "), depth + 1);
    }

    if PYTHON_BINARIES.contains(binary.as_str()) {
        if let Some(parsed) = parse_python_module(&tokens, i + 1) {
            return Some(parsed);
        }
    }

    if binary == "node" && tokens[i + 1..].iter().any(|t| t == "--test") {
        return Some(verb(&binary, Some("--test")));
    }

    let sub_index = skip_leading_options(&tokens, i + 1);
    if sub_index >= tokens.len() || !MULTIWORD_BINARIES.contains(binary.as_str()) {
        return Some(verb(&binary, None));
    }

    let mut subcommand = tokens[sub_index].clone();
    if PACKAGE_RUNNERS.contains(binary.as_str())
        && subcommand == "run"
        && sub_index + 1 < tokens.len()
    {
        subcommand = tokens[sub_index + 1].clone();
    } else if let Some(nested) = TWO_PART_SUBCOMMANDS.get(binary.as_str()) {
        if nested.contains(&subcommand.as_str()) && sub_index + 1 < tokens.len() {
            subcommand = format!("{} {}", subcommand, tokens[sub_index + 1]);
        }
    }

    Some(verb(&binary, Some(&subcommand)))
}

fn verb(binary: &str, subcommand: Option<&str>) -> BashParse {
    let normalized = match subcommand {
        Some(sub) => format!("{binary} {sub}"),
        None => binary.to_string(),
    };
    BashParse {
        binary: binary.to_string(),
        subcommand: subcommand.map(str::to_string),
        normalized,
    }
}

fn shell_parse() -> BashParse {
    BashParse {
        binary: "(shell)".to_string(),
        subcommand: None,
        normalized: "(shell)".to_string(),
    }
}

fn has_heredoc(cmd: &str) -> bool {
    static RE: LazyLock<Regex> = LazyLock::new(|| build_re(r"<<-?\s*\S+"));
    RE.is_match(cmd)
}

fn starts_with_compound_shell_syntax(cmd: &str) -> bool {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| build_re(r"^(?:for|while|until|if|case|select|function)\b"));
    static BRACE_RE: LazyLock<Regex> = LazyLock::new(|| build_re(r"^\{\s"));
    RE.is_match(cmd) || BRACE_RE.is_match(cmd)
}

fn has_balanced_shell_delimiters(cmd: &str) -> bool {
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth: i32 = 0;
    for &b in cmd.as_bytes() {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            continue;
        }
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth < 0 {
                return false;
            }
        }
    }
    quote.is_none() && depth == 0
}

fn unwrap_subshell(cmd: &str) -> Option<String> {
    if !cmd.starts_with('(') || !cmd.ends_with(')') {
        return None;
    }
    let bytes = cmd.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth: i32 = 0;
    for (i, &b) in bytes.iter().enumerate() {
        if escaped {
            escaped = false;
            continue;
        }
        if b == b'\\' && quote != Some(b'\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            continue;
        }
        if b == b'(' {
            depth += 1;
        } else if b == b')' {
            depth -= 1;
            if depth == 0 && i != bytes.len() - 1 {
                return None;
            }
            if depth < 0 {
                return None;
            }
        }
    }
    if quote.is_some() || depth != 0 {
        return None;
    }
    Some(cmd[1..cmd.len() - 1].trim().to_string())
}

fn strip_leading_cd_prefix(cmd: &str) -> Option<String> {
    let op = first_top_level_operator(cmd, &["&&", ";"])?;
    let words = shell_words(cmd[..op.index].trim())?;
    let cmd_idx = skip_env_assignments(&words, 0);
    if words.get(cmd_idx).map(String::as_str) != Some("cd") {
        return None;
    }
    Some(cmd[op.index + op.operator.len()..].trim().to_string())
}

fn first_top_level_segment(cmd: &str) -> Option<String> {
    match first_top_level_operator(cmd, &["&&", "||", ";", "|", "\n"]) {
        Some(op) => Some(cmd[..op.index].to_string()),
        None => Some(cmd.to_string()),
    }
}

struct OpHit {
    index: usize,
    operator: &'static str,
}

fn first_top_level_operator(cmd: &str, operators: &[&'static str]) -> Option<OpHit> {
    let bytes = cmd.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth: i32 = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if escaped {
            escaped = false;
            i += 1;
            continue;
        }
        if b == b'\\' && quote != Some(b'\'') {
            escaped = true;
            i += 1;
            continue;
        }
        if let Some(q) = quote {
            if b == q {
                quote = None;
            }
            i += 1;
            continue;
        }
        if b == b'"' || b == b'\'' {
            quote = Some(b);
            i += 1;
            continue;
        }
        if b == b'(' {
            depth += 1;
            i += 1;
            continue;
        }
        if b == b')' {
            depth -= 1;
            if depth < 0 {
                return None;
            }
            i += 1;
            continue;
        }
        if depth == 0 {
            for op in operators {
                // Compare against the byte slice rather than `cmd[i..]` —
                // operators are all ASCII, and `i` may sit inside a multi-byte
                // UTF-8 sequence (e.g. when `cmd` contains a path like
                // `café.txt`), where `&str` slicing would panic.
                if bytes[i..].starts_with(op.as_bytes()) {
                    return Some(OpHit {
                        index: i,
                        operator: op,
                    });
                }
            }
        }
        i += 1;
    }
    None
}

fn shell_words(segment: &str) -> Option<Vec<String>> {
    let mut words = Vec::new();
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut current = String::new();
    for ch in segment.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if ch == q {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '"' || ch == '\'' {
            quote = Some(ch);
            continue;
        }
        if ch.is_whitespace() {
            if !current.is_empty() {
                words.push(std::mem::take(&mut current));
            }
            continue;
        }
        current.push(ch);
    }
    if escaped {
        current.push('\\');
    }
    if quote.is_some() {
        return None;
    }
    if !current.is_empty() {
        words.push(current);
    }
    Some(words)
}

fn skip_env_assignments(tokens: &[String], start: usize) -> usize {
    let mut i = start;
    while i < tokens.len() && ENV_ASSIGN_RE.is_match(&tokens[i]) {
        i += 1;
    }
    i
}

fn skip_leading_options(tokens: &[String], start: usize) -> usize {
    let mut i = start;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--" {
            return i + 1;
        }
        if !token.starts_with('-') {
            return i;
        }
        let has_eq = token.contains('=');
        let option_name = if has_eq {
            token.split('=').next().unwrap()
        } else {
            token.as_str()
        };
        i += 1;
        if !has_eq && OPTION_TAKES_VALUE.contains(option_name) && i < tokens.len() {
            i += 1;
        }
    }
    i
}

fn shell_command_arg(tokens: &[String], start: usize) -> Option<String> {
    for (offset, token) in tokens[start..].iter().enumerate() {
        let i = start + offset;
        if token == "-c"
            || (token.starts_with('-') && !token.starts_with("--") && token.contains('c'))
        {
            return tokens.get(i + 1).cloned();
        }
    }
    None
}

fn env_command_args(tokens: &[String], start: usize) -> Vec<String> {
    let mut i = start;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--" {
            i += 1;
            break;
        }
        if ENV_ASSIGN_RE.is_match(token) {
            i += 1;
            continue;
        }
        if token.starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    tokens[i..].to_vec()
}

fn parse_python_module(tokens: &[String], start: usize) -> Option<BashParse> {
    let module_flag = find_python_module_flag(tokens, start)?;
    if module_flag + 1 >= tokens.len() {
        return None;
    }
    let module = normalize_binary(&tokens[module_flag + 1]);
    if module == "pytest" {
        return Some(verb("pytest", None));
    }
    if module == "pip" || module == "pip3" {
        let sub_index = skip_leading_options(tokens, module_flag + 2);
        if sub_index < tokens.len() {
            return Some(verb(&module, Some(&tokens[sub_index])));
        }
        return Some(verb(&module, None));
    }
    Some(verb(&module, None))
}

fn find_python_module_flag(tokens: &[String], start: usize) -> Option<usize> {
    let mut i = start;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--" {
            return None;
        }
        if token == "-m" {
            return Some(i);
        }
        if token == "-c" {
            return None;
        }
        if !token.starts_with('-') || token == "-" {
            return None;
        }
        let has_eq = token.contains('=');
        let option_name = if has_eq {
            token.split('=').next().unwrap()
        } else {
            token.as_str()
        };
        if !has_eq && PYTHON_OPTION_TAKES_VALUE.contains(option_name) {
            i += 1;
        }
        i += 1;
    }
    None
}

fn normalize_binary(raw: &str) -> String {
    raw.split('/')
        .rfind(|s| !s.is_empty())
        .unwrap_or(raw)
        .to_string()
}
