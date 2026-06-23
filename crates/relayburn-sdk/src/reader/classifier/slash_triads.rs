//! Slash-command triad + task-notification row detection — extracted from the
//! classifier.

use serde_json::{Map, Value};

// ---------------------------------------------------------------------------
// Harness-injected `<task-notification>` row detector.
//
// Claude Code's harness writes synthetic rows when a background Bash task
// finishes. They share the envelope shape of queued user prompts, so a
// text-prefix-only filter (just checking for `<task-notification>` in
// content) would false-positive on a real user prompt that legitimately
// types that string. Each clause AND-checks shape AND purpose so
// legitimate prompts with the same shape but a different `commandMode` /
// `origin.kind` survive.
// ---------------------------------------------------------------------------

/// True when a raw JSONL row represents a harness-injected
/// `<task-notification>` synthetic message rather than a real user prompt.
/// Three clauses, each ANDing a shape check with a purpose check:
///
/// 1. `type == "queue-operation"` AND `content` is a string starting with
///    `<task-notification>`.
/// 2. `origin.kind == "task-notification"`.
/// 3. `attachment.type == "queued_command"` AND
///    `attachment.commandMode == "task-notification"`.
pub fn is_task_notification(row: &Map<String, Value>) -> bool {
    // Clause 1: queue-operation row whose top-level content starts with the
    // `<task-notification>` marker.
    if row.get("type").and_then(Value::as_str) == Some("queue-operation")
        && row
            .get("content")
            .and_then(Value::as_str)
            .is_some_and(|s| s.starts_with("<task-notification>"))
    {
        return true;
    }
    // Clause 2: explicit `origin.kind` marker. The shape check is implicit
    // in dereferencing `origin` as an object; the purpose check is the
    // `task-notification` string match.
    if row
        .get("origin")
        .and_then(Value::as_object)
        .and_then(|o| o.get("kind"))
        .and_then(Value::as_str)
        == Some("task-notification")
    {
        return true;
    }
    // Clause 3: queued-command attachment whose `commandMode` is
    // `task-notification`. Both fields must match — a legitimate queued
    // user prompt has `attachment.type == "queued_command"` but a different
    // `commandMode`, and must survive this check.
    if let Some(att) = row.get("attachment").and_then(Value::as_object) {
        let ty = att.get("type").and_then(Value::as_str);
        let mode = att.get("commandMode").and_then(Value::as_str);
        if ty == Some("queued_command") && mode == Some("task-notification") {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Slash-command triad detector — see #438.
//
// Claude Code's slash commands (`/review`, `/init`, custom skills) emit a
// deterministic three-row sequence in the JSONL transcript:
//
//   caveat       — synthetic user row introducing the command. Body opens
//                  with the literal `Caveat:` prefix and the row is the
//                  apparent root of the parent chain for the next two
//                  rows. Carries no real user intent — it's a harness
//                  artifact.
//   invocation   — user row whose `parentUuid == caveat.uuid`. Body
//                  carries the synthetic command envelope:
//                  `<command-name>`, `<command-message>`, optionally
//                  `<command-args>`.
//   stdout       — user row whose `parentUuid == invocation.uuid`. Body
//                  carries the captured stdout in a `<local-command-stdout>`
//                  block.
//
// The classifier historically treated each row as a separate activity,
// which trebled the apparent activity count for sessions that lean on
// slash commands. The detector here collapses the triad into one
// synthetic `Skill` activity for downstream rollups; token attribution
// stays on the underlying rows so `burn hotspots` isn't double-charged.
//
// Pinning detection on parent-UUID chain shape (not on the exact text
// prefix of the caveat row) is deliberate: the literal `Caveat:` opener
// has drifted across Claude Code versions, but the chain shape — three
// rows linked caveat → invocation → stdout — has stayed stable. We use
// the text markers `<command-name>` and `<local-command-stdout>` as
// purpose checks on the invocation and stdout rows (so an unrelated
// three-row chain that happens to look structurally similar — e.g. a
// real user prompt followed by an assistant reply followed by a tool
// result — does NOT misdetect), but the chain-shape predicate carries
// the primary signal. See `is_task_notification` (#442) for the same
// shape-AND-purpose pattern.

const CAVEAT_PREFIX: &str = "Caveat:";
const COMMAND_NAME_OPEN: &str = "<command-name>";
const COMMAND_NAME_CLOSE: &str = "</command-name>";
const LOCAL_STDOUT_OPEN: &str = "<local-command-stdout>";

/// One detected slash-command triad: the three row indices into the
/// original `rows` slice and the extracted skill name (when the
/// invocation body exposed a `<command-name>` block; `None` otherwise).
///
/// Indices are stable references into the caller's input — the
/// downstream wiring in the Claude reader uses them to override per-row
/// activity attribution without re-walking the input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashTriad {
    pub caveat_idx: usize,
    pub invocation_idx: usize,
    pub stdout_idx: usize,
    pub skill_name: Option<String>,
}

/// Detect Claude slash-command triads in a flat slice of raw JSONL rows
/// (already-parsed as `serde_json::Map`s).
///
/// The walk is O(n): build a `uuid -> index` index once, then for each
/// candidate caveat row look up the two children by their UUID chain.
/// Each row is consumed by at most one triad — the row-index sets are
/// disjoint by construction (we mark each used index in a bitset).
///
/// Caller obligations:
///
/// - The slice must preserve JSONL emission order. The detector does
///   not sort or filter — it inspects rows in place.
/// - Rows are matched on the parent-UUID chain shape; text checks on
///   the invocation and stdout rows are purpose guards that block
///   structurally-similar but semantically-different chains (e.g. a
///   normal user → assistant → tool_result chain) from misdetecting.
pub fn detect_slash_triads(rows: &[Map<String, Value>]) -> Vec<SlashTriad> {
    if rows.len() < 3 {
        return Vec::new();
    }
    // Track rows already consumed by a prior triad so a single row can't
    // be the invocation of triad A and the caveat of triad B simultaneously.
    let mut consumed = vec![false; rows.len()];
    let mut out: Vec<SlashTriad> = Vec::new();

    for (caveat_idx, caveat) in rows.iter().enumerate() {
        if consumed[caveat_idx] {
            continue;
        }
        if !is_caveat_row(caveat) {
            continue;
        }
        let caveat_uuid = match caveat.get("uuid").and_then(Value::as_str) {
            Some(u) if !u.is_empty() => u,
            _ => continue,
        };
        // The invocation row must (a) point at the caveat via parentUuid
        // AND (b) carry an invocation body. The latter is the purpose
        // check that blocks an unrelated child of the caveat from
        // promoting the chain into a Skill.
        let (invocation_idx, invocation) = match find_first_unconsumed_child(
            rows,
            &consumed,
            caveat_uuid,
            caveat_idx + 1,
            is_invocation_row,
        ) {
            Some(pair) => pair,
            None => continue,
        };
        let invocation_uuid = match invocation.get("uuid").and_then(Value::as_str) {
            Some(u) if !u.is_empty() => u,
            _ => continue,
        };
        let (stdout_idx, _stdout) = match find_first_unconsumed_child(
            rows,
            &consumed,
            invocation_uuid,
            invocation_idx + 1,
            is_stdout_row,
        ) {
            Some(pair) => pair,
            None => continue,
        };
        let skill_name = extract_skill_name(invocation).or_else(|| extract_skill_name(caveat));
        consumed[caveat_idx] = true;
        consumed[invocation_idx] = true;
        consumed[stdout_idx] = true;
        out.push(SlashTriad {
            caveat_idx,
            invocation_idx,
            stdout_idx,
            skill_name,
        });
    }
    out
}

/// True when this row matches the caveat shape: a user-typed row whose
/// extracted text body begins with the `Caveat:` literal. We deliberately
/// keep this lightweight — the structural check (caveat is the root of
/// the chain a downstream invocation/stdout points at) carries the
/// primary signal.
fn is_caveat_row(row: &Map<String, Value>) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    extract_row_text(row)
        .map(|s| s.trim_start().starts_with(CAVEAT_PREFIX))
        .unwrap_or(false)
}

/// True when this row carries a `<command-name>` block — the invocation
/// payload of a slash command.
fn is_invocation_row(row: &Map<String, Value>) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    extract_row_text(row)
        .map(|s| s.contains(COMMAND_NAME_OPEN))
        .unwrap_or(false)
}

/// True when this row carries a `<local-command-stdout>` block — the
/// stdout-capture payload of a slash command.
fn is_stdout_row(row: &Map<String, Value>) -> bool {
    if row.get("type").and_then(Value::as_str) != Some("user") {
        return false;
    }
    extract_row_text(row)
        .map(|s| s.contains(LOCAL_STDOUT_OPEN))
        .unwrap_or(false)
}

/// Walk forward from `start_idx` looking for the first not-yet-consumed
/// row whose `parentUuid == parent_uuid` and which passes the purpose
/// `check`. Returns `(idx, &row)` or `None`. Forward-only because the
/// JSONL ordering for these rows is stable in practice (the harness
/// writes caveat → invocation → stdout adjacently); a sibling reorder
/// would land both the invocation and stdout *after* the caveat.
fn find_first_unconsumed_child<'a, F>(
    rows: &'a [Map<String, Value>],
    consumed: &[bool],
    parent_uuid: &str,
    start_idx: usize,
    check: F,
) -> Option<(usize, &'a Map<String, Value>)>
where
    F: Fn(&Map<String, Value>) -> bool,
{
    for (offset, row) in rows[start_idx..].iter().enumerate() {
        let idx = start_idx + offset;
        if consumed[idx] {
            continue;
        }
        let pu = row.get("parentUuid").and_then(Value::as_str).unwrap_or("");
        if pu != parent_uuid {
            continue;
        }
        if check(row) {
            return Some((idx, row));
        }
    }
    None
}

/// Pull `<command-name>...</command-name>` text from either the caveat
/// or invocation row. Returns `None` if no marker block is present.
fn extract_skill_name(row: &Map<String, Value>) -> Option<String> {
    let text = extract_row_text(row)?;
    let open = text.find(COMMAND_NAME_OPEN)?;
    let after = &text[open + COMMAND_NAME_OPEN.len()..];
    let close = after.find(COMMAND_NAME_CLOSE)?;
    let raw = after[..close].trim();
    if raw.is_empty() {
        return None;
    }
    // Tolerate the optional leading `/` (matches the ghost_surface
    // miner's accept-with-or-without convention).
    Some(raw.trim_start_matches('/').to_string())
}

/// Extract the row's plain text body (string content, or concatenation
/// of `text` / `content` strings inside an array body). Mirrors
/// `extract_plain_user_text_from_obj` from the Claude reader closely
/// enough to detect markers; we read the `content` field of a user-typed
/// row whose body may be a string or a list of blocks.
fn extract_row_text(row: &Map<String, Value>) -> Option<String> {
    let body = row.get("message").and_then(|m| m.get("content"))?;
    if let Some(s) = body.as_str() {
        if s.is_empty() {
            return None;
        }
        return Some(s.to_string());
    }
    let arr = body.as_array()?;
    let mut parts: Vec<String> = Vec::new();
    for block in arr {
        let bo = match block.as_object() {
            Some(o) => o,
            None => continue,
        };
        // `{"type":"text","text":"..."}` — assistant-style text block.
        if bo.get("type").and_then(Value::as_str) == Some("text") {
            if let Some(s) = bo.get("text").and_then(Value::as_str) {
                if !s.is_empty() {
                    parts.push(s.to_string());
                }
            }
            continue;
        }
        // `{"type":"tool_result","content":"..."}` — string-content tool
        // result envelope. The slash-command stdout row can ship its
        // payload this way; we still want the marker visible to the
        // purpose check.
        if let Some(s) = bo.get("content").and_then(Value::as_str) {
            if !s.is_empty() {
                parts.push(s.to_string());
            }
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("\n"))
    }
}
