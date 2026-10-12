use super::*;

fn result_from(value: Value) -> HotspotsResult {
    serde_json::from_value(value).expect("valid HotspotsResult fixture")
}

#[test]
fn bash_rows_carry_refusal_fields() {
    let result = result_from(json!({
        "kind": "bash",
        "rows": [{
            "argsHash": "h1", "command": "ls", "callCount": 2, "totalCost": 0.5,
            "initialTokens": 10.0, "persistenceTokens": 20.0, "totalOutputBytes": 30,
            "maxOutputBytes": 25, "truncatedCount": 1
        }],
        "refused": true,
        "refusalReason": "too few turns"
    }));
    let value = hotspots_result_to_json(&result);
    assert_eq!(value["rows"].as_array().unwrap().len(), 1);
    assert_eq!(value["rows"][0]["argsHash"], json!("h1"));
    assert_eq!(value["rows"][0]["command"], json!("ls"));
    assert_eq!(value["refused"], json!(true));
    assert_eq!(value["refusalReason"], json!("too few turns"));
    assert_eq!(value.as_object().unwrap().len(), 3);
}

#[test]
fn bash_verb_rows_serialize_with_null_refusal() {
    let result = result_from(json!({
        "kind": "bash-verb",
        "rows": [{
            "verb": "git", "callCount": 3, "distinctCommands": 2, "totalCost": 1.0,
            "initialTokens": 1.0, "persistenceTokens": 2.0, "avgPersistenceTurns": 0.5,
            "topExamples": ["git status"], "totalOutputBytes": 9, "maxOutputBytes": 9,
            "truncatedCount": 0
        }]
    }));
    let value = hotspots_result_to_json(&result);
    assert_eq!(value["rows"][0]["verb"], json!("git"));
    assert_eq!(value["rows"][0]["topExamples"], json!(["git status"]));
    assert_eq!(value["refused"], Value::Null);
    assert_eq!(value["refusalReason"], Value::Null);
}

#[test]
fn file_rows_use_file_mapper() {
    let result = result_from(json!({
        "kind": "file",
        "rows": [{
            "path": "src/lib.rs", "toolCallCount": 4, "initialTokens": 1.0,
            "persistenceTokens": 2.0, "ridingTurns": 3, "totalCost": 0.25,
            "firstEmitTs": "2026-01-01T00:00:00Z", "firstEmitTurnIndex": 7,
            "totalOutputBytes": 100, "maxOutputBytes": 60, "truncatedCount": 2
        }],
        "refused": false
    }));
    let value = hotspots_result_to_json(&result);
    assert_eq!(value["rows"][0]["path"], json!("src/lib.rs"));
    assert_eq!(value["rows"][0]["firstEmitTurnIndex"], json!(7));
    assert_eq!(value["refused"], json!(false));
}

#[test]
fn subagent_rows_use_subagent_mapper() {
    let result = result_from(json!({
        "kind": "subagent",
        "rows": [
            {
                "subagentType": "Explore", "callCount": 1, "totalCost": 0.1,
                "initialTokens": 1.0, "persistenceTokens": 1.0, "totalOutputBytes": 1,
                "maxOutputBytes": 1, "truncatedCount": 0
            },
            {
                "subagentType": "Plan", "callCount": 2, "totalCost": 0.2,
                "initialTokens": 1.0, "persistenceTokens": 1.0, "totalOutputBytes": 1,
                "maxOutputBytes": 1, "truncatedCount": 0
            }
        ]
    }));
    let value = hotspots_result_to_json(&result);
    let rows = value["rows"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["subagentType"], json!("Explore"));
    assert_eq!(rows[1]["subagentType"], json!("Plan"));
    assert_eq!(rows[1]["callCount"], json!(2));
}

#[test]
fn findings_pass_through_with_summary() {
    let result = result_from(json!({
        "kind": "findings",
        "findings": [{
            "kind": "retry-loop", "severity": "warn", "sessionId": "s1",
            "title": "t", "detail": "d", "estimatedSavings": { "usdPerSession": 0.1 },
            "actions": []
        }],
        "summary": { "total": 1 }
    }));
    let value = hotspots_result_to_json(&result);
    assert_eq!(value["findings"][0]["kind"], json!("retry-loop"));
    assert_eq!(value["findings"][0]["severity"], json!("warn"));
    assert_eq!(value["summary"], json!({ "total": 1 }));
    assert_eq!(value.as_object().unwrap().len(), 2);
}

#[test]
fn attribution_delegates_to_attribution_mapper() {
    let result = result_from(json!({
        "kind": "attribution",
        "turnsAnalyzed": 12,
        "grandTotal": 3.0,
        "attributedTotal": 2.0,
        "unattributedTotal": 1.0,
        "attributionDegraded": false,
        "sessions": [],
        "files": [],
        "bashVerbs": [],
        "bash": [],
        "subagents": [],
        "mcpServers": [],
        "fidelity": { "analyzed": 12, "excluded": 0, "summary": {}, "refused": false }
    }));
    let HotspotsResult::Attribution(a) = &result else {
        panic!("expected attribution");
    };
    let value = hotspots_result_to_json(&result);
    assert_eq!(value, attribution_to_json(a));
    assert_eq!(value["turnsAnalyzed"], json!(12));
    assert!(value.get("rows").is_none());
}
