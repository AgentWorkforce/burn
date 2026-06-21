//! CLAUDE.md attribution + trim recommendations — Rust port of
//! `packages/analyze/src/claude-md.ts`.
//!
//! Per-session CLAUDE.md cost is attributed to the cacheRead tariff for any
//! turn whose `cacheRead >= totalTokens` (treating CLAUDE.md as cached once
//! the prompt cache is large enough). Section-level cost is split by byte
//! share so Σ section.totalCost ≤ totalCost holds exactly. Trim
//! recommendations re-emit the largest non-preamble sections as a unified
//! diff that hand-applies cleanly. The diff format is byte-aligned with the
//! TS implementation since CLI/MCP consumers may grep on it.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use crate::reader::TurnRecord;
use indexmap::IndexMap;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::analyze::cost::{lookup_model_rate, PER_MILLION};
use crate::analyze::pricing::PricingTable;
use crate::analyze::util::{group_turns_by_session, tokens_from_bytes};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarkdownSection {
    pub heading: String,
    /// 0 for preamble, 1-6 for `#` through `######`.
    pub level: u32,
    /// 1-indexed.
    pub start_line: u64,
    /// 1-indexed inclusive.
    pub end_line: u64,
    pub bytes: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ParsedClaudeMd {
    pub path: String,
    pub total_lines: u64,
    pub bytes: u64,
    pub tokens: u64,
    pub sections: Vec<MarkdownSection>,
    /// 1 or 2; 0 if no headings.
    pub grouping_level: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionClaudeMdCost {
    pub session_id: String,
    pub cost: f64,
    pub riding_turns: u64,
    pub total_turns: u64,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SectionCost {
    pub file_path: String,
    pub section: MarkdownSection,
    /// `section.bytes / Σ file.bytes` — additive across sections.
    pub token_share: f64,
    pub cost_per_session: f64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClaudeMdAttributionResult {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub session_costs: Vec<SessionClaudeMdCost>,
    pub section_costs: Vec<SectionCost>,
    pub per_session_avg: f64,
    pub per_session_p95: f64,
    pub session_count: u64,
}

#[cfg(test)]
#[derive(Debug, Clone)]
pub struct AttributeClaudeMdInput<'a> {
    pub files: &'a [ParsedClaudeMd],
    pub turns: &'a [TurnRecord],
    pub pricing: &'a PricingTable,
}

#[cfg(test)]
pub fn find_claude_md_files(project_path: &Path) -> io::Result<Vec<PathBuf>> {
    let candidates = [
        project_path.join("CLAUDE.md"),
        project_path.join(".claude").join("CLAUDE.md"),
    ];
    let mut found = Vec::new();
    for c in candidates {
        match fs::metadata(&c) {
            Ok(m) if m.is_file() => found.push(c),
            _ => {}
        }
    }
    Ok(found)
}

pub fn load_claude_md_file(file_path: &Path) -> io::Result<ParsedClaudeMd> {
    let text = fs::read_to_string(file_path)?;
    Ok(parse_claude_md(&file_path.to_string_lossy(), &text))
}

pub fn parse_claude_md(file_path: &str, text: &str) -> ParsedClaudeMd {
    // Normalize CRLF → LF and drop a single trailing newline so `total_lines`
    // and per-section `end_line` match what a user sees in an editor. Empty
    // text => 0 lines.
    let normalized = text.replace("\r\n", "\n");
    let had_trailing_newline = normalized.ends_with('\n');
    let trimmed_end: &str = if had_trailing_newline {
        &normalized[..normalized.len() - 1]
    } else {
        &normalized
    };
    let lines: Vec<&str> = if trimmed_end.is_empty() {
        Vec::new()
    } else {
        trimmed_end.split('\n').collect()
    };
    let total_lines = lines.len() as u64;
    let total_bytes = normalized.len() as u64;
    let tokens = tokens_from_bytes(total_bytes);

    let line_bytes: Vec<u64> = lines.iter().map(|l| l.len() as u64).collect();
    let line_with_newline_weight = |idx: usize| -> u64 {
        let base = line_bytes.get(idx).copied().unwrap_or(0);
        let is_last = idx + 1 == lines.len();
        if is_last && !had_trailing_newline {
            base
        } else {
            base + 1
        }
    };
    let range_bytes = |start1: u64, end1: u64| -> u64 {
        let mut sum = 0u64;
        let start = start1.saturating_sub(1) as usize;
        let end = end1.saturating_sub(1) as usize;
        for i in start..=end {
            sum += line_with_newline_weight(i);
        }
        sum
    };

    let headings = find_headings(&lines);
    let grouping_level = if headings.iter().any(|h| h.level == 2) {
        2
    } else if headings.iter().any(|h| h.level == 1) {
        1
    } else {
        0
    };

    let mut sections: Vec<MarkdownSection> = Vec::new();
    if grouping_level == 0 {
        if total_lines > 0 && total_bytes > 0 {
            sections.push(MarkdownSection {
                heading: "(preamble)".to_string(),
                level: 0,
                start_line: 1,
                end_line: total_lines,
                bytes: total_bytes,
                tokens,
            });
        }
        return ParsedClaudeMd {
            path: file_path.to_string(),
            total_lines,
            bytes: total_bytes,
            tokens,
            sections,
            grouping_level,
        };
    }

    let group_headings: Vec<&HeadingInfo> = headings
        .iter()
        .filter(|h| h.level == grouping_level)
        .collect();
    let first_start = group_headings
        .first()
        .map(|h| h.line)
        .unwrap_or(total_lines + 1);
    if first_start > 1 {
        let pb_bytes = range_bytes(1, first_start - 1);
        if pb_bytes > 0 {
            sections.push(MarkdownSection {
                heading: "(preamble)".to_string(),
                level: 0,
                start_line: 1,
                end_line: first_start - 1,
                bytes: pb_bytes,
                tokens: tokens_from_bytes(pb_bytes),
            });
        }
    }

    for i in 0..group_headings.len() {
        let h = group_headings[i];
        let next = group_headings.get(i + 1).copied();
        let end_line = match next {
            Some(n) => n.line - 1,
            None => total_lines,
        };
        let sec_bytes = range_bytes(h.line, end_line);
        sections.push(MarkdownSection {
            heading: h.text.clone(),
            level: h.level,
            start_line: h.line,
            end_line,
            bytes: sec_bytes,
            tokens: tokens_from_bytes(sec_bytes),
        });
    }

    ParsedClaudeMd {
        path: file_path.to_string(),
        total_lines,
        bytes: total_bytes,
        tokens,
        sections,
        grouping_level,
    }
}

#[derive(Debug, Clone)]
struct HeadingInfo {
    line: u64,
    level: u32,
    text: String,
}

static OPEN_FENCE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(`{3,}|~{3,})").unwrap());
static HEADING_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(#{1,6})\s+(.*\S)\s*$").unwrap());

fn find_headings(lines: &[&str]) -> Vec<HeadingInfo> {
    let open_re = &*OPEN_FENCE_RE;
    let heading_re = &*HEADING_RE;
    let mut out = Vec::new();
    let mut fence_char: Option<char> = None;
    let mut fence_len: usize = 0;
    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if fence_char.is_none() {
            if let Some(m) = open_re.captures(trimmed) {
                let s = m.get(1).unwrap().as_str();
                fence_char = s.chars().next();
                fence_len = s.len();
                continue;
            }
        } else {
            let ch = fence_char.unwrap();
            // Closing fence: a run of the same char at least as long as the
            // opener, followed only by whitespace.
            if matches_close_fence(trimmed, ch, fence_len) {
                fence_char = None;
                fence_len = 0;
            }
            continue;
        }
        if let Some(m) = heading_re.captures(line) {
            let hashes = m.get(1).unwrap().as_str();
            let body = m.get(2).unwrap().as_str();
            out.push(HeadingInfo {
                line: (i as u64) + 1,
                level: hashes.len() as u32,
                text: format!("{hashes} {body}"),
            });
        }
    }
    out
}

fn matches_close_fence(s: &str, ch: char, min_len: usize) -> bool {
    let mut chars = s.chars();
    let mut run = 0usize;
    while let Some(c) = chars.clone().next() {
        if c == ch {
            run += 1;
            chars.next();
        } else {
            break;
        }
    }
    if run < min_len {
        return false;
    }
    chars.all(|c| c.is_whitespace())
}

#[cfg(test)]
pub fn attribute_claude_md(input: &AttributeClaudeMdInput<'_>) -> ClaudeMdAttributionResult {
    let turns: Vec<&TurnRecord> = input.turns.iter().collect();
    attribute_claude_md_refs(input.files, &turns, input.pricing)
}

/// Per-file CLAUDE.md attribution: borrow-based entry point used by the
/// per-file overhead attribution loop, which has already pre-filtered turns
/// into a `Vec<&TurnRecord>` and would otherwise pay a per-turn clone.
pub(crate) fn attribute_claude_md_refs(
    files: &[ParsedClaudeMd],
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> ClaudeMdAttributionResult {
    let total_tokens: u64 = files.iter().map(|f| f.tokens).sum();
    if total_tokens == 0 {
        return ClaudeMdAttributionResult {
            total_tokens: 0,
            total_cost: 0.0,
            session_costs: Vec::new(),
            section_costs: Vec::new(),
            per_session_avg: 0.0,
            per_session_p95: 0.0,
            session_count: 0,
        };
    }

    let by_session = group_turns_by_session(turns.iter().copied());

    let mut session_costs: Vec<SessionClaudeMdCost> = Vec::new();
    let mut total_cost = 0.0_f64;
    for (session_id, turns) in by_session {
        let mut turns = turns;
        turns.sort_by_key(|t| t.turn_index);
        let mut cost = 0.0_f64;
        let mut riding_turns: u64 = 0;
        let mut model_counts: IndexMap<String, u64> = IndexMap::new();
        for t in &turns {
            let Some(rate) = lookup_model_rate(&t.model, pricing) else {
                continue;
            };
            *model_counts.entry(t.model.clone()).or_insert(0) += 1;
            if t.usage.cache_read < total_tokens {
                continue;
            }
            cost += (total_tokens as f64 / PER_MILLION) * rate.cache_read;
            riding_turns += 1;
        }
        let dominant = pick_dominant_model(&model_counts);
        session_costs.push(SessionClaudeMdCost {
            session_id,
            cost,
            riding_turns,
            total_turns: turns.len() as u64,
            model: dominant,
        });
        total_cost += cost;
    }

    let mut session_cost_values: Vec<f64> = session_costs.iter().map(|s| s.cost).collect();
    session_cost_values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let per_session_avg = if session_cost_values.is_empty() {
        0.0
    } else {
        session_cost_values.iter().sum::<f64>() / session_cost_values.len() as f64
    };
    let per_session_p95 = percentile(&session_cost_values, 0.95);

    let total_bytes: u64 = files.iter().map(|f| f.bytes).sum();
    let mut section_costs: Vec<SectionCost> = Vec::new();
    for f in files {
        for section in &f.sections {
            let token_share = if total_bytes > 0 {
                section.bytes as f64 / total_bytes as f64
            } else {
                0.0
            };
            let total_sec_cost = total_cost * token_share;
            let per_session_sec_cost = per_session_avg * token_share;
            section_costs.push(SectionCost {
                file_path: f.path.clone(),
                section: section.clone(),
                token_share,
                cost_per_session: per_session_sec_cost,
                total_cost: total_sec_cost,
            });
        }
    }
    section_costs.sort_by(|a, b| {
        b.total_cost
            .partial_cmp(&a.total_cost)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let session_count = session_costs.len() as u64;
    ClaudeMdAttributionResult {
        total_tokens,
        total_cost,
        session_costs,
        section_costs,
        per_session_avg,
        per_session_p95,
        session_count,
    }
}

fn pick_dominant_model(counts: &IndexMap<String, u64>) -> String {
    let mut best_model = String::new();
    let mut best_count: i64 = -1;
    for (m, c) in counts {
        let c = *c as i64;
        if c > best_count {
            best_model = m.clone();
            best_count = c;
        }
    }
    best_model
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }
    let raw = (p * sorted.len() as f64).ceil() as i64 - 1;
    let idx = raw.clamp(0, sorted.len() as i64 - 1) as usize;
    sorted[idx]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrimRecommendation {
    pub file_path: String,
    pub section: MarkdownSection,
    pub projected_savings_per_session: f64,
    pub projected_savings_across_window: f64,
    pub token_share: f64,
}

pub fn build_trim_recommendations(
    attribution: &ClaudeMdAttributionResult,
    top_n: usize,
) -> Vec<TrimRecommendation> {
    attribution
        .section_costs
        .iter()
        .filter(|s| s.section.level > 0)
        .take(top_n)
        .map(|s| TrimRecommendation {
            file_path: s.file_path.clone(),
            section: s.section.clone(),
            projected_savings_per_session: s.cost_per_session,
            projected_savings_across_window: s.total_cost,
            token_share: s.token_share,
        })
        .collect()
}

pub fn render_unified_diff_for_recommendation(
    file_path: &str,
    file_text: &str,
    rec: &TrimRecommendation,
    base_dir: Option<&Path>,
) -> String {
    let normalized = file_text.replace("\r\n", "\n");
    let had_trailing = normalized.ends_with('\n');
    let trimmed_end: &str = if had_trailing {
        &normalized[..normalized.len() - 1]
    } else {
        &normalized
    };
    let lines: Vec<&str> = if trimmed_end.is_empty() {
        Vec::new()
    } else {
        trimmed_end.split('\n').collect()
    };
    let start = rec.section.start_line as usize;
    let end = rec.section.end_line as usize;
    let removed: Vec<&str> = lines
        .iter()
        .copied()
        .skip(start - 1)
        .take(end - (start - 1))
        .collect();
    let display = to_posix_relative(file_path, base_dir);
    let header_a = format!("--- a/{display}");
    let header_b = format!("+++ b/{display}");
    let hunk = format!("@@ -{},{} +{},0 @@", start, removed.len(), start);
    let body = removed
        .iter()
        .map(|l| format!("-{l}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "# TRIM: {heading}\n# projected savings per session: ${ps:.4}\n# projected savings across window: ${pw:.4}\n{header_a}\n{header_b}\n{hunk}\n{body}",
        heading = rec.section.heading,
        ps = rec.projected_savings_per_session,
        pw = rec.projected_savings_across_window,
    )
}

fn to_posix_relative(file_path: &str, base_dir: Option<&Path>) -> String {
    let path = Path::new(file_path);
    let mut p: PathBuf = path.to_path_buf();
    if let Some(base) = base_dir {
        if let Ok(rel) = path.strip_prefix(base) {
            if !rel.as_os_str().is_empty() && !rel.starts_with("..") {
                p = rel.to_path_buf();
            }
        }
    }
    let s = p.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/");
    // Strip leading slashes so headers aren't `--- a//abs/path`.
    s.trim_start_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::pricing::load_builtin_pricing;
    use crate::reader::{SourceKind, ToolCall, TurnRecord, Usage};
    use std::fs;
    use tempfile::TempDir;

    fn make_turn(session_id: &str, message_id: &str, turn_index: u64, usage: Usage) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session_id.into(),
            session_path: None,
            message_id: message_id.into(),
            turn_index,
            ts: "2026-04-23T00:00:00.000Z".into(),
            model: "claude-sonnet-4-6".into(),
            project: None,
            project_key: None,
            usage,
            tool_calls: Vec::<ToolCall>::new(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    fn zero_usage() -> Usage {
        Usage {
            input: 0,
            output: 0,
            reasoning: 0,
            cache_read: 0,
            cache_create_5m: 0,
            cache_create_1h: 0,
        }
    }

    #[test]
    fn returns_a_single_preamble_section_for_a_file_with_no_headings() {
        let parsed = parse_claude_md("/p/CLAUDE.md", "just a paragraph\nwith some content");
        assert_eq!(parsed.sections.len(), 1);
        assert_eq!(parsed.sections[0].level, 0);
        assert_eq!(parsed.sections[0].heading, "(preamble)");
        assert_eq!(parsed.grouping_level, 0);
    }

    #[test]
    fn groups_by_h2_when_h2_sections_exist_treating_leading_content_as_preamble() {
        let text = [
            "# Title",
            "intro paragraph",
            "",
            "## Architecture",
            "arch line 1",
            "arch line 2",
            "",
            "## Testing",
            "testing line 1",
        ]
        .join("\n");
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        assert_eq!(parsed.grouping_level, 2);
        assert_eq!(parsed.sections.len(), 3);
        assert_eq!(parsed.sections[0].level, 0);
        assert_eq!(parsed.sections[1].heading, "## Architecture");
        assert_eq!(parsed.sections[2].heading, "## Testing");
        assert_eq!(parsed.sections[1].start_line, 4);
        assert_eq!(parsed.sections[1].end_line, 7);
        assert_eq!(parsed.sections[2].start_line, 8);
        assert_eq!(parsed.sections[2].end_line, 9);
    }

    #[test]
    fn groups_by_h1_when_no_h2_exists() {
        let text = ["# Section A", "a body", "# Section B", "b body"].join("\n");
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        assert_eq!(parsed.grouping_level, 1);
        assert_eq!(parsed.sections.len(), 2);
        assert_eq!(parsed.sections[0].heading, "# Section A");
        assert_eq!(parsed.sections[1].heading, "# Section B");
    }

    #[test]
    fn ignores_headings_inside_fenced_code_blocks() {
        let text = [
            "## Real heading",
            "body",
            "",
            "```",
            "## not a heading",
            "```",
            "",
            "## Another real heading",
        ]
        .join("\n");
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        assert_eq!(parsed.sections.len(), 2);
        assert_eq!(parsed.sections[0].heading, "## Real heading");
        assert_eq!(parsed.sections[1].heading, "## Another real heading");
    }

    #[test]
    fn a_python_line_inside_a_3_backtick_fence_does_not_close_the_fence() {
        let text = [
            "```",
            "## inside block",
            "````python",
            "## should-be-inside",
            "```",
            "## should-be-outside",
        ]
        .join("\n");
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        let headings: Vec<&str> = parsed
            .sections
            .iter()
            .filter(|s| s.level > 0)
            .map(|s| s.heading.as_str())
            .collect();
        assert_eq!(headings, vec!["## should-be-outside"]);
    }

    #[test]
    fn does_not_count_a_trailing_newline_as_an_extra_line() {
        let parsed = parse_claude_md("/p/CLAUDE.md", "## Section\nbody\n");
        assert_eq!(parsed.total_lines, 2);
        assert_eq!(parsed.sections[0].end_line, 2);
    }

    #[test]
    fn normalizes_crlf_line_endings() {
        let parsed = parse_claude_md("/p/CLAUDE.md", "## A\r\nbody\r\n## B\r\nb\r\n");
        assert_eq!(parsed.sections.len(), 2);
        assert_eq!(parsed.sections[0].heading, "## A");
        assert_eq!(parsed.sections[1].heading, "## B");
    }

    #[test]
    fn returns_zero_sections_for_empty_input() {
        let parsed = parse_claude_md("/p/CLAUDE.md", "");
        assert_eq!(parsed.total_lines, 0);
        assert_eq!(parsed.sections.len(), 0);
    }

    #[test]
    fn attributes_per_turn_cost_within_10_pct_of_hand_computed_truth() {
        let pricing = load_builtin_pricing();
        let rate = pricing.get("claude-sonnet-4-6").unwrap().clone();
        let mut text = String::from("# Title\n");
        text.push_str(&"x".repeat(4000 - 8));
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);

        let session_id = "s-cm-1";
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..5 {
            turns.push(make_turn(
                session_id,
                &format!("m-{i}"),
                i,
                Usage {
                    input: 50,
                    output: 30,
                    reasoning: 0,
                    cache_read: parsed.tokens + 5000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ));
        }
        let files = vec![parsed.clone()];
        let result = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let expected = 5.0 * (parsed.tokens as f64 / 1_000_000.0) * rate.cache_read;
        assert!(
            (result.total_cost - expected).abs() <= expected * 0.10,
            "total={} expected={} diff>10%",
            result.total_cost,
            expected,
        );
        assert_eq!(result.session_count, 1);
        assert_eq!(result.session_costs[0].riding_turns, 5);
    }

    #[test]
    fn section_cost_is_proportional_to_its_token_share() {
        let pricing = load_builtin_pricing();
        let mut text = String::new();
        text.push_str("## Big\n");
        text.push_str(&"x".repeat(8000));
        text.push_str("\n## Small\n");
        text.push_str(&"x".repeat(2000));
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        let session_id = "s-cm-sec";
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..3 {
            turns.push(make_turn(
                session_id,
                &format!("m-{i}"),
                i,
                Usage {
                    input: 50,
                    output: 10,
                    reasoning: 0,
                    cache_read: parsed.tokens + 1000,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ));
        }
        let files = vec![parsed];
        let result = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let big = result
            .section_costs
            .iter()
            .find(|s| s.section.heading == "## Big")
            .unwrap();
        let small = result
            .section_costs
            .iter()
            .find(|s| s.section.heading == "## Small")
            .unwrap();
        assert!(big.total_cost > small.total_cost);
        let ratio = big.total_cost / small.total_cost;
        let token_ratio = big.section.tokens as f64 / small.section.tokens as f64;
        assert!((ratio - token_ratio).abs() / token_ratio < 0.05);
    }

    #[test]
    fn skips_turns_where_cache_read_is_below_claude_md_size() {
        let pricing = load_builtin_pricing();
        let mut text = String::from("## Big\n");
        text.push_str(&"x".repeat(40_000));
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        let session_id = "s-cm-skip";
        let turns = vec![
            make_turn(
                session_id,
                "m0",
                0,
                Usage {
                    input: 5000,
                    output: 10,
                    reasoning: 0,
                    cache_read: 100,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ),
            make_turn(
                session_id,
                "m1",
                1,
                Usage {
                    input: 50,
                    output: 10,
                    reasoning: 0,
                    cache_read: parsed.tokens + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ),
        ];
        let files = vec![parsed];
        let result = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        assert_eq!(result.session_costs[0].riding_turns, 1);
    }

    #[test]
    fn returns_zero_cost_when_claude_md_is_empty() {
        let parsed = parse_claude_md("/p/CLAUDE.md", "");
        let pricing = PricingTable::new();
        let turns = vec![make_turn("s", "m", 0, zero_usage())];
        let files = vec![parsed];
        let result = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        assert_eq!(result.total_cost, 0.0);
        assert_eq!(result.session_costs.len(), 0);
    }

    #[test]
    fn includes_zero_cost_sessions_in_session_count_so_avg_p95_are_not_biased_upward() {
        let pricing = load_builtin_pricing();
        let mut text = String::from("## Body\n");
        text.push_str(&"x".repeat(4000));
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        let turns = vec![
            make_turn(
                "s-A",
                "m",
                0,
                Usage {
                    input: 10,
                    output: 10,
                    reasoning: 0,
                    cache_read: parsed.tokens + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ),
            make_turn(
                "s-B",
                "m",
                0,
                Usage {
                    input: 500,
                    output: 10,
                    reasoning: 0,
                    cache_read: 0,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ),
        ];
        let files = vec![parsed];
        let result = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        assert_eq!(result.session_count, 2);
        let b = result
            .session_costs
            .iter()
            .find(|s| s.session_id == "s-B")
            .unwrap();
        assert_eq!(b.cost, 0.0);
        assert_eq!(b.riding_turns, 0);
        let a = result
            .session_costs
            .iter()
            .find(|s| s.session_id == "s-A")
            .unwrap();
        assert!((result.per_session_avg - a.cost / 2.0).abs() < 1e-9);
    }

    #[test]
    fn sum_of_section_costs_stays_below_or_equal_total_cost() {
        let pricing = load_builtin_pricing();
        let mut parts = String::new();
        for i in 0..20 {
            parts.push_str(&format!("## Section {i}\n{}\n", "x".repeat(123)));
        }
        let parsed = parse_claude_md("/p/CLAUDE.md", &parts);
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..5 {
            turns.push(make_turn(
                "s-sum",
                &format!("m{i}"),
                i,
                Usage {
                    input: 10,
                    output: 10,
                    reasoning: 0,
                    cache_read: parsed.tokens + 500,
                    cache_create_5m: 0,
                    cache_create_1h: 0,
                },
            ));
        }
        let files = vec![parsed];
        let result = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let sum: f64 = result.section_costs.iter().map(|s| s.total_cost).sum();
        assert!(sum <= result.total_cost + 1e-9);
        let sum_shares: f64 = result.section_costs.iter().map(|s| s.token_share).sum();
        assert!((sum_shares - 1.0).abs() < 1e-9);
    }

    #[test]
    fn finds_root_claude_md_and_dot_claude_claude_md() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("CLAUDE.md"), "# Root").unwrap();
        fs::create_dir_all(tmp.path().join(".claude")).unwrap();
        fs::write(tmp.path().join(".claude").join("CLAUDE.md"), "# Nested").unwrap();
        let files = find_claude_md_files(tmp.path()).unwrap();
        assert_eq!(files.len(), 2);
        assert!(files.iter().any(|f| {
            f.file_name().unwrap() == "CLAUDE.md"
                && f.parent()
                    .unwrap()
                    .file_name()
                    .map(|s| s != ".claude")
                    .unwrap_or(true)
        }));
        assert!(files.iter().any(|f| {
            f.file_name().unwrap() == "CLAUDE.md"
                && f.parent()
                    .unwrap()
                    .file_name()
                    .map(|s| s == ".claude")
                    .unwrap_or(false)
        }));
    }

    #[test]
    fn loads_parsed_content_via_load_claude_md_file() {
        let tmp = TempDir::new().unwrap();
        let target = tmp.path().join("CLAUDE.md");
        fs::write(&target, "## Section\nbody").unwrap();
        let parsed = load_claude_md_file(&target).unwrap();
        assert_eq!(parsed.sections[0].heading, "## Section");
    }

    #[test]
    fn emits_a_trim_diff_for_the_largest_section_that_hand_applies_cleanly() {
        let pricing = load_builtin_pricing();
        let mut text = String::new();
        text.push_str("## Big\n");
        text.push_str(&"x".repeat(8000));
        text.push_str("\n## Small\n");
        text.push_str(&"x".repeat(2000));
        let parsed = parse_claude_md("/p/CLAUDE.md", &text);
        let turns = vec![make_turn(
            "s-cm-advise",
            "m0",
            0,
            Usage {
                input: 50,
                output: 10,
                reasoning: 0,
                cache_read: parsed.tokens + 1000,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
        )];
        let files = vec![parsed];
        let attribution = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let recs = build_trim_recommendations(&attribution, 1);
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].section.heading, "## Big");
        assert!(recs[0].token_share > 0.0);
        let diff = render_unified_diff_for_recommendation("/p/CLAUDE.md", &text, &recs[0], None);
        assert!(diff.contains("# TRIM: ## Big"));
        assert!(diff.contains("--- a/"));
        assert!(diff.contains("+++ b/"));
        assert!(diff.contains("@@ -1,2 +1,0 @@"));
    }

    #[test]
    fn emits_a_project_relative_posix_path_in_the_diff_header_when_base_dir_is_given() {
        let pricing = load_builtin_pricing();
        let text = "## Only\nbody\n";
        let parsed = parse_claude_md("/home/u/repo/CLAUDE.md", text);
        let turns = vec![make_turn(
            "s",
            "m",
            0,
            Usage {
                input: 10,
                output: 10,
                reasoning: 0,
                cache_read: parsed.tokens + 100,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
        )];
        let files = vec![parsed];
        let attribution = attribute_claude_md(&AttributeClaudeMdInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let recs = build_trim_recommendations(&attribution, 1);
        let diff = render_unified_diff_for_recommendation(
            "/home/u/repo/CLAUDE.md",
            text,
            &recs[0],
            Some(Path::new("/home/u/repo")),
        );
        assert!(diff.contains("--- a/CLAUDE.md"));
        assert!(diff.contains("+++ b/CLAUDE.md"));
        assert!(!diff.contains("a//"));
    }
}
