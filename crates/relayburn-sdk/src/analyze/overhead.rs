//! Per-file overhead attribution (`CLAUDE.md` for Claude Code, `AGENTS.md`
//! for Codex/OpenCode). Rust port of `packages/analyze/src/overhead.ts`.
//!
//! Composes the `claude_md` parser/attributor over a small set of
//! source-filtered file inputs: each file declares which `SourceKind`s read
//! it into their cached prompt prefix, and only matching turns contribute to
//! that file's cost. The math is `f64` and matches the TS reduce order so the
//! per-file / per-section USD totals stay within the 1e-9 USD precision
//! contract called out in AgentWorkforce/burn#244 and #276.

use std::fs;
use std::path::Path;

use crate::reader::{SourceKind, TurnRecord};
use serde::{Deserialize, Serialize};

use crate::analyze::claude_md::{
    attribute_claude_md_refs, load_claude_md_file, ClaudeMdAttributionResult, ParsedClaudeMd,
};
use crate::analyze::pricing::PricingTable;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OverheadFileKind {
    ClaudeMd,
    AgentsMd,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverheadFile {
    pub kind: OverheadFileKind,
    pub path: String,
    /// Which agent sources read this file into their cached context. A turn's
    /// `source` must be in this list for the file to count toward that turn.
    pub applies_to: Vec<SourceKind>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOverheadFile {
    pub file: OverheadFile,
    pub parsed: ParsedClaudeMd,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverheadFileAttribution {
    pub file: OverheadFile,
    pub parsed: ParsedClaudeMd,
    pub attribution: ClaudeMdAttributionResult,
}

#[derive(Debug, Clone, PartialEq)]
pub struct OverheadAttribution {
    pub per_file: Vec<OverheadFileAttribution>,
    pub grand_total: f64,
    /// Count of distinct turns that contributed to at least one file's cost.
    /// Not the sum of per-file `riding_turns` — a turn could ride along in
    /// multiple files (e.g. `CLAUDE.md` + `.claude/CLAUDE.md`) and we don't
    /// want to double-count.
    pub total_riding_turns: u64,
}

pub struct AttributeOverheadInput<'a> {
    pub files: &'a [ParsedOverheadFile],
    pub turns: &'a [TurnRecord],
    pub pricing: &'a PricingTable,
}

struct Candidate {
    kind: OverheadFileKind,
    parts: &'static [&'static str],
    applies_to: &'static [SourceKind],
}

const CANDIDATES: &[Candidate] = &[
    Candidate {
        kind: OverheadFileKind::ClaudeMd,
        parts: &["CLAUDE.md"],
        applies_to: &[SourceKind::ClaudeCode],
    },
    Candidate {
        kind: OverheadFileKind::ClaudeMd,
        parts: &[".claude", "CLAUDE.md"],
        applies_to: &[SourceKind::ClaudeCode],
    },
    Candidate {
        kind: OverheadFileKind::AgentsMd,
        parts: &["AGENTS.md"],
        applies_to: &[SourceKind::Codex, SourceKind::Opencode],
    },
];

pub fn find_overhead_files(project_path: &Path) -> Vec<OverheadFile> {
    let mut out = Vec::new();
    for c in CANDIDATES {
        let mut abs = project_path.to_path_buf();
        for p in c.parts {
            abs = abs.join(p);
        }
        match fs::metadata(&abs) {
            Ok(meta) if meta.is_file() => {
                out.push(OverheadFile {
                    kind: c.kind,
                    path: abs.to_string_lossy().into_owned(),
                    applies_to: c.applies_to.to_vec(),
                });
            }
            _ => {}
        }
    }
    out
}

pub fn load_overhead_file(file: OverheadFile) -> std::io::Result<ParsedOverheadFile> {
    let parsed = load_claude_md_file(Path::new(&file.path))?;
    Ok(ParsedOverheadFile { file, parsed })
}

pub fn attribute_overhead(input: AttributeOverheadInput<'_>) -> OverheadAttribution {
    let mut per_file: Vec<OverheadFileAttribution> = Vec::new();
    // Per-session max riding-turns across every file. The eviction check is
    // `cache_read >= file_tokens`, so a smaller file's rides are a strict
    // superset of a larger file's rides for the same session+source. Taking
    // the max per session yields the correct count of distinct turns without
    // double-counting when CLAUDE.md and .claude/CLAUDE.md both attribute to
    // the same Claude Code session.
    let mut max_riding_by_session: std::collections::HashMap<String, u64> =
        std::collections::HashMap::new();

    for pf in input.files {
        let filtered: Vec<&TurnRecord> = input
            .turns
            .iter()
            .filter(|t| pf.file.applies_to.contains(&t.source))
            .collect();
        let attribution =
            attribute_claude_md_refs(std::slice::from_ref(&pf.parsed), &filtered, input.pricing);

        for sc in &attribution.session_costs {
            let prev = max_riding_by_session
                .get(&sc.session_id)
                .copied()
                .unwrap_or(0);
            if sc.riding_turns > prev {
                max_riding_by_session.insert(sc.session_id.clone(), sc.riding_turns);
            }
        }

        per_file.push(OverheadFileAttribution {
            file: pf.file.clone(),
            parsed: pf.parsed.clone(),
            attribution,
        });
    }

    let grand_total = per_file.iter().map(|f| f.attribution.total_cost).sum();
    let total_riding_turns = max_riding_by_session.values().sum();

    OverheadAttribution {
        per_file,
        grand_total,
        total_riding_turns,
    }
}

pub fn describe_applies_to(applies_to: &[SourceKind]) -> String {
    let mut as_strs: Vec<&'static str> = applies_to.iter().map(SourceKind::wire_str).collect();
    as_strs.sort_unstable();
    as_strs.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::claude_md::parse_claude_md;
    use crate::analyze::pricing::{ModelCost, ReasoningMode};
    use crate::reader::Usage;

    fn pricing_with(model: &str, cache_read: f64) -> PricingTable {
        let mut p = PricingTable::new();
        p.insert(
            model.to_string(),
            ModelCost {
                input: 3.0,
                output: 15.0,
                cache_read,
                cache_write: 3.75,
                reasoning: None,
                reasoning_mode: ReasoningMode::IncludedInOutput,
            },
        );
        p
    }

    fn mk_turn(session: &str, idx: u64, source: SourceKind, cache_read: u64) -> TurnRecord {
        TurnRecord {
            v: 1,
            source,
            session_id: session.to_string(),
            session_path: None,
            message_id: format!("m-{}", idx),
            turn_index: idx,
            ts: "2026-04-23T00:00:00.000Z".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            project: None,
            project_key: None,
            usage: Usage {
                input: 10,
                output: 10,
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
    fn find_overhead_files_discovers_all_three_with_correct_applies_to() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fs::write(root.join("CLAUDE.md"), "# root").unwrap();
        fs::create_dir_all(root.join(".claude")).unwrap();
        fs::write(root.join(".claude").join("CLAUDE.md"), "# nested").unwrap();
        fs::write(root.join("AGENTS.md"), "# agents").unwrap();

        let files = find_overhead_files(root);
        assert_eq!(files.len(), 3);
        let agents = files
            .iter()
            .find(|f| f.kind == OverheadFileKind::AgentsMd)
            .unwrap();
        assert_eq!(
            agents.applies_to,
            vec![SourceKind::Codex, SourceKind::Opencode]
        );
        let claude_count = files
            .iter()
            .filter(|f| f.kind == OverheadFileKind::ClaudeMd)
            .count();
        assert_eq!(claude_count, 2);
        for f in &files {
            if f.kind == OverheadFileKind::ClaudeMd {
                assert_eq!(f.applies_to, vec![SourceKind::ClaudeCode]);
            }
        }
    }

    #[test]
    fn routes_turns_by_source_and_grand_total_matches_per_file_sum_within_1e_9() {
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let claude_md =
            parse_claude_md("/p/CLAUDE.md", &format!("## Claude\n{}", "c".repeat(4000)));
        let agents_md =
            parse_claude_md("/p/AGENTS.md", &format!("## Agents\n{}", "a".repeat(4000)));

        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    applies_to: vec![SourceKind::ClaudeCode],
                },
                parsed: claude_md.clone(),
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::AgentsMd,
                    path: "/p/AGENTS.md".to_string(),
                    applies_to: vec![SourceKind::Codex, SourceKind::Opencode],
                },
                parsed: agents_md.clone(),
            },
        ];

        let turns = vec![
            mk_turn("s-cc", 0, SourceKind::ClaudeCode, claude_md.tokens + 500),
            mk_turn("s-cx", 0, SourceKind::Codex, agents_md.tokens + 500),
            mk_turn("s-oc", 0, SourceKind::Opencode, agents_md.tokens + 500),
        ];

        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        assert_eq!(result.per_file.len(), 2);

        let claude_attr = result
            .per_file
            .iter()
            .find(|p| p.file.kind == OverheadFileKind::ClaudeMd)
            .unwrap();
        let agents_attr = result
            .per_file
            .iter()
            .find(|p| p.file.kind == OverheadFileKind::AgentsMd)
            .unwrap();

        // Claude Code session attributes only to CLAUDE.md.
        assert_eq!(claude_attr.attribution.session_count, 1);
        assert_eq!(claude_attr.attribution.session_costs[0].session_id, "s-cc");
        let expected_claude = (claude_md.tokens as f64 / 1_000_000.0) * 0.30;
        assert!(
            (claude_attr.attribution.total_cost - expected_claude).abs() <= expected_claude * 0.10,
            "claude cost={} expected~{}",
            claude_attr.attribution.total_cost,
            expected_claude
        );

        // Agents file attributes to two sessions (codex + opencode).
        assert_eq!(agents_attr.attribution.session_count, 2);
        let expected_agents = 2.0 * (agents_md.tokens as f64 / 1_000_000.0) * 0.30;
        assert!(
            (agents_attr.attribution.total_cost - expected_agents).abs() <= expected_agents * 0.10,
            "agents cost={} expected~{}",
            agents_attr.attribution.total_cost,
            expected_agents
        );

        // 1e-9 USD precision gate: grand_total is the additive sum of per-file
        // total_cost. Same f64 reduce order as the TS implementation.
        let summed = claude_attr.attribution.total_cost + agents_attr.attribution.total_cost;
        assert!((result.grand_total - summed).abs() < 1e-9);
    }

    #[test]
    fn total_riding_turns_takes_max_per_session_not_sum() {
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let small = parse_claude_md("/p/CLAUDE.md", &format!("## S\n{}", "x".repeat(2000)));
        let big = parse_claude_md(
            "/p/.claude/CLAUDE.md",
            &format!("## B\n{}", "y".repeat(36000)),
        );
        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    applies_to: vec![SourceKind::ClaudeCode],
                },
                parsed: small.clone(),
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/.claude/CLAUDE.md".to_string(),
                    applies_to: vec![SourceKind::ClaudeCode],
                },
                parsed: big.clone(),
            },
        ];
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..5 {
            turns.push(mk_turn(
                "s-both",
                i,
                SourceKind::ClaudeCode,
                big.tokens + 1000,
            ));
        }
        for i in 5..8 {
            turns.push(mk_turn(
                "s-both",
                i,
                SourceKind::ClaudeCode,
                small.tokens + 500,
            ));
        }
        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let small_attr = result
            .per_file
            .iter()
            .find(|p| p.file.path == "/p/CLAUDE.md")
            .unwrap();
        let big_attr = result
            .per_file
            .iter()
            .find(|p| p.file.path == "/p/.claude/CLAUDE.md")
            .unwrap();
        assert_eq!(small_attr.attribution.session_costs[0].riding_turns, 8);
        assert_eq!(big_attr.attribution.session_costs[0].riding_turns, 5);
        // Correct: max(8, 5) == 8 (NOT 13).
        assert_eq!(result.total_riding_turns, 8);
    }

    #[test]
    fn does_not_cross_attribute_codex_to_claude_md() {
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let claude_md = parse_claude_md("/p/CLAUDE.md", &format!("## C\n{}", "x".repeat(4000)));
        let agents_md = parse_claude_md("/p/AGENTS.md", &format!("## A\n{}", "y".repeat(4000)));
        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    applies_to: vec![SourceKind::ClaudeCode],
                },
                parsed: claude_md,
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::AgentsMd,
                    path: "/p/AGENTS.md".to_string(),
                    applies_to: vec![SourceKind::Codex, SourceKind::Opencode],
                },
                parsed: agents_md,
            },
        ];
        let turns = vec![mk_turn("s-cx", 0, SourceKind::Codex, 50_000)];
        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });
        let claude_attr = result
            .per_file
            .iter()
            .find(|p| p.file.kind == OverheadFileKind::ClaudeMd)
            .unwrap();
        assert_eq!(claude_attr.attribution.total_cost, 0.0);
        assert_eq!(claude_attr.attribution.session_count, 0);
    }

    #[test]
    fn load_overhead_file_round_trips_via_find() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("AGENTS.md"), "## Section\nbody").unwrap();
        let files = find_overhead_files(dir.path());
        assert_eq!(files.len(), 1);
        let f = files.into_iter().next().unwrap();
        let parsed = load_overhead_file(f).unwrap();
        assert_eq!(parsed.parsed.sections[0].heading, "## Section");
    }

    #[test]
    fn describe_applies_to_returns_sorted_csv() {
        assert_eq!(
            describe_applies_to(&[SourceKind::Opencode, SourceKind::Codex]),
            "codex, opencode"
        );
        assert_eq!(
            describe_applies_to(&[SourceKind::ClaudeCode]),
            "claude-code"
        );
    }

    #[test]
    fn per_section_attribution_matches_grand_total_within_1e_9() {
        // Multi-file fixture corpus: per-file, per-section USD totals must
        // sum to the grand total within 1e-9 USD.
        let pricing = pricing_with("claude-sonnet-4-6", 0.30);
        let claude_md = parse_claude_md(
            "/p/CLAUDE.md",
            "## Alpha\nalpha-body alpha-body alpha-body\n\n## Beta\nbeta beta beta beta beta\n",
        );
        let agents_md = parse_claude_md(
            "/p/AGENTS.md",
            "## Gamma\ngamma gamma\n\n## Delta\ndelta delta delta\n",
        );
        let files = vec![
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::ClaudeMd,
                    path: "/p/CLAUDE.md".to_string(),
                    applies_to: vec![SourceKind::ClaudeCode],
                },
                parsed: claude_md.clone(),
            },
            ParsedOverheadFile {
                file: OverheadFile {
                    kind: OverheadFileKind::AgentsMd,
                    path: "/p/AGENTS.md".to_string(),
                    applies_to: vec![SourceKind::Codex, SourceKind::Opencode],
                },
                parsed: agents_md.clone(),
            },
        ];
        let mut turns: Vec<TurnRecord> = Vec::new();
        for i in 0..3 {
            turns.push(mk_turn(
                "s-cc",
                i,
                SourceKind::ClaudeCode,
                claude_md.tokens + 1000,
            ));
        }
        for i in 0..2 {
            turns.push(mk_turn(
                "s-cx",
                i,
                SourceKind::Codex,
                agents_md.tokens + 1000,
            ));
        }
        let result = attribute_overhead(AttributeOverheadInput {
            files: &files,
            turns: &turns,
            pricing: &pricing,
        });

        // For each per-file attribution, Σ section.total_cost ≤ file.total_cost
        // (≤ because section share is byte-additive while file tokens are
        // ceil-rounded). The grand total across files matches the additive
        // sum of per-file costs to within 1e-9 USD.
        let mut summed_per_file = 0.0_f64;
        for fa in &result.per_file {
            summed_per_file += fa.attribution.total_cost;
            let sec_sum: f64 = fa
                .attribution
                .section_costs
                .iter()
                .map(|s| s.total_cost)
                .sum();
            assert!(sec_sum <= fa.attribution.total_cost + 1e-9);
        }
        assert!((result.grand_total - summed_per_file).abs() < 1e-9);
    }
}
