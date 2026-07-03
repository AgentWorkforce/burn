import XCTest
import Foundation
@testable import BurnCore

/// A `UsageProvider` that counts `fetch()` calls and returns a minimal healthy
/// status, so `UsageViewModel`'s refresh loop can be driven without network.
final class CountingProvider: UsageProvider, @unchecked Sendable {
    let name: ProviderName
    private let lock = NSLock()
    private var count = 0

    /// Number of times `fetch()` has been invoked, in a thread-safe read.
    var fetchCount: Int {
        lock.lock(); defer { lock.unlock() }
        return count
    }

    init(name: ProviderName) {
        self.name = name
    }

    func fetch() async -> ProviderStatus {
        lock.lock(); count += 1; lock.unlock()
        // Empty metrics keep `refresh()` on its happy path without needing the
        // ledger, and make spend loading a no-op.
        return ProviderStatus(provider: name, status: .ok, plan: "Test", metrics: [], message: nil)
    }
}

/// Lifecycle / deallocation tests for the menu bar view models. These pin down
/// the memory-leak and background-slowness field reports: the refresh timers
/// must be torn down when a model stops or deallocates, rather than being left
/// firing forever on the main run loop.
@MainActor
final class ViewModelLifecycleTests: XCTestCase {
    /// JSON that satisfies every `BurnLedger` query the models make (cost,
    /// summary, and an empty time-series), so injected refreshes always succeed.
    private let comboJSON = #"{"totalCost": {"total": 0}, "buckets": []}"#

    override func setUp() {
        super.setUp()
        // Make `selectedProvider` deterministic (`.codex`) regardless of any
        // value a developer's defaults might carry.
        UserDefaults.standard.removeObject(forKey: "selectedProvider")
    }

    // MARK: - Helpers

    /// A throwaway history store backed by a unique temp file, so tests never
    /// touch the shared on-disk history.
    private func makeTempHistory() -> UsageHistoryStore {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("burn-lifecycle-\(UUID().uuidString).json")
        return UsageHistoryStore(fileURL: url)
    }

    /// Pumps the main run loop and the cooperative executor for `seconds` so
    /// in-flight `Task`s settle and any due timer would get a chance to fire.
    private func settle(_ seconds: TimeInterval) async {
        let deadline = Date().addingTimeInterval(seconds)
        while Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.02))
            await Task.yield()
        }
    }

    /// Pumps the run loop until `condition` holds or `timeout` elapses.
    @discardableResult
    private func wait(timeout: TimeInterval, until condition: () -> Bool) async -> Bool {
        let deadline = Date().addingTimeInterval(timeout)
        while Date() < deadline {
            if condition() { return true }
            RunLoop.main.run(until: Date().addingTimeInterval(0.02))
            await Task.yield()
        }
        return condition()
    }

    // MARK: - LiveBurnViewModel

    func testLiveBurnViewModelDeallocatesAfterStop() async {
        let fake = FakeRunner(output: comboJSON)
        weak var weakVM: LiveBurnViewModel?
        do {
            let vm = LiveBurnViewModel(ledger: BurnLedger(runner: fake))
            weakVM = vm
            vm.start()
            vm.stop()
        }
        await wait(timeout: 2) { weakVM == nil }
        XCTAssertNil(weakVM, "LiveBurnViewModel leaked after start()/stop()")
    }

    /// Even when the view forgets to call `stop()` (e.g. an NSPopover close that
    /// doesn't fire SwiftUI's `.onDisappear`), the `[weak self]` timer plus the
    /// deinit teardown must let the model deallocate.
    func testLiveBurnViewModelDeallocatesWithoutStop() async {
        let fake = FakeRunner(output: comboJSON)
        weak var weakVM: LiveBurnViewModel?
        do {
            let vm = LiveBurnViewModel(ledger: BurnLedger(runner: fake))
            weakVM = vm
            vm.start()
            // Deliberately no stop().
        }
        await wait(timeout: 2) { weakVM == nil }
        XCTAssertNil(weakVM, "LiveBurnViewModel leaked when stop() was never called")
    }

    func testLiveBurnViewModelStopsQueryingAfterStop() async {
        let fake = FakeRunner(output: comboJSON)
        let vm = LiveBurnViewModel(ledger: BurnLedger(runner: fake))
        vm.start()
        // The immediate refresh queries every provider once (2 providers).
        await wait(timeout: 3) { fake.capturedArgs.count >= 2 }
        vm.stop()
        XCTAssertFalse(vm.isRefreshTimerActive, "stop() left the refresh timer scheduled")
        let callsAtStop = fake.capturedArgs.count
        // Wait past one .m5 refresh interval (3s): a live timer would have fired.
        await settle(3.5)
        XCTAssertEqual(fake.capturedArgs.count, callsAtStop,
                       "runner kept being called after stop() — timer was not invalidated")
    }

    // MARK: - UsageViewModel

    /// With `autostart: false` the model must not fetch or schedule anything,
    /// and must deallocate immediately once released.
    func testUsageViewModelNoSideEffectsWhenAutostartFalse() async {
        let provider = CountingProvider(name: .codex)
        let fake = FakeRunner(output: comboJSON)
        weak var weakVM: UsageViewModel?
        do {
            let vm = UsageViewModel(
                providers: [.codex: provider, .claude: CountingProvider(name: .claude)],
                history: makeTempHistory(),
                ledger: BurnLedger(runner: fake),
                autostart: false
            )
            weakVM = vm
            XCTAssertFalse(vm.isRefreshTimerActive, "autostart:false must not schedule a timer")
        }
        await wait(timeout: 2) { weakVM == nil }
        XCTAssertNil(weakVM, "UsageViewModel leaked even with autostart:false")
        XCTAssertEqual(provider.fetchCount, 0, "provider was fetched despite autostart:false")
        XCTAssertEqual(fake.capturedArgs.count, 0, "ledger was queried despite autostart:false")
    }

    func testUsageViewModelDeallocatesAfterStartStop() async {
        let provider = CountingProvider(name: .codex)
        let fake = FakeRunner(output: comboJSON)
        weak var weakVM: UsageViewModel?
        do {
            let vm = UsageViewModel(
                providers: [.codex: provider, .claude: CountingProvider(name: .claude)],
                history: makeTempHistory(),
                ledger: BurnLedger(runner: fake),
                autostart: false
            )
            weakVM = vm
            vm.start()
            await settle(0.3)   // let the immediate refresh + spend load settle
            vm.stop()
        }
        await wait(timeout: 2) { weakVM == nil }
        XCTAssertNil(weakVM, "UsageViewModel leaked after start()/stop()")
    }

    func testUsageViewModelTimerStopsOnStop() async {
        let provider = CountingProvider(name: .codex)
        let vm = UsageViewModel(
            providers: [.codex: provider, .claude: CountingProvider(name: .claude)],
            history: makeTempHistory(),
            ledger: BurnLedger(runner: FakeRunner(output: comboJSON)),
            autostart: false
        )
        vm.start()
        XCTAssertTrue(vm.isRefreshTimerActive, "start() did not schedule the refresh timer")
        await wait(timeout: 2) { provider.fetchCount >= 1 }   // immediate refresh fetched once
        vm.stop()
        XCTAssertFalse(vm.isRefreshTimerActive, "stop() did not invalidate the refresh timer")
        let fetchesAtStop = provider.fetchCount
        await settle(0.5)
        XCTAssertEqual(provider.fetchCount, fetchesAtStop, "provider fetched again after stop()")
    }
}
