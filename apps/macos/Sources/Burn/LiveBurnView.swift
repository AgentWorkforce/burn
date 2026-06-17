import SwiftUI
import Charts

/// The "Burn rate" tab: a moving, streaming chart of token burn that updates in
/// real time. The headline is the per-interval burn rate (tokens/sec) — a moving
/// rate is the compelling read — with a secondary cumulative-tokens line.
///
/// Owns a `LiveBurnViewModel` whose lifecycle (poll timer + background ingest
/// watch) is bound to this view's appearance: started in `onAppear`, torn down
/// in `onDisappear`. Falls back to a hint when burn can't be queried, mirroring
/// how the rest of the app no-ops on a missing binary.
struct LiveBurnView: View {
    @ObservedObject var viewModel: LiveBurnViewModel

    private let rateColor = Color.orange
    private let cumulativeColor = Color.blue

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if viewModel.unavailable {
                hint
            } else if viewModel.samples.count < 2 {
                warming
            } else {
                headline
                rateChart.frame(height: 130)
                cumulativeChart.frame(height: 90)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        .onAppear { viewModel.start() }
        .onDisappear { viewModel.stop() }
    }

    // MARK: Headline

    private var headline: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 1) {
                Text("Burn rate")
                    .font(.title3.weight(.bold))
                Text("live · trailing 5 min")
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 1) {
                Text(rateLabel)
                    .font(.headline.weight(.bold))
                    .foregroundStyle(rateColor)
                    .monospacedDigit()
                Text(spendLabel)
                    .font(.caption2)
                    .foregroundStyle(.secondary)
                    .monospacedDigit()
            }
        }
    }

    private var latest: LiveBurnSample? { viewModel.samples.last }

    private var rateLabel: String {
        let rate = latest?.tokensPerSecond ?? 0
        if rate >= 1000 {
            return String(format: "%.1fk tok/s", rate / 1000)
        }
        return "\(Int(rate.rounded())) tok/s"
    }

    private var spendLabel: String {
        let perMin = latest?.dollarsPerMinute ?? 0
        return String(format: "$%.2f/min", perMin)
    }

    // MARK: Charts

    /// The moving burn-rate line (tokens/sec per polled interval).
    private var rateChart: some View {
        Chart(viewModel.samples) { sample in
            AreaMark(
                x: .value("Time", sample.date),
                y: .value("Tokens/s", sample.tokensPerSecond)
            )
            .foregroundStyle(
                .linearGradient(
                    colors: [rateColor.opacity(0.28), rateColor.opacity(0.02)],
                    startPoint: .top,
                    endPoint: .bottom
                )
            )
            .interpolationMethod(.monotone)

            LineMark(
                x: .value("Time", sample.date),
                y: .value("Tokens/s", sample.tokensPerSecond)
            )
            .foregroundStyle(rateColor)
            .lineStyle(StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
            .interpolationMethod(.monotone)
        }
        .chartXScale(domain: xDomain)
        .chartXAxis(.hidden)
        .chartYAxis {
            AxisMarks(position: .leading) { value in
                AxisGridLine().foregroundStyle(Color.primary.opacity(0.08))
                AxisValueLabel {
                    if let v = value.as(Double.self) {
                        Text(tokenAxisLabel(v))
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
            }
        }
        .chartCard()
    }

    /// Cumulative tokens over the rolling window — a slower-moving companion.
    private var cumulativeChart: some View {
        Chart(viewModel.samples) { sample in
            LineMark(
                x: .value("Time", sample.date),
                y: .value("Tokens", sample.tokens)
            )
            .foregroundStyle(cumulativeColor)
            .lineStyle(StrokeStyle(lineWidth: 2, lineCap: .round, lineJoin: .round))
            .interpolationMethod(.monotone)
        }
        .chartXScale(domain: xDomain)
        .chartXAxis(.hidden)
        .chartYAxis {
            AxisMarks(position: .leading, values: .automatic(desiredCount: 3)) { value in
                AxisGridLine().foregroundStyle(Color.primary.opacity(0.08))
                AxisValueLabel {
                    if let v = value.as(Double.self) {
                        Text(tokenAxisLabel(v))
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
            }
        }
        .chartCard()
    }

    /// Always span the full sample window so the line visibly slides left as new
    /// samples arrive, even when the buffer isn't full yet.
    private var xDomain: ClosedRange<Date> {
        let dates = viewModel.samples.map(\.date)
        guard let first = dates.first, let last = dates.last, first < last else {
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
