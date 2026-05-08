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

// READ_ONLY_TOOLS lived in the TS classifier as commentary on the priority-6
// branch, but both arms of that branch return `exploration` (see
// `pick_category` priority 6 below), so the set is unreferenced. Tracked in
// AgentWorkforce/burn#254 — keeping it out of the Rust port avoids a dead-code
// warning while preserving identical behavior.

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
}
