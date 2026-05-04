//! OpenCode streaming ingestor — Rust port of
//! `packages/reader/src/opencode-stream.ts`.
//!
//! Stateful per-session ingestor consuming OpenCode `session.*` /
//! `message.*` SSE-style events and emitting per-turn `TurnRecord` /
//! `ContentRecord` / `ToolResultEventRecord` / `UserTurnRecord` /
//! `SessionRelationshipRecord` batches as each session becomes idle. Cursor
//! state round-trips byte-identically with the TS implementation so a
//! session ingested partly by TS and partly by Rust resumes cleanly.
//!
//! Watch-loop semantics live in `relayburn-ingest` (#245) — this crate
//! exposes only the per-tick ingest primitive.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{Map, Value};

use crate::classifier::{classify_activity, ClassificationInput};
use crate::fidelity::classify_fidelity;
use crate::git::ProjectResolver;
use crate::hash::{args_hash, content_hash};
use crate::types::{
    ContentKind, ContentRecord, ContentRole, ContentStoreMode, ContentToolResult, ContentToolUse,
    Coverage, Fidelity, RelationshipSourceKind, RelationshipType, SessionRelationshipRecord,
    SourceKind, Subagent, ToolCall, ToolResultEventRecord, ToolResultEventSource,
    ToolResultStatus, TurnRecord, Usage, UsageAttribution, UsageGranularity, UserTurnBlock,
    UserTurnRecord,
};
use crate::user_turn::{HeuristicCounter, TokenCounter, UserTurnTokenizer};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Round-trippable cursor capturing dedup state across `ingest` calls.
///
/// Field shape mirrors `OpencodeStreamCursorState` in the TS port; serde
/// rename keeps the on-wire JSON identical so a TS-written cursor can be
/// resumed by Rust and vice versa.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpencodeStreamCursorState {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitted_message_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub emitted_tool_event_ids: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_tool_event_index_by_session: Option<BTreeMap<String, u64>>,
}

#[derive(Debug, Clone, Default)]
pub struct OpencodeStreamIngestOptions {
    pub content_mode: Option<ContentStoreMode>,
    pub tokenizer: Option<UserTurnTokenizer>,
    pub cursor: Option<OpencodeStreamCursorState>,
}

#[derive(Debug, Clone, Default)]
pub struct OpencodeStreamIngestResult {
    pub turns: Vec<TurnRecord>,
    pub content: Vec<ContentRecord>,
    pub relationships: Vec<SessionRelationshipRecord>,
    pub tool_result_events: Vec<ToolResultEventRecord>,
    pub user_turns: Vec<UserTurnRecord>,
    pub cursor: OpencodeStreamCursorState,
}

/// Construct a fresh ingestor. Mirrors `createOpencodeStreamIngestor`. The
/// constructor is fallible because `Cl100k` is not yet wired up in the Rust
/// port (see #246) — passing it returns an error rather than silently
/// downgrading to the heuristic counter.
pub fn create_opencode_stream_ingestor(
    options: OpencodeStreamIngestOptions,
) -> std::io::Result<OpencodeStreamIngestor> {
    OpencodeStreamIngestor::new(options)
}

pub struct OpencodeStreamIngestor {
    content_mode: ContentStoreMode,
    counter: HeuristicCounter,
    project_resolver: ProjectResolver,
    sessions: HashMap<String, SessionInfo>,
    stream_owned_sessions: HashSet<String>,
    /// Insertion-ordered to mirror TS Map iteration (we sort by `time` later
    /// but the insertion-ordered queue makes the tie-breaking deterministic).
    message_order: Vec<String>,
    messages: HashMap<String, Message>,
    /// Per-message bucket of parts. Inner map is keyed by part id (or
    /// fallback `partKey`) and iterated lexicographically when emitting.
    parts_by_message: HashMap<String, BTreeMap<String, Value>>,
    emitted_message_ids: HashSet<String>,
    emitted_message_ids_order: Vec<String>,
    emitted_tool_event_ids: HashSet<String>,
    emitted_tool_event_ids_order: Vec<String>,
    next_tool_event_index_by_session: BTreeMap<String, u64>,
    last_event_id: Option<String>,
}

impl OpencodeStreamIngestor {
    fn new(options: OpencodeStreamIngestOptions) -> std::io::Result<Self> {
        if matches!(options.tokenizer, Some(UserTurnTokenizer::Cl100k)) {
            return Err(std::io::Error::other(
                "cl100k tokenizer is not yet available in the Rust port; \
                 omit `tokenizer` or pass `Some(Heuristic)` (see AgentWorkforce/burn#246)",
            ));
        }
        let cursor = options.cursor.unwrap_or_default();
        let emitted_message_ids_order = cursor.emitted_message_ids.clone().unwrap_or_default();
        let emitted_message_ids: HashSet<String> =
            emitted_message_ids_order.iter().cloned().collect();
        let emitted_tool_event_ids_order =
            cursor.emitted_tool_event_ids.clone().unwrap_or_default();
        let emitted_tool_event_ids: HashSet<String> =
            emitted_tool_event_ids_order.iter().cloned().collect();
        let mut next_tool_event_index_by_session: BTreeMap<String, u64> = BTreeMap::new();
        match cursor.next_tool_event_index_by_session.as_ref() {
            Some(map) => {
                for (sid, n) in map.iter() {
                    next_tool_event_index_by_session.insert(sid.clone(), *n);
                }
            }
            None => {
                for (sid, n) in derive_next_tool_event_index_by_session(&emitted_tool_event_ids_order) {
                    let cur = next_tool_event_index_by_session.get(&sid).copied().unwrap_or(0);
                    if n > cur {
                        next_tool_event_index_by_session.insert(sid, n);
                    }
                }
            }
        }
        Ok(Self {
            content_mode: options.content_mode.unwrap_or(ContentStoreMode::Full),
            counter: HeuristicCounter,
            project_resolver: ProjectResolver::new(),
            sessions: HashMap::new(),
            stream_owned_sessions: HashSet::new(),
            message_order: Vec::new(),
            messages: HashMap::new(),
            parts_by_message: HashMap::new(),
            emitted_message_ids,
            emitted_message_ids_order,
            emitted_tool_event_ids,
            emitted_tool_event_ids_order,
            next_tool_event_index_by_session,
            last_event_id: cursor.last_event_id,
        })
    }

    /// Ingest a single SSE-style event. `event_id` is the optional SSE id
    /// (when present and non-empty it's recorded in the cursor so resume
    /// picks up after this event).
    pub fn ingest(
        &mut self,
        payload: &Value,
        event_id: Option<&str>,
    ) -> OpencodeStreamIngestResult {
        if let Some(id) = event_id {
            if !id.is_empty() {
                self.last_event_id = Some(id.to_string());
            }
        }
        let mut flush: Vec<String> = Vec::new();
        if let Some(ev) = normalize_event(payload) {
            match ev.event_type.as_str() {
                "session.created" => {
                    self.update_session(&ev.properties);
                    if let Some(id) = session_id_from_info(ev.properties.get("info")) {
                        self.stream_owned_sessions.insert(id);
                    }
                }
                "session.updated" => {
                    self.update_session(&ev.properties);
                }
                "session.deleted" => {
                    let id = session_id_from_info(ev.properties.get("info"))
                        .or_else(|| string_prop(&ev.properties, "sessionID"));
                    if let Some(id) = id {
                        self.drop_session(&id);
                    }
                }
                "message.updated" => self.update_message(&ev.properties),
                "message.part.updated" => self.update_part(&ev.properties),
                "message.part.removed" => self.remove_part(&ev.properties),
                "session.idle" => {
                    if let Some(id) = string_prop(&ev.properties, "sessionID") {
                        if !flush.contains(&id) {
                            flush.push(id);
                        }
                    }
                }
                "session.status" => {
                    if is_idle_status(ev.properties.get("status")) {
                        if let Some(id) = string_prop(&ev.properties, "sessionID") {
                            if !flush.contains(&id) {
                                flush.push(id);
                            }
                        }
                    }
                }
                _ => {}
            }
        }
        self.flush(&flush)
    }

    pub fn snapshot_cursor(&self) -> OpencodeStreamCursorState {
        OpencodeStreamCursorState {
            last_event_id: self.last_event_id.clone(),
            emitted_message_ids: Some(self.emitted_message_ids_order.clone()),
            emitted_tool_event_ids: Some(self.emitted_tool_event_ids_order.clone()),
            next_tool_event_index_by_session: if self.next_tool_event_index_by_session.is_empty() {
                None
            } else {
                Some(self.next_tool_event_index_by_session.clone())
            },
        }
    }

    fn update_session(&mut self, properties: &Map<String, Value>) {
        let raw = match properties.get("info") {
            Some(Value::Object(m)) => m,
            _ => return,
        };
        let id = match raw.get("id").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => return,
        };
        let parent_id = raw
            .get("parentID")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let directory = raw
            .get("directory")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        self.sessions.insert(
            id.clone(),
            SessionInfo {
                id,
                parent_id,
                directory,
            },
        );
    }

    fn update_message(&mut self, properties: &Map<String, Value>) {
        let raw = match properties.get("info") {
            Some(Value::Object(m)) => m,
            _ => return,
        };
        if is_complete_assistant_like(raw) {
            let session_id = raw["sessionID"].as_str().unwrap_or("").to_string();
            if !self.stream_owned_sessions.contains(&session_id) {
                return;
            }
            let id = raw["id"].as_str().unwrap_or("").to_string();
            let msg = AssistantMessage::from_raw(raw);
            if !self.messages.contains_key(&id) {
                self.message_order.push(id.clone());
            }
            self.messages.insert(id, Message::Assistant(msg));
        } else if is_complete_user_like(raw) {
            let session_id = raw["sessionID"].as_str().unwrap_or("").to_string();
            if !self.stream_owned_sessions.contains(&session_id) {
                return;
            }
            let id = raw["id"].as_str().unwrap_or("").to_string();
            let msg = UserMessage::from_raw(raw);
            if !self.messages.contains_key(&id) {
                self.message_order.push(id.clone());
            }
            self.messages.insert(id, Message::User(msg));
        }
    }

    fn update_part(&mut self, properties: &Map<String, Value>) {
        let raw = match properties.get("part") {
            Some(Value::Object(m)) => m,
            _ => return,
        };
        let session_id = match raw.get("sessionID").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return,
        };
        let message_id = match raw.get("messageID").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => return,
        };
        if !self.stream_owned_sessions.contains(&session_id) {
            return;
        }
        let id = match raw.get("id").and_then(|v| v.as_str()) {
            Some(s) if !s.is_empty() => s.to_string(),
            _ => part_key(raw),
        };
        let part = Value::Object(raw.clone());
        self.parts_by_message
            .entry(message_id)
            .or_default()
            .insert(id, part);
    }

    fn remove_part(&mut self, properties: &Map<String, Value>) {
        let message_id = match string_prop(properties, "messageID") {
            Some(s) => s,
            None => return,
        };
        let part_id = match string_prop(properties, "partID") {
            Some(s) => s,
            None => return,
        };
        let session_id = match string_prop(properties, "sessionID") {
            Some(s) => s,
            None => return,
        };
        if !self.stream_owned_sessions.contains(&session_id) {
            return;
        }
        if let Some(bucket) = self.parts_by_message.get_mut(&message_id) {
            bucket.remove(&part_id);
        }
    }

    fn drop_session(&mut self, session_id: &str) {
        self.sessions.remove(session_id);
        self.stream_owned_sessions.remove(session_id);
        let mut deleted_message_ids: HashSet<String> = HashSet::new();
        let to_drop: Vec<String> = self
            .message_order
            .iter()
            .filter(|id| {
                self.messages
                    .get(*id)
                    .map(|m| m.session_id() == session_id)
                    .unwrap_or(false)
            })
            .cloned()
            .collect();
        for id in to_drop {
            deleted_message_ids.insert(id.clone());
            self.messages.remove(&id);
        }
        self.message_order.retain(|id| !deleted_message_ids.contains(id));

        let buckets_to_drop: Vec<String> = self
            .parts_by_message
            .iter()
            .filter_map(|(message_id, bucket)| {
                if deleted_message_ids.contains(message_id) {
                    return Some(message_id.clone());
                }
                let any_match = bucket.values().any(|p| {
                    p.get("sessionID").and_then(|v| v.as_str()) == Some(session_id)
                });
                if any_match {
                    Some(message_id.clone())
                } else {
                    None
                }
            })
            .collect();
        for k in buckets_to_drop {
            self.parts_by_message.remove(&k);
        }
    }

    fn flush(&mut self, session_ids: &[String]) -> OpencodeStreamIngestResult {
        let mut turns: Vec<TurnRecord> = Vec::new();
        let mut content: Vec<ContentRecord> = Vec::new();
        let mut relationships: Vec<SessionRelationshipRecord> = Vec::new();
        let mut tool_result_events: Vec<ToolResultEventRecord> = Vec::new();
        let mut user_turns: Vec<UserTurnRecord> = Vec::new();

        for session_id in session_ids {
            if !self.stream_owned_sessions.contains(session_id) {
                continue;
            }
            let session = self
                .sessions
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| SessionInfo {
                    id: session_id.clone(),
                    parent_id: None,
                    directory: None,
                });

            let assistants = self.assistants_for_session(session_id);
            let users = self.users_for_session(session_id);
            relationships.extend(build_relationships(&session, &assistants));

            // Tool-result event emission first (mirrors TS order).
            let candidates = collect_tool_result_event_candidates_for_session(
                session_id,
                &assistants,
                &self.parts_by_message,
            );
            let mut next_index = self
                .next_tool_event_index_by_session
                .get(session_id)
                .copied()
                .unwrap_or(0);
            let mut emitted_any = false;
            for cand in candidates {
                if self.emitted_tool_event_ids.contains(&cand.key) {
                    continue;
                }
                self.emitted_tool_event_ids.insert(cand.key.clone());
                self.emitted_tool_event_ids_order.push(cand.key.clone());
                let mut ev = cand.record;
                ev.event_index = next_index;
                next_index += 1;
                tool_result_events.push(ev);
                emitted_any = true;
            }
            if emitted_any {
                self.next_tool_event_index_by_session
                    .insert(session_id.clone(), next_index);
            }

            // Per-assistant turn emission.
            for (i, m) in assistants.iter().enumerate() {
                if self.emitted_message_ids.contains(&m.id) {
                    continue;
                }
                let parts = self.parts_for(&m.id);
                if !is_final_assistant(m, &parts) {
                    continue;
                }
                let prev = if i > 0 { Some(&assistants[i - 1]) } else { None };
                let user_msg = find_preceding_user(&users, m.time_created);
                let user_msg_for_gap: Option<&UserMessage> = match user_msg {
                    Some(u) => match prev {
                        Some(p) if u.time_created <= p.time_created => None,
                        _ => Some(u),
                    },
                    None => None,
                };
                let prev_parts = prev
                    .map(|p| self.parts_for(&p.id))
                    .unwrap_or_default();
                let user_parts_for_gap = user_msg_for_gap
                    .map(|u| self.parts_for(&u.id))
                    .unwrap_or_default();
                if let Some(ut) = build_user_turn_record(
                    session_id,
                    prev,
                    m,
                    user_msg_for_gap,
                    &prev_parts,
                    &user_parts_for_gap,
                    &self.counter,
                ) {
                    user_turns.push(ut);
                }

                let extracted = extract_tools_and_files(&parts);
                let usage = to_usage(m.tokens.as_ref());
                let user_text = user_msg
                    .map(|u| {
                        extract_text_parts(
                            &self.parts_for(&u.id),
                            ExtractTextOpts {
                                include_synthetic: false,
                            },
                        )
                        .join("\n")
                    })
                    .unwrap_or_default();
                let record = build_turn_record(
                    &session,
                    m,
                    i as u64,
                    &parts,
                    &extracted.tool_calls,
                    &extracted.files_touched,
                    &extracted.errored_call_ids,
                    usage,
                    &user_text,
                    &self.project_resolver,
                );
                let record_ts = record.ts.clone();
                turns.push(record);
                self.emitted_message_ids.insert(m.id.clone());
                self.emitted_message_ids_order.push(m.id.clone());

                if matches!(self.content_mode, ContentStoreMode::Full) {
                    if let Some(u) = user_msg {
                        let user_ts = unix_ms_to_iso(u.time_created);
                        for t in extract_text_parts(
                            &self.parts_for(&u.id),
                            ExtractTextOpts {
                                include_synthetic: false,
                            },
                        ) {
                            content.push(ContentRecord {
                                v: 1,
                                source: SourceKind::Opencode,
                                session_id: session_id.clone(),
                                message_id: u.id.clone(),
                                ts: user_ts.clone(),
                                role: ContentRole::User,
                                kind: ContentKind::Text,
                                text: Some(t),
                                tool_use: None,
                                tool_result: None,
                            });
                        }
                    }
                    content.extend(extract_assistant_content(
                        &parts,
                        session_id,
                        &m.id,
                        &record_ts,
                    ));
                }
            }
        }

        OpencodeStreamIngestResult {
            turns,
            content,
            relationships,
            tool_result_events,
            user_turns,
            cursor: self.snapshot_cursor(),
        }
    }

    fn assistants_for_session(&self, session_id: &str) -> Vec<AssistantMessage> {
        let mut out: Vec<AssistantMessage> = Vec::new();
        for id in &self.message_order {
            if let Some(Message::Assistant(m)) = self.messages.get(id) {
                if m.session_id == session_id {
                    out.push(m.clone());
                }
            }
        }
        out.sort_by(|a, b| a.time_created.cmp(&b.time_created));
        out
    }

    fn users_for_session(&self, session_id: &str) -> Vec<UserMessage> {
        let mut out: Vec<UserMessage> = Vec::new();
        for id in &self.message_order {
            if let Some(Message::User(m)) = self.messages.get(id) {
                if m.session_id == session_id {
                    out.push(m.clone());
                }
            }
        }
        out.sort_by(|a, b| a.time_created.cmp(&b.time_created));
        out
    }

    fn parts_for(&self, message_id: &str) -> Vec<Value> {
        match self.parts_by_message.get(message_id) {
            None => Vec::new(),
            Some(bucket) => bucket.values().cloned().collect(),
        }
    }
}

// ---------------------------------------------------------------------------
// Internal types / event normalization
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SessionInfo {
    id: String,
    parent_id: Option<String>,
    directory: Option<String>,
}

#[derive(Debug, Clone)]
struct MessageTokens {
    input: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    cache_read: Option<u64>,
    cache_write: Option<u64>,
}

#[derive(Debug, Clone)]
struct AssistantMessage {
    id: String,
    session_id: String,
    time_created: i64,
    provider_id: Option<String>,
    model_id: Option<String>,
    cwd: Option<String>,
    tokens: Option<MessageTokens>,
}

#[derive(Debug, Clone)]
struct UserMessage {
    id: String,
    session_id: String,
    time_created: i64,
}

#[derive(Debug, Clone)]
enum Message {
    Assistant(AssistantMessage),
    User(UserMessage),
}

impl Message {
    fn session_id(&self) -> &str {
        match self {
            Message::Assistant(m) => &m.session_id,
            Message::User(m) => &m.session_id,
        }
    }
}

impl AssistantMessage {
    fn from_raw(raw: &Map<String, Value>) -> Self {
        Self {
            id: raw["id"].as_str().unwrap_or("").to_string(),
            session_id: raw["sessionID"].as_str().unwrap_or("").to_string(),
            time_created: raw
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
            provider_id: raw
                .get("providerID")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            model_id: raw
                .get("modelID")
                .and_then(|v| v.as_str())
                .map(str::to_string),
            cwd: raw
                .get("path")
                .and_then(|p| p.get("cwd"))
                .and_then(|v| v.as_str())
                .map(str::to_string),
            tokens: raw.get("tokens").and_then(message_tokens_from_value),
        }
    }
}

impl UserMessage {
    fn from_raw(raw: &Map<String, Value>) -> Self {
        Self {
            id: raw["id"].as_str().unwrap_or("").to_string(),
            session_id: raw["sessionID"].as_str().unwrap_or("").to_string(),
            time_created: raw
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0),
        }
    }
}

fn message_tokens_from_value(v: &Value) -> Option<MessageTokens> {
    let obj = v.as_object()?;
    let cache = obj.get("cache").and_then(|c| c.as_object());
    Some(MessageTokens {
        input: obj.get("input").and_then(|x| x.as_u64()),
        output: obj.get("output").and_then(|x| x.as_u64()),
        reasoning: obj.get("reasoning").and_then(|x| x.as_u64()),
        cache_read: cache
            .and_then(|c| c.get("read"))
            .and_then(|x| x.as_u64()),
        cache_write: cache
            .and_then(|c| c.get("write"))
            .and_then(|x| x.as_u64()),
    })
}

struct NormalizedEvent {
    event_type: String,
    properties: Map<String, Value>,
}

fn normalize_event(payload: &Value) -> Option<NormalizedEvent> {
    let obj = payload.as_object()?;
    if let Some(nested) = obj.get("payload") {
        if nested.is_object() {
            return normalize_event(nested);
        }
    }
    let event_type = obj.get("type")?.as_str()?.to_string();
    let properties = match obj.get("properties") {
        Some(Value::Object(m)) => m.clone(),
        _ => Map::new(),
    };
    Some(NormalizedEvent {
        event_type,
        properties,
    })
}

fn session_id_from_info(raw: Option<&Value>) -> Option<String> {
    raw?.as_object()?
        .get("id")?
        .as_str()
        .map(str::to_string)
}

fn string_prop(rec: &Map<String, Value>, key: &str) -> Option<String> {
    rec.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_idle_status(raw: Option<&Value>) -> bool {
    match raw {
        Some(Value::String(s)) => s == "idle",
        Some(Value::Object(m)) => m.get("type").and_then(|v| v.as_str()) == Some("idle"),
        _ => false,
    }
}

fn is_complete_assistant_like(rec: &Map<String, Value>) -> bool {
    rec.get("role").and_then(|v| v.as_str()) == Some("assistant")
        && rec.get("id").and_then(|v| v.as_str()).is_some()
        && rec.get("sessionID").and_then(|v| v.as_str()).is_some()
        && rec
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .is_some()
}

fn is_complete_user_like(rec: &Map<String, Value>) -> bool {
    rec.get("role").and_then(|v| v.as_str()) == Some("user")
        && rec.get("id").and_then(|v| v.as_str()).is_some()
        && rec.get("sessionID").and_then(|v| v.as_str()).is_some()
        && rec
            .get("time")
            .and_then(|t| t.get("created"))
            .and_then(|v| v.as_i64())
            .is_some()
}

fn part_key(part: &Map<String, Value>) -> String {
    let message_id = part.get("messageID").and_then(|v| v.as_str()).unwrap_or("");
    let part_type = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let serialized = serde_json::to_string(&Value::Object(part.clone())).unwrap_or_default();
    format!("{}:{}:{}", message_id, part_type, serialized.len())
}

fn is_final_assistant(m: &AssistantMessage, parts: &[Value]) -> bool {
    if m.tokens.is_some() {
        return true;
    }
    parts
        .iter()
        .any(|p| p.get("type").and_then(|v| v.as_str()) == Some("step-finish"))
}

fn build_relationships(
    session: &SessionInfo,
    assistants: &[AssistantMessage],
) -> Vec<SessionRelationshipRecord> {
    let first_ts = assistants.first().map(|a| unix_ms_to_iso(a.time_created));
    let mut out = Vec::new();
    out.push(SessionRelationshipRecord {
        v: 1,
        source: RelationshipSourceKind::Opencode,
        session_id: session.id.clone(),
        related_session_id: None,
        relationship_type: RelationshipType::Root,
        ts: first_ts.clone(),
        source_session_id: None,
        source_version: None,
        parent_tool_use_id: None,
        agent_id: None,
        subagent_type: None,
        description: None,
    });
    if let Some(parent) = session.parent_id.as_ref() {
        if !parent.is_empty() {
            out.push(SessionRelationshipRecord {
                v: 1,
                source: RelationshipSourceKind::NativeOpencode,
                session_id: session.id.clone(),
                related_session_id: Some(parent.clone()),
                relationship_type: RelationshipType::Subagent,
                ts: first_ts,
                source_session_id: None,
                source_version: None,
                parent_tool_use_id: None,
                agent_id: None,
                subagent_type: None,
                description: None,
            });
        }
    }
    out
}

struct ToolResultEventCandidate {
    key: String,
    record: ToolResultEventRecord,
}

fn collect_tool_result_event_candidates_for_session(
    session_id: &str,
    assistants: &[AssistantMessage],
    parts_by_message: &HashMap<String, BTreeMap<String, Value>>,
) -> Vec<ToolResultEventCandidate> {
    let mut out: Vec<ToolResultEventCandidate> = Vec::new();
    let mut call_index_counters: HashMap<String, u64> = HashMap::new();
    for m in assistants {
        let parts: Vec<Value> = parts_by_message
            .get(&m.id)
            .map(|b| b.values().cloned().collect())
            .unwrap_or_default();
        if !is_final_assistant(m, &parts) {
            continue;
        }
        let terminal: Vec<&Value> = parts.iter().filter(|p| is_terminal_tool_part(p)).collect();
        let turn_usage = to_usage(m.tokens.as_ref());
        let usage_shares: Vec<Usage> = if terminal.is_empty() {
            Vec::new()
        } else {
            distribute_usage(&turn_usage, terminal.len() as u64)
        };
        let usage_attribution = if terminal.len() == 1 {
            Some(UsageAttribution::SingleToolTurn)
        } else if terminal.len() > 1 {
            Some(UsageAttribution::EvenSplitTurn)
        } else {
            None
        };
        for (i, tp) in terminal.iter().enumerate() {
            let call_id = tp
                .get("callID")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let is_error = is_failed_tool(tp);
            let call_index = *call_index_counters.get(&call_id).unwrap_or(&0);
            call_index_counters.insert(call_id.clone(), call_index + 1);
            let measured = measure_tool_output(tp.get("state").and_then(|s| s.get("output")));
            let usage_share = usage_shares.get(i).cloned();
            let record = ToolResultEventRecord {
                v: 1,
                source: SourceKind::Opencode,
                session_id: session_id.to_string(),
                message_id: Some(m.id.clone()),
                tool_use_id: call_id.clone(),
                call_index: Some(call_index),
                event_index: 0,
                ts: Some(unix_ms_to_iso(m.time_created)),
                status: if is_error {
                    ToolResultStatus::Errored
                } else {
                    ToolResultStatus::Completed
                },
                event_source: ToolResultEventSource::ToolResult,
                content_length: measured.length,
                content_hash: measured.hash,
                is_error: if is_error { Some(true) } else { None },
                usage: usage_share.clone(),
                usage_attribution: if usage_share.is_some() {
                    usage_attribution
                } else {
                    None
                },
                subagent_session_id: None,
                agent_id: None,
                replaced_tools: None,
                collapsed_calls: None,
            };
            let part_id = tp
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            let key = tool_event_key(session_id, &m.id, &call_id, part_id.as_deref(), call_index);
            out.push(ToolResultEventCandidate { key, record });
        }
    }
    out
}

fn tool_event_key(
    session_id: &str,
    message_id: &str,
    call_id: &str,
    part_id: Option<&str>,
    call_index: u64,
) -> String {
    let suffix = match part_id {
        Some(p) => p.to_string(),
        None => call_index.to_string(),
    };
    format!("{}|{}|{}|{}", session_id, message_id, call_id, suffix)
}

fn derive_next_tool_event_index_by_session(keys: &[String]) -> BTreeMap<String, u64> {
    let mut out: BTreeMap<String, u64> = BTreeMap::new();
    for key in keys {
        let first = match key.find('|') {
            Some(idx) if idx > 0 => idx,
            _ => continue,
        };
        let last = match key.rfind('|') {
            Some(idx) if idx > first => idx,
            _ => continue,
        };
        // Mirror TS `Number.parseInt(suffix, 10)`: accept a leading
        // numeric prefix (e.g. "1abc" → 1) rather than requiring the whole
        // suffix to parse. Keys with a partID suffix always fail this
        // check; only the call-index fallback path produces purely-numeric
        // suffixes that should bump the per-session counter.
        let idx = match parse_int_prefix(&key[last + 1..]) {
            Some(v) => v,
            None => continue,
        };
        let session_id = key[..first].to_string();
        let cur = out.get(&session_id).copied().unwrap_or(0);
        if idx + 1 > cur {
            out.insert(session_id, idx + 1);
        }
    }
    out
}

/// JavaScript `Number.parseInt(s, 10)` for non-negative integers: trim
/// leading whitespace, then collect leading ASCII digits and parse those.
/// Returns `None` if there's no digit prefix (TS would yield `NaN`, which
/// the original `Number.isFinite` check already filters out).
fn parse_int_prefix(s: &str) -> Option<u64> {
    let trimmed = s.trim_start();
    let mut end = 0usize;
    for (i, c) in trimmed.char_indices() {
        if c.is_ascii_digit() {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    trimmed[..end].parse().ok()
}

#[allow(clippy::too_many_arguments)]
fn build_turn_record(
    session: &SessionInfo,
    m: &AssistantMessage,
    turn_index: u64,
    parts: &[Value],
    tool_calls: &[ToolCall],
    files_touched: &[String],
    errored_call_ids: &BTreeSet<String>,
    usage: Usage,
    user_text: &str,
    project_resolver: &ProjectResolver,
) -> TurnRecord {
    let model = build_model(m.provider_id.as_deref(), m.model_id.as_deref());
    let project = m
        .cwd
        .clone()
        .or_else(|| session.directory.clone());
    let mut usage_coverage = coverage_from_tokens(m.tokens.as_ref());
    for sf in step_finish_tokens(parts) {
        usage_coverage = merge_usage_coverage(&usage_coverage, &coverage_from_tokens(Some(&sf)));
    }
    let mut record = TurnRecord {
        v: 1,
        source: SourceKind::Opencode,
        session_id: m.session_id.clone(),
        session_path: None,
        message_id: m.id.clone(),
        turn_index,
        ts: unix_ms_to_iso(m.time_created),
        model,
        project: None,
        project_key: None,
        usage,
        tool_calls: tool_calls.to_vec(),
        files_touched: if files_touched.is_empty() {
            None
        } else {
            Some(files_touched.to_vec())
        },
        subagent: None,
        stop_reason: None,
        activity: None,
        retries: None,
        has_edits: None,
        fidelity: Some(build_fidelity(&usage_coverage)),
    };
    if let Some(p) = project.as_ref() {
        let resolved = project_resolver.resolve(p);
        record.project = Some(resolved.project);
        record.project_key = resolved.project_key;
    }
    if session.parent_id.as_deref().filter(|s| !s.is_empty()).is_some() {
        record.subagent = Some(Subagent {
            is_sidechain: true,
            parent_tool_use_id: None,
            agent_id: None,
            parent_agent_id: None,
            subagent_type: None,
            description: None,
        });
    }
    if let Some(reason) = last_step_finish_reason(parts) {
        record.stop_reason = Some(reason);
    }
    let assistant_text = extract_assistant_text(parts);
    let has_failed_tool = tool_calls.iter().any(|tc| errored_call_ids.contains(&tc.id));
    let combined = join_nonempty(&[user_text, &assistant_text], "\n");
    let classified = classify_activity(ClassificationInput {
        tool_calls,
        text: &combined,
        has_failed_tool,
        reasoning_tokens: record.usage.reasoning,
    });
    record.activity = Some(classified.activity);
    record.retries = Some(classified.retries);
    record.has_edits = Some(classified.has_edits);
    record
}

fn build_user_turn_record<C: TokenCounter + ?Sized>(
    session_id: &str,
    prev: Option<&AssistantMessage>,
    next: &AssistantMessage,
    user_msg: Option<&UserMessage>,
    prev_parts: &[Value],
    user_parts: &[Value],
    counter: &C,
) -> Option<UserTurnRecord> {
    let mut blocks: Vec<UserTurnBlock> = Vec::new();
    if prev.is_some() {
        for p in prev_parts {
            if !is_terminal_tool_part(p) {
                continue;
            }
            let call_id = p
                .get("callID")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let output = p
                .get("state")
                .and_then(|s| s.get("output"))
                .cloned()
                .unwrap_or(Value::String(String::new()));
            let is_error = is_failed_tool(p);
            blocks.push(UserTurnBlock::tool_result(
                call_id,
                &output,
                Some(is_error),
                counter,
            ));
        }
    }
    let mut ts = user_msg
        .map(|u| unix_ms_to_iso(u.time_created))
        .unwrap_or_default();
    for text in extract_text_parts(
        user_parts,
        ExtractTextOpts {
            include_synthetic: true,
        },
    ) {
        blocks.push(UserTurnBlock::text(&text, counter));
    }
    if blocks.is_empty() {
        return None;
    }
    if ts.is_empty() {
        ts = unix_ms_to_iso(next.time_created);
    }
    let user_uuid = match user_msg {
        Some(u) => u.id.clone(),
        None => format!(
            "{}:{}->{}",
            session_id,
            prev.map(|p| p.id.as_str()).unwrap_or("start"),
            next.id
        ),
    };
    let preceding = prev.map(|p| p.id.clone());
    Some(UserTurnRecord {
        v: 1,
        source: SourceKind::Opencode,
        session_id: session_id.to_string(),
        user_uuid,
        ts,
        preceding_message_id: preceding,
        following_message_id: Some(next.id.clone()),
        blocks,
    })
}

struct ExtractedTools {
    tool_calls: Vec<ToolCall>,
    files_touched: Vec<String>,
    errored_call_ids: BTreeSet<String>,
}

fn extract_tools_and_files(parts: &[Value]) -> ExtractedTools {
    let mut tool_calls: Vec<ToolCall> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut files: Vec<String> = Vec::new();
    let mut files_seen: HashSet<String> = HashSet::new();
    let mut errored: BTreeSet<String> = BTreeSet::new();
    for p in parts {
        if p.get("type").and_then(|v| v.as_str()) != Some("tool") {
            continue;
        }
        let call_id = match p.get("callID").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        let tool = match p.get("tool").and_then(|v| v.as_str()) {
            Some(s) => s.to_string(),
            None => continue,
        };
        if seen.contains(&call_id) {
            continue;
        }
        seen.insert(call_id.clone());
        let input = p
            .get("state")
            .and_then(|s| s.get("input"))
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let target = pick_target(&tool, &input);
        let mut call = ToolCall {
            id: call_id.clone(),
            name: tool.clone(),
            target: target.clone(),
            args_hash: args_hash(&input),
            is_error: None,
            edit_pre_hash: None,
            edit_post_hash: None,
            skill_name: None,
            replaced_tools: None,
            collapsed_calls: None,
        };
        if tool == "skill" {
            if let Some(obj) = input.as_object() {
                for k in ["skill", "name", "skill_name"] {
                    if let Some(s) = obj.get(k).and_then(|v| v.as_str()) {
                        call.skill_name = Some(s.to_string());
                        break;
                    }
                }
            }
        }
        tool_calls.push(call);
        if let Some(t) = target {
            if is_file_tool(&tool) && files_seen.insert(t.clone()) {
                files.push(t);
            }
        }
        if is_failed_tool(p) {
            errored.insert(call_id);
        }
    }
    ExtractedTools {
        tool_calls,
        files_touched: files,
        errored_call_ids: errored,
    }
}

fn extract_assistant_content(
    parts: &[Value],
    session_id: &str,
    message_id: &str,
    ts: &str,
) -> Vec<ContentRecord> {
    let mut out: Vec<ContentRecord> = Vec::new();
    for p in parts {
        let part_type = p.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if part_type == "text" {
            if p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
                continue;
            }
            if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    out.push(ContentRecord {
                        v: 1,
                        source: SourceKind::Opencode,
                        session_id: session_id.to_string(),
                        message_id: message_id.to_string(),
                        ts: ts.to_string(),
                        role: ContentRole::Assistant,
                        kind: ContentKind::Text,
                        text: Some(text.to_string()),
                        tool_use: None,
                        tool_result: None,
                    });
                }
            }
            continue;
        }
        if part_type == "tool" {
            let call_id = match p.get("callID").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let tool = match p.get("tool").and_then(|v| v.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let input = p
                .get("state")
                .and_then(|s| s.get("input"))
                .cloned()
                .unwrap_or_else(|| Value::Object(Map::new()));
            let input_map: BTreeMap<String, Value> = match input.as_object() {
                Some(obj) => obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                None => BTreeMap::new(),
            };
            out.push(ContentRecord {
                v: 1,
                source: SourceKind::Opencode,
                session_id: session_id.to_string(),
                message_id: message_id.to_string(),
                ts: ts.to_string(),
                role: ContentRole::Assistant,
                kind: ContentKind::ToolUse,
                text: None,
                tool_use: Some(ContentToolUse {
                    id: call_id.clone(),
                    name: tool,
                    input: input_map,
                }),
                tool_result: None,
            });
            let state = p.get("state");
            let has_output = state
                .and_then(|s| s.as_object())
                .map(|s| s.contains_key("output"))
                .unwrap_or(false);
            if has_output {
                let output = state
                    .and_then(|s| s.get("output"))
                    .cloned()
                    .unwrap_or(Value::String(String::new()));
                let content = if matches!(&output, Value::Null) {
                    Value::String(String::new())
                } else {
                    output
                };
                let is_error = is_failed_tool(p);
                out.push(ContentRecord {
                    v: 1,
                    source: SourceKind::Opencode,
                    session_id: session_id.to_string(),
                    message_id: message_id.to_string(),
                    ts: ts.to_string(),
                    role: ContentRole::ToolResult,
                    kind: ContentKind::ToolResult,
                    text: None,
                    tool_use: None,
                    tool_result: Some(ContentToolResult {
                        tool_use_id: call_id,
                        content,
                        is_error: if is_error { Some(true) } else { None },
                    }),
                });
            }
        }
    }
    out
}

#[derive(Clone, Copy)]
struct ExtractTextOpts {
    include_synthetic: bool,
}

fn extract_text_parts(parts: &[Value], opts: ExtractTextOpts) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in parts {
        if p.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        if !opts.include_synthetic && p.get("synthetic").and_then(|v| v.as_bool()) == Some(true) {
            continue;
        }
        if let Some(text) = p.get("text").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                out.push(text.to_string());
            }
        }
    }
    out
}

fn is_terminal_tool_part(p: &Value) -> bool {
    if p.get("type").and_then(|v| v.as_str()) != Some("tool") {
        return false;
    }
    let call_id_ok = p
        .get("callID")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);
    if !call_id_ok {
        return false;
    }
    p.get("state")
        .and_then(|s| s.as_object())
        .map(|s| s.contains_key("output"))
        .unwrap_or(false)
}

fn is_failed_tool(p: &Value) -> bool {
    let state = match p.get("state").and_then(|s| s.as_object()) {
        Some(s) => s,
        None => return false,
    };
    if state.get("status").and_then(|v| v.as_str()) == Some("error") {
        return true;
    }
    if let Some(exit) = state
        .get("metadata")
        .and_then(|m| m.get("exit"))
        .and_then(|v| v.as_i64())
    {
        if exit != 0 {
            return true;
        }
    }
    false
}

fn extract_assistant_text(parts: &[Value]) -> String {
    extract_text_parts(
        parts,
        ExtractTextOpts {
            include_synthetic: false,
        },
    )
    .join("\n")
}

fn find_preceding_user(users: &[UserMessage], asst_time: i64) -> Option<&UserMessage> {
    let mut best: Option<&UserMessage> = None;
    for u in users {
        if u.time_created <= asst_time {
            best = Some(u);
        } else {
            break;
        }
    }
    best
}

fn pick_target(name: &str, input: &Value) -> Option<String> {
    let s = |k: &str| -> Option<String> {
        input
            .get(k)
            .and_then(|v| v.as_str())
            .map(str::to_string)
    };
    match name {
        "read" | "write" | "edit" => s("filePath").or_else(|| s("file_path")).or_else(|| s("path")),
        "bash" => s("command"),
        "grep" | "glob" => s("pattern"),
        "webfetch" => s("url"),
        "task" => s("subagent_type")
            .or_else(|| s("description"))
            .or_else(|| s("prompt")),
        _ => s("filePath")
            .or_else(|| s("file_path"))
            .or_else(|| s("path"))
            .or_else(|| s("url"))
            .or_else(|| s("command")),
    }
}

fn is_file_tool(name: &str) -> bool {
    matches!(name, "read" | "write" | "edit")
}

fn last_step_finish_reason(parts: &[Value]) -> Option<String> {
    for p in parts.iter().rev() {
        if p.get("type").and_then(|v| v.as_str()) == Some("step-finish") {
            if let Some(reason) = p.get("reason").and_then(|v| v.as_str()) {
                return Some(reason.to_string());
            }
        }
    }
    None
}

fn step_finish_tokens(parts: &[Value]) -> Vec<MessageTokens> {
    let mut out: Vec<MessageTokens> = Vec::new();
    for p in parts {
        if p.get("type").and_then(|v| v.as_str()) != Some("step-finish") {
            continue;
        }
        if let Some(t) = p.get("tokens").and_then(message_tokens_from_value) {
            out.push(t);
        }
    }
    out
}

fn build_model(provider_id: Option<&str>, model_id: Option<&str>) -> String {
    match (provider_id, model_id) {
        (Some(p), Some(m)) => format!("{}/{}", p, m),
        (_, Some(m)) => m.to_string(),
        (Some(p), _) => p.to_string(),
        _ => String::new(),
    }
}

fn to_usage(t: Option<&MessageTokens>) -> Usage {
    Usage {
        input: t.and_then(|x| x.input).unwrap_or(0),
        output: t.and_then(|x| x.output).unwrap_or(0),
        reasoning: t.and_then(|x| x.reasoning).unwrap_or(0),
        cache_read: t.and_then(|x| x.cache_read).unwrap_or(0),
        cache_create_5m: t.and_then(|x| x.cache_write).unwrap_or(0),
        cache_create_1h: 0,
    }
}

/// Split a turn's usage across `n` tool-result events while preserving
/// per-field sums. The TS port emits floating-point shares
/// (`usage.input / divisor`); the Rust port keeps `Usage` as `u64`, so we
/// distribute the integer-division remainder across the leading events
/// (`base + 1` for the first `total % n` events, `base` after) instead of
/// truncating every share. This trades the TS "every event identical share"
/// invariant for the stronger "sum of event shares == turn usage" invariant
/// — important because downstream consumers aggregate tool-event usage to
/// recompute per-turn cost.
fn distribute_usage(u: &Usage, n: u64) -> Vec<Usage> {
    if n == 0 {
        return Vec::new();
    }
    if n == 1 {
        return vec![u.clone()];
    }
    (0..n)
        .map(|i| Usage {
            input: split_field(u.input, n, i),
            output: split_field(u.output, n, i),
            reasoning: split_field(u.reasoning, n, i),
            cache_read: split_field(u.cache_read, n, i),
            cache_create_5m: split_field(u.cache_create_5m, n, i),
            cache_create_1h: split_field(u.cache_create_1h, n, i),
        })
        .collect()
}

fn split_field(total: u64, n: u64, i: u64) -> u64 {
    let base = total / n;
    let rem = total % n;
    base + if i < rem { 1 } else { 0 }
}

#[derive(Default, Clone)]
struct OpencodeUsageCoverage {
    has_input_tokens: bool,
    has_output_tokens: bool,
    has_reasoning_tokens: bool,
    has_cache_read_tokens: bool,
    has_cache_create_tokens: bool,
}

fn coverage_from_tokens(t: Option<&MessageTokens>) -> OpencodeUsageCoverage {
    OpencodeUsageCoverage {
        has_input_tokens: t.map(|x| x.input.is_some()).unwrap_or(false),
        has_output_tokens: t.map(|x| x.output.is_some()).unwrap_or(false),
        has_reasoning_tokens: t.map(|x| x.reasoning.is_some()).unwrap_or(false),
        has_cache_read_tokens: t.map(|x| x.cache_read.is_some()).unwrap_or(false),
        has_cache_create_tokens: t.map(|x| x.cache_write.is_some()).unwrap_or(false),
    }
}

fn merge_usage_coverage(
    a: &OpencodeUsageCoverage,
    b: &OpencodeUsageCoverage,
) -> OpencodeUsageCoverage {
    OpencodeUsageCoverage {
        has_input_tokens: a.has_input_tokens || b.has_input_tokens,
        has_output_tokens: a.has_output_tokens || b.has_output_tokens,
        has_reasoning_tokens: a.has_reasoning_tokens || b.has_reasoning_tokens,
        has_cache_read_tokens: a.has_cache_read_tokens || b.has_cache_read_tokens,
        has_cache_create_tokens: a.has_cache_create_tokens || b.has_cache_create_tokens,
    }
}

fn build_fidelity(usage_coverage: &OpencodeUsageCoverage) -> Fidelity {
    let coverage = Coverage {
        has_input_tokens: usage_coverage.has_input_tokens,
        has_output_tokens: usage_coverage.has_output_tokens,
        has_reasoning_tokens: usage_coverage.has_reasoning_tokens,
        has_cache_read_tokens: usage_coverage.has_cache_read_tokens,
        has_cache_create_tokens: usage_coverage.has_cache_create_tokens,
        has_tool_calls: true,
        has_tool_result_events: true,
        has_session_relationships: true,
        has_raw_content: true,
    };
    let class = classify_fidelity(UsageGranularity::PerTurn, &coverage);
    Fidelity {
        granularity: UsageGranularity::PerTurn,
        coverage,
        class,
    }
}

struct Measured {
    length: Option<u64>,
    hash: Option<String>,
}

fn measure_tool_output(output: Option<&Value>) -> Measured {
    match output {
        None | Some(Value::Null) => Measured {
            length: None,
            hash: None,
        },
        Some(Value::String(s)) => Measured {
            length: Some(s.len() as u64),
            hash: Some(content_hash(s)),
        },
        Some(other) => match serde_json::to_string(other) {
            Ok(serialized) => Measured {
                length: Some(serialized.len() as u64),
                hash: Some(content_hash(&serialized)),
            },
            Err(_) => Measured {
                length: None,
                hash: None,
            },
        },
    }
}

fn unix_ms_to_iso(ms: i64) -> String {
    // Match JavaScript `new Date(ms).toISOString()`: always millisecond
    // precision, always UTC, always `YYYY-MM-DDTHH:MM:SS.sssZ`.
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000) as u32;
    let (year, month, day, hour, minute, second) = unix_secs_to_components(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        year, month, day, hour, minute, second, millis
    )
}

fn unix_secs_to_components(secs: i64) -> (i32, u32, u32, u32, u32, u32) {
    // Algorithm from Howard Hinnant's date library: civil_from_days.
    let days = secs.div_euclid(86_400);
    let mut sod = secs.rem_euclid(86_400);
    let hour = (sod / 3600) as u32;
    sod %= 3600;
    let minute = (sod / 60) as u32;
    let second = (sod % 60) as u32;

    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let year = (y + if m <= 2 { 1 } else { 0 }) as i32;
    (year, m, d, hour, minute, second)
}

fn join_nonempty(parts: &[&str], sep: &str) -> String {
    let mut out: Vec<&str> = Vec::with_capacity(parts.len());
    for p in parts {
        if !p.is_empty() {
            out.push(p);
        }
    }
    out.join(sep)
}

#[cfg(test)]
mod tests;
