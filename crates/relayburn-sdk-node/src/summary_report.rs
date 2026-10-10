//! SDK-owned summary report contract: capability handshake plus the
//! versioned summary report and time-series envelopes.

use napi::Error as NapiError;
use napi_derive::napi;
use serde::Deserialize;
use serde_json::Value as JsonValue;

use relayburn_sdk as sdk;

use crate::{invalid_arg, sdk_err, BigIntPromoting, BurnError, SDK_ERROR_CODE};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SummaryTimeseriesEnvelopeOptions {
    bucket_seconds: u64,
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

/// Bucket width (required) plus the report options. A zero width is
/// rejected by the SDK partitioner.
fn summary_timeseries_options_from_value(
    opts: JsonValue,
) -> Result<SummaryTimeseriesEnvelopeOptions, BurnError> {
    serde_json::from_value(opts)
        .map_err(|e| invalid_arg(format!("invalid summaryTimeseries options: {e}")))
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
    let parsed = summary_timeseries_options_from_value(opts)?;
    let result =
        sdk::summary_timeseries_envelope(parsed.report, parsed.bucket_seconds).map_err(sdk_err)?;
    let value = serde_json::to_value(&result).map_err(|e| {
        NapiError::new(SDK_ERROR_CODE, format!("serialize summary_timeseries: {e}"))
    })?;
    Ok(BigIntPromoting(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_options_parse_camel_case_fields() {
        let opts = summary_report_options_from_value(Some(json!({
            "since": "7d",
            "until": "2026-04-23T00:00:30Z",
            "includeQuality": true,
        })))
        .unwrap();
        assert_eq!(opts.since.as_deref(), Some("7d"));
        assert_eq!(opts.until.as_deref(), Some("2026-04-23T00:00:30Z"));
        assert!(opts.include_quality);
        assert!(summary_report_options_from_value(None)
            .unwrap()
            .since
            .is_none());
        assert!(summary_report_options_from_value(Some(json!({ "since": 7 }))).is_err());
    }

    #[test]
    fn timeseries_options_require_bucket_seconds() {
        let parsed =
            summary_timeseries_options_from_value(json!({ "bucketSeconds": 60, "since": "1h" }))
                .unwrap();
        assert_eq!(parsed.bucket_seconds, 60);
        assert_eq!(parsed.report.since.as_deref(), Some("1h"));
        let err = summary_timeseries_options_from_value(json!({ "since": "1h" })).unwrap_err();
        assert!(err.reason.contains("bucketSeconds"), "{}", err.reason);
    }
}
