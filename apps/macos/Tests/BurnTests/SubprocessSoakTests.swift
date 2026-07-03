import XCTest
import Darwin
@testable import BurnCore

/// Soak tests that drive the *real* `SystemBurnRunner` Process/Pipe/timeout
/// machinery (via `explicitBinaryURL`, which execs a fake `burn` script
/// directly) and assert the process doesn't leak file descriptors or memory
/// across many spawns. These guard the subprocess plumbing behind the live
/// spend/burn-rate polling, which runs indefinitely while the app is open.
///
/// Thresholds are deliberately loose: the assertions catch *unbounded growth*
/// (an fd left open per spawn, a retained Pipe/Data per call), not small,
/// bounded allocator noise.
final class SubprocessSoakTests: XCTestCase {

    // MARK: - Measurement helpers

    /// Count of open file descriptors for this process, via `/dev/fd`.
    private func openFDCount() -> Int {
        (try? FileManager.default.contentsOfDirectory(atPath: "/dev/fd").count) ?? 0
    }

    /// This process's physical memory footprint in bytes (`phys_footprint` from
    /// `TASK_VM_INFO`) — the same figure Xcode's memory gauge reports.
    private func physFootprint() -> UInt64 {
        var info = task_vm_info_data_t()
        var count = mach_msg_type_number_t(
            MemoryLayout<task_vm_info_data_t>.size / MemoryLayout<natural_t>.size
        )
        let kr = withUnsafeMutablePointer(to: &info) { ptr -> kern_return_t in
            ptr.withMemoryRebound(to: integer_t.self, capacity: Int(count)) { intPtr in
                task_info(mach_task_self_, task_flavor_t(TASK_VM_INFO), intPtr, &count)
            }
        }
        return kr == KERN_SUCCESS ? UInt64(info.phys_footprint) : 0
    }

    private func footprintDeltaMB(_ baseline: UInt64) -> Double {
        Double(Int64(bitPattern: physFootprint()) - Int64(bitPattern: baseline)) / (1024 * 1024)
    }

    // MARK: - Fixtures

    /// Writes an executable shell script and returns its URL. Callers delete it.
    private func makeScript(_ body: String) throws -> URL {
        let url = FileManager.default.temporaryDirectory
            .appendingPathComponent("burn-fake-\(UUID().uuidString).sh")
        try body.write(to: url, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes(
            [.posixPermissions: 0o755], ofItemAtPath: url.path
        )
        return url
    }

    /// A fake `burn` that ignores its args and prints a small canned
    /// `summary --json` payload, then exits 0.
    private func makeEchoScript() throws -> URL {
        try makeScript(
            """
            #!/bin/sh
            echo '{"totalCost":{"total":1.5},"byModel":[],"buckets":[{"start":"2026-07-03T00:00:00Z","totalTokens":100,"totalCost":{"total":0.1}}]}'
            """
        )
    }

    /// A fake `burn` that hangs, to exercise the capture timeout/kill path.
    private func makeSleepScript() throws -> URL {
        try makeScript(
            """
            #!/bin/sh
            exec sleep 60
            """
        )
    }

    // MARK: - Tests

    /// 200 back-to-back real subprocess spawns must not grow the open-fd count
    /// or the memory footprint without bound. A leaked Pipe read-end (the
    /// classic Process/Pipe bug) would show up as steadily climbing fds.
    ///
    /// 200 trivial spawns run in well under the CI budget (tens of seconds worst
    /// case); drop to 100 if this ever proves flaky on a slow runner.
    func testRepeatedRunsDoNotLeakFDsOrMemory() async throws {
        let script = try makeEchoScript()
        defer { try? FileManager.default.removeItem(at: script) }
        let runner = SystemBurnRunner(explicitBinaryURL: script)

        // Warm up so first-call allocations (formatters, thread pool) don't count.
        for _ in 0..<10 { _ = await runner.run(["summary", "--json"]) }

        let fdBaseline = openFDCount()
        let memBaseline = physFootprint()

        for _ in 0..<200 { _ = await runner.run(["summary", "--json"]) }

        let fdDelta = openFDCount() - fdBaseline
        let memDeltaMB = footprintDeltaMB(memBaseline)
        XCTAssertLessThan(fdDelta, 10, "open fd count grew by \(fdDelta) over 200 runs — a Process/Pipe leak")
        XCTAssertLessThan(memDeltaMB, 20, "phys footprint grew by \(memDeltaMB) MB over 200 runs")
    }

    /// A hung child must be reaped by the capture timeout: `run` returns nil and
    /// the descriptors it held are reclaimed. Uses an injected 1s timeout so CI
    /// doesn't wait the production 30s.
    func testTimeoutPathReapsProcessAndFDs() async throws {
        let script = try makeSleepScript()
        defer { try? FileManager.default.removeItem(at: script) }

        let fdBaseline = openFDCount()
        let runner = SystemBurnRunner(explicitBinaryURL: script, timeout: 1)

        let output = await runner.run(["summary", "--json"])
        XCTAssertNil(output, "a timed-out run must return nil")

        // The child is SIGTERM'd (then SIGKILL'd); give the background reader a
        // beat to hit EOF and release the pipe fds.
        try await Task.sleep(for: .seconds(1))

        let fdDelta = openFDCount() - fdBaseline
        XCTAssertLessThan(fdDelta, 10, "fds not reclaimed after a timeout kill: delta \(fdDelta)")
    }

    /// 10 concurrent callers hitting the runner must all complete without a
    /// subprocess pile-up (captures are serialized on the runner's serial exec
    /// queue, so only one child is ever live). fd/memory deltas stay bounded
    /// afterward.
    func testConcurrentCallersSerializeWithoutPileup() async throws {
        let script = try makeEchoScript()
        defer { try? FileManager.default.removeItem(at: script) }
        let runner = SystemBurnRunner(explicitBinaryURL: script)

        _ = await runner.run(["summary", "--json"]) // warm

        let fdBaseline = openFDCount()
        let memBaseline = physFootprint()

        let results = await withTaskGroup(of: String?.self) { group -> [String?] in
            for _ in 0..<10 {
                group.addTask { await runner.run(["summary", "--json"]) }
            }
            var collected: [String?] = []
            for await result in group { collected.append(result) }
            return collected
        }

        XCTAssertEqual(results.count, 10)
        XCTAssertTrue(results.allSatisfy { $0 != nil }, "every concurrent run should complete with output")

        // Let any just-finished captures release their pipes before snapshotting.
        try await Task.sleep(for: .milliseconds(300))

        let fdDelta = openFDCount() - fdBaseline
        let memDeltaMB = footprintDeltaMB(memBaseline)
        XCTAssertLessThan(fdDelta, 10, "fd count grew by \(fdDelta) under concurrent load")
        XCTAssertLessThan(memDeltaMB, 20, "phys footprint grew by \(memDeltaMB) MB under concurrent load")
    }
}
