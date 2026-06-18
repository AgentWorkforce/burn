import SwiftUI
import Charts

/// The "Burn rate" tab: a bucketed chart of token burn over a selectable time
/// range (5m/1h/12h/1d/7d), with one color-coded line per provider (Claude,
/// Codex) overlaid and per-provider show/hide toggles. The headline is the
/// combined burn rate (tokens/sec) of the latest bucket across the shown
/// providers, with a cumulative line.
///
/// Owns a `LiveBurnViewModel` whose lifecycle (refresh timer + background ingest
/// watch) is bound to this view's appearance. Falls back to a hint when burn
/// can't be queried, mirroring how the rest of the app no-ops on a missing
/// binary.
struct LiveBurnView: View {
    @ObservedObject var viewModel: LiveBurnViewModel

    /// Providers currently shown, in a stable order.
    private var shown: [ProviderName] { ProviderName.allCases.filter(viewModel.isEnabled) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            rangePicker
            if viewModel.unavailable {
                hint
            } else if !hasData {
                warming
            } else {
                headline
                rateChart.frame(height: 130)
                cumulativeChart.frame(height: 90)
            }
            providerToggles
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .onAppear { viewModel.start() }
        .onDisappear { viewModel.stop() }
    }

    /// Segmented switch for the chart's time range.
    private var rangePicker: some View {
        Picker("", selection: Binding(
            get: { viewModel.range },
            set: { viewModel.setRange($0) }
        )) {
            ForEach(LiveRange.allCases) { range in
                Text(range.label).tag(range)
            }
        }
        .pickerStyle(.segmented)
        .labelsHidden()
    }

    private var hasData: Bool {
        shown.contains { (viewModel.series[$0]?.count ?? 0) >= 2 }
    }

    // MARK: Headline (combined across shown providers)

    private var headline: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 1) {
                Text("Burn rate")
                    .font(.title3.weight(.bold))
                Text("last \(viewModel.range.label) · combined")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 1) {
                Text(rateLabel)
                    .font(.headline.weight(.bold))
                    .monospacedDigit()
                Text(spendLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        }
    }

    private var totalRate: Double {
        shown.reduce(0) { $0 + (viewModel.series[$1]?.last?.tokensPerSecond ?? 0) }
    }
    private var totalSpend: Double {
        shown.reduce(0) { $0 + (viewModel.series[$1]?.last?.dollarsPerMinute ?? 0) }
    }
    private var rateLabel: String {
        totalRate >= 1000 ? String(format: "%.1fk tok/s", totalRate / 1000)
                          : "\(Int(totalRate.rounded())) tok/s"
    }
    private var spendLabel: String { String(format: "$%.2f/min", totalSpend) }

    // MARK: Charts

    private var rateChart: some View {
        Chart {
            ForEach(shown, id: \.self) { provider in
                ForEach(viewModel.series[provider] ?? []) { sample in
                    LineMark(
                        x: .value("Time", sample.date),
                        y: .value("Tokens/s", sample.tokensPerSecond),
                        series: .value("Provider", provider.displayName)
                    )
                    .foregroundStyle(by: .value("Provider", provider.displayName))
                    .lineStyle(StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
                    .interpolationMethod(.monotone)
                }
            }
        }
        .chartForegroundStyleScale(domain: shown.map(\.displayName), range: shown.map(\.brandColor))
        .chartLegend(.hidden) // the toggles serve as the legend
        .chartXScale(domain: xDomain)
        .chartXAxis { timeAxis(desiredCount: 4) }
        .chartYAxis { tokenAxis(desiredCount: nil) }
        .chartCard()
    }

    private var cumulativeChart: some View {
        Chart {
            ForEach(shown, id: \.self) { provider in
                ForEach(viewModel.series[provider] ?? []) { sample in
                    LineMark(
                        x: .value("Time", sample.date),
                        y: .value("Tokens", sample.tokens),
                        series: .value("Provider", provider.displayName)
                    )
                    .foregroundStyle(by: .value("Provider", provider.displayName))
                    .lineStyle(StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
                    .interpolationMethod(.monotone)
                }
            }
        }
        .chartForegroundStyleScale(domain: shown.map(\.displayName), range: shown.map(\.brandColor))
        .chartLegend(.hidden)
        .chartXScale(domain: xDomain)
        .chartXAxis { timeAxis(desiredCount: 4) }
        .chartYAxis { tokenAxis(desiredCount: 3) }
        .chartCard()
    }

    private func tokenAxis(desiredCount: Int?) -> some AxisContent {
        AxisMarks(position: .leading, values: desiredCount.map { .automatic(desiredCount: $0) } ?? .automatic) { value in
            AxisGridLine().foregroundStyle(Color.primary.opacity(0.06))
            AxisValueLabel {
                if let v = value.as(Double.self) {
                    Text(tokenAxisLabel(v)).font(.caption2).foregroundStyle(.tertiary)
                }
            }
        }
    }

    /// Time (x) axis with labels that adapt to the selected range — the "legend
    /// for time" across the bottom of each chart.
    private func timeAxis(desiredCount: Int) -> some AxisContent {
        AxisMarks(values: .automatic(desiredCount: desiredCount)) { value in
            AxisGridLine().foregroundStyle(Color.primary.opacity(0.06))
            AxisValueLabel {
                if let date = value.as(Date.self) {
                    Text(date, format: xAxisFormat)
                        .font(.caption2)
                        .foregroundStyle(.tertiary)
                }
            }
        }
    }

    /// Label granularity per range: clock time for intraday windows, calendar
    /// date for the multi-day window.
    private var xAxisFormat: Date.FormatStyle {
        switch viewModel.range {
        case .m5, .h1, .h12, .d1: return .dateTime.hour().minute()
        case .d7: return .dateTime.month(.abbreviated).day()
        }
    }

    /// Span the union of all shown series so lines slide left together.
    private var xDomain: ClosedRange<Date> {
        let dates = shown.flatMap { viewModel.series[$0] ?? [] }.map(\.date)
        guard let first = dates.min(), let last = dates.max(), first < last else {
            let now = Date()
            return now.addingTimeInterval(-1)...now
        }
        return first...last
    }

    private func tokenAxisLabel(_ value: Double) -> String {
        if value >= 1_000_000 { return String(format: "%.1fM", value / 1_000_000) }
        if value >= 1_000 { return String(format: "%.0fk", value / 1_000) }
        return "\(Int(value))"
    }

    // MARK: Provider toggles (also the legend)

    private var providerToggles: some View {
        HStack(spacing: 8) {
            ForEach(ProviderName.allCases) { provider in
                let on = viewModel.isEnabled(provider)
                Button { viewModel.toggle(provider) } label: {
                    HStack(spacing: 5) {
                        Circle().fill(provider.brandColor).frame(width: 8, height: 8)
                        Text(provider.displayName).font(.caption)
                    }
                    .padding(.horizontal, 9)
                    .padding(.vertical, 4)
                    .background(
                        Capsule().fill(on ? provider.brandColor.opacity(0.16) : Color.primary.opacity(0.05))
                    )
                    .opacity(on ? 1 : 0.45)
                    .contentShape(Capsule())
                }
                .buttonStyle(.plain)
                .help("\(on ? "Hide" : "Show") \(provider.displayName)")
            }
            Spacer()
        }
    }

    // MARK: Empty / warming states

    private var warming: some View {
        HStack(spacing: 8) {
            ProgressView().controlSize(.small)
            Text("Watching for live burn…")
                .font(.callout)
                .foregroundStyle(.secondary)
        }
        .frame(maxWidth: .infinity, minHeight: 220, alignment: .center)
    }

    private var hint: some View {
        VStack(spacing: 8) {
            Image(systemName: "flame")
                .font(.title)
                .foregroundStyle(.tertiary)
            Text("Live burn needs the bundled burn helper.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity, minHeight: 220, alignment: .center)
        .padding(.horizontal, 12)
    }
}

private extension View {
    /// Shared card chrome matching `BurndownChartView`'s framing.
    func chartCard() -> some View {
        self
            .padding(12)
            .background(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .fill(Color.primary.opacity(0.04))
            )
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .stroke(Color.primary.opacity(0.06), lineWidth: 1)
            )
    }
}
