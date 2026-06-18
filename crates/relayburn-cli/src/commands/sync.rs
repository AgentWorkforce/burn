//! `burn sync` — push the local ledger to a hosted burn backend.
//!
//! Thin client over the SDK's [`export_ledger`] verb: it streams every event
//! row (in the same `{"v":1,"kind":…,"record":…}` envelope the backend
//! ingests), batches them, and POSTs each batch to `<url>/v1/ingest` with a
//! bearer token. The push is idempotent — the backend dedups on each record's
//! natural key — so re-running is always safe and doubles as backfill.
//!
//! This is the manual half of sync. Automatic push during `burn ingest`
//! (a durable outbox wired into the watch loop) is the planned follow-up; the
//! wire contract and credentials handling here are shared with it.

use std::fs;
use std::path::{Path, PathBuf};

use relayburn_sdk::{export_ledger, ledger_home, ExportLedgerOptions};
use serde_json::{json, Value};

use crate::cli::{GlobalArgs, SyncArgs};
use crate::render::error::report_error;
use crate::render::json::render_json;

const DEFAULT_URL: &str = "https://burn.agentrelay.com";
const DEFAULT_BATCH_SIZE: usize = 500;

pub fn run(globals: &GlobalArgs, args: SyncArgs) -> i32 {
    match run_inner(globals, &args) {
        Ok(summary) => {
            if globals.json {
                let _ = render_json(&summary);
            } else {
                println!(
                    "burn sync: pushed {} record(s) to {} ({} accepted) from machine {}",
                    summary.received, summary.url, summary.accepted, summary.machine_id
                );
            }
            0
        }
        Err(err) => report_error(&err, globals),
    }
}

struct SyncSummary {
    url: String,
    machine_id: String,
    received: u64,
    accepted: u64,
    batches: u64,
}

impl serde::Serialize for SyncSummary {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("SyncSummary", 5)?;
        st.serialize_field("url", &self.url)?;
        st.serialize_field("machineId", &self.machine_id)?;
        st.serialize_field("received", &self.received)?;
        st.serialize_field("accepted", &self.accepted)?;
        st.serialize_field("batches", &self.batches)?;
        st.end()
    }
}

fn run_inner(globals: &GlobalArgs, args: &SyncArgs) -> anyhow::Result<SyncSummary> {
    let home = globals.ledger_path.clone().unwrap_or_else(ledger_home);
    let creds = load_credentials(&home);

    let url = args
        .url
        .clone()
        .or_else(|| std::env::var("BURN_CLOUD_URL").ok())
        .or_else(|| creds.as_ref().and_then(|c| c.api_url.clone()))
        .unwrap_or_else(|| DEFAULT_URL.to_string());
    let url = url.trim_end_matches('/').to_string();

    let token = args
        .token
        .clone()
        .or_else(|| std::env::var("BURN_CLOUD_TOKEN").ok())
        .or_else(|| creds.as_ref().and_then(|c| c.access_token.clone()))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "no token: run `burn login`, pass --token, or set BURN_CLOUD_TOKEN"
            )
        })?;

    let machine_id = resolve_machine_id(&home)?;
    let hostname = std::env::var("HOSTNAME").ok().filter(|h| !h.is_empty());
    let batch_size = args.batch_size.unwrap_or(DEFAULT_BATCH_SIZE).max(1);

    let records: Vec<Value> = export_ledger(ExportLedgerOptions {
        ledger_home: Some(home.clone()),
    })?
    .collect();

    let ingest_url = format!("{url}/v1/ingest");
    let mut received: u64 = 0;
    let mut accepted: u64 = 0;
    let mut batches: u64 = 0;

    for chunk in records.chunks(batch_size) {
        let body = json!({
            "machine": {
                "id": machine_id,
                "hostname": hostname,
                "label": args.label,
                "os": std::env::consts::OS,
                "burnVersion": env!("CARGO_PKG_VERSION"),
            },
            "batchId": new_uuid(),
            "records": chunk,
        });

        let resp = ureq::post(&ingest_url)
            .set("authorization", &format!("Bearer {token}"))
            .send_json(body)
            .map_err(map_http_error)?;

        let parsed: Value = resp.into_json().unwrap_or(Value::Null);
        received += parsed.get("received").and_then(Value::as_u64).unwrap_or(chunk.len() as u64);
        accepted += parsed.get("accepted").and_then(Value::as_u64).unwrap_or(0);
        batches += 1;
    }

    Ok(SyncSummary {
        url,
        machine_id,
        received,
        accepted,
        batches,
    })
}

/// Turn a non-2xx response into a readable error including the server body.
fn map_http_error(err: ureq::Error) -> anyhow::Error {
    match err {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            if code == 401 {
                anyhow::anyhow!("unauthorized (401): token invalid or expired — run `burn login`")
            } else {
                anyhow::anyhow!("backend returned {code}: {body}")
            }
        }
        ureq::Error::Transport(t) => anyhow::anyhow!("network error: {t}"),
    }
}

struct Credentials {
    api_url: Option<String>,
    access_token: Option<String>,
}

/// Read `$RELAYBURN_HOME/credentials.json` (written by `burn login`). Missing or
/// malformed files are treated as "no stored credentials", not an error.
fn load_credentials(home: &Path) -> Option<Credentials> {
    let raw = fs::read_to_string(home.join("credentials.json")).ok()?;
    let v: Value = serde_json::from_str(&raw).ok()?;
    Some(Credentials {
        api_url: v.get("apiUrl").and_then(Value::as_str).map(str::to_string),
        access_token: v
            .get("accessToken")
            .and_then(Value::as_str)
            .map(str::to_string),
    })
}

/// Stable per-machine id, persisted at `$RELAYBURN_HOME/machine-id`. Generated
/// on first sync so the backend can attribute records to this computer.
fn resolve_machine_id(home: &Path) -> anyhow::Result<String> {
    let path: PathBuf = home.join("machine-id");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Ok(trimmed.to_string());
        }
    }
    let id = new_uuid();
    fs::create_dir_all(home).ok();
    fs::write(&path, &id)?;
    Ok(id)
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}
