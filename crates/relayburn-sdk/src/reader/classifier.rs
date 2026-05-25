//! Activity classifier — Rust port of `packages/reader/src/classifier.ts`.
//!
//! The classifier is rule-based and deterministic. The lookup tables live as
//! `phf` static maps (perfect-hash, zero allocation, zero startup cost) and
//! the bash heuristics are expressed as [`BashRule`] data — pattern + optional
//! `forbid` clause that emulates the TS negative-lookahead idioms — instead of
//! a stringly-typed post-filter. Adding a new harness is still a single-file
//! change: drop entries into the relevant table.

use std::sync::LazyLock;

use phf::{phf_map, phf_set};
use regex::Regex;
use serde_json::{Map, Value};

use crate::reader::types::{ActivityCategory, ToolCall};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationInput<'a> {
    pub tool_calls: &'a [ToolCall],
    pub text: &'a str,
    pub has_failed_tool: bool,
    pub reasoning_tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClassificationResult {
    pub activity: ActivityCategory,
    pub retries: u64,
    pub has_edits: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashParse {
    pub binary: String,
    pub subcommand: Option<String>,
    pub normalized: String,
}

// ---------------------------------------------------------------------------
// Static tool-name tables (phf — compile-time perfect hashing).
// ---------------------------------------------------------------------------

static EDIT_TOOLS: phf::Set<&'static str> = phf_set! {
    "Edit", "Write", "NotebookEdit", "MultiEdit",
};

static DELEGATION_TOOLS: phf::Set<&'static str> = phf_set! { "Agent", "Task" };

/// Harness-specific tool names mapped to the canonical (Claude Code) names the
/// rule tables are written against. Adding a new harness is a one-line change
/// here.
static TOOL_ALIASES: phf::Map<&'static str, &'static str> = phf_map! {
    // Codex
    "apply_patch"       => "Edit",
    "exec_command"      => "Bash",
    "shell"             => "Bash",
    "read_file"         => "Read",
    "write_file"        => "Write",
    "update_plan"       => "ExitPlanMode",
    "spawn_agent"       => "Agent",
    "send_input"        => "Task",
    "wait_agent"        => "Task",
    "close_agent"       => "Task",
    "resume_agent"      => "Task",
    "view_image"        => "Read",
    "read_mcp_resource" => "Read",
    // OpenCode (lowercase names)
    "read"     => "Read",
    "write"    => "Write",
    "edit"     => "Edit",
    "bash"     => "Bash",
    "grep"     => "Grep",
    "glob"     => "Glob",
    "webfetch" => "WebFetch",
    "task"     => "Task",
};

pub fn normalize_tool_name(name: &str) -> &str {
    TOOL_ALIASES.get(name).copied().unwrap_or(name)
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

// ---------------------------------------------------------------------------
// Activity-keyword regexes (case-insensitive, word-boundary).
// ---------------------------------------------------------------------------

fn build_re(s: &str) -> Regex {
    Regex::new(s).expect("classifier regex failed to compile")
}

static DEBUG_RE: LazyLock<Regex> = LazyLock::new(|| {
    build_re(
        r"(?i)\b(bug|error|crash|traceback|stack\s*trace|failure|failing|broken|fix\s+the|not\s+working|throws?)\b",
    )
});
static REVIEW_RE: LazyLock<Regex> = LazyLock::new(|| {
    build_re(r"(?i)\b(review|audit|inspect|look\s+over|code\s+review|pr\s+review)\b")
});
static REFACTOR_RE: LazyLock<Regex> = LazyLock::new(|| {
    build_re(
        r"(?i)\b(refactor|refactoring|cleanup|clean\s+up|rename|extract|restructure|move\s+this|reorganize)\b",
    )
});
static FEATURE_RE: LazyLock<Regex> = LazyLock::new(|| {
    build_re(r"(?i)\b(add|create|implement|new\s+feature|build\s+the|introduce|support\s+for)\b")
});
static BRAINSTORM_RE: LazyLock<Regex> = LazyLock::new(|| {
    build_re(
        r"(?i)\b(brainstorm|what\s+if|think\s+through|explore(?:\s+ideas)?|design|should\s+we|approach(?:es)?)\b",
    )
});
static PLANNING_RE: LazyLock<Regex> =
    LazyLock::new(|| build_re(r"(?i)\b(plan(?:ning)?|outline|roadmap|strategy)\b"));

/// Matches a leading `KEY=` shell env-assignment token. Shared between
/// `skip_env_assignments` and `env_command_args` so the same compiled regex
/// is reused.
static ENV_ASSIGN_RE: LazyLock<Regex> = LazyLock::new(|| build_re(r"^[A-Za-z_][A-Za-z0-9_]*="));

// ---------------------------------------------------------------------------
// BashRule — pattern plus optional `forbid` clause that emulates the TS
// negative-lookahead idioms (e.g. `(?!.*--check\b)`). Encodes intent as data
// instead of as a stringly-typed post-filter.
// ---------------------------------------------------------------------------

struct BashRule {
    pattern: Regex,
    forbid: Option<Regex>,
}

impl BashRule {
    fn pattern(p: &str) -> Self {
        Self {
            pattern: build_re(p),
            forbid: None,
        }
    }

    fn forbidding(mut self, forbid: &str) -> Self {
        self.forbid = Some(build_re(forbid));
        self
    }

    fn matches(&self, cmd: &str) -> bool {
        self.pattern.is_match(cmd) && !self.forbid.as_ref().is_some_and(|r| r.is_match(cmd))
    }
}

fn any_match(rules: &[BashRule], cmd: &str) -> bool {
    rules.iter().any(|r| r.matches(cmd))
}

// Bash heuristics — match on the first non-env token after stripping leading
// `FOO=bar` assignments (e.g. `CI=1 pytest` -> `pytest`).
static TEST_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    [
        r"\bpytest\b",
        r"\bpython\s+-m\s+pytest\b",
        r"\bvitest\b",
        r"\bbun\s+test\b",
        r"\bjest\b",
        r"\bmocha\b",
        r"\brspec\b",
        r"\bphpunit\b",
        r"\bgo\s+test\b",
        r"\bcargo\s+test\b",
        r"\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?test\b",
        r"\bnode\s+--test\b",
        r"\bmake\s+test\b",
        r"\bctest\b",
        r"\bplaywright\b",
        r"\bcypress\b",
        r"\bpuppeteer\b",
    ]
    .into_iter()
    .map(BashRule::pattern)
    .collect()
});

static REVIEW_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    [
        r"\bgit\s+status\b",
        r"\bgit\s+diff\b",
        r"\bgit\s+show\b",
        r"\bgit\s+log\b",
        r"\bgit\s+blame\b",
        r"\bgh\s+pr\s+(?:view|diff|checks)\b",
        r"\bgh\s+run\s+view\b",
    ]
    .into_iter()
    .map(BashRule::pattern)
    .collect()
});

static GIT_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    vec![BashRule::pattern(
        r"\bgit\s+(?:push|pull|fetch|commit|merge|rebase|checkout|cherry-pick|reset|revert|switch|tag|stash)\b",
    )]
});

static DEPS_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    [
        r"\b(?:npm|yarn|pnpm|bun)\s+(?:install|add|remove|uninstall|update|upgrade|ci)\b",
        r"\bpip\s+(?:install|uninstall)\b",
        r"\bpip3\s+(?:install|uninstall)\b",
        r"\bpython\s+-m\s+pip\s+(?:install|uninstall)\b",
        r"\buv\s+(?:add|remove|sync|pip\s+install)\b",
        r"\bpoetry\s+(?:add|remove|install|update)\b",
        r"\bcargo\s+(?:add|remove|update)\b",
        r"\bgo\s+(?:get|mod\s+(?:tidy|download))\b",
        r"\bbundle\s+(?:install|update|add)\b",
        r"\bgem\s+(?:install|uninstall)\b",
        r"\bbrew\s+(?:install|uninstall|upgrade|update)\b",
        r"\bapt(?:-get)?\s+(?:install|remove)\b",
    ]
    .into_iter()
    .map(BashRule::pattern)
    .collect()
});

// `forbidding(r"--check\b")` encodes the TS `(?!.*--check\b)` negative
// lookahead — `--check` means "verify, don't mutate" so it should never be
// classified as `format`.
static FORMAT_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    vec![
        BashRule::pattern(r"\bprettier\b.*(?:--write|-w)(?:\s|$)"),
        BashRule::pattern(r"\beslint\b.*--fix\b"),
        BashRule::pattern(r"\bbiome\s+format\b"),
        BashRule::pattern(r"\bbiome\s+check\b.*--apply\b"),
        BashRule::pattern(r"\bblack\b").forbidding(r"--check\b"),
        BashRule::pattern(r"\bruff\s+format\b"),
        BashRule::pattern(r"\bisort\b"),
        BashRule::pattern(r"\brustfmt\b"),
        BashRule::pattern(r"\bcargo\s+fmt\b").forbidding(r"--check\b"),
        BashRule::pattern(r"\bgofmt\b"),
        BashRule::pattern(r"\bgoimports\b"),
        BashRule::pattern(r"\bdprint\s+fmt\b"),
    ]
});

static VERIFICATION_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    vec![
        BashRule::pattern(r"\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?lint\b"),
        BashRule::pattern(r"\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?typecheck\b"),
        BashRule::pattern(r"\bprettier\b.*--check\b"),
        BashRule::pattern(r"\beslint\b").forbidding(r"--fix\b"),
        BashRule::pattern(r"\bbiome\s+check\b").forbidding(r"--apply\b"),
        BashRule::pattern(r"\bblack\b.*--check\b"),
        BashRule::pattern(r"\bruff\s+check\b"),
        BashRule::pattern(r"\bflake8\b"),
        BashRule::pattern(r"\bmypy\b"),
        BashRule::pattern(r"\bpyright\b"),
        BashRule::pattern(r"\btsc\b").forbidding(r"\btsc\s+--build\b"),
        BashRule::pattern(r"\bcargo\s+check\b"),
        BashRule::pattern(r"\bcargo\s+fmt\b.*--check\b"),
        BashRule::pattern(r"\bgolangci-lint\b"),
        BashRule::pattern(r"\bshellcheck\b"),
        BashRule::pattern(r"\bhadolint\b"),
        BashRule::pattern(r"\bterraform\s+validate\b"),
        BashRule::pattern(r"\bmake\s+(?:lint|check|typecheck|verify)\b"),
    ]
});

static BUILD_DEPLOY_RULES: LazyLock<Vec<BashRule>> = LazyLock::new(|| {
    [
        r"\bdocker\s+(?:build|compose\s+build|push)\b",
        r"\bcargo\s+build\b",
        r"\bgo\s+build\b",
        r"\bmake\s+(?:build|release|dist|package|deploy)\b",
        r"\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?build\b",
        r"\b(?:webpack|vite|next|rollup|esbuild)\s+build\b",
        r"\btsc\s+--build\b",
        r"\bpm2\s+",
        r"\bkubectl\s+(?:apply|rollout|set)\b",
        r"\bhelm\s+(?:install|upgrade)\b",
        r"\bterraform\s+(?:apply|plan)\b",
        r"\bterraform\s+destroy\b",
        r"\bserverless\s+deploy\b",
        r"\b(?:vercel|netlify|flyctl|railway|sst)\s+(?:deploy|up)\b",
    ]
    .into_iter()
    .map(BashRule::pattern)
    .collect()
});

static DOC_FILE_RULES: LazyLock<Vec<Regex>> = LazyLock::new(|| {
    [
        r"(?i)\.md$",
        r"(?i)\.mdx$",
        r"(?i)\.rst$",
        r"(?i)\.adoc$",
        r"(?i)\.txt$",
        r"(?i)(?:^|/)README(?:\.[^/]*)?$",
        r"(?i)(?:^|/)CHANGELOG(?:\.[^/]*)?$",
        r"(?:^|/)docs/",
    ]
    .into_iter()
    .map(build_re)
    .collect()
});

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

// ---------------------------------------------------------------------------
// classify_activity priority ladder.
// ---------------------------------------------------------------------------

pub fn classify_activity(input: ClassificationInput<'_>) -> ClassificationResult {
    let has_edits = input
        .tool_calls
        .iter()
        .any(|t| EDIT_TOOLS.contains(normalize_tool_name(&t.name)));
    let retries = count_retries(input.tool_calls);
    let activity = pick_category(PickInput {
        tool_calls: input.tool_calls,
        text: input.text,
        has_failed_tool: input.has_failed_tool,
        has_edits,
        retries,
        reasoning_tokens: input.reasoning_tokens,
    });
    ClassificationResult {
        activity,
        retries,
        has_edits,
    }
}

struct PickInput<'a> {
    tool_calls: &'a [ToolCall],
    text: &'a str,
    has_failed_tool: bool,
    has_edits: bool,
    retries: u64,
    reasoning_tokens: u64,
}

fn pick_category(p: PickInput<'_>) -> ActivityCategory {
    // Priority 1: delegation
    if p.tool_calls
        .iter()
        .any(|t| DELEGATION_TOOLS.contains(normalize_tool_name(&t.name)))
    {
        return ActivityCategory::Delegation;
    }
    // Priority 2: explicit plan-mode marker
    if p.tool_calls
        .iter()
        .any(|t| normalize_tool_name(&t.name) == "ExitPlanMode")
    {
        return ActivityCategory::Planning;
    }
    // Priority 3: edits present
    if p.has_edits {
        if p.has_failed_tool {
            return ActivityCategory::Debugging;
        }
        if p.retries >= 2 {
            return ActivityCategory::Debugging;
        }
        if all_edits_are_docs(p.tool_calls) {
            return ActivityCategory::Docs;
        }
        if let Some(refined) = refine_edit_by_keywords(p.text) {
            return refined;
        }
        return ActivityCategory::Coding;
    }
    // Priority 4: failed tool on a non-edit turn
    if p.has_failed_tool {
        return ActivityCategory::Debugging;
    }
    // Priority 5: bash heuristics
    for tc in p
        .tool_calls
        .iter()
        .filter(|t| normalize_tool_name(&t.name) == "Bash")
    {
        let cmd = strip_env(tc.target.as_deref().unwrap_or(""));
        if cmd.is_empty() {
            continue;
        }
        if any_match(&TEST_RULES, &cmd) {
            return ActivityCategory::Testing;
        }
        if any_match(&REVIEW_RULES, &cmd) {
            return ActivityCategory::Review;
        }
        if any_match(&GIT_RULES, &cmd) {
            return ActivityCategory::Git;
        }
        if any_match(&DEPS_RULES, &cmd) {
            return ActivityCategory::Deps;
        }
        if any_match(&FORMAT_RULES, &cmd) {
            return ActivityCategory::Format;
        }
        if any_match(&VERIFICATION_RULES, &cmd) {
            return ActivityCategory::Verification;
        }
        if any_match(&BUILD_DEPLOY_RULES, &cmd) {
            return ActivityCategory::BuildDeploy;
        }
    }
    // Priority 6: any tools used at all
    if !p.tool_calls.is_empty() {
        if let Some(refined) = refine_intent_by_keywords(p.text) {
            return refined;
        }
        return ActivityCategory::Exploration;
    }
    // Priority 7: keyword-only
    if p.has_failed_tool {
        return ActivityCategory::Debugging;
    }
    if let Some(refined) = refine_intent_by_keywords(p.text) {
        return refined;
    }
    if BRAINSTORM_RE.is_match(p.text) {
        return ActivityCategory::Brainstorming;
    }
    if PLANNING_RE.is_match(p.text) {
        return ActivityCategory::Planning;
    }
    if p.reasoning_tokens > 0 {
        return ActivityCategory::Reasoning;
    }
    ActivityCategory::Conversation
}

fn all_edits_are_docs(tool_calls: &[ToolCall]) -> bool {
    let mut saw_edit = false;
    for tc in tool_calls
        .iter()
        .filter(|t| EDIT_TOOLS.contains(normalize_tool_name(&t.name)))
    {
        saw_edit = true;
        let Some(target) = tc.target.as_deref() else {
            return false;
        };
        if target.is_empty() || !DOC_FILE_RULES.iter().any(|re| re.is_match(target)) {
            return false;
        }
    }
    saw_edit
}

fn refine_edit_by_keywords(text: &str) -> Option<ActivityCategory> {
    if text.is_empty() {
        return None;
    }
    if DEBUG_RE.is_match(text) {
        return Some(ActivityCategory::Debugging);
    }
    if REFACTOR_RE.is_match(text) {
        return Some(ActivityCategory::Refactoring);
    }
    if FEATURE_RE.is_match(text) {
        return Some(ActivityCategory::Feature);
    }
    None
}

fn refine_intent_by_keywords(text: &str) -> Option<ActivityCategory> {
    if text.is_empty() {
        return None;
    }
    if DEBUG_RE.is_match(text) {
        return Some(ActivityCategory::Debugging);
    }
    if REVIEW_RE.is_match(text) {
        return Some(ActivityCategory::Review);
    }
    if REFACTOR_RE.is_match(text) {
        return Some(ActivityCategory::Refactoring);
    }
    if FEATURE_RE.is_match(text) {
        return Some(ActivityCategory::Feature);
    }
    None
}

fn strip_env(cmd: &str) -> String {
    static RE: LazyLock<Regex> = LazyLock::new(|| build_re(r"^(?:\s*[A-Z_][A-Z0-9_]*=\S+\s+)+"));
    RE.replace(cmd, "").into_owned()
}

pub fn count_retries(tool_calls: &[ToolCall]) -> u64 {
    let mut retries: u64 = 0;
    let mut seen_edit = false;
    let mut seen_bash_after_edit = false;
    for tc in tool_calls {
        let name = normalize_tool_name(&tc.name);
        if EDIT_TOOLS.contains(name) {
            if seen_edit && seen_bash_after_edit {
                retries += 1;
                seen_bash_after_edit = false;
            }
            seen_edit = true;
        } else if name == "Bash" && seen_edit {
            seen_bash_after_edit = true;
        }
    }
    retries
}

// ---------------------------------------------------------------------------
// Harness-injected `<task-notification>` row detector.
//
// Claude Code's harness writes synthetic rows when a background Bash task
// finishes. They share the envelope shape of queued user prompts, so a
// text-prefix-only filter (just checking for `<task-notification>` in
// content) would false-positive on a real user prompt that legitimately
// types that string. Each clause AND-checks shape AND purpose so
// legitimate prompts with the same shape but a different `commandMode` /
// `origin.kind` survive.
// ---------------------------------------------------------------------------

/// True when a raw JSONL row represents a harness-injected
/// `<task-notification>` synthetic message rather than a real user prompt.
/// Three clauses, each ANDing a shape check with a purpose check:
///
/// 1. `type == "queue-operation"` AND `content` is a string starting with
///    `<task-notification>`.
/// 2. `origin.kind == "task-notification"`.
/// 3. `attachment.type == "queued_command"` AND
///    `attachment.commandMode == "task-notification"`.
pub fn is_task_notification(row: &Map<String, Value>) -> bool {
    // Clause 1: queue-operation row whose top-level content starts with the
    // `<task-notification>` marker.
    if row.get("type").and_then(Value::as_str) == Some("queue-operation")
        && row
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("<task-notification>"))
    {
        return true;
    }
    // Clause 2: explicit `origin.kind` marker. The shape check is implicit
    // in dereferencing `origin` as an object; the purpose check is the
    // `task-notification` string match.
    if row
        .get("origin")
        .and_then(Value::as_object)
        .and_then(|o| o.get("kind"))
        .and_then(Value::as_str)
        == Some("task-notification")
    {
        return true;
    }
    // Clause 3: queued-command attachment whose `commandMode` is
    // `task-notification`. Both fields must match — a legitimate queued
    // user prompt has `attachment.type == "queued_command"` but a different
    // `commandMode`, and must survive this check.
    if let Some(att) = row.get("attachment").and_then(Value::as_object) {
        let ty = att.get("type").and_then(Value::as_str);
        let mode = att.get("commandMode").and_then(Value::as_str);
        if ty == Some("queued_command") && mode == Some("task-notification") {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Slash-command triad detector — see #438.
//
// Claude Code's slash commands (`/review`, `/init`, custom skills) emit a
// deterministic three-row sequence in the JSONL transcript:
//
//   caveat       — synthetic user row introducing the command. Body opens
//                  with the literal `Caveat:` prefix and the row is the
//                  apparent root of the parent chain for the next two
//                  rows. Carries no real user intent — it's a harness
//                  artifact.
//   invocation   — user row whose `parentUuid == caveat.uuid`. Body
//                  carries the synthetic command envelope:
//                  `<command-name>`, `<command-message>`, optionally
//                  `<command-args>`.
//   stdout       — user row whose `parentUuid == invocation.uuid`. Body
//                  carries the captured stdout in a `<local-command-stdout>`
//                  block.
//
// The classifier historically treated each row as a separate activity,
// which trebled the apparent activity count for sessions that lean on
// slash commands. The detector here collapses the triad into one
// synthetic `Skill` activity for downstream rollups; token attribution
// stays on the underlying rows so `burn hotspots` isn't double-charged.
//
// Pinning detection on parent-UUID chain shape (not on the exact text
// prefix of the caveat row) is deliberate: the literal `Caveat:` opener
// has drifted across Claude Code versions, but the chain shape — three
// rows linked caveat → invocation → stdout — has stayed stable. We use
// the text markers `<command-name>` and `<local-command-stdout>` as
// purpose checks on the invocation and stdout rows (so an unrelated
// three-row chain that happens to look structurally similar — e.g. a
// real user prompt followed by an assistant reply followed by a tool
// result — does NOT misdetect), but the chain-shape predicate carries
// the primary signal. See `is_task_notification` (#442) for the same
// shape-AND-purpose pattern.

const CAVEAT_PREFIX: &str = "Caveat:";
const COMMAND_NAME_OPEN: &str = "<command-name>";
const COMMAND_NAME_CLOSE: &str = "</command-name>";
const LOCAL_STDOUT_OPEN: &str = "<local-command-stdout>";

/// One detected slash-command triad: the three row indices into the
/// original `rows` slice and the extracted skill name (when the
/// invocation body exposed a `<command-name>` block; `None` otherwise).
///
/// Indices are stable references into the caller's input — the
/// downstream wiring in the Claude reader uses them to override per-row
/// activity attribution without re-walking the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashTriad {
    pub caveat_idx: usize,
    pub invocation_idx: usize,
    pub stdout_idx: usize,
    pub skill_name: Option<String>,
}

/// Detect Claude slash-command triads in a flat slice of raw JSONL rows
/// (already-parsed as `serde_json::Map`s).
///
/// The walk is O(n): build a `uuid -> index` index once, then for each
/// candidate caveat row look up the two children by their UUID chain.
/// Each row is consumed by at most one triad — the row-index sets are
/// disjoint by construction (we mark each used index in a bitset).
///
/// Caller obligations:
///
/// - The slice must preserve JSONL emission order. The detector does
///   not sort or filter — it inspects rows in place.
/// - Rows are matched on the parent-UUID chain shape; text checks on
///   the invocation and stdout rows are purpose guards that block
///   structurally-similar but semantically-different chains (e.g. a
///   normal user → assistant → tool_result chain) from misdetecting.
pub fn detect_slash_triads(rows: &[Map<String, Value>]) -> Vec<SlashTriad> {
    if rows.len() < 3 {
        return Vec::new();
    }
    // Track rows already consumed by a prior triad so a single row can't
    // be the invocation of triad A and the caveat of triad B simultaneously.
    let mut consumed = vec![false; rows.len()];
    let mut out: Vec<SlashTriad> = Vec::new();

    for (caveat_idx, caveat) in rows.iter().enumerate() {
        if consumed[caveat_idx] {
            continue;
        }
        if !is_caveat_row(caveat) {
            continue;
        }
        let caveat_uuid = match caveat.get("uuid").and_then(Value::as_str) {
            Some(u) if !u.is_empty() => u,
            _ => continue,
        };
        // The invocation row must (a) point at the caveat via parentUuid
        // AND (b) carry an invocation body. The latter is the purpose
        // check that blocks an unrelated child of the caveat from
        // promoting the chain into a Skill.
        let (invocation_idx, invocation) = match find_first_unconsumed_child(
            rows,
            &consumed,
            caveat_uuid,
            caveat_idx + 1,
            is_invocation_row,
        ) {
            Some(pair) => pair,
            None => continue,
        };
        let invocation_uuid = match invocation.get("uuid").and_then(Value::as_str) {
            Some(u) if !u.is_empty() => u,
            _ => continue,
        };
        let (stdout_idx, _stdout) = match find_first_unconsumed_child(
            rows,
            &consumed,
            invocation_uuid,
            invocation_idx + 1,
            is_stdout_row,
        ) {
            Some(pair) => pair,
            None => continue,
        };
        let skill_name = extract_skill_name(invocation).or_else(|| extract_skill_name(caveat));
        consumed[caveat_idx] = true;
        consumed[invocation_idx] = true;
        consumed[stdout_idx] = true;
        out.push(SlashTriad {
            caveat_idx,
            invocation_idx,
            stdout_idx,
            skill_name,
        });
    }
    out
}

/// True when this row matches the caveat shape: a user-typed row whose
/// extracted text body begins with the `Caveat:` literal. We deliberately
/// keep this lightweight — the structural check (caveat is the root of
/// the chain a downstream invocation/stdout points at) carries the
/// primary signal.
fn is_caveat_row(row: &Map<String, Value>) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    extract_row_text(row)
        .map(|s| s.trim_start().starts_with(CAVEAT_PREFIX))
        .unwrap_or(false)
}

/// True when this row carries a `<command-name>` block — the invocation
/// payload of a slash command.
fn is_invocation_row(row: &Map<String, Value>) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    extract_row_text(row)
        .map(|s| s.contains(COMMAND_NAME_OPEN))
        .unwrap_or(false)
}

/// True when this row carries a `<local-command-stdout>` block — the
/// stdout-capture payload of a slash command.
fn is_stdout_row(row: &Map<String, Value>) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    extract_row_text(row)
        .map(|s| s.contains(LOCAL_STDOUT_OPEN))
        .unwrap_or(false)
}

/// Walk forward from `start_idx` looking for the first not-yet-consumed
/// row whose `parentUuid == parent_uuid` and which passes the purpose
/// `check`. Returns `(idx, &row)` or `None`. Forward-only because the
/// JSONL ordering for these rows is stable in practice (the harness
/// writes caveat → invocation → stdout adjacently); a sibling reorder
/// would land both the invocation and stdout *after* the caveat.
fn find_first_unconsumed_child<'a, F>(
    rows: &'a [Map<String, Value>],
    consumed: &[bool],
    parent_uuid: &str,
    start_idx: usize,
    check: F,
) -> Option<(usize, &'a Map<String, Value>)>
where
    F: Fn(&Map<String, Value>) -> bool,
{
    for (offset, row) in rows[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        if consumed[idx] {
            continue;
        }
        let pu = row.get("parentUuid").and_then(Value::as_str).unwrap_or("");
        if pu != parent_uuid {
            continue;
        }
        if check(row) {
            return Some((idx, row));
        }
    }
    None
}

/// Pull `<command-name>...</command-name>` text from either the caveat
/// or invocation row. Returns `None` if no marker block is present.
fn extract_skill_name(row: &Map<String, Value>) -> Option<String> {
    let text = extract_row_text(row)?;
    let open = text.find(COMMAND_NAME_OPEN)?;
    let after = &text[open + COMMAND_NAME_OPEN.len()..];
    let close = after.find(COMMAND_NAME_CLOSE)?;
    let raw = after[..close].trim();
    if raw.is_empty() {
        return None;
    }
    // Tolerate the optional leading `/` (matches the ghost_surface
    // miner's accept-with-or-without convention).
    Some(raw.trim_start_matches('/').to_string())
}

/// Extract the row's plain text body (string content, or concatenation
/// of `text` / `content` strings inside an array body). Mirrors
/// `extract_plain_user_text_from_obj` from the Claude reader closely
/// enough to detect markers; we read the `content` field of a user-typed
/// row whose body may be a string or a list of blocks.
fn extract_row_text(row: &Map<String, Value>) -> Option<String> {
    let body = row.get("message").and_then(|m| m.get("content"))?;
    if let Some(s) = body.as_str() {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    let arr = body.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        // `{"type":"text","text":"..."}` — assistant-style text block.
        if bo.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(s) = bo.get("text").and_then(Value::as_str) {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
            continue;
        }
        // `{"type":"tool_result","content":"..."}` — string-content tool
        // result envelope. The slash-command stdout row can ship its
        // payload this way; we still want the marker visible to the
        // purpose check.
        if let Some(s) = bo.get("content").and_then(Value::as_str) {
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    fn tc(name: &str, target: Option<&str>) -> ToolCall {
        ToolCall {
            id: name.to_string(),
            name: name.to_string(),
            target: target.map(|t| t.to_string()),
            args_hash: "h".to_string(),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        }
    }

    fn classify(tools: &[ToolCall]) -> ClassificationResult {
        classify_activity(ClassificationInput {
            tool_calls: tools,
            text: "",
            has_failed_tool: false,
            reasoning_tokens: 0,
        })
    }

    #[test]
    fn delegation_dominates_even_with_edits() {
        let r = classify(&[tc("Agent", Some("Explore")), tc("Edit", Some("/a.ts"))]);
        assert_eq!(r.activity, ActivityCategory::Delegation);
    }

    #[test]
    fn exit_plan_mode_is_planning() {
        let r = classify(&[tc("ExitPlanMode", None)]);
        assert_eq!(r.activity, ActivityCategory::Planning);
    }

    #[test]
    fn bash_test_runners_are_testing() {
        let cases = [
            "pytest",
            "vitest run",
            "bun test",
            "npm test",
            "go test ./...",
            "cargo test",
            "node --test",
            "make test",
        ];
        for cmd in cases {
            let r = classify(&[tc("Bash", Some(cmd))]);
            assert_eq!(r.activity, ActivityCategory::Testing, "case: {cmd}");
        }
    }

    #[test]
    fn read_only_git_inspection_is_review() {
        for cmd in [
            "git status",
            "git diff --stat",
            "git show HEAD~1",
            "gh pr diff 123",
            "gh pr view 123",
        ] {
            let r = classify(&[tc("Bash", Some(cmd))]);
            assert_eq!(r.activity, ActivityCategory::Review, "case: {cmd}");
        }
    }

    #[test]
    fn ignores_leading_env_assignments() {
        let r = classify(&[tc("Bash", Some("CI=1 NODE_ENV=test pytest -q"))]);
        assert_eq!(r.activity, ActivityCategory::Testing);
    }

    #[test]
    fn git_state_change_is_git() {
        for cmd in [
            "git push origin main",
            r#"git commit -m "x""#,
            "git rebase main",
        ] {
            let r = classify(&[tc("Bash", Some(cmd))]);
            assert_eq!(r.activity, ActivityCategory::Git, "case: {cmd}");
        }
    }

    #[test]
    fn build_deploy_commands() {
        for cmd in [
            "npm run build",
            "docker build .",
            "cargo build --release",
            "kubectl apply -f k8s/",
            "terraform apply",
            "make build",
        ] {
            let r = classify(&[tc("Bash", Some(cmd))]);
            assert_eq!(r.activity, ActivityCategory::BuildDeploy, "case: {cmd}");
        }
    }

    #[test]
    fn lint_typecheck_is_verification() {
        for cmd in [
            "npm run lint",
            "eslint .",
            "ruff check src/",
            "cargo check",
            "tsc --noEmit",
            "make lint",
            "prettier --check .",
            "cargo fmt --check",
        ] {
            let r = classify(&[tc("Bash", Some(cmd))]);
            assert_eq!(r.activity, ActivityCategory::Verification, "case: {cmd}");
        }
    }

    #[test]
    fn make_lint_beats_build_deploy() {
        let r = classify(&[tc("Bash", Some("make lint"))]);
        assert_eq!(r.activity, ActivityCategory::Verification);
    }

    #[test]
    fn kubectl_logs_is_exploration() {
        let r = classify(&[tc("Bash", Some("kubectl logs deploy/api"))]);
        assert_eq!(r.activity, ActivityCategory::Exploration);
    }

    #[test]
    fn plain_edit_is_coding() {
        let r = classify(&[tc("Edit", Some("/a.ts")), tc("Write", Some("/b.ts"))]);
        assert_eq!(r.activity, ActivityCategory::Coding);
        assert!(r.has_edits);
    }

    #[test]
    fn read_only_tool_turns_are_exploration() {
        let r = classify(&[tc("Read", Some("/a.ts")), tc("Grep", Some("foo"))]);
        assert_eq!(r.activity, ActivityCategory::Exploration);
    }

    #[test]
    fn count_retries_counts_edit_bash_edit_cycles() {
        let calls = [tc("Edit", None), tc("Bash", None), tc("Edit", None)];
        assert_eq!(count_retries(&calls), 1);
        let calls = [
            tc("Edit", None),
            tc("Bash", None),
            tc("Edit", None),
            tc("Bash", None),
            tc("Edit", None),
        ];
        assert_eq!(count_retries(&calls), 2);
        let calls = [tc("Edit", None), tc("Edit", None)];
        assert_eq!(count_retries(&calls), 0);
        let calls = [tc("Bash", None), tc("Edit", None), tc("Edit", None)];
        assert_eq!(count_retries(&calls), 0);
    }

    #[test]
    fn parse_bash_normalizes_examples() {
        let cases = [
            ("pytest", "pytest"),
            ("python -m pytest -q", "pytest"),
            ("python -I -X dev -m pytest -q", "pytest"),
            ("vitest run", "vitest"),
            ("bun test", "bun test"),
            ("npm test", "npm test"),
            ("go test ./...", "go test"),
            ("cargo test", "cargo test"),
            ("node --test", "node --test"),
            ("make test", "make test"),
            ("git status", "git status"),
            ("git push origin main", "git push"),
            ("npm run build", "npm build"),
            ("docker build .", "docker build"),
            ("docker compose build", "docker compose build"),
            ("gh pr view 123", "gh pr view"),
            ("gh run view 123", "gh run view"),
            ("kubectl logs deploy/api", "kubectl logs"),
        ];
        for (cmd, expected) in cases {
            let parsed = parse_bash_command(cmd).expect("parse");
            assert_eq!(parsed.normalized, expected, "case: {cmd}");
        }
    }

    #[test]
    fn parse_bash_detects_compound_shell_syntax() {
        let parsed = parse_bash_command("if [ -f foo ]; then echo y; fi").expect("parse");
        assert_eq!(parsed.normalized, "(shell)");
    }

    #[test]
    fn parse_bash_strips_leading_cd() {
        let parsed = parse_bash_command("cd /tmp && pytest -q").expect("parse");
        assert_eq!(parsed.normalized, "pytest");
    }

    #[test]
    fn parse_bash_handles_multibyte_utf8_arguments() {
        // Regression: the operator scanner used to slice `cmd[i..]` at byte
        // offsets, which panics when `i` lands inside a multi-byte sequence
        // (e.g. an `é` in a path). Verify the parser stays alive.
        let parsed = parse_bash_command("cat café.txt && pytest -q").expect("parse");
        assert_eq!(parsed.normalized, "cat");
        let parsed = parse_bash_command("git commit -m \"日本語\"").expect("parse");
        assert_eq!(parsed.normalized, "git commit");
    }

    #[test]
    fn normalize_tool_name_aliases() {
        assert_eq!(normalize_tool_name("apply_patch"), "Edit");
        assert_eq!(normalize_tool_name("exec_command"), "Bash");
        assert_eq!(normalize_tool_name("read"), "Read");
        assert_eq!(normalize_tool_name("Bash"), "Bash");
        assert_eq!(normalize_tool_name("UnknownTool"), "UnknownTool");
    }

    #[test]
    fn doc_only_edits_classify_as_docs() {
        let r = classify(&[
            tc("Edit", Some("README.md")),
            tc("Write", Some("docs/foo.md")),
        ]);
        assert_eq!(r.activity, ActivityCategory::Docs);
    }

    #[test]
    fn failed_edit_classifies_as_debugging() {
        let r = classify_activity(ClassificationInput {
            tool_calls: &[tc("Edit", Some("/a.ts"))],
            text: "",
            has_failed_tool: true,
            reasoning_tokens: 0,
        });
        assert_eq!(r.activity, ActivityCategory::Debugging);
    }

    #[test]
    fn reasoning_only_no_tools_no_text_is_reasoning() {
        let r = classify_activity(ClassificationInput {
            tool_calls: &[],
            text: "",
            has_failed_tool: false,
            reasoning_tokens: 1234,
        });
        assert_eq!(r.activity, ActivityCategory::Reasoning);
    }

    #[test]
    fn empty_input_is_conversation() {
        let r = classify(&[]);
        assert_eq!(r.activity, ActivityCategory::Conversation);
    }

    #[test]
    fn bash_rule_forbid_clause_excludes_match() {
        // BashRule's `forbid` clause emulates the TS negative-lookahead idiom.
        let rule = BashRule::pattern(r"\bblack\b").forbidding(r"--check\b");
        assert!(rule.matches("black ."));
        assert!(!rule.matches("black --check ."));
    }

    // ---------------------------------------------------------------------
    // is_task_notification — see #439.
    //
    // Three positive clauses + one false-positive guard. Each positive
    // exercises one clause in isolation so a regression in any single
    // branch surfaces as a single failing test rather than a
    // hard-to-diagnose aggregate.
    // ---------------------------------------------------------------------

    fn row(json: serde_json::Value) -> Map<String, Value> {
        json.as_object().expect("fixture must be an object").clone()
    }

    #[test]
    fn task_notification_clause_1_queue_operation_with_prefix() {
        let r = row(serde_json::json!({
            "type": "queue-operation",
            "content": "<task-notification>background bash 1 finished</task-notification>",
        }));
        assert!(is_task_notification(&r));
    }

    #[test]
    fn task_notification_clause_2_origin_kind() {
        // Envelope looks like a normal user prompt but `origin.kind` marks
        // it as a synthetic task-notification — purpose check fires.
        let r = row(serde_json::json!({
            "type": "user",
            "origin": { "kind": "task-notification" },
            "message": { "role": "user", "content": "ignored" },
        }));
        assert!(is_task_notification(&r));
    }

    #[test]
    fn task_notification_clause_3_queued_command_attachment() {
        // Queued-command attachment with the task-notification commandMode.
        let r = row(serde_json::json!({
            "type": "user",
            "attachment": {
                "type": "queued_command",
                "commandMode": "task-notification",
            },
            "message": { "role": "user", "content": "ignored" },
        }));
        assert!(is_task_notification(&r));
    }

    #[test]
    fn task_notification_does_not_match_user_typed_marker_string() {
        // False-positive guard: a real user prompt that literally types
        // `<task-notification>` is NOT filtered, because neither
        // `origin.kind` nor `attachment.commandMode` is `task-notification`
        // and `type` is `user`, not `queue-operation`. This is the case
        // that motivates shape AND purpose matching over a text-prefix
        // sniff. See #439.
        let r = row(serde_json::json!({
            "type": "user",
            "message": {
                "role": "user",
                "content": "<task-notification>literal text in a prompt</task-notification>",
            },
            "origin": { "kind": "user-prompt" },
            "attachment": {
                "type": "queued_command",
                "commandMode": "user-prompt",
            },
        }));
        assert!(!is_task_notification(&r));
    }

    #[test]
    fn task_notification_clause_1_requires_prefix() {
        // Shape check for clause 1: `type == "queue-operation"` alone is
        // not enough — the content must start with `<task-notification>`.
        let r = row(serde_json::json!({
            "type": "queue-operation",
            "content": "some other queued payload",
        }));
        assert!(!is_task_notification(&r));
    }

    #[test]
    fn task_notification_clause_3_requires_both_fields() {
        // Shape check for clause 3: attachment.type must be `queued_command`
        // AND attachment.commandMode must be `task-notification`. Either
        // one alone does not match.
        let only_type = row(serde_json::json!({
            "type": "user",
            "attachment": { "type": "queued_command", "commandMode": "user-prompt" },
        }));
        assert!(!is_task_notification(&only_type));

        let only_mode = row(serde_json::json!({
            "type": "user",
            "attachment": { "type": "other", "commandMode": "task-notification" },
        }));
        assert!(!is_task_notification(&only_mode));
    }

    #[test]
    fn task_notification_empty_row_is_false() {
        let r = row(serde_json::json!({}));
        assert!(!is_task_notification(&r));
    }

    // -----------------------------------------------------------------
    // detect_slash_triads — see #438.
    //
    // Each test exercises one structural property of the detector in
    // isolation. The fixture builder below mirrors the Claude JSONL
    // shape just closely enough for the parent-UUID chain check and
    // the `<command-name>` / `<local-command-stdout>` purpose checks
    // to fire.
    // -----------------------------------------------------------------

    fn caveat(uuid: &str, parent: Option<&str>) -> Map<String, Value> {
        row(serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "message": { "role": "user", "content": "Caveat: harness preamble" },
        }))
    }

    fn invocation(uuid: &str, parent: &str, name: &str) -> Map<String, Value> {
        row(serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "message": {
                "role": "user",
                "content": format!(
                    "<command-message>{name} is running…</command-message>\n<command-name>/{name}</command-name>",
                ),
            },
        }))
    }

    fn stdout_row(uuid: &str, parent: &str, body: &str) -> Map<String, Value> {
        row(serde_json::json!({
            "type": "user",
            "uuid": uuid,
            "parentUuid": parent,
            "message": {
                "role": "user",
                "content": format!("<local-command-stdout>{body}</local-command-stdout>"),
            },
        }))
    }

    #[test]
    fn detect_slash_triads_finds_two_distinct_triads() {
        let rows = vec![
            // Real user prompt before any slash commands; must not be touched.
            row(serde_json::json!({
                "type": "user",
                "uuid": "u-prompt",
                "parentUuid": serde_json::Value::Null,
                "message": { "role": "user", "content": "hello" },
            })),
            // Triad 1: /review
            caveat("u-cav-1", Some("u-prompt")),
            invocation("u-inv-1", "u-cav-1", "review"),
            stdout_row("u-out-1", "u-inv-1", "no issues found"),
            // Interleaved real user prompt to prove the detector doesn't
            // greedily span across unrelated rows.
            row(serde_json::json!({
                "type": "user",
                "uuid": "u-prompt-2",
                "parentUuid": "u-out-1",
                "message": { "role": "user", "content": "thanks" },
            })),
            // Triad 2: /init
            caveat("u-cav-2", Some("u-prompt-2")),
            invocation("u-inv-2", "u-cav-2", "init"),
            stdout_row("u-out-2", "u-inv-2", "initialized"),
        ];
        let triads = detect_slash_triads(&rows);
        assert_eq!(triads.len(), 2, "two slash commands → two triads");
        assert_eq!(triads[0].caveat_idx, 1);
        assert_eq!(triads[0].invocation_idx, 2);
        assert_eq!(triads[0].stdout_idx, 3);
        assert_eq!(triads[0].skill_name.as_deref(), Some("review"));
        assert_eq!(triads[1].caveat_idx, 5);
        assert_eq!(triads[1].invocation_idx, 6);
        assert_eq!(triads[1].stdout_idx, 7);
        assert_eq!(triads[1].skill_name.as_deref(), Some("init"));
    }

    #[test]
    fn detect_slash_triads_rejects_caveat_without_invocation_child() {
        // A row whose text starts with `Caveat:` but whose downstream
        // chain doesn't carry a `<command-name>` invocation MUST NOT
        // promote into a triad. This is the false-positive guard: real
        // user content can begin with the word "Caveat:" and the
        // structural shape must still survive that overlap. Mirrors the
        // shape-AND-purpose pattern from `is_task_notification` (#442).
        let rows = vec![
            row(serde_json::json!({
                "type": "user",
                "uuid": "u-cav",
                "parentUuid": serde_json::Value::Null,
                "message": { "role": "user", "content": "Caveat: this is a normal user prompt that happens to start with the literal word Caveat." },
            })),
            // A child user row, but it's a normal prompt — no
            // `<command-name>` marker, so the purpose check fails.
            row(serde_json::json!({
                "type": "user",
                "uuid": "u-child",
                "parentUuid": "u-cav",
                "message": { "role": "user", "content": "follow-up from the user" },
            })),
            row(serde_json::json!({
                "type": "user",
                "uuid": "u-grand",
                "parentUuid": "u-child",
                "message": { "role": "user", "content": "another follow-up" },
            })),
        ];
        let triads = detect_slash_triads(&rows);
        assert!(
            triads.is_empty(),
            "false-positive guard: no triad without an invocation row",
        );
    }

    #[test]
    fn detect_slash_triads_rejects_broken_parent_chain() {
        // Caveat + invocation are present but the stdout row's parentUuid
        // doesn't link back to the invocation. Structural shape is the
        // primary signal — without the chain, no triad.
        let rows = vec![
            caveat("u-cav", None),
            invocation("u-inv", "u-cav", "review"),
            // Stdout body looks correct, but parents point at the wrong
            // row (sibling of the caveat instead of child of invocation).
            stdout_row("u-out", "u-cav", "should not match"),
        ];
        assert!(detect_slash_triads(&rows).is_empty());
    }

    #[test]
    fn detect_slash_triads_extracts_skill_name_with_or_without_slash() {
        // Skill name is extracted with the leading `/` trimmed off, and
        // unconditionally returns `None` when neither row exposes a
        // `<command-name>` block at all (e.g. a triad shape with a
        // truncated invocation body).
        let rows = vec![
            caveat("u-cav", None),
            invocation("u-inv", "u-cav", "init"), // → skill_name = "init"
            stdout_row("u-out", "u-inv", "ok"),
        ];
        let triads = detect_slash_triads(&rows);
        assert_eq!(triads[0].skill_name.as_deref(), Some("init"));

        // Purpose check requires the `<command-name>` marker on the
        // invocation row. A row that completes the parent-UUID chain but
        // carries an empty / unrelated body MUST NOT be picked up as an
        // invocation — the shape AND purpose checks both have to fire.
        let rows_no_name = vec![
            caveat("u-cav", None),
            row(serde_json::json!({
                "type": "user",
                "uuid": "u-inv",
                "parentUuid": "u-cav",
                "message": { "role": "user", "content": "no command marker here" },
            })),
            stdout_row("u-out", "u-inv", "ok"),
        ];
        assert!(
            detect_slash_triads(&rows_no_name).is_empty(),
            "invocation without `<command-name>` doesn't match",
        );
    }

    #[test]
    fn detect_slash_triads_returns_empty_for_short_input() {
        // Inputs shorter than three rows can't contain a triad. Cheap
        // guard so the row-by-row scan doesn't waste work on tiny
        // session prefixes.
        let empty: Vec<Map<String, Value>> = Vec::new();
        assert!(detect_slash_triads(&empty).is_empty());
        let one = vec![caveat("u-cav", None)];
        assert!(detect_slash_triads(&one).is_empty());
        let two = vec![caveat("u-cav", None), invocation("u-inv", "u-cav", "x")];
        assert!(detect_slash_triads(&two).is_empty());
    }

    #[test]
    fn detect_slash_triads_does_not_consume_rows_twice() {
        // Two triads share the boundary row (the stdout of triad 1 is
        // also the parent of triad 2's caveat). The detector marks each
        // row as consumed by exactly one triad so a single row can't be
        // counted twice in the activity rollup.
        let rows = vec![
            caveat("u-cav-1", None),
            invocation("u-inv-1", "u-cav-1", "a"),
            stdout_row("u-out-1", "u-inv-1", "x"),
            caveat("u-cav-2", Some("u-out-1")),
            invocation("u-inv-2", "u-cav-2", "b"),
            stdout_row("u-out-2", "u-inv-2", "y"),
        ];
        let triads = detect_slash_triads(&rows);
        assert_eq!(triads.len(), 2);
        let mut all: Vec<usize> = triads
            .iter()
            .flat_map(|t| [t.caveat_idx, t.invocation_idx, t.stdout_idx])
            .collect();
        all.sort();
        // Six distinct indices — no row used by more than one triad.
        let unique = {
            let mut u = all.clone();
            u.dedup();
            u
        };
        assert_eq!(all, unique, "row indices are disjoint across triads");
    }
}
