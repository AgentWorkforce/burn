use super::*;

use std::collections::HashMap;

use crate::reader::{SourceKind, TurnRecord, UserTurnRecord};

use crate::analyze::cost::total_cost_for_turn;
use crate::analyze::findings::{SkillPruningProtection, SkillRecallDup, SystemPromptTax};
use crate::analyze::pricing::PricingTable;

pub(crate) fn detect_skill_recall_dups_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<SkillRecallDup> {
    if turns.is_empty() || turns[0].source != SourceKind::Opencode {
        return Vec::new();
    }
    let mut order: Vec<String> = Vec::new();
    let mut by_name: HashMap<String, Vec<ToolCallRef<'_>>> = HashMap::new();
    let flat = flatten_tool_calls(turns);
    for r in &flat {
        if r.call.name != "skill" {
            continue;
        }
        let Some(skill_name) = r.call.skill_name.as_deref() else {
            continue;
        };
        if !by_name.contains_key(skill_name) {
            order.push(skill_name.to_string());
        }
        by_name.entry(skill_name.to_string()).or_default().push(*r);
    }
    let mut out: Vec<SkillRecallDup> = Vec::new();
    for name in order {
        let refs = by_name.get(&name).unwrap();
        if refs.len() < 2 {
            continue;
        }
        let first = refs.first().unwrap();
        let last = refs.last().unwrap();
        let turns_in_streak: Vec<&TurnRecord> = refs.iter().map(|r| r.turn).collect();
        let contributing = dedup_turns(turns_in_streak);
        out.push(SkillRecallDup {
            session_id: session_id.to_string(),
            skill_name: name,
            call_count: refs.len() as u64,
            first_turn_index: first.turn.turn_index,
            last_turn_index: last.turn.turn_index,
            cost: sum_cost_for_turns(&contributing, pricing),
        });
    }
    out
}

pub(crate) fn detect_skill_pruning_protection_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
) -> Vec<SkillPruningProtection> {
    if turns.is_empty() || turns[0].source != SourceKind::Opencode {
        return Vec::new();
    }
    let mut out: Vec<SkillPruningProtection> = Vec::new();
    let flat = flatten_tool_calls(turns);
    for r in &flat {
        if r.call.name != "skill" {
            continue;
        }
        let Some(skill_name) = r.call.skill_name.clone() else {
            continue;
        };
        let invoke_index = r.turn.turn_index;
        let mut riding_turns = 0_u64;
        let mut last_cached_turn_index = invoke_index;
        let mut riding_cost = 0.0_f64;
        for t in turns {
            if t.turn_index <= invoke_index {
                continue;
            }
            if t.usage.cache_read > 0 {
                riding_turns += 1;
                last_cached_turn_index = t.turn_index;
                riding_cost += total_cost_for_turn(t, pricing);
            }
        }
        if riding_turns == 0 {
            continue;
        }
        let invoke_cost = total_cost_for_turn(r.turn, pricing);
        out.push(SkillPruningProtection {
            session_id: session_id.to_string(),
            skill_name,
            invoked_turn_index: invoke_index,
            riding_turns,
            last_cached_turn_index,
            cost: invoke_cost + riding_cost,
        });
    }
    out
}

pub(crate) fn detect_system_prompt_tax_for_session(
    session_id: &str,
    turns: &[&TurnRecord],
    pricing: &PricingTable,
    user_turns: Option<&[UserTurnRecord]>,
) -> Vec<SystemPromptTax> {
    if turns.is_empty() || turns[0].source != SourceKind::Opencode {
        return Vec::new();
    }
    let first_turn = turns[0];
    let first_cache_create = first_turn.usage.cache_create_5m + first_turn.usage.cache_create_1h;
    if first_cache_create == 0 {
        return Vec::new();
    }
    let mut first_user_tokens = 0_u64;
    if let Some(ut) = user_turns {
        if let Some(first_user_turn) = ut.first() {
            for block in &first_user_turn.blocks {
                first_user_tokens += block.approx_tokens;
            }
        }
    }
    if first_user_tokens == 0 {
        return Vec::new();
    }
    let system_prompt_tokens = first_cache_create.saturating_sub(first_user_tokens);
    if system_prompt_tokens == 0 {
        return Vec::new();
    }

    let mut riding_turns = 0_u64;
    let mut total_cost = 0.0_f64;
    for t in turns {
        // Skip the first turn — its cost is the cacheCreate, not the riding
        // tax (patterns.ts:1241-1243).
        if t.message_id == first_turn.message_id && t.turn_index == first_turn.turn_index {
            continue;
        }
        if t.usage.cache_read > 0 {
            riding_turns += 1;
            total_cost += total_cost_for_turn(t, pricing);
        }
    }
    if riding_turns == 0 {
        return Vec::new();
    }
    vec![SystemPromptTax {
        session_id: session_id.to_string(),
        first_turn_cache_create: first_cache_create,
        first_user_message_tokens: first_user_tokens,
        estimated_system_prompt_tokens: system_prompt_tokens,
        riding_turns,
        total_cost,
    }]
}
