//! Activity classifier — Rust port of `packages/reader/src/classifier.ts`.
//!
//! The classifier is rule-based and deterministic. The rule tables
//! (`EDIT_TOOLS`, `TOOL_ALIASES`, regex pattern lists) are kept flat so adding
//! a new harness is a one-file change.

use std::collections::{HashMap, HashSet};

use once_cell::sync::Lazy;
use regex::Regex;

use crate::types::{ActivityCategory, ToolCall};

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
// Static rule tables.
// ---------------------------------------------------------------------------

static EDIT_TOOLS: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    ["Edit", "Write", "NotebookEdit", "MultiEdit"]
        .into_iter()
        .collect()
});

static DELEGATION_TOOLS: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["Agent", "Task"].into_iter().collect());

// READ_ONLY_TOOLS lived in the TS classifier as commentary on the priority-6
// branch, but both arms of that branch return `exploration` (see
// `pick_category` priority 6 below), so the set is unreferenced. Keeping it
// out of the Rust port avoids a dead-code warning while preserving identical
// behavior.

static TOOL_ALIASES: Lazy<HashMap<&'static str, &'static str>> = Lazy::new(|| {
    [
        // Codex
        ("apply_patch", "Edit"),
        ("exec_command", "Bash"),
        ("shell", "Bash"),
        ("read_file", "Read"),
        ("write_file", "Write"),
        ("update_plan", "ExitPlanMode"),
        ("spawn_agent", "Agent"),
        ("send_input", "Task"),
        ("wait_agent", "Task"),
        ("close_agent", "Task"),
        ("resume_agent", "Task"),
        ("view_image", "Read"),
        ("read_mcp_resource", "Read"),
        // OpenCode (lowercase names)
        ("read", "Read"),
        ("write", "Write"),
        ("edit", "Edit"),
        ("bash", "Bash"),
        ("grep", "Grep"),
        ("glob", "Glob"),
        ("webfetch", "WebFetch"),
        ("task", "Task"),
    ]
    .into_iter()
    .collect()
});

pub fn normalize_tool_name(name: &str) -> String {
    TOOL_ALIASES.get(name).copied().unwrap_or(name).to_string()
}

static MULTIWORD_BINARIES: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "git",
        "gh",
        "npm",
        "pnpm",
        "yarn",
        "bun",
        "pip",
        "pip3",
        "uv",
        "poetry",
        "cargo",
        "make",
        "docker",
        "kubectl",
        "helm",
        "terraform",
        "brew",
        "apt",
        "apt-get",
        "gem",
        "bundle",
        "go",
    ]
    .into_iter()
    .collect()
});

static PACKAGE_RUNNERS: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["npm", "pnpm", "yarn", "bun"].into_iter().collect());
static SHELL_BINARIES: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["bash", "sh", "zsh"].into_iter().collect());
static PYTHON_BINARIES: Lazy<HashSet<&'static str>> =
    Lazy::new(|| ["python", "python3"].into_iter().collect());

static TWO_PART_SUBCOMMANDS: Lazy<HashMap<&'static str, HashSet<&'static str>>> = Lazy::new(|| {
    let mut m: HashMap<&'static str, HashSet<&'static str>> = HashMap::new();
    m.insert("docker", ["compose"].into_iter().collect());
    m.insert(
        "gh",
        ["pr", "run", "issue", "repo", "workflow", "release"]
            .into_iter()
            .collect(),
    );
    m.insert("go", ["mod"].into_iter().collect());
    m.insert("uv", ["pip"].into_iter().collect());
    m
});

static OPTION_TAKES_VALUE: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    [
        "-C",
        "-c",
        "-F",
        "--config",
        "--filter",
        "--git-dir",
        "--namespace",
        "--prefix",
        "--repo",
        "--repository",
        "--work-tree",
    ]
    .into_iter()
    .collect()
});

static PYTHON_OPTION_TAKES_VALUE: Lazy<HashSet<&'static str>> = Lazy::new(|| {
    ["-c", "-m", "-W", "-X", "--check-hash-based-pycs"]
        .into_iter()
        .collect()
});

// Activity-keyword regexes. Word-boundary `\b` and case-insensitive flags match
// the TS regex literals.
static DEBUG_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(bug|error|crash|traceback|stack\s*trace|failure|failing|broken|fix\s+the|not\s+working|throws?)\b",
    )
    .unwrap()
});
static REVIEW_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(review|audit|inspect|look\s+over|code\s+review|pr\s+review)\b").unwrap()
});
static REFACTOR_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(refactor|refactoring|cleanup|clean\s+up|rename|extract|restructure|move\s+this|reorganize)\b",
    )
    .unwrap()
});
static FEATURE_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(r"(?i)\b(add|create|implement|new\s+feature|build\s+the|introduce|support\s+for)\b")
        .unwrap()
});
static BRAINSTORM_RE: Lazy<Regex> = Lazy::new(|| {
    Regex::new(
        r"(?i)\b(brainstorm|what\s+if|think\s+through|explore(?:\s+ideas)?|design|should\s+we|approach(?:es)?)\b",
    )
    .unwrap()
});
static PLANNING_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r"(?i)\b(plan(?:ning)?|outline|roadmap|strategy)\b").unwrap());

// Bash heuristics — match on the first non-env token after stripping leading
// `FOO=bar` assignments (e.g. `CI=1 pytest` -> `pytest`).
static TEST_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
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
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

static REVIEW_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
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
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

static GIT_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [r"\bgit\s+(?:push|pull|fetch|commit|merge|rebase|checkout|cherry-pick|reset|revert|switch|tag|stash)\b"]
        .into_iter()
        .map(|p| Regex::new(p).unwrap())
        .collect()
});

static DEPS_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
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
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

static FORMAT_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"\bprettier\b.*(?:--write|-w)(?:\s|$)",
        r"\beslint\b.*--fix\b",
        r"\bbiome\s+format\b",
        r"\bbiome\s+check\b.*--apply\b",
        // Negative lookaheads aren't supported in the regex crate; emulate
        // `\bblack\b(?!.*--check\b)` by checking the absence of `--check` at
        // call sites for the matching patterns. We keep the simple positive
        // form here and gate via post-filter below.
        r"\bblack\b",
        r"\bruff\s+format\b",
        r"\bisort\b",
        r"\brustfmt\b",
        r"\bcargo\s+fmt\b",
        r"\bgofmt\b",
        r"\bgoimports\b",
        r"\bdprint\s+fmt\b",
    ]
    .into_iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

// Companion gates: when a format candidate matches, reject it if `--check` is
// present (TS uses negative lookaheads; we encode the same intent without).
fn format_reject(cmd: &str) -> bool {
    static HAS_CHECK: Lazy<Regex> = Lazy::new(|| Regex::new(r"--check\b").unwrap());
    HAS_CHECK.is_match(cmd)
        // Only the black/cargo-fmt patterns in the TS list use a negative
        // lookahead, but applying the exclusion uniformly across format
        // candidates is conservative — `--check` always means "verify, don't
        // mutate" so it shouldn't classify as `format`.
        && (cmd.contains("black") || cmd.contains("cargo fmt"))
}

static VERIFICATION_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
    [
        r"\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?lint\b",
        r"\b(?:npm|yarn|pnpm|bun)\s+(?:run\s+)?typecheck\b",
        r"\bprettier\b.*--check\b",
        // `\beslint\b(?!.*--fix\b)` — emulated below in `verification_match`.
        r"\beslint\b",
        // `\bbiome\s+check\b(?!.*--apply\b)` — emulated below.
        r"\bbiome\s+check\b",
        r"\bblack\b.*--check\b",
        r"\bruff\s+check\b",
        r"\bflake8\b",
        r"\bmypy\b",
        r"\bpyright\b",
        // `\btsc\b(?!\s+--build\b)` — emulated below.
        r"\btsc\b",
        r"\bcargo\s+check\b",
        r"\bcargo\s+fmt\b.*--check\b",
        r"\bgolangci-lint\b",
        r"\bshellcheck\b",
        r"\bhadolint\b",
        r"\bterraform\s+validate\b",
        r"\bmake\s+(?:lint|check|typecheck|verify)\b",
    ]
    .into_iter()
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

fn verification_match(cmd: &str) -> bool {
    static FIX_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"--fix\b").unwrap());
    static APPLY_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"--apply\b").unwrap());
    static TSC_BUILD_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\btsc\s+--build\b").unwrap());

    for re in VERIFICATION_PATTERNS.iter() {
        if !re.is_match(cmd) {
            continue;
        }
        // eslint without --fix; biome check without --apply; tsc without --build.
        let s = re.as_str();
        if s == r"\beslint\b" && FIX_RE.is_match(cmd) {
            continue;
        }
        if s == r"\bbiome\s+check\b" && APPLY_RE.is_match(cmd) {
            continue;
        }
        if s == r"\btsc\b" && TSC_BUILD_RE.is_match(cmd) {
            continue;
        }
        return true;
    }
    false
}

static BUILD_DEPLOY_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
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
    .map(|p| Regex::new(p).unwrap())
    .collect()
});

static DOC_FILE_PATTERNS: Lazy<Vec<Regex>> = Lazy::new(|| {
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
    .map(|p| Regex::new(p).unwrap())
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
    let cmd = command.trim().to_string();
    if cmd.is_empty() {
        return None;
    }
    if has_heredoc(&cmd) || starts_with_compound_shell_syntax(&cmd) {
        return Some(shell_parse());
    }
    if !has_balanced_shell_delimiters(&cmd) {
        return Some(shell_parse());
    }

    if let Some(unwrapped) = unwrap_subshell(&cmd) {
        return parse_bash_command_inner(&unwrapped, depth + 1);
    }
    if let Some(rest) = strip_leading_cd_prefix(&cmd) {
        return parse_bash_command_inner(&rest, depth + 1);
    }

    let first = first_top_level_segment(&cmd)?;
    let cmd = first.trim().to_string();
    if cmd.is_empty() {
        return None;
    }
    if has_heredoc(&cmd) || starts_with_compound_shell_syntax(&cmd) {
        return Some(shell_parse());
    }
    if let Some(unwrapped) = unwrap_subshell(&cmd) {
        return parse_bash_command_inner(&unwrapped, depth + 1);
    }

    let tokens = match shell_words(&cmd) {
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
        if nested.contains(subcommand.as_str()) && sub_index + 1 < tokens.len() {
            subcommand = format!("{} {}", subcommand, tokens[sub_index + 1]);
        }
    }

    Some(verb(&binary, Some(&subcommand)))
}

fn verb(binary: &str, subcommand: Option<&str>) -> BashParse {
    match subcommand {
        None => BashParse {
            binary: binary.to_string(),
            subcommand: None,
            normalized: binary.to_string(),
        },
        Some(sub) => BashParse {
            binary: binary.to_string(),
            subcommand: Some(sub.to_string()),
            normalized: format!("{} {}", binary, sub),
        },
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
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<<-?\s*\S+").unwrap());
    RE.is_match(cmd)
}

fn starts_with_compound_shell_syntax(cmd: &str) -> bool {
    static RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r"^(?:for|while|until|if|case|select|function)\b").unwrap());
    static BRACE_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\{\s").unwrap());
    RE.is_match(cmd) || BRACE_RE.is_match(cmd)
}

fn has_balanced_shell_delimiters(cmd: &str) -> bool {
    let bytes = cmd.as_bytes();
    let mut quote: Option<u8> = None;
    let mut escaped = false;
    let mut depth: i32 = 0;
    for &b in bytes {
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
    let before = cmd[..op.index].trim();
    let words = shell_words(before)?;
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
                if cmd[i..].starts_with(op) {
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
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap());
    let mut i = start;
    while i < tokens.len() && RE.is_match(&tokens[i]) {
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
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^[A-Za-z_][A-Za-z0-9_]*=").unwrap());
    let mut i = start;
    while i < tokens.len() {
        let token = &tokens[i];
        if token == "--" {
            i += 1;
            break;
        }
        if RE.is_match(token) {
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
// classifyActivity priority ladder.
// ---------------------------------------------------------------------------

pub fn classify_activity(input: ClassificationInput<'_>) -> ClassificationResult {
    let has_edits = input
        .tool_calls
        .iter()
        .any(|t| EDIT_TOOLS.contains(normalize_tool_name(&t.name).as_str()));
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
        .any(|t| DELEGATION_TOOLS.contains(normalize_tool_name(&t.name).as_str()))
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
    let bash_calls: Vec<&ToolCall> = p
        .tool_calls
        .iter()
        .filter(|t| normalize_tool_name(&t.name) == "Bash")
        .collect();
    for call in &bash_calls {
        let raw = call.target.as_deref().unwrap_or("");
        let cmd = strip_env(raw);
        if cmd.is_empty() {
            continue;
        }
        if TEST_PATTERNS.iter().any(|re| re.is_match(&cmd)) {
            return ActivityCategory::Testing;
        }
        if REVIEW_PATTERNS.iter().any(|re| re.is_match(&cmd)) {
            return ActivityCategory::Review;
        }
        if GIT_PATTERNS.iter().any(|re| re.is_match(&cmd)) {
            return ActivityCategory::Git;
        }
        if DEPS_PATTERNS.iter().any(|re| re.is_match(&cmd)) {
            return ActivityCategory::Deps;
        }
        if FORMAT_PATTERNS.iter().any(|re| re.is_match(&cmd)) && !format_reject(&cmd) {
            return ActivityCategory::Format;
        }
        if verification_match(&cmd) {
            return ActivityCategory::Verification;
        }
        if BUILD_DEPLOY_PATTERNS.iter().any(|re| re.is_match(&cmd)) {
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
    let edits: Vec<&ToolCall> = tool_calls
        .iter()
        .filter(|t| EDIT_TOOLS.contains(normalize_tool_name(&t.name).as_str()))
        .collect();
    if edits.is_empty() {
        return false;
    }
    edits.iter().all(|t| match &t.target {
        Some(target) if !target.is_empty() => {
            DOC_FILE_PATTERNS.iter().any(|re| re.is_match(target))
        }
        _ => false,
    })
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
    static RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^(?:\s*[A-Z_][A-Z0-9_]*=\S+\s+)+").unwrap());
    RE.replace(cmd, "").into_owned()
}

pub fn count_retries(tool_calls: &[ToolCall]) -> u64 {
    let mut retries: u64 = 0;
    let mut seen_edit = false;
    let mut seen_bash_after_edit = false;
    for tc in tool_calls {
        let name = normalize_tool_name(&tc.name);
        if EDIT_TOOLS.contains(name.as_str()) {
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
}
