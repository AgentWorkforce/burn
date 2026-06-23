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

mod bash_parse;
mod slash_triads;

pub use bash_parse::{parse_bash_command, BashParse};
// `SlashTriad` is part of the public `classifier::` surface even though no
// in-crate consumer names it yet; re-export it for reachability.
#[allow(unused_imports)]
pub use slash_triads::{detect_slash_triads, is_task_notification, SlashTriad};

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
// Activity-keyword regexes (case-insensitive, word-boundary).
// ---------------------------------------------------------------------------

pub(super) fn build_re(s: &str) -> Regex {
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
    use serde_json::{Map, Value};

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
