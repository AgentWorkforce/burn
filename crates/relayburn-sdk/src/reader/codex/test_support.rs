//! Whole-file Codex parse entry point for tests: wraps
//! [`parse_codex_session_incremental`] from offset zero with no resume state.

use std::path::Path;

use crate::reader::inference::Inference;
use crate::reader::types::{
    CompactionEvent, ContentRecord, ContentStoreMode, SessionRelationshipRecord,
    ToolResultEventRecord, TurnRecord, UserTurnRecord,
};
use crate::reader::user_turn::UserTurnTokenizer;

use super::{
    parse_codex_session_incremental, ParseCodexIncrementalOptions, ParseCodexIncrementalResult,
};

#[derive(Debug, Clone, Default)]
pub struct ParseCodexOptions {
    pub session_path: Option<String>,
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
}

#[derive(Debug, Clone, Default)]
pub struct ParseCodexResult {
    pub turns: Vec<TurnRecord>,
    pub inferences: Vec<Inference>,
    pub content: Vec<ContentRecord>,
    pub events: Vec<CompactionEvent>,
    pub user_turns: Vec<UserTurnRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
}

pub fn parse_codex_session(
    file_path: impl AsRef<Path>,
    options: &ParseCodexOptions,
) -> std::io::Result<ParseCodexResult> {
    let inc_opts = ParseCodexIncrementalOptions {
        session_path: options.session_path.clone(),
        content_mode: options.content_mode,
        tokenizer: options.tokenizer,
        start_offset: Some(0),
        resume: None,
    };
    parse_codex_session_incremental(file_path, &inc_opts).map(ParseCodexResult::from)
}

impl From<ParseCodexIncrementalResult> for ParseCodexResult {
    fn from(r: ParseCodexIncrementalResult) -> Self {
        Self {
            turns: r.turns,
            inferences: r.inferences,
            content: r.content,
            events: r.events,
            user_turns: r.user_turns,
            relationships: r.relationships,
            tool_result_events: r.tool_result_events,
        }
    }
}
