//! One-session, input/output metrics surface.
//!
//! Unlike [`crate::ingest`], this module never discovers session stores and
//! never opens or mutates a Burn ledger. The caller supplies one exact
//! session source plus its harness; Burn parses that input and returns a
//! versioned metrics document suitable for a control plane such as Cloud.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::analyze::{cost_for_turn, load_pricing, provider_for};
use crate::reader::{
    parse_claude_session, parse_codex_session_incremental, parse_opencode_session_incremental,
    ClaudeParseOptions, ParseCodexIncrementalOptions, ParseOpencodeIncrementalOptions, TurnRecord,
};
use crate::Harness;

pub const SESSION_METRICS_SCHEMA: &str = "burn.session-metrics.v1";

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MeasureSessionOptions {
    /// The one session source to parse. Claude Code and Codex use a transcript
    /// file. OpenCode uses the selected session metadata file inside its
    /// storage tree and reads only that session's message/part records.
    pub input_path: PathBuf,
    pub harness: Harness,
    /// Optional pricing override in models.dev format. Built-in pricing is
    /// always available and this file only overlays it.
    pub pricing_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionTokenMetrics {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub reasoning_tokens: u64,
    pub total_tokens: u64,
}

impl SessionTokenMetrics {
    fn add_turn(&mut self, turn: &TurnRecord) {
        let usage = &turn.usage;
        let cache_write = usage.cache_create_5m.saturating_add(usage.cache_create_1h);
        self.input_tokens = self.input_tokens.saturating_add(usage.input);
        self.output_tokens = self.output_tokens.saturating_add(usage.output);
        self.cache_read_tokens = self.cache_read_tokens.saturating_add(usage.cache_read);
        self.cache_write_tokens = self.cache_write_tokens.saturating_add(cache_write);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(usage.reasoning);
        self.total_tokens = self
            .total_tokens
            .saturating_add(usage.input)
            .saturating_add(usage.output)
            // Codex includes reasoning in output; the other supported sources
            // expose a separate, billable reasoning bucket.
            .saturating_add(if matches!(turn.source, crate::reader::SourceKind::Codex) {
                0
            } else {
                usage.reasoning
            })
            .saturating_add(usage.cache_read)
            .saturating_add(cache_write);
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModelMetrics {
    pub provider: String,
    pub model: String,
    pub turn_count: u64,
    pub usage: SessionTokenMetrics,
    /// `None` means at least one contributing turn had no known price. Cloud
    /// must preserve that as unknown rather than silently displaying $0.
    pub cost_usd_micros: Option<u64>,
    pub priced_turns: u64,
    pub unpriced_turns: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionMetrics {
    pub schema: String,
    pub session_id: Option<String>,
    pub harness: Harness,
    pub turn_count: u64,
    pub usage: SessionTokenMetrics,
    /// Present only when every turn was priced.
    pub cost_usd_micros: Option<u64>,
    pub priced_turns: u64,
    pub unpriced_turns: u64,
    pub models: Vec<SessionModelMetrics>,
}

#[derive(Default)]
struct ModelAccumulator {
    turn_count: u64,
    usage: SessionTokenMetrics,
    cost_usd: f64,
    priced_turns: u64,
    unpriced_turns: u64,
}

fn usd_to_micros(value: f64) -> Option<u64> {
    let micros = value * 1_000_000.0;
    (micros.is_finite() && micros >= 0.0 && micros <= u64::MAX as f64)
        .then(|| micros.round() as u64)
}

fn parse_one(options: &MeasureSessionOptions) -> Result<Vec<TurnRecord>> {
    if !options.input_path.is_file() {
        bail!(
            "session input is not a file: {}",
            options.input_path.display()
        );
    }

    let session_path = Some(options.input_path.to_string_lossy().into_owned());
    let turns = match options.harness {
        Harness::ClaudeCode => {
            parse_claude_session(
                &options.input_path,
                &ClaudeParseOptions {
                    session_path,
                    content_mode: None,
                    file_session_id: None,
                },
            )
            .context("parse Claude Code session")?
            .turns
        }
        Harness::Codex => {
            parse_codex_session_incremental(
                &options.input_path,
                &ParseCodexIncrementalOptions {
                    session_path,
                    content_mode: None,
                    tokenizer: None,
                    start_offset: Some(0),
                    resume: None,
                },
            )
            .context("parse Codex session")?
            .turns
        }
        Harness::Opencode => {
            parse_opencode_session_incremental(
                &options.input_path,
                &ParseOpencodeIncrementalOptions {
                    session_path,
                    content_mode: None,
                    tokenizer: None,
                    seen_message_ids: None,
                },
            )
            .context("parse OpenCode session")?
            .turns
        }
    };
    Ok(turns)
}

/// Measure one caller-selected session without discovery, ingestion, or a
/// ledger. This is the preferred runtime boundary for sandbox → Cloud usage
/// reporting; discovery-oriented Burn commands remain available for local
/// historical analysis.
pub fn measure_session(options: MeasureSessionOptions) -> Result<SessionMetrics> {
    let turns = parse_one(&options)?;
    let pricing = load_pricing(options.pricing_path.as_deref());
    let mut session_ids = BTreeSet::new();
    let mut usage = SessionTokenMetrics::default();
    let mut by_model: BTreeMap<(String, String), ModelAccumulator> = BTreeMap::new();
    let mut priced_turns = 0_u64;
    let mut unpriced_turns = 0_u64;

    if turns.is_empty() {
        if matches!(options.harness, Harness::Opencode) {
            bail!(
                "OpenCode session produced no measurable turns; provide the selected session metadata file inside a complete storage tree containing message/<sessionId> and part/<messageId> records"
            );
        }
        bail!("session input produced no measurable turns");
    }

    for turn in &turns {
        session_ids.insert(turn.session_id.clone());
        usage.add_turn(turn);
        let provider = provider_for(turn).provider;
        let row = by_model.entry((provider, turn.model.clone())).or_default();
        row.turn_count = row.turn_count.saturating_add(1);
        row.usage.add_turn(turn);
        if let Some(cost) = cost_for_turn(turn, &pricing) {
            row.cost_usd += cost.total;
            row.priced_turns = row.priced_turns.saturating_add(1);
            priced_turns = priced_turns.saturating_add(1);
        } else {
            row.unpriced_turns = row.unpriced_turns.saturating_add(1);
            unpriced_turns = unpriced_turns.saturating_add(1);
        }
    }

    let session_id = match session_ids.len() {
        0 => None,
        1 => session_ids.into_iter().next(),
        _ => bail!("input contains turns from more than one session"),
    };
    let models: Vec<SessionModelMetrics> = by_model
        .into_iter()
        .map(|((provider, model), row)| SessionModelMetrics {
            provider,
            model,
            turn_count: row.turn_count,
            usage: row.usage,
            cost_usd_micros: (row.unpriced_turns == 0)
                .then(|| usd_to_micros(row.cost_usd))
                .flatten(),
            priced_turns: row.priced_turns,
            unpriced_turns: row.unpriced_turns,
        })
        .collect();
    // The document-level integer total is derived from the already-rounded
    // model rows. This makes Cloud's reconciliation invariant exact:
    // session cost == sum(models[].cost), never a second independent f64
    // rounding of the same turns.
    let cost_usd_micros = if unpriced_turns == 0 {
        models.iter().try_fold(0_u64, |total, row| {
            row.cost_usd_micros.and_then(|cost| total.checked_add(cost))
        })
    } else {
        None
    };

    Ok(SessionMetrics {
        schema: SESSION_METRICS_SCHEMA.to_string(),
        session_id,
        harness: options.harness,
        turn_count: turns.len() as u64,
        usage,
        cost_usd_micros,
        priced_turns,
        unpriced_turns,
        models,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(path: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/fixtures")
            .join(path)
    }

    #[test]
    fn measures_one_claude_transcript_without_a_ledger() {
        let report = measure_session(MeasureSessionOptions {
            input_path: fixture("claude/simple-turn.jsonl"),
            harness: Harness::ClaudeCode,
            pricing_path: None,
        })
        .expect("measure fixture");

        assert_eq!(report.schema, SESSION_METRICS_SCHEMA);
        assert_eq!(
            report.session_id.as_deref(),
            Some("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(report.turn_count, 1);
        assert_eq!(report.usage.input_tokens, 10);
        assert_eq!(report.usage.output_tokens, 5);
        assert_eq!(report.usage.cache_read_tokens, 500);
        assert_eq!(report.usage.cache_write_tokens, 100);
        assert_eq!(report.usage.total_tokens, 615);
        assert_eq!(report.models.len(), 1);
        assert_eq!(report.models[0].provider, "anthropic");
        assert_eq!(report.models[0].model, "claude-sonnet-4-6");
    }

    #[test]
    fn measures_one_codex_transcript_with_reasoning_bucket() {
        let report = measure_session(MeasureSessionOptions {
            input_path: fixture("codex/simple-turn.jsonl"),
            harness: Harness::Codex,
            pricing_path: None,
        })
        .expect("measure fixture");

        assert_eq!(report.session_id.as_deref(), Some("sess_simple_1"));
        assert_eq!(report.turn_count, 1);
        assert_eq!(report.usage.input_tokens, 600);
        assert_eq!(report.usage.cache_read_tokens, 400);
        assert_eq!(report.usage.output_tokens, 120);
        assert_eq!(report.usage.reasoning_tokens, 30);
        // Codex output includes reasoning and input excludes the cache-read
        // bucket, so totalTokens counts the primary billing buckets once.
        assert_eq!(report.usage.total_tokens, 1_120);
        assert_eq!(report.models[0].provider, "openai");
    }

    #[test]
    fn opencode_counts_separate_reasoning_and_reconciles_model_costs() {
        let report = measure_session(MeasureSessionOptions {
            input_path: fixture("opencode/multi-turn/storage/session/global/ses_multi.json"),
            harness: Harness::Opencode,
            pricing_path: None,
        })
        .expect("measure fixture");

        assert_eq!(report.turn_count, 2);
        assert_eq!(report.usage.reasoning_tokens, 50);
        assert_eq!(report.usage.total_tokens, 33_360);
        assert_eq!(report.models.len(), 2);
        assert_eq!(
            report.cost_usd_micros,
            Some(
                report
                    .models
                    .iter()
                    .map(|model| model.cost_usd_micros.expect("priced fixture"))
                    .sum()
            )
        );
    }

    #[test]
    fn rejects_incomplete_opencode_session_instead_of_reporting_zero_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("ses_incomplete.json");
        std::fs::write(&input, r#"{"id":"ses_incomplete","directory":"/tmp"}"#)
            .expect("write fixture");

        let error = measure_session(MeasureSessionOptions {
            input_path: input,
            harness: Harness::Opencode,
            pricing_path: None,
        })
        .expect_err("incomplete OpenCode session must fail closed");

        assert!(error.to_string().contains("no measurable turns"));
    }

    #[test]
    fn preserves_unknown_cost_instead_of_reporting_free_usage() {
        let dir = tempfile::tempdir().expect("tempdir");
        let input = dir.path().join("unknown-model.jsonl");
        let fixture = std::fs::read_to_string(fixture("claude/simple-turn.jsonl"))
            .expect("read fixture")
            .replace("claude-sonnet-4-6", "unpriced-house-model");
        std::fs::write(&input, fixture).expect("write fixture");

        let report = measure_session(MeasureSessionOptions {
            input_path: input,
            harness: Harness::ClaudeCode,
            pricing_path: None,
        })
        .expect("measure fixture");

        assert_eq!(report.cost_usd_micros, None);
        assert_eq!(report.priced_turns, 0);
        assert_eq!(report.unpriced_turns, 1);
        assert_eq!(report.models[0].cost_usd_micros, None);
    }
}
