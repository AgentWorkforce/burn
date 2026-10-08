use super::*;

// ---------------------------------------------------------------------------
// overhead + overhead_trim — share `gather_overhead`
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadOptions {
    pub project: Option<PathBuf>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadSectionCost {
    pub file_path: String,
    pub section: MarkdownSection,
    pub token_share: f64,
    pub cost_per_session: f64,
    pub total_cost: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadAttributionDetail {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub session_costs: Vec<SessionClaudeMdCost>,
    pub section_costs: Vec<OverheadSectionCost>,
    pub per_session_avg: f64,
    pub per_session_p95: f64,
    pub session_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadFileSummary {
    pub kind: OverheadFileKind,
    pub path: String,
    pub applies_to: Vec<SourceKind>,
    pub total_lines: u64,
    pub bytes: u64,
    pub tokens: u64,
    pub sections: Vec<MarkdownSection>,
    pub grouping_level: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadPerFileEntry {
    pub path: String,
    pub kind: OverheadFileKind,
    pub applies_to: Vec<SourceKind>,
    pub attribution: OverheadAttributionDetail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadResult {
    pub project: String,
    pub files: Vec<OverheadFileSummary>,
    pub per_file: Vec<OverheadPerFileEntry>,
    pub grand_total: f64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimOptions {
    pub project: Option<PathBuf>,
    pub since: Option<String>,
    pub kind: Option<OverheadFileKind>,
    pub ledger_home: Option<PathBuf>,
    pub top: Option<u64>,
    pub include_diff: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimSection {
    pub heading: String,
    pub start_line: u64,
    pub end_line: u64,
    pub tokens: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimProjectedSavings {
    pub per_session_usd: f64,
    pub across_window_usd: f64,
    pub tokens: u64,
    pub token_share: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimRecommendation {
    pub file: String,
    pub kind: OverheadFileKind,
    pub applies_to: Vec<SourceKind>,
    pub section: OverheadTrimSection,
    pub projected_savings: OverheadTrimProjectedSavings,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimSummary {
    pub files_analyzed: u64,
    pub files_with_recommendations: u64,
    pub total_recommendations: u64,
    pub total_projected_savings_per_session: f64,
    pub total_projected_savings_across_window: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OverheadTrimResult {
    pub project: String,
    pub since: String,
    pub recommendations: Vec<OverheadTrimRecommendation>,
    pub summary: OverheadTrimSummary,
}

/// Shape knobs for [`overhead_trim_report`].
pub(crate) struct TrimShape<'a> {
    pub top: Option<u64>,
    pub include_diff: bool,
    pub since_label: &'a str,
}

/// Every instruction file of `kind` (all kinds when `None`) under
/// `project_path`, parsed into sections.
pub(crate) fn load_overhead_files(
    project_path: &Path,
    kind: Option<OverheadFileKind>,
) -> Result<Vec<ParsedOverheadFile>> {
    find_overhead_files(project_path)
        .into_iter()
        .filter(|f| kind.is_none_or(|want| f.kind == want))
        .map(|f| Ok(load_overhead_file(f)?))
        .collect()
}

struct GatheredOverhead {
    project_path: PathBuf,
    files: Vec<ParsedOverheadFile>,
    turns: Vec<TurnRecord>,
    pricing: PricingTable,
}

fn gather_overhead(
    handle: &LedgerHandle,
    project: Option<&Path>,
    since: Option<&str>,
    kind: Option<OverheadFileKind>,
) -> Result<GatheredOverhead> {
    let project_path: PathBuf = match project {
        Some(p) => fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf()),
        None => std::env::current_dir()?,
    };
    let files = load_overhead_files(&project_path, kind)?;
    let turns = if files.is_empty() {
        Vec::new()
    } else {
        let resolved = resolve_project(&project_path.to_string_lossy());
        let q = Query {
            project: Some(resolved.project_key.unwrap_or(resolved.project)),
            since: normalize_since(since)?,
            ..Default::default()
        };
        collect_turns(handle, &q)?
    };
    Ok(GatheredOverhead {
        project_path,
        files,
        turns,
        pricing: load_pricing_for_ledger(handle),
    })
}

/// Per-file and per-section cost of `files` across `turns`.
pub(crate) fn overhead_report(
    project_path: &Path,
    files: &[ParsedOverheadFile],
    turns: &[TurnRecord],
    pricing: &PricingTable,
) -> OverheadResult {
    let project = project_path.to_string_lossy().into_owned();
    if files.is_empty() {
        return OverheadResult {
            project,
            files: Vec::new(),
            per_file: Vec::new(),
            grand_total: 0.0,
        };
    }
    let attribution = attribute_overhead(AttributeOverheadInput {
        files,
        turns,
        pricing,
    });
    OverheadResult {
        project,
        files: files.iter().map(file_summary).collect(),
        per_file: attribution.per_file.iter().map(per_file_entry).collect(),
        grand_total: attribution.grand_total,
    }
}

fn file_summary(pf: &ParsedOverheadFile) -> OverheadFileSummary {
    OverheadFileSummary {
        kind: pf.file.kind,
        path: pf.file.path.clone(),
        applies_to: pf.file.applies_to.clone(),
        total_lines: pf.parsed.total_lines,
        bytes: pf.parsed.bytes,
        tokens: pf.parsed.tokens,
        sections: pf.parsed.sections.clone(),
        grouping_level: pf.parsed.grouping_level,
    }
}

fn per_file_entry(p: &crate::analyze::OverheadFileAttribution) -> OverheadPerFileEntry {
    OverheadPerFileEntry {
        path: p.file.path.clone(),
        kind: p.file.kind,
        applies_to: p.file.applies_to.clone(),
        attribution: OverheadAttributionDetail {
            total_tokens: p.attribution.total_tokens,
            total_cost: p.attribution.total_cost,
            session_costs: p.attribution.session_costs.clone(),
            section_costs: p
                .attribution
                .section_costs
                .iter()
                .map(|sc| OverheadSectionCost {
                    file_path: sc.file_path.clone(),
                    section: sc.section.clone(),
                    token_share: sc.token_share,
                    cost_per_session: sc.cost_per_session,
                    total_cost: sc.total_cost,
                })
                .collect(),
            per_session_avg: p.attribution.per_session_avg,
            per_session_p95: p.attribution.per_session_p95,
            session_count: p.attribution.session_count,
        },
    }
}

/// The top sections of each file in `files` worth trimming, with their
/// projected savings across `turns`.
pub(crate) fn overhead_trim_report(
    project_path: &Path,
    files: &[ParsedOverheadFile],
    turns: &[TurnRecord],
    pricing: &PricingTable,
    shape: TrimShape<'_>,
) -> Result<OverheadTrimResult> {
    let attribution = attribute_overhead(AttributeOverheadInput {
        files,
        turns,
        pricing,
    });
    let top_n = parse_top_n(shape.top);
    let mut recommendations: Vec<OverheadTrimRecommendation> = Vec::new();
    let mut files_with_recs: u64 = 0;
    for fa in &attribution.per_file {
        let recs = build_trim_recommendations(&fa.attribution, top_n);
        if recs.is_empty() {
            continue;
        }
        files_with_recs += 1;
        let file_text = if shape.include_diff {
            Some(fs::read_to_string(&fa.file.path)?)
        } else {
            None
        };
        for rec in &recs {
            let diff = file_text.as_deref().map(|text| {
                render_unified_diff_for_recommendation(&fa.file.path, text, rec, Some(project_path))
            });
            recommendations.push(OverheadTrimRecommendation {
                file: to_project_relative(&fa.file.path, project_path),
                kind: fa.file.kind,
                applies_to: fa.file.applies_to.clone(),
                section: OverheadTrimSection {
                    heading: rec.section.heading.clone(),
                    start_line: rec.section.start_line,
                    end_line: rec.section.end_line,
                    tokens: rec.section.tokens,
                },
                projected_savings: OverheadTrimProjectedSavings {
                    per_session_usd: rec.projected_savings_per_session,
                    across_window_usd: rec.projected_savings_across_window,
                    tokens: rec.section.tokens,
                    token_share: rec.token_share,
                },
                diff,
            });
        }
    }
    Ok(OverheadTrimResult {
        project: project_path.to_string_lossy().into_owned(),
        since: shape.since_label.to_string(),
        summary: OverheadTrimSummary {
            files_analyzed: files.len() as u64,
            files_with_recommendations: files_with_recs,
            total_recommendations: recommendations.len() as u64,
            total_projected_savings_per_session: recommendations
                .iter()
                .map(|r| r.projected_savings.per_session_usd)
                .sum(),
            total_projected_savings_across_window: recommendations
                .iter()
                .map(|r| r.projected_savings.across_window_usd)
                .sum(),
        },
        recommendations,
    })
}

impl LedgerHandle {
    pub fn overhead(&self, opts: OverheadOptions) -> Result<OverheadResult> {
        let data = gather_overhead(
            self,
            opts.project.as_deref(),
            opts.since.as_deref(),
            opts.kind,
        )?;
        Ok(overhead_report(
            &data.project_path,
            &data.files,
            &data.turns,
            &data.pricing,
        ))
    }

    pub fn overhead_trim(&self, opts: OverheadTrimOptions) -> Result<OverheadTrimResult> {
        let since_label = opts.since.clone().unwrap_or_else(|| "all time".to_string());
        let data = gather_overhead(
            self,
            opts.project.as_deref(),
            opts.since.as_deref(),
            opts.kind,
        )?;
        overhead_trim_report(
            &data.project_path,
            &data.files,
            &data.turns,
            &data.pricing,
            TrimShape {
                top: opts.top,
                include_diff: opts.include_diff.unwrap_or(true),
                since_label: &since_label,
            },
        )
    }
}

pub fn overhead(opts: OverheadOptions) -> Result<OverheadResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.overhead(OverheadOptions {
        ledger_home: None,
        ..opts
    })
}

pub fn overhead_trim(opts: OverheadTrimOptions) -> Result<OverheadTrimResult> {
    let handle = open_with(opts.ledger_home.as_deref())?;
    handle.overhead_trim(OverheadTrimOptions {
        ledger_home: None,
        ..opts
    })
}

fn parse_top_n(v: Option<u64>) -> usize {
    match v {
        Some(n) if n > 0 => n as usize,
        _ => 3,
    }
}

fn to_project_relative(file_path: &str, project_path: &Path) -> String {
    let p = Path::new(file_path);
    match p.strip_prefix(project_path) {
        Ok(r) if !r.as_os_str().is_empty() => {
            r.to_string_lossy().replace(std::path::MAIN_SEPARATOR, "/")
        }
        _ => file_path.replace(std::path::MAIN_SEPARATOR, "/"),
    }
}
