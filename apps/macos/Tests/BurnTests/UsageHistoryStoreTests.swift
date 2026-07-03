import XCTest
@testable import BurnCore

/// Pins the memory-growth and persistence behavior of `UsageHistoryStore`.
///
/// Every test points the store at a unique temp file via `init(fileURL:)` and
/// drives it with explicit dates, so no wall-clock time or sleeps are needed
/// (the one debounce assertion checks "not yet written" and then flushes).
final class UsageHistoryStoreTests: XCTestCase {
    private var fileURL: URL!

    override func setUp() {
        super.setUp()
        fileURL = FileManager.default.temporaryDirectory
            .appendingPathComponent(UUID().uuidString + ".json")
    }

    override func tearDown() {
        if let fileURL { try? FileManager.default.removeItem(at: fileURL) }
        fileURL = nil
        super.tearDown()
    }

    // MARK: - Helpers

    private func metric(_ name: String, _ percentage: Double, resetsAt: Date?) -> UsageMetric {
        UsageMetric(name: name, percentage: percentage, resetsAt: resetsAt, periodSeconds: 0)
    }

    private func loadDict() throws -> [String: [UsageSample]] {
        let data = try Data(contentsOf: fileURL)
        return try JSONDecoder().decode([String: [UsageSample]].self, from: data)
    }

    private func timestampedKey(_ provider: ProviderName, _ name: String, reset: Date) -> String {
        "\(provider.rawValue)|\(name)|\(Int(reset.timeIntervalSince1970))"
    }

    private func nilKey(_ provider: ProviderName, _ name: String) -> String {
        "\(provider.rawValue)|\(name)|none"
    }

    /// A fixed base instant with an integer `timeIntervalSince1970` so keys built
    /// in the test match the store's `Int(...)` truncation exactly.
    private let base = Date(timeIntervalSince1970: 1_700_000_000)

    // MARK: - 1. Nil-reset series are pruned by staleness

    func testNilResetSeriesPrunedByStaleness() throws {
        let store = UsageHistoryStore(fileURL: fileURL)

        // Stale nil-reset series: newest sample at `base`.
        store.record(provider: .claude, metric: metric("StaleWeekly", 10, resetsAt: nil), at: base)

        // Recording a fresh nil-reset metric 25h later drives pruneStaleWindows.
        let later = base.addingTimeInterval(25 * 3600)
        store.record(provider: .claude, metric: metric("RecentWeekly", 20, resetsAt: nil), at: later)

        store.flushForTesting()
        let dict = try loadDict()

        XCTAssertNil(dict[nilKey(.claude, "StaleWeekly")],
                     "nil-reset series older than 24h should be pruned")
        XCTAssertNotNil(dict[nilKey(.claude, "RecentWeekly")],
                        "nil-reset series with a recent sample should survive")
    }

    // MARK: - 2. Timestamped windows pruned once reset > 1h ago (existing behavior)

    func testTimestampedWindowPruning() throws {
        let store = UsageHistoryStore(fileURL: fileURL)
        let reference = base

        let staleReset = reference.addingTimeInterval(-2 * 3600)   // reset 2h ago -> drop
        let freshReset = reference.addingTimeInterval(-30 * 60)    // reset 30m ago -> keep
        let futureReset = reference.addingTimeInterval(24 * 3600)  // driver, never stale

        let recordAt = reference.addingTimeInterval(-3 * 3600)
        store.record(provider: .claude, metric: metric("Stale", 10, resetsAt: staleReset), at: recordAt)
        store.record(provider: .claude, metric: metric("Fresh", 20, resetsAt: freshReset), at: recordAt)

        // Driver record at `reference` triggers the prune pass.
        store.record(provider: .claude, metric: metric("Driver", 30, resetsAt: futureReset), at: reference)

        store.flushForTesting()
        let dict = try loadDict()

        XCTAssertNil(dict[timestampedKey(.claude, "Stale", reset: staleReset)],
                     "window whose reset is >1h old should be dropped")
        XCTAssertNotNil(dict[timestampedKey(.claude, "Fresh", reset: freshReset)],
                        "window whose reset is 30m ago should survive")
    }

    // MARK: - 3. Per-series cap enforcement + decimation

    func testCapEnforcementDecimatesOldWhileKeepingNewest() {
        let store = UsageHistoryStore(fileURL: fileURL)
        let cap = UsageHistoryStore.maxSamplesPerSeries
        let total = cap + 500
        let futureReset = base.addingTimeInterval(10 * 365 * 24 * 3600) // never prunes

        var series: [UsageSample] = []
        for i in 0..<total {
            let at = base.addingTimeInterval(Double(i) * 60) // one minute apart, no collapse
            series = store.record(provider: .claude,
                                  metric: metric("Cap", Double(i), resetsAt: futureReset),
                                  at: at)
        }

        XCTAssertLessThanOrEqual(series.count, cap, "series must stay within the cap")

        // First-ever sample retained.
        XCTAssertEqual(series.first?.percentage, 0)
        XCTAssertEqual(series.first?.date, base)

        // Newest samples intact at full resolution / precision.
        let lastDate = base.addingTimeInterval(Double(total - 1) * 60)
        XCTAssertEqual(series.last?.percentage, Double(total - 1))
        XCTAssertEqual(series.last?.date, lastDate)

        // Dates strictly ascending.
        for i in 1..<series.count {
            XCTAssertLessThan(series[i - 1].date, series[i].date,
                              "dates must remain strictly ascending after decimation")
        }
    }

    func testOversizedLegacySeriesIsCappedOnFirstRecord() throws {
        // Seed the file with a legacy series far above the cap (a weekly window
        // sampled every 60s ≈ 10k samples). A single record() must bound it —
        // one decimation pass only trims to ~75%, so the cap requires looping.
        let cap = UsageHistoryStore.maxSamplesPerSeries
        let legacyCount = 10_000
        let futureReset = base.addingTimeInterval(10 * 365 * 24 * 3600)
        let key = timestampedKey(.claude, "Legacy", reset: futureReset)

        var legacy: [UsageSample] = []
        legacy.reserveCapacity(legacyCount)
        for i in 0..<legacyCount {
            legacy.append(UsageSample(date: base.addingTimeInterval(Double(i) * 60),
                                      percentage: Double(i)))
        }
        try JSONEncoder().encode([key: legacy]).write(to: fileURL)

        let store = UsageHistoryStore(fileURL: fileURL)
        let newDate = base.addingTimeInterval(Double(legacyCount) * 60)
        let series = store.record(provider: .claude,
                                  metric: metric("Legacy", 99, resetsAt: futureReset),
                                  at: newDate)

        XCTAssertLessThanOrEqual(series.count, cap,
                                 "one record() must bound an oversized legacy series")
        XCTAssertEqual(series.first?.date, base, "first-ever sample must be retained")
        XCTAssertEqual(series.last?.date, newDate)
        XCTAssertEqual(series.last?.percentage, 99)
        for i in 1..<series.count {
            XCTAssertLessThan(series[i - 1].date, series[i].date)
        }
    }

    // MARK: - 4. Duplicate collapse within 1s (existing behavior)

    func testDuplicateCollapse() {
        let store = UsageHistoryStore(fileURL: fileURL)
        let reset = base.addingTimeInterval(3600)

        store.record(provider: .claude, metric: metric("W", 10, resetsAt: reset), at: base)
        let series = store.record(provider: .claude,
                                  metric: metric("W", 20, resetsAt: reset),
                                  at: base.addingTimeInterval(0.5))

        XCTAssertEqual(series.count, 1, "records <1s apart should collapse into one sample")
        XCTAssertEqual(series.first?.percentage, 20, "collapsed sample should hold the newer value")
    }

    // MARK: - 5. Debounced persistence + roundtrip

    func testPersistenceIsDebouncedAndRoundtrips() throws {
        let store = UsageHistoryStore(fileURL: fileURL)
        let reset = base.addingTimeInterval(24 * 3600)

        store.record(provider: .claude, metric: metric("Week", 10, resetsAt: reset), at: base)
        store.record(provider: .claude, metric: metric("Week", 20, resetsAt: reset),
                     at: base.addingTimeInterval(60))

        // Debounced: nothing should be on disk yet.
        XCTAssertFalse(FileManager.default.fileExists(atPath: fileURL.path),
                       "record() should defer the disk write behind a debounce")

        store.flushForTesting()

        let dict = try loadDict()
        let key = timestampedKey(.claude, "Week", reset: reset)
        XCTAssertEqual(dict[key]?.count, 2)
        XCTAssertEqual(dict[key]?.last?.percentage, 20)

        // A new store on the same file loads the persisted series.
        let reopened = UsageHistoryStore(fileURL: fileURL)
        let series = reopened.record(provider: .claude, metric: metric("Week", 30, resetsAt: reset),
                                     at: base.addingTimeInterval(120))
        XCTAssertEqual(series.count, 3, "reopened store should have loaded the persisted samples")
        XCTAssertEqual(series.first?.percentage, 10)
        XCTAssertEqual(series.last?.percentage, 30)
    }

    // MARK: - 6. Loads an on-disk dict written in the current shape

    func testLoadsPersistedDictShape() throws {
        let reset = base.addingTimeInterval(3600)
        let key = timestampedKey(.claude, "Legacy", reset: reset)
        let fixture: [String: [UsageSample]] = [
            key: [UsageSample(date: base, percentage: 42)]
        ]
        let data = try JSONEncoder().encode(fixture)
        try data.write(to: fileURL)

        let store = UsageHistoryStore(fileURL: fileURL)
        let series = store.record(provider: .claude, metric: metric("Legacy", 55, resetsAt: reset),
                                  at: base.addingTimeInterval(60))

        XCTAssertEqual(series.count, 2, "store should load the on-disk sample then append")
        XCTAssertEqual(series.first?.percentage, 42, "the persisted sample should be preserved")
        XCTAssertEqual(series.last?.percentage, 55)
    }
}
