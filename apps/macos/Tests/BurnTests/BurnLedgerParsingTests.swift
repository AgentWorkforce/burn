import XCTest
@testable import BurnCore

/// A `BurnRunner` that returns canned stdout and records the args it was asked
/// to run, so `BurnLedger`'s JSON parsing can be exercised without spawning the
/// real `burn` binary.
final class FakeRunner: BurnRunner, @unchecked Sendable {
    private let output: String?
    private let lock = NSLock()
    private var recorded: [[String]] = []

    /// Every arg vector passed to `run(_:)`, in call order.
    var capturedArgs: [[String]] {
        withLock { recorded }
    }

    init(output: String?) {
        self.output = output
    }

    func run(_ args: [String]) async -> String? {
        withLock { recorded.append(args) }
        return output
    }

    /// NSLock's lock()/unlock() are `noasync`; scope them inside a synchronous
    /// helper so `run` stays callable from async contexts under Swift 6.
    private func withLock<T>(_ body: () -> T) -> T {
        lock.lock()
        defer { lock.unlock() }
        return body()
    }
}

final class BurnLedgerParsingTests: XCTestCase {
    private let epoch = Date(timeIntervalSince1970: 0)

    // MARK: - cost

    func testCostParsesTotal() async throws {
        let ledger = BurnLedger(runner: FakeRunner(output: #"{"totalCost": {"total": 1.23}}"#))
        let cost = try XCTUnwrap(await ledger.cost(provider: "anthropic", since: epoch))
        XCTAssertEqual(cost, 1.23, accuracy: 0.0001)
    }

    func testCostNilOnGarbage() async {
        let ledger = BurnLedger(runner: FakeRunner(output: "not json at all"))
        let cost = await ledger.cost(provider: "anthropic", since: epoch)
        XCTAssertNil(cost)
    }

    func testCostNilOnNilOutput() async {
        let ledger = BurnLedger(runner: FakeRunner(output: nil))
        let cost = await ledger.cost(provider: "anthropic", since: epoch)
        XCTAssertNil(cost)
    }

    func testCostSendsExpectedArgs() async {
        let runner = FakeRunner(output: #"{"totalCost": {"total": 0}}"#)
        let ledger = BurnLedger(runner: runner)
        _ = await ledger.cost(provider: "anthropic", since: epoch)
        let iso = ISO8601DateFormatter().string(from: epoch)
        XCTAssertEqual(
            runner.capturedArgs.first,
            ["summary", "--provider", "anthropic", "--since", iso, "--json"]
        )
    }

    // MARK: - summary

    func testSummarySumsTokenFieldsAcrossModelRows() async throws {
        let json = """
        {
          "totalCost": {"total": 2.5},
          "byModel": [
            {"usage": {"input": 10, "output": 20, "reasoning": 5,
                       "cacheRead": 1, "cacheCreate5m": 2, "cacheCreate1h": 3}},
            {"usage": {"input": 100, "output": 0, "reasoning": 0,
                       "cacheRead": 0, "cacheCreate5m": 0, "cacheCreate1h": 0}}
          ]
        }
        """
        let ledger = BurnLedger(runner: FakeRunner(output: json))
        let summary = try XCTUnwrap(await ledger.summary(provider: "anthropic", since: epoch))
        XCTAssertEqual(summary.cost, 2.5, accuracy: 0.0001)
        // 10+20+5+1+2+3 = 41 in the first row, 100 in the second → 141.
        XCTAssertEqual(summary.tokens, 141)
    }

    func testSummaryZeroTokensWhenNoModelRows() async throws {
        let ledger = BurnLedger(runner: FakeRunner(output: #"{"totalCost": {"total": 0.5}}"#))
        let summary = try XCTUnwrap(await ledger.summary(provider: "anthropic", since: epoch))
        XCTAssertEqual(summary.cost, 0.5, accuracy: 0.0001)
        XCTAssertEqual(summary.tokens, 0)
    }

    func testSummaryNilOnGarbage() async {
        let ledger = BurnLedger(runner: FakeRunner(output: "garbage"))
        let summary = await ledger.summary(provider: "anthropic", since: epoch)
        XCTAssertNil(summary)
    }

    // MARK: - timeseries

    func testTimeseriesParsesFractionalAndPlainTimestamps() async throws {
        let json = """
        {
          "buckets": [
            {"start": "2026-01-01T00:00:00.500Z", "totalTokens": 42, "totalCost": {"total": 0.5}},
            {"start": "2026-01-01T01:00:00Z", "totalTokens": 7, "totalCost": {"total": 0.1}}
          ]
        }
        """
        let ledger = BurnLedger(runner: FakeRunner(output: json))
        let points = try XCTUnwrap(await ledger.timeseries(provider: "anthropic", since: epoch, bucket: "1h"))
        XCTAssertEqual(points.count, 2)
        // Fractional-second timestamp parsed by the .withFractionalSeconds formatter.
        XCTAssertEqual(points[0].tokens, 42)
        XCTAssertEqual(points[0].cost, 0.5, accuracy: 0.0001)
        // Plain timestamp parsed by the fallback formatter.
        XCTAssertEqual(points[1].tokens, 7)
        XCTAssertEqual(points[1].cost, 0.1, accuracy: 0.0001)
        XCTAssertLessThan(points[0].date, points[1].date)
    }

    func testTimeseriesSkipsBucketsWithUnparseableStart() async throws {
        let json = """
        {
          "buckets": [
            {"start": "not-a-date", "totalTokens": 1, "totalCost": {"total": 0.1}},
            {"start": "2026-01-01T00:00:00Z", "totalTokens": 9, "totalCost": {"total": 0.2}}
          ]
        }
        """
        let ledger = BurnLedger(runner: FakeRunner(output: json))
        let points = try XCTUnwrap(await ledger.timeseries(provider: "openai", since: epoch, bucket: "30s"))
        XCTAssertEqual(points.count, 1)
        XCTAssertEqual(points[0].tokens, 9)
    }

    func testTimeseriesNilOnGarbage() async {
        let ledger = BurnLedger(runner: FakeRunner(output: "garbage"))
        let points = await ledger.timeseries(provider: "openai", since: epoch, bucket: "30s")
        XCTAssertNil(points)
    }

    func testTimeseriesSendsExpectedArgs() async {
        let runner = FakeRunner(output: #"{"buckets": []}"#)
        let ledger = BurnLedger(runner: runner)
        _ = await ledger.timeseries(provider: "openai", since: epoch, bucket: "30s")
        let iso = ISO8601DateFormatter().string(from: epoch)
        XCTAssertEqual(
            runner.capturedArgs.first,
            ["summary", "--provider", "openai", "--since", iso, "--bucket", "30s", "--json"]
        )
    }
}
