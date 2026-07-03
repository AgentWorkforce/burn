import XCTest
@testable import BurnCore

/// XCTest performance baselines for the app's hot paths. These have no stored
/// baselines, so they never fail CI on timing — they establish measurements
/// (visible in the test report), catch crashes/regressions in the measured
/// code, and give a place to attach a baseline later. Each uses the clock and
/// memory metrics.
final class PerformanceTests: XCTestCase {

    /// Building a burndown from a large sample series (the per-refresh cost that
    /// feeds the popover chart). 10,000 samples across the window.
    func testBurndownBuildPerformance() {
        let now = Date()
        let periodSeconds: Double = 5 * 3600
        let resetsAt = now.addingTimeInterval(periodSeconds / 2)
        let windowStart = resetsAt.addingTimeInterval(-periodSeconds) // == now - period/2
        let metric = UsageMetric(
            name: "5-hour",
            percentage: 42,
            resetsAt: resetsAt,
            periodSeconds: periodSeconds
        )

        let count = 10_000
        // Spread samples over [windowStart, now] so they all fall inside the
        // builder's `windowStart < date < now` filter.
        let span = now.timeIntervalSince(windowStart)
        let samples: [UsageSample] = (0..<count).map { i in
            let fraction = Double(i) / Double(count)
            return UsageSample(
                date: windowStart.addingTimeInterval(fraction * span),
                percentage: fraction * 90
            )
        }

        measure(metrics: [XCTClockMetric(), XCTMemoryMetric()]) {
            _ = BurndownBuilder.build(metric: metric, samples: samples, now: now)
        }
    }

    /// Parsing a large `burn summary --bucket` payload (JSONSerialization + the
    /// per-bucket compactMap) — the live-burn chart's per-refresh parse cost.
    /// 5,000 buckets, fed through a FakeRunner so no subprocess is spawned.
    func testTimeseriesParsePerformance() {
        var entries: [String] = []
        entries.reserveCapacity(5_000)
        for i in 0..<5_000 {
            entries.append(#"{"start":"2026-01-01T00:00:00Z","totalTokens":\#(i),"totalCost":{"total":0.01}}"#)
        }
        let json = #"{"buckets":["# + entries.joined(separator: ",") + "]}"
        let ledger = BurnLedger(runner: FakeRunner(output: json))
        let since = Date(timeIntervalSince1970: 0)

        measure(metrics: [XCTClockMetric(), XCTMemoryMetric()]) {
            let done = expectation(description: "parse")
            Task {
                _ = await ledger.timeseries(provider: "anthropic", since: since, bucket: "1h")
                done.fulfill()
            }
            wait(for: [done], timeout: 30)
        }
    }

    /// `UsageHistoryStore.record` against a store pre-seeded with 50 series ×
    /// 1,000 samples. record() rewrites the whole cache to disk, so this
    /// measures the known full-rewrite cost — a candidate for incremental
    /// persistence — and surfaces regressions/improvements.
    func testHistoryRecordFullRewritePerformance() throws {
        let seedFile = FileManager.default.temporaryDirectory
            .appendingPathComponent("burn-history-perf-\(UUID().uuidString).json")
        defer { try? FileManager.default.removeItem(at: seedFile) }

        // Future reset so the store's stale-window pruning keeps every series.
        let reset = Int(Date().addingTimeInterval(3_600).timeIntervalSince1970)
        let base = Date().addingTimeInterval(-3_000)
        var seed: [String: [UsageSample]] = [:]
        for series in 0..<50 {
            // Key shape mirrors UsageHistoryStore.key: provider|name|resetEpoch.
            let key = "codex|window\(series)|\(reset)"
            seed[key] = (0..<1_000).map { i in
                UsageSample(date: base.addingTimeInterval(Double(i)), percentage: Double(i % 100))
            }
        }
        try JSONEncoder().encode(seed).write(to: seedFile)

        let store = UsageHistoryStore(fileURL: seedFile)
        // Matches the "codex|window0|<reset>" series so record() appends to it.
        let metric = UsageMetric(
            name: "window0",
            percentage: 55,
            resetsAt: Date(timeIntervalSince1970: Double(reset)),
            periodSeconds: 18_000
        )

        measure(metrics: [XCTClockMetric(), XCTMemoryMetric()]) {
            _ = store.record(provider: .codex, metric: metric, at: Date())
        }
    }
}
