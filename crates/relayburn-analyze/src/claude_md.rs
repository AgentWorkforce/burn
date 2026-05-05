//! Markdown section parser + per-session attribution for `CLAUDE.md` /
//! `AGENTS.md` overhead files. Rust port of `packages/analyze/src/claude-md.ts`.
//!
//! This module is the parsing/attribution kernel that the higher-level
//! `overhead` module composes when answering "how much of this session's
//! spend went to system overhead context loaded into every prompt?". Math is
//! `f64` and accumulation order matches the TS reduce loops so the
//! per-session/per-section USD totals stay within the 1e-9 USD precision
//! contract the parent issue gates on.
//!
//! See AgentWorkforce/burn#272 (claude-md attribution) and #276 (overhead).

use std::fs;
use std::path::{Path, PathBuf};

use indexmap::IndexMap;
use regex::Regex;
use relayburn_reader::TurnRecord;

use crate::cost::lookup_model_rate;
use crate::pricing::PricingTable;

const PER_MILLION: f64 = 1_000_000.0;
const CHARS_PER_TOKEN: u64 = 4;

#[derive(Debug, Clone, PartialEq)]
pub struct MarkdownSection {
    pub heading: String,
    /// 0 for preamble, 1-6 for `#` through `######`.
    pub level: u32,
    pub start_line: u32,
    pub end_line: u32,
    pub bytes: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedClaudeMd {
    pub path: String,
    pub total_lines: u32,
    pub bytes: u64,
    pub tokens: u64,
    pub sections: Vec<MarkdownSection>,
    /// 1 or 2 (the heading depth at which sections were grouped); 0 if no
    /// headings exist at all.
    pub grouping_level: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionClaudeMdCost {
    pub session_id: String,
    pub cost: f64,
    pub riding_turns: u64,
    pub total_turns: u64,
    pub model: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SectionCost {
    pub file_path: String,
    pub section: MarkdownSection,
    pub token_share: f64,
    pub cost_per_session: f64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ClaudeMdAttributionResult {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub session_costs: Vec<SessionClaudeMdCost>,
    pub section_costs: Vec<SectionCost>,
    pub per_session_avg: f64,
    pub per_session_p95: f64,
    pub session_count: u64,
}

pub struct AttributeClaudeMdInput<'a> {
    pub files: &'a [ParsedClaudeMd],
    pub turns: &'a [TurnRecord],
    pub pricing: &'a PricingTable,
}

pub fn find_claude_md_files(project_path: &Path) -> Vec<PathBuf> {
    let candidates = [
        project_path.join("CLAUDE.md"),
        project_path.join(".claude").join("CLAUDE.md"),
    ];
    candidates
        .into_iter()
        .filter(|p| fs::metadata(p).map(|m| m.is_file()).unwrap_or(false))
        .collect()
}

pub fn load_claude_md_file(file_path: &Path) -> std::io::Result<ParsedClaudeMd> {
    let text = fs::read_to_string(file_path)?;
    Ok(parse_claude_md(&file_path.to_string_lossy(), &text))
}

pub fn parse_claude_md(file_path: &str, text: &str) -> ParsedClaudeMd {
    // Normalize CRLF → LF and drop a single trailing newline so `total_lines`
    // and per-section `end_line` match what a user sees in an editor.
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
    let total_lines = lines.len() as u32;
    let total_bytes = normalized.len() as u64;
    let tokens = ceil_div(total_bytes, CHARS_PER_TOKEN);

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
    let range_bytes = |start_line1: u32, end_line1: u32| -> u64 {
        let mut sum = 0u64;
        for i in start_line1..=end_line1 {
            sum += line_with_newline_weight((i - 1) as usize);
        }
        sum
    };

    let headings = find_headings(&lines);
    let mut grouping_level = 0u32;
    if headings.iter().any(|h| h.level == 2) {
        grouping_level = 2;
    } else if headings.iter().any(|h| h.level == 1) {
        grouping_level = 1;
    }

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
                tokens: ceil_div(pb_bytes, CHARS_PER_TOKEN),
            });
        }
    }
    for (i, h) in group_headings.iter().enumerate() {
        let next = group_headings.get(i + 1);
        let end_line = next.map(|n| n.line - 1).unwrap_or(total_lines);
        let sec_bytes = range_bytes(h.line, end_line);
        sections.push(MarkdownSection {
            heading: h.text.clone(),
            level: h.level,
            start_line: h.line,
            end_line,
            bytes: sec_bytes,
            tokens: ceil_div(sec_bytes, CHARS_PER_TOKEN),
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

struct HeadingInfo {
    line: u32,
    level: u32,
    /// Includes the leading hashes for display (matches TS shape).
    text: String,
}

fn find_headings(lines: &[&str]) -> Vec<HeadingInfo> {
    // Heading: 1-6 hashes, whitespace, then text ending with a non-space char.
    let heading_re = Regex::new(r"^(#{1,6})\s+(.*\S)\s*$").unwrap();
    // Opening fence: 3+ backticks or tildes (after trim).
    let open_re = Regex::new(r"^(`{3,}|~{3,})").unwrap();

    let mut out: Vec<HeadingInfo> = Vec::new();
    let mut fence_char: Option<char> = None;
    let mut fence_len: usize = 0;

    for (i, line) in lines.iter().enumerate() {
        let trimmed = line.trim();
        if fence_char.is_none() {
            if let Some(caps) = open_re.captures(trimmed) {
                let m = caps.get(1).unwrap().as_str();
                fence_char = m.chars().next();
                fence_len = m.len();
                continue;
            }
        } else {
            let ch = fence_char.unwrap();
            // Closing fence per CommonMark: a run of the same fence char at
            // least as long as the opening run, followed only by whitespace.
            let mut closes = false;
            let s = trimmed;
            let mut run = 0usize;
            for c in s.chars() {
                if c == ch {
                    run += 1;
                } else {
                    break;
                }
            }
            if run >= fence_len {
                let rest = &s[run..];
                if rest.chars().all(|c| c.is_whitespace()) {
                    closes = true;
                }
            }
            if closes {
                fence_char = None;
                fence_len = 0;
            }
            continue;
        }
        if let Some(caps) = heading_re.captures(line) {
            let hashes = caps.get(1).unwrap().as_str();
            let body = caps.get(2).unwrap().as_str();
            out.push(HeadingInfo {
                line: (i + 1) as u32,
                level: hashes.len() as u32,
                text: format!("{} {}", hashes, body),
            });
        }
    }
    out
}

pub fn attribute_claude_md(input: AttributeClaudeMdInput<'_>) -> ClaudeMdAttributionResult {
    let total_tokens: u64 = input.files.iter().map(|f| f.tokens).sum();
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

    // Insertion-order-preserving group by sessionId — matches TS `Map`
    // iteration order, so downstream stable sorts tie-break consistently.
    let mut by_session: IndexMap<String, Vec<TurnRecord>> = IndexMap::new();
    for t in input.turns {
        by_session
            .entry(t.session_id.clone())
            .or_default()
            .push(t.clone());
    }

    let mut session_costs: Vec<SessionClaudeMdCost> = Vec::new();
    let mut total_cost = 0.0_f64;
    for (session_id, mut turns) in by_session.into_iter() {
        turns.sort_by_key(|t| t.turn_index);
        let mut cost = 0.0_f64;
        let mut riding_turns: u64 = 0;
        let mut model_counts: IndexMap<String, u64> = IndexMap::new();
        for t in &turns {
            let rate = match lookup_model_rate(&t.model, input.pricing) {
                Some(r) => r,
                None => continue,
            };
            *model_counts.entry(t.model.clone()).or_insert(0) += 1;
            // Treat the file as cache-resident only once a turn reads at least
            // `total_tokens` cached tokens — conservative match for the TS
            // `cacheRead < totalTokens` early-continue.
            if t.usage.cache_read < total_tokens {
                continue;
            }
            cost += (total_tokens as f64 / PER_MILLION) * rate.cache_read;
            riding_turns += 1;
        }
        let dominant_model = pick_dominant_model(&model_counts);
        session_costs.push(SessionClaudeMdCost {
            session_id,
            cost,
            riding_turns,
            total_turns: turns.len() as u64,
            model: dominant_model,
        });
        total_cost += cost;
    }

    let mut sorted_session_costs: Vec<f64> = session_costs.iter().map(|s| s.cost).collect();
    sorted_session_costs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let per_session_avg = if sorted_session_costs.is_empty() {
        0.0
    } else {
        sorted_session_costs.iter().sum::<f64>() / sorted_session_costs.len() as f64
    };
    let per_session_p95 = percentile(&sorted_session_costs, 0.95);

    // Use bytes (additive) rather than per-section token counts (each ceil()ed
    // independently, so they can sum to more than `total_tokens`) for the
    // share. This keeps Σ section_cost ≤ total_cost exactly.
    let total_bytes: u64 = input.files.iter().map(|f| f.bytes).sum();
    let mut section_costs: Vec<SectionCost> = Vec::new();
    for f in input.files {
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
    // TS uses `bestCount = -1` as a sentinel; matched by `Option<u64>` here.
    let mut best_count: Option<u64> = None;
    for (m, c) in counts {
        if best_count.is_none_or(|bc| *c > bc) {
            best_model = m.clone();
            best_count = Some(*c);
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
    let idx_f = (p * sorted.len() as f64).ceil() - 1.0;
    let idx = idx_f.max(0.0).min((sorted.len() - 1) as f64) as usize;
    sorted[idx]
}

fn ceil_div(n: u64, d: u64) -> u64 {
    if n == 0 {
        0
    } else {
        n.div_ceil(d)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use relayburn_reader::{SourceKind, Usage};

    fn turn(session: &str, idx: u64, model: &str, cache_read: u64) -> TurnRecord {
        TurnRecord {
            v: 1,
            source: SourceKind::ClaudeCode,
            session_id: session.to_string(),
            session_path: None,
            message_id: format!("m-{}", idx),
            turn_index: idx,
            ts: "2026-04-23T00:00:00.000Z".to_string(),
            model: model.to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 0,
                output: 0,
                reasoning: 0,
                cache_read,
                cache_create_5m: 0,
                cache_create_1h: 0,
            },
            tool_calls: Vec::new(),
            files_touched: None,
            subagent: None,
            stop_reason: None,
            activity: None,
            retries: None,
            has_edits: None,
            fidelity: None,
        }
    }

    #[test]
    fn parses_no_heading_file_as_single_preamble() {
        let parsed = parse_claude_md("/p/C.md", "just some prose\nover two lines\n");
        assert_eq!(parsed.grouping_level, 0);
        assert_eq!(parsed.sections.len(), 1);
        assert_eq!(parsed.sections[0].heading, "(preamble)");
        assert_eq!(parsed.total_lines, 2);
        // Σ section bytes == total bytes (preamble covers the whole file).
        assert_eq!(parsed.sections[0].bytes, parsed.bytes);
    }

    #[test]
    fn groups_at_h2_when_h2_exists() {
        let text = "# Title\n\n## Alpha\nbody-a\n## Beta\nbody-b\n";
        let parsed = parse_claude_md("/p/C.md", text);
        assert_eq!(parsed.grouping_level, 2);
        // preamble (Title) + Alpha + Beta
        assert_eq!(parsed.sections.len(), 3);
        assert_eq!(parsed.sections[0].heading, "(preamble)");
        assert_eq!(parsed.sections[1].heading, "## Alpha");
        assert_eq!(parsed.sections[2].heading, "## Beta");
        // Σ section.bytes == file.bytes (additivity invariant the attribution
        // arithmetic relies on for the 1e-9 USD precision contract).
        let sum: u64 = parsed.sections.iter().map(|s| s.bytes).sum();
        assert_eq!(sum, parsed.bytes);
    }

    #[test]
    fn skips_headings_inside_fenced_code_blocks() {
        let text = "## Real\nbody\n```\n## Fake\n```\n## Also Real\n";
        let parsed = parse_claude_md("/p/C.md", text);
        assert_eq!(parsed.grouping_level, 2);
        let names: Vec<_> = parsed
            .sections
            .iter()
            .map(|s| s.heading.as_str())
            .collect();
        assert_eq!(names, vec!["## Real", "## Also Real"]);
    }

    #[test]
    fn attribution_zero_when_total_tokens_zero() {
        let pricing = PricingTable::new();
        let parsed = ParsedClaudeMd {
            path: "/p/C.md".to_string(),
            total_lines: 0,
            bytes: 0,
            tokens: 0,
            sections: Vec::new(),
            grouping_level: 0,
        };
        let turns = vec![turn("s", 0, "claude-sonnet-4-6", 1000)];
        let result = attribute_claude_md(AttributeClaudeMdInput {
            files: &[parsed],
            turns: &turns,
            pricing: &pricing,
        });
        assert_eq!(result.total_tokens, 0);
        assert_eq!(result.total_cost, 0.0);
        assert_eq!(result.session_count, 0);
    }
}
