use super::*;

#[test]
fn format_line_range_pads_to_four() {
    assert_eq!(format_line_range(7, 11), "   7-  11");
    assert_eq!(format_line_range(100, 200), " 100- 200");
}

#[test]
fn short_turn_label_trims_msg_prefix() {
    assert_eq!(short_turn_label("msg_abcdef1234"), "Tabcdef12");
    assert_eq!(short_turn_label("msg-deadbeef"), "Tdeadbeef");
    assert_eq!(short_turn_label("xyz"), "Txyz");
}

#[test]
fn driver_summary_singles_out_compaction() {
    let steps = vec![
        InterveningStep::ToolResult {
            tool_use_id: "tu-1".into(),
            tool_name: "Bash".into(),
            approx_tokens: 100,
            approx_bytes: 400,
            truncated: false,
        },
        InterveningStep::Compaction { tokens_freed: 5000 },
    ];
    let s = driver_summary(&steps);
    assert!(s.contains("compaction"));
}

#[test]
fn driver_summary_picks_largest_step() {
    let steps = vec![
        InterveningStep::ToolResult {
            tool_use_id: "tu-1".into(),
            tool_name: "Bash".into(),
            approx_tokens: 100,
            approx_bytes: 400,
            truncated: false,
        },
        InterveningStep::ToolResult {
            tool_use_id: "tu-2".into(),
            tool_name: "Read".into(),
            approx_tokens: 5000,
            approx_bytes: 20_000,
            truncated: false,
        },
    ];
    let s = driver_summary(&steps);
    assert!(s.contains("Read"), "got {s}");
    assert!(s.contains("more"), "got {s}");
}

#[test]
fn format_signed_tokens_handles_positive_and_zero() {
    assert_eq!(format_signed_tokens(0), "0");
    assert!(format_signed_tokens(5_000).starts_with('+'));
}

#[test]
fn parse_since_duration_converts_each_unit() {
    use std::time::Duration;
    assert_eq!(parse_since_duration("2h"), Some(Duration::from_secs(7_200)));
    assert_eq!(
        parse_since_duration("3d"),
        Some(Duration::from_secs(259_200))
    );
    assert_eq!(
        parse_since_duration("1w"),
        Some(Duration::from_secs(604_800))
    );
    assert_eq!(
        parse_since_duration("1m"),
        Some(Duration::from_secs(2_592_000))
    );
    assert_eq!(parse_since_duration("0h"), Some(Duration::from_secs(0)));
}

#[test]
fn parse_since_duration_rejects_malformed_input() {
    assert_eq!(parse_since_duration(""), None);
    assert_eq!(parse_since_duration("5"), None);
    assert_eq!(parse_since_duration("5s"), None);
    assert_eq!(parse_since_duration("h"), None);
    assert_eq!(parse_since_duration("-5h"), None);
    assert_eq!(parse_since_duration("1.5d"), None);
    assert_eq!(parse_since_duration("2026-01-01"), None);
}

#[test]
fn parse_since_duration_rejects_overflow() {
    assert_eq!(parse_since_duration("99999999999999999999h"), None);
    let max_h = format!("{}h", u64::MAX / 3_600 + 1);
    assert_eq!(parse_since_duration(&max_h), None);
    let max_d = format!("{}d", u64::MAX / 86_400 + 1);
    assert_eq!(parse_since_duration(&max_d), None);
    let max_w = format!("{}w", u64::MAX / (7 * 86_400) + 1);
    assert_eq!(parse_since_duration(&max_w), None);
    let max_m = format!("{}m", u64::MAX / (30 * 86_400) + 1);
    assert_eq!(parse_since_duration(&max_m), None);
    let ok_w = format!("{}w", u64::MAX / (7 * 86_400));
    assert!(parse_since_duration(&ok_w).is_some());
}

#[test]
fn explain_step_tool_result() {
    let step = InterveningStep::ToolResult {
        tool_use_id: "tu-1".into(),
        tool_name: "Bash".into(),
        approx_tokens: 1_500,
        approx_bytes: 6_000,
        truncated: false,
    };
    assert_eq!(
        explain_step(&step),
        "tool_result Bash (id=tu-1): ~1.5k tok / 6,000 bytes"
    );
    let truncated = InterveningStep::ToolResult {
        tool_use_id: "tu-2".into(),
        tool_name: "Read".into(),
        approx_tokens: 10,
        approx_bytes: 40,
        truncated: true,
    };
    assert_eq!(
        explain_step(&truncated),
        "tool_result Read (id=tu-2): ~10 tok / 40 bytes [truncated]"
    );
}

#[test]
fn explain_step_user_prompt() {
    let plain = InterveningStep::UserPrompt {
        approx_tokens: 42,
        has_system_reminder: false,
    };
    assert_eq!(explain_step(&plain), "user prompt: ~42 tok");
    let with_reminder = InterveningStep::UserPrompt {
        approx_tokens: 2_000,
        has_system_reminder: true,
    };
    assert_eq!(
        explain_step(&with_reminder),
        "user prompt: ~2.0k tok (with system-reminder)"
    );
}

#[test]
fn explain_step_reminder_compaction_and_other() {
    let reminder = InterveningStep::SystemReminder {
        source: relayburn_sdk::ReminderSource::Harness,
        approx_tokens: 7,
    };
    assert_eq!(explain_step(&reminder), "system-reminder (Harness): ~7 tok");
    let compaction = InterveningStep::Compaction {
        tokens_freed: 12_300,
    };
    assert_eq!(explain_step(&compaction), "compaction: -12.3k tok freed");
    assert_eq!(explain_step(&InterveningStep::Other), "other");
}
