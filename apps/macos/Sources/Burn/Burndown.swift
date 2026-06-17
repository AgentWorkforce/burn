import Foundation

/// A point on the actual-usage curve, paired with the ideal value at that time
/// so the chart can shade the gap between them.
struct AreaPoint: Identifiable {
    let id = UUID()
    let date: Date
    let actual: Double
    let ideal: Double
}

/// A point on a simple two-point line (the ideal burndown).
struct LinePoint: Identifiable {
    let id = UUID()
    let date: Date
    let value: Double
}

/// Everything the chart needs to render one limit window as a burndown.
///
/// Values are expressed as *remaining* quota (100% at the start of the window,
/// 0% when exhausted), matching the visual in the menu bar popover.
struct BurndownData {
    let actualPoints: [AreaPoint]   // window start ... now
    let idealLine: [LinePoint]      // window start ... window end
    let windowStart: Date
    let windowEnd: Date
    let now: Date
    let actualRemaining: Double     // at now
    let idealRemaining: Double      // at now
    /// True when the window is short enough to label the axis with times only.
    let compact: Bool

    /// Positive when you have less remaining than the ideal pace (burning too
    /// fast → "over pace"); negative when you have headroom ("under pace").
    var paceDelta: Double { idealRemaining - actualRemaining }
    var isOverPace: Bool { paceDelta > 0 }
}

enum BurndownBuilder {
    /// Builds chart data for a metric, or `nil` if the window has no reset time
    /// (in which case the caller should fall back to a simple bar).
    static func build(metric: UsageMetric, samples: [UsageSample], now: Date = Date()) -> BurndownData? {
        guard let resetsAt = metric.resetsAt, metric.periodSeconds > 0 else { return nil }

        let windowEnd = resetsAt
        let windowStart = resetsAt.addingTimeInterval(-metric.periodSeconds)
        let total = windowEnd.timeIntervalSince(windowStart)
        guard total > 0 else { return nil }

        func idealRemaining(at date: Date) -> Double {
            let fraction = max(0, min(1, windowEnd.timeIntervalSince(date) / total))
            return fraction * 100
        }

        // Actual remaining curve: starts full at the window start, walks through
        // recorded samples, and ends at the current value.
        var series: [(date: Date, remaining: Double)] = [(windowStart, 100)]
        for sample in samples where sample.date > windowStart && sample.date < now {
            series.append((sample.date, max(0, 100 - sample.percentage)))
        }
        series.append((now, max(0, 100 - metric.percentage)))
        series.sort { $0.date < $1.date }

        let actualPoints = series.map {
            AreaPoint(date: $0.date, actual: $0.remaining, ideal: idealRemaining(at: $0.date))
        }

        let idealLine = [
            LinePoint(date: windowStart, value: 100),
            LinePoint(date: windowEnd, value: 0),
        ]

        return BurndownData(
            actualPoints: actualPoints,
            idealLine: idealLine,
            windowStart: windowStart,
            windowEnd: windowEnd,
            now: now,
            actualRemaining: max(0, 100 - metric.percentage),
            idealRemaining: idealRemaining(at: now),
            compact: metric.periodSeconds <= 24 * 3600
        )
    }
}
