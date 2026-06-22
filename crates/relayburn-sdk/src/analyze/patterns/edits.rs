use super::*;

use std::collections::HashMap;

use crate::reader::{count_retries, normalize_tool_name, SourceKind, ToolCall, TurnRecord};

use crate::analyze::findings::{EditHeavySession, EditRevertCycle, EditRevertSamplePreview};
use crate::analyze::pricing::PricingTable;

use super::shell::shell_command_has_file_read;

pub(crate) fn detect_edit_reverts_for_session<'a>(
    session_id: &str,
    turns: &'a [&'a TurnRecord],
    pricing: &PricingTable,
    content_index: Option<&ContentIndex>,
) -> Vec<EditRevertCycle> {
    struct EditSlot<'a> {
        pre_hash: Option<String>,
        post_hash: Option<String>,
        turn: &'a TurnRecord,
        tool_use_id: String,
    }
    let mut by_file: HashMap<String, Vec<EditSlot<'a>>> = HashMap::new();
    let mut cycles: Vec<EditRevertCycle> = Vec::new();

    let flat = flatten_tool_calls(turns);
    for r in &flat {
        let call = r.call;
        let Some(target) = call.target.as_deref() else {
            continue;
        };
        if call.name != "Edit" && call.name != "Write" && call.name != "NotebookEdit" {
            continue;
        }
        // Failed edits don't actually change file state. Mirrors patterns.ts:951-952.
        if call.is_error == Some(true) {
            continue;
        }
        let slot = EditSlot {
            pre_hash: call.edit_pre_hash.clone(),
            post_hash: call.edit_post_hash.clone(),
            turn: r.turn,
            tool_use_id: call.id.clone(),
        };
        let history = by_file.entry(target.to_string()).or_default();
        if let Some(post_hash) = &slot.post_hash {
            let match_idx = history
                .iter()
                .position(|prior| prior.pre_hash.as_deref() == Some(post_hash.as_str()));
            if let Some(idx) = match_idx {
                let first = &history[idx];
                let mut cycle = EditRevertCycle {
                    session_id: session_id.to_string(),
                    file_path: target.to_string(),
                    first_edit_turn_index: first.turn.turn_index,
                    revert_turn_index: r.turn.turn_index,
                    span_turns: r.turn.turn_index - first.turn.turn_index,
                    cost: sum_cost_for_turns(&dedup_turns(vec![first.turn, r.turn]), pricing),
                    sample_preview: None,
                };
                if let Some(content_idx) = content_index {
                    let first_edit = extract_edit_preview(
                        content_idx
                            .tool_uses
                            .get(&first.tool_use_id)
                            .map(|tu| &tu.input),
                    );
                    let revert = extract_edit_preview(
                        content_idx
                            .tool_uses
                            .get(&slot.tool_use_id)
                            .map(|tu| &tu.input),
                    );
                    if let (Some(first_edit), Some(revert)) = (first_edit, revert) {
                        cycle.sample_preview = Some(EditRevertSamplePreview { first_edit, revert });
                    }
                }
                cycles.push(cycle);
                // Reset the file's history. patterns.ts:982-984.
                by_file.insert(target.to_string(), Vec::new());
                continue;
            }
        }
        history.push(slot);
    }
    cycles
}

pub(crate) fn detect_edit_heavy_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<EditHeavySession> {
    if turns.is_empty() {
        return Vec::new();
    }
    let mut read_count: u64 = 0;
    let mut edit_count: u64 = 0;
    let mut likely_retries: u64 = 0;
    let mut edit_turns: Vec<&TurnRecord> = Vec::new();

    for t in turns {
        let mut turn_has_edit = false;
        for call in &t.tool_calls {
            let name = normalize_tool_name(&call.name);
            if is_read_for_edit_heavy(call, t.source) {
                read_count += 1;
            } else if is_edit_tool(name) {
                edit_count += 1;
                turn_has_edit = true;
            }
        }
        if turn_has_edit {
            edit_turns.push(*t);
        }
        likely_retries += count_retries(&t.tool_calls);
    }

    if edit_count < EDIT_HEAVY_MIN_EDITS {
        return Vec::new();
    }
    let ratio = if read_count == 0 {
        f64::INFINITY
    } else {
        edit_count as f64 / read_count as f64
    };
    if ratio <= EDIT_HEAVY_RATIO {
        return Vec::new();
    }
    vec![EditHeavySession {
        source: turns[0].source,
        session_id: session_id.to_string(),
        read_count,
        edit_count,
        ratio,
        likely_retries,
        cost: sum_cost_for_turns(&dedup_turns(edit_turns), pricing),
    }]
}

fn is_read_for_edit_heavy(call: &ToolCall, source: SourceKind) -> bool {
    if is_read_tool(normalize_tool_name(&call.name)) {
        return true;
    }
    source == SourceKind::Codex && is_codex_shell_file_read(call)
}

fn is_codex_shell_file_read(call: &ToolCall) -> bool {
    if !is_codex_shell_name(&call.name) {
        return false;
    }
    let Some(target) = call.target.as_deref() else {
        return false;
    };
    shell_command_has_file_read(target)
}
