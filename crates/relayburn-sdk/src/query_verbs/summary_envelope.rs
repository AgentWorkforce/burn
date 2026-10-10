//! Versioned summary report envelopes: schema name/version, capability
//! handshake, and the normalized query window around a summary report or
//! bucketed time series.

use super::*;

pub const SUMMARY_REPORT_SCHEMA_NAME: &str = "relayburn.report.summary.v1";
pub const SUMMARY_TIMESERIES_SCHEMA_NAME: &str = "relayburn.report.summaryTimeseries.v1";
pub const REPORT_SCHEMA_VERSION: u32 = 1;

const REPORT_CAPABILITIES: &[&str] = &[
    "summaryReportEnvelope",
    "summaryTimeseriesEnvelope",
    "summaryUntil",
    "summaryProviderGrouping",
    "summaryTagGrouping",
    "summaryQuality",
    "summaryRelationships",
];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportSchemaVersion {
    pub name: String,
    pub version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportCapabilities {
    pub package_name: String,
    pub package_version: String,
    pub features: Vec<String>,
}

impl ReportCapabilities {
    pub fn current() -> Self {
        Self {
            package_name: env!("CARGO_PKG_NAME").to_string(),
            package_version: env!("CARGO_PKG_VERSION").to_string(),
            features: REPORT_CAPABILITIES
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportBucket {
    pub seconds: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ReportWindow {
    pub since: Option<String>,
    pub until: Option<String>,
    pub bucket: Option<ReportBucket>,
}

impl ReportWindow {
    fn from_query(q: &Query, bucket_secs: Option<u64>) -> Self {
        Self {
            since: q.since.clone(),
            until: q.until.clone(),
            bucket: bucket_secs.map(|seconds| ReportBucket { seconds }),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryReportEnvelope {
    pub schema: ReportSchemaVersion,
    pub capabilities: ReportCapabilities,
    pub window: ReportWindow,
    pub report: SummaryReport,
}

impl SummaryReportEnvelope {
    fn new(q: &Query, report: SummaryReport) -> Self {
        Self {
            schema: ReportSchemaVersion {
                name: SUMMARY_REPORT_SCHEMA_NAME.to_string(),
                version: REPORT_SCHEMA_VERSION,
            },
            capabilities: ReportCapabilities::current(),
            window: ReportWindow::from_query(q, None),
            report,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SummaryTimeseriesEnvelope {
    pub schema: ReportSchemaVersion,
    pub capabilities: ReportCapabilities,
    pub window: ReportWindow,
    pub timeseries: SummaryTimeseries,
}

impl SummaryTimeseriesEnvelope {
    fn new(q: &Query, bucket_secs: u64, timeseries: SummaryTimeseries) -> Self {
        Self {
            schema: ReportSchemaVersion {
                name: SUMMARY_TIMESERIES_SCHEMA_NAME.to_string(),
                version: REPORT_SCHEMA_VERSION,
            },
            capabilities: ReportCapabilities::current(),
            window: ReportWindow::from_query(q, Some(bucket_secs)),
            timeseries,
        }
    }
}

pub fn report_capabilities() -> ReportCapabilities {
    ReportCapabilities::current()
}

impl LedgerHandle {
    pub fn summary_report_envelope(
        &self,
        opts: SummaryReportOptions,
    ) -> Result<SummaryReportEnvelope> {
        let q = build_summary_report_query(&opts)?;
        let mut normalized_opts = opts;
        normalized_opts.since = q.since.clone();
        normalized_opts.until = q.until.clone();
        let report = self.summary_report(normalized_opts)?;
        Ok(SummaryReportEnvelope::new(&q, report))
    }

    pub fn summary_timeseries_envelope(
        &self,
        opts: SummaryReportOptions,
        bucket_secs: u64,
    ) -> Result<SummaryTimeseriesEnvelope> {
        let q = build_summary_report_query(&opts)?;
        let mut normalized_opts = opts;
        normalized_opts.since = q.since.clone();
        normalized_opts.until = q.until.clone();
        let timeseries = self.summary_timeseries(normalized_opts, bucket_secs)?;
        Ok(SummaryTimeseriesEnvelope::new(&q, bucket_secs, timeseries))
    }
}

pub fn summary_report_envelope(opts: SummaryReportOptions) -> Result<SummaryReportEnvelope> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary_report_envelope(SummaryReportOptions {
        ledger_home: None,
        ..opts
    })
}

pub fn summary_timeseries(
    opts: SummaryReportOptions,
    bucket_secs: u64,
) -> Result<SummaryTimeseries> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary_timeseries(
        SummaryReportOptions {
            ledger_home: None,
            ..opts
        },
        bucket_secs,
    )
}

pub fn summary_timeseries_envelope(
    opts: SummaryReportOptions,
    bucket_secs: u64,
) -> Result<SummaryTimeseriesEnvelope> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.summary_timeseries_envelope(
        SummaryReportOptions {
            ledger_home: None,
            ..opts
        },
        bucket_secs,
    )
}
