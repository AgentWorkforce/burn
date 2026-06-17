import SwiftUI
import Charts

/// Renders one limit window as a burndown card: a dashed ideal line, the real
/// usage curve, the shaded gap between them, and a summary of pace.
struct BurndownChartView: View {
    let title: String
    let data: BurndownData

    private var accent: Color { data.isOverPace ? .red : .green }
    private let lineColor = Color.blue

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            headerRow
            chart.frame(height: 150)
            axisRow
        }
        .padding(14)
        .background(
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .fill(Color.primary.opacity(0.04))
        )
        .overlay(
            RoundedRectangle(cornerRadius: 14, style: .continuous)
                .stroke(Color.primary.opacity(0.06), lineWidth: 1)
        )
    }

    // MARK: Header

    private var headerRow: some View {
        HStack(alignment: .firstTextBaseline) {
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.title3.weight(.bold))
                Text(resetsInText)
                    .font(.caption2)
                    .foregroundStyle(.tertiary)
            }
            Spacer()
            VStack(alignment: .trailing, spacing: 4) {
                HStack(spacing: 4) {
                    Text("\(Int(data.actualRemaining.rounded()))%")
                        .font(.headline.weight(.bold))
                        .foregroundStyle(lineColor)
                    Text("left")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                Text("\(abs(Int(data.paceDelta.rounded())))% \(data.isOverPace ? "over pace" : "under pace")")
                    .font(.caption.weight(.semibold))
                    .foregroundStyle(accent)
                    .padding(.horizontal, 7)
                    .padding(.vertical, 2)
                    .background(Capsule().fill(accent.opacity(0.14)))
            }
        }
    }

    private var resetsInText: String {
        let seconds = max(0, data.windowEnd.timeIntervalSince(data.now))
        let hours = Int(seconds) / 3600
        let minutes = (Int(seconds) % 3600) / 60
        if hours >= 24 {
            return "resets in \(hours / 24)d \(hours % 24)h"
        } else if hours > 0 {
            return "resets in \(hours)h \(minutes)m"
        } else {
            return "resets in \(minutes)m"
        }
    }

    // MARK: Chart

    private var chart: some View {
        Chart {
            // Shaded gap between the actual and ideal curves.
            ForEach(data.actualPoints) { point in
                AreaMark(
                    x: .value("Time", point.date),
                    yStart: .value("Ideal", point.ideal),
                    yEnd: .value("Actual", point.actual)
                )
                .foregroundStyle(
                    .linearGradient(
                        colors: [accent.opacity(0.28), accent.opacity(0.06)],
                        startPoint: .top,
                        endPoint: .bottom
                    )
                )
                .interpolationMethod(.linear)
            }

            // Ideal burndown (dashed, full window).
            ForEach(data.idealLine) { point in
                LineMark(
                    x: .value("Time", point.date),
                    y: .value("Remaining", point.value),
                    series: .value("Series", "ideal")
                )
                .foregroundStyle(lineColor.opacity(0.45))
                .lineStyle(StrokeStyle(lineWidth: 1.5, dash: [5, 4]))
                .interpolationMethod(.linear)
            }

            // Actual usage curve.
            ForEach(data.actualPoints) { point in
                LineMark(
                    x: .value("Time", point.date),
                    y: .value("Remaining", point.actual),
                    series: .value("Series", "actual")
                )
                .foregroundStyle(lineColor)
                .lineStyle(StrokeStyle(lineWidth: 2.5, lineCap: .round, lineJoin: .round))
                .interpolationMethod(.linear)
            }

            // Vertical connector at "now" showing the pace gap.
            RuleMark(
                x: .value("Now", data.now),
                yStart: .value("Remaining", min(data.actualRemaining, data.idealRemaining)),
                yEnd: .value("Remaining", max(data.actualRemaining, data.idealRemaining))
            )
            .foregroundStyle(accent)
            .lineStyle(StrokeStyle(lineWidth: 2))

            // Current position marker with a white ring.
            PointMark(
                x: .value("Now", data.now),
                y: .value("Remaining", data.actualRemaining)
            )
            .foregroundStyle(lineColor)
            .symbol {
                Circle()
                    .fill(lineColor)
                    .frame(width: 9, height: 9)
                    .overlay(Circle().stroke(.background, lineWidth: 2))
            }
        }
        .chartYScale(domain: 0.0...100.0)
        .chartXScale(domain: data.windowStart...data.windowEnd)
        .chartXAxis(.hidden)
        .chartYAxis {
            AxisMarks(position: .leading, values: [0.0, 50.0, 100.0]) { value in
                AxisGridLine().foregroundStyle(Color.primary.opacity(0.08))
                AxisValueLabel {
                    if let v = value.as(Double.self) {
                        Text("\(Int(v))%")
                            .font(.caption2)
                            .foregroundStyle(.tertiary)
                    }
                }
            }
        }
    }

    private var axisRow: some View {
        HStack {
            Text(data.windowStart, format: axisFormat)
            Spacer()
            Text(data.windowEnd, format: axisFormat)
        }
        .font(.caption2)
        .foregroundStyle(.secondary)
    }

    private var axisFormat: Date.FormatStyle {
        data.compact
            ? .dateTime.hour().minute()
            : .dateTime.month(.abbreviated).day().hour().minute()
    }
}
