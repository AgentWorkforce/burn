use std::io::Write;
use std::process::{Command, Stdio};

use serde_json::{json, Value};

#[test]
fn stdio_catalog_rejects_bad_input_and_keeps_serving() {
    let home = tempfile::tempdir().expect("temp ledger home");
    let mut child = Command::new(env!("CARGO_BIN_EXE_burn"))
        .args([
            "--ledger-path",
            home.path().to_str().expect("temp path is utf-8"),
            "mcp-server",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn burn mcp-server");

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
    {
        let stdin = child.stdin.as_mut().expect("child stdin");
        for request in requests {
            writeln!(stdin, "{request}").expect("write JSON-RPC request");
        }
    }
    drop(child.stdin.take());

    let output = child.wait_with_output().expect("wait for mcp-server");
    assert!(
        output.status.success(),
        "mcp-server failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let responses: Vec<Value> = String::from_utf8(output.stdout)
        .expect("stdout is utf-8")
        .lines()
        .map(|line| serde_json::from_str(line).expect("response is JSON"))
        .collect();
    assert_eq!(responses.len(), 4, "one response per request");

    let names: Vec<&str> = responses[0]["result"]["tools"]
        .as_array()
        .expect("tools array")
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
    assert_eq!(responses[1]["result"]["isError"], json!(true));
    assert!(responses[1]["result"].get("structuredContent").is_none());
    assert_eq!(responses[2]["id"], json!(3));
    assert!(responses[2]["result"]["structuredContent"].is_object());
    assert_eq!(responses[3]["error"]["code"], json!(-32601));
}
