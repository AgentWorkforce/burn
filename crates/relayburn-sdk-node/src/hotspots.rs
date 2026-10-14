//! `hotspots` binding — a discriminated union serialized via serde_json so
//! the `kind` discriminant and per-variant rows survive the boundary. The
//! Node facade's `.d.ts` documents the shape (`HotspotsResult` union).

use napi::bindgen_prelude::{BigInt, Error as NapiError};
use napi_derive::napi;

use relayburn_sdk as sdk;

use crate::{bigint_to_u64, maybe_path, sdk_err, BigIntPromoting, BurnError, SDK_ERROR_CODE};

/// Mirror of `sdk::HotspotsGroupBy`. Wire values match
/// The Node facade's
/// `'attribution' | 'bash' | 'bash-verb' | 'file' | 'subagent' |
/// 'findings'` literal union.
#[napi(string_enum = "kebab-case")]
pub enum HotspotsGroupBy {
    Attribution,
    Bash,
    BashVerb,
    File,
    Subagent,
    Findings,
}

impl From<HotspotsGroupBy> for sdk::HotspotsGroupBy {
    fn from(g: HotspotsGroupBy) -> Self {
        match g {
            HotspotsGroupBy::Attribution => sdk::HotspotsGroupBy::Attribution,
            HotspotsGroupBy::Bash => sdk::HotspotsGroupBy::Bash,
            HotspotsGroupBy::BashVerb => sdk::HotspotsGroupBy::BashVerb,
            HotspotsGroupBy::File => sdk::HotspotsGroupBy::File,
            HotspotsGroupBy::Subagent => sdk::HotspotsGroupBy::Subagent,
            HotspotsGroupBy::Findings => sdk::HotspotsGroupBy::Findings,
        }
    }
}

#[napi(object)]
pub struct HotspotsOptions {
    pub session: Option<String>,
    pub project: Option<String>,
    pub since: Option<String>,
    pub group_by: Option<HotspotsGroupBy>,
    pub patterns: Option<Vec<String>>,
    pub workflow: Option<String>,
    pub provider: Option<Vec<String>>,
    pub context_output_ratio_threshold: Option<f64>,
    pub context_output_min_tokens: Option<BigInt>,
    pub ledger_home: Option<String>,
}

/// Per-axis hotspot attribution + pattern-finding queries. Returns a
/// JSON-shaped discriminated union — see `HotspotsResult` in
/// `packages/sdk-node/src/index.d.ts`. u64 row counts (`callCount`,
/// `distinctCommands`, `ridingTurns`, `firstEmitTurnIndex`,
/// `toolCallCount`, `turnsAnalyzed`, `analyzed`, `excluded`) cross as
/// `BigInt` per the file header rule.
#[napi(ts_return_type = "import('./index').HotspotsResult")]
pub fn hotspots(opts: Option<HotspotsOptions>) -> Result<BigIntPromoting, BurnError> {
    let opts = opts.unwrap_or(HotspotsOptions {
        session: None,
        project: None,
        since: None,
        group_by: None,
        patterns: None,
        workflow: None,
        provider: None,
        context_output_ratio_threshold: None,
        context_output_min_tokens: None,
        ledger_home: None,
    });
    let raw = sdk::HotspotsOptions {
        session: opts.session,
        project: opts.project,
        since: opts.since,
        group_by: opts.group_by.map(Into::into),
        patterns: opts.patterns,
        workflow: opts.workflow,
        provider: opts.provider,
        context_output_ratio_threshold: opts.context_output_ratio_threshold,
        context_output_min_tokens: opts
            .context_output_min_tokens
            .map(bigint_to_u64)
            .transpose()?,
        ledger_home: maybe_path(opts.ledger_home),
    };
    let result = sdk::hotspots(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize hotspots: {e}")))?;
    Ok(BigIntPromoting(value))
}
