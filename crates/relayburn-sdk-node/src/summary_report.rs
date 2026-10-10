//! SDK-owned summary report contract: capability handshake plus the
//! versioned summary report and time-series envelopes.

use napi::Error as NapiError;
use napi_derive::napi;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use relayburn_sdk as sdk;

use crate::{invalid_arg, sdk_err, BigIntPromoting, BurnError, SDK_ERROR_CODE};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryTimeseriesEnvelopeOptions {
    #[serde(default)]
    bucket_seconds: Option<u64>,
    #[serde(flatten)]
    report: sdk::SummaryReportOptions,
}

fn summary_report_options_from_value(
    opts: Option<JsonValue>,
) -> Result<sdk::SummaryReportOptions, BurnError> {
    let parsed = opts.map(serde_json::from_value).transpose();
    let parsed = parsed.map_err(|e| invalid_arg(format!("invalid summaryReport options: {e}")))?;
    Ok(parsed.unwrap_or_default())
}

/// Version/capability handshake for SDK-owned report contracts.
#[napi(ts_return_type = "import('./index').ReportCapabilities")]
pub fn capabilities() -> Result<BigIntPromoting, BurnError> {
    let value = serde_json::to_value(sdk::report_capabilities())
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize capabilities: {e}")))?;
    Ok(BigIntPromoting(value))
}

/// SDK-owned summary report envelope. The result shape is documented as
/// `SummaryReportEnvelope` in the Node facade types.
#[napi(
    js_name = "summaryReport",
    ts_return_type = "import('./index').SummaryReportEnvelope"
)]
pub fn summary_report(opts: Option<JsonValue>) -> Result<BigIntPromoting, BurnError> {
    let raw = summary_report_options_from_value(opts)?;
    let result = sdk::summary_report_envelope(raw).map_err(sdk_err)?;
    let value = serde_json::to_value(&result)
        .map_err(|e| NapiError::new(SDK_ERROR_CODE, format!("serialize summary_report: {e}")))?;
    Ok(BigIntPromoting(value))
}

/// SDK-owned bucketed summary time-series envelope.
#[napi(
    js_name = "summaryTimeseries",
    ts_return_type = "import('./index').SummaryTimeseriesEnvelope"
)]
pub fn summary_timeseries(opts: JsonValue) -> Result<BigIntPromoting, BurnError> {
    let parsed: SummaryTimeseriesEnvelopeOptions = serde_json::from_value(opts)
        .map_err(|e| invalid_arg(format!("invalid summaryTimeseries options: {e}")))?;
    let bucket_seconds = parsed
        .bucket_seconds
        .filter(|n| *n > 0)
        .ok_or_else(|| invalid_arg("summaryTimeseries requires a positive bucketSeconds"))?;
    let result =
        sdk::summary_timeseries_envelope(parsed.report, bucket_seconds).map_err(sdk_err)?;
    let value = serde_json::to_value(&result).map_err(|e| {
        NapiError::new(SDK_ERROR_CODE, format!("serialize summary_timeseries: {e}"))
    })?;
    Ok(BigIntPromoting(value))
}
