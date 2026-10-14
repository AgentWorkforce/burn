//! `context-output-ratio` hotspot finding: a session whose context-window
//! work dwarfs the output it generated.

use crate::analyze::findings::{
    hotspots_action, EstimatedSavings, FindingPricingStatus, WasteFinding, WasteSeverity,
};

/// Build the ratio-driven finding used by the public hotspots verb. Severity
/// and inclusion are deliberately independent of dollar cost.
pub(crate) struct ContextOutputRatioFindingInput<'a> {
    pub session_id: &'a str,
    pub high: bool,
    pub ratio_label: &'a str,
    pub context_tokens: u64,
    pub output_tokens: u64,
    pub threshold: f64,
    pub min_context_tokens: u64,
}

pub(crate) fn context_output_ratio_finding(
    input: ContextOutputRatioFindingInput<'_>,
) -> WasteFinding {
    WasteFinding {
        kind: "context-output-ratio".to_string(),
        severity: if input.high {
            WasteSeverity::High
        } else {
            WasteSeverity::Warn
        },
        session_id: input.session_id.to_string(),
        title: format!("{} context-to-output ratio", input.ratio_label),
        detail: format!(
            "{} context tokens (input + cache reads + cache creation) / {} generated output tokens (including reasoning); flat inspection threshold {}:1 with {} minimum context tokens (not length-normalized)",
            input.context_tokens,
            input.output_tokens,
            input.threshold,
            input.min_context_tokens,
        ),
        estimated_savings: EstimatedSavings::default(),
        actions: vec![hotspots_action(input.session_id)],
        event_source: None,
        pricing_status: FindingPricingStatus::Priced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyze::findings::WasteAction;

    fn input(high: bool) -> ContextOutputRatioFindingInput<'static> {
        ContextOutputRatioFindingInput {
            session_id: "sess-1",
            high,
            ratio_label: "764.0:1",
            context_tokens: 2_292_000,
            output_tokens: 3_000,
            threshold: 382.0,
            min_context_tokens: 1_000_000,
        }
    }

    #[test]
    fn builds_a_session_scoped_ratio_finding_without_dollar_savings() {
        assert_eq!(
            context_output_ratio_finding(input(true)),
            WasteFinding {
                kind: "context-output-ratio".to_string(),
                severity: WasteSeverity::High,
                session_id: "sess-1".to_string(),
                title: "764.0:1 context-to-output ratio".to_string(),
                detail: "2292000 context tokens (input + cache reads + cache creation) / 3000 generated output tokens (including reasoning); flat inspection threshold 382:1 with 1000000 minimum context tokens (not length-normalized)".to_string(),
                estimated_savings: EstimatedSavings::default(),
                actions: vec![WasteAction::Command {
                    label: "Inspect this session".to_string(),
                    text: "burn hotspots --session sess-1".to_string(),
                }],
                event_source: None,
                pricing_status: FindingPricingStatus::Priced,
            }
        );
    }

    #[test]
    fn non_high_ratio_findings_warn() {
        assert_eq!(
            context_output_ratio_finding(input(false)).severity,
            WasteSeverity::Warn
        );
    }
}
