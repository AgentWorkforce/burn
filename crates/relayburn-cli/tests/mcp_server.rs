use std::time::Duration;

use serde_json::{json, Value};

#[test]
fn stdio_catalog_rejects_bad_input_and_keeps_serving() {
    let home = tempfile::tempdir().expect("temp ledger home");
    let requests = [
        json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list", "params": {} }),
        json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "burn__summary", "arguments": [] }
        }),
        json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call",
            "params": { "name": "burn__summary", "arguments": {} }
        }),
        json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": { "name": "burn__unknown", "arguments": {} }
        }),
    ];
    let input = requests
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let output = assert_cmd::Command::new(env!("CARGO_BIN_EXE_burn"))
        .args([
            "--ledger-path",
            home.path().to_str().expect("temp path is utf-8"),
            "mcp-server",
        ])
        .write_stdin(input)
        .timeout(Duration::from_secs(10))
        .assert()
        .success()
        .get_output()
        .clone();
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .expect("stdout is utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("response is JSON"))
        .collect();
    assert_eq!(responses.len(), 4, "one response per request");
    for (index, response) in responses.iter().enumerate() {
        assert_eq!(response["id"], json!(index + 1), "response {index}");
    }

    let tools = responses[0]["result"]["tools"]
        .as_array()
        .expect("tools array");
    let names: Vec<&str> = tools
        .iter()
        .map(|tool| tool["name"].as_str().expect("tool name"))
        .collect();
    assert_eq!(
        names,
        [
            "burn__sessionCost",
            "burn__fingerprint",
            "burn__summary",
            "burn__hotspots",
            "burn__overhead",
            "burn__overheadTrim",
            "burn__compare",
        ]
    );
    let property_names = |tool: &Value| {
        tool["inputSchema"]["properties"]
            .as_object()
            .expect("schema properties")
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    };
    assert_eq!(
        property_names(&tools[2]),
        ["session", "project", "since", "tags", "groupByTag"]
    );
    assert_eq!(
        tools[3]["inputSchema"]["properties"]["groupBy"]["enum"],
        json!([
            "attribution",
            "bash",
            "bash-verb",
            "file",
            "subagent",
            "findings"
        ])
    );
    assert_eq!(
        tools[4]["inputSchema"]["properties"]["kind"]["enum"],
        json!(["claude-md", "agents-md"])
    );
    assert_eq!(
        tools[5]["inputSchema"]["properties"]["top"],
        json!({
            "type": "integer",
            "minimum": 1,
            "maximum": u32::MAX,
            "description": "Maximum number of recommendations."
        })
    );
    assert_eq!(tools[6]["inputSchema"]["required"], json!(["models"]));
    assert_eq!(
        tools[6]["inputSchema"]["properties"]["models"]["minItems"],
        json!(2)
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["additionalProperties"], json!(false));
    }
    assert_eq!(responses[1]["result"]["isError"], json!(true));
    assert!(responses[1]["result"].get("structuredContent").is_none());
    assert!(responses[2]["result"]["structuredContent"].is_object());
    assert_eq!(responses[3]["error"]["code"], json!(-32601));
}
