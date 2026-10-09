//! The configuration a Codex task runs under.
//!
//! Codex writes a `turn_context` record just after each `task_started`, and
//! relayhistory keeps one only when the configuration changes. A task
//! therefore runs under the latest `turn_context` in rollout order: one
//! written inside the open task, else the one carried forward from an
//! earlier task.

use serde_json::Value;

use crate::reader::ReasoningConfig;

/// One `turn_context` record's settings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(super) struct TurnContext<'a> {
    pub model: Option<&'a str>,
    pub cwd: Option<&'a str>,
    pub effort: Option<&'a str>,
    pub summary: Option<&'a str>,
}

impl<'a> TurnContext<'a> {
    /// The settings of a `turn_context` payload. Effort is the record's
    /// `effort`, else its collaboration mode's `reasoning_effort`.
    pub(super) fn read(payload: &'a Value) -> Self {
        let text = |value: Option<&'a Value>| value.and_then(Value::as_str);
        Self {
            model: text(payload.get("model")),
            cwd: text(payload.get("cwd")),
            effort: text(payload.get("effort"))
                .or_else(|| text(payload.pointer("/collaboration_mode/settings/reasoning_effort"))),
            summary: text(payload.get("summary")),
        }
    }

    /// The reasoning settings, when the record carries any.
    pub(super) fn reasoning(&self) -> Option<ReasoningConfig> {
        (self.effort.is_some() || self.summary.is_some()).then(|| ReasoningConfig {
            effort: self.effort.map(str::to_string),
            summary: self.summary.map(str::to_string),
        })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reads_effort_and_summary() {
        let payload = json!({
            "turn_id": "t1", "model": "gpt-5.4", "cwd": "/tmp/p",
            "effort": "high", "summary": "detailed",
            "collaboration_mode": {"settings": {"reasoning_effort": "low"}}
        });
        let context = TurnContext::read(&payload);
        assert_eq!(
            context,
            TurnContext {
                model: Some("gpt-5.4"),
                cwd: Some("/tmp/p"),
                effort: Some("high"),
                summary: Some("detailed"),
            }
        );
        assert_eq!(
            context.reasoning(),
            Some(ReasoningConfig {
                effort: Some("high".into()),
                summary: Some("detailed".into()),
            })
        );
    }

    #[test]
    fn falls_back_to_the_collaboration_mode_effort() {
        let payload = json!({"collaboration_mode": {"settings": {"reasoning_effort": "medium"}}});
        assert_eq!(TurnContext::read(&payload).effort, Some("medium"));
    }

    #[test]
    fn a_record_without_reasoning_settings_has_none() {
        let payload = json!({"model": "gpt-5.4", "effort": null});
        assert_eq!(TurnContext::read(&payload).reasoning(), None);
    }
}
