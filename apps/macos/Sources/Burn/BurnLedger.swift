import Foundation
import os

/// Runs the `burn` CLI and returns its stdout. A seam so tests can inject a fake
/// that returns canned JSON without spawning a subprocess.
protocol BurnRunner: Sendable {
    /// Runs `burn` with the given arguments, returning stdout on success or nil
    /// on failure/timeout/missing binary.
    func run(_ args: [String]) async -> String?

    /// URL of the bundled native `burn` helper, when this runner is backed by a
    /// real app bundle. The long-lived ingest watch needs a directly spawnable
    /// binary (a login-shell child can't be cleanly managed), so it only starts
    /// when this is non-nil. Defaults to `nil` for fakes / PATH-only setups.
    func bundledBinaryURL() async -> URL?
}

extension BurnRunner {
    func bundledBinaryURL() async -> URL? { nil }
}

/// Production `BurnRunner`: resolves and spawns the real `burn` binary. Prefers
/// the native helper bundled inside the app (so spend works with no separate
/// install), and falls back to a `burn` on `PATH` for dev builds run via
/// `swift run`. An actor because it caches the resolved `Tool`.
actor SystemBurnRunner: BurnRunner {
    private enum Tool {
        case unknown
        case bundled(URL) // self-contained native binary in the app bundle
        case path         // a `burn` on PATH (resolved via a login shell)
        case missing
    }
    private var tool: Tool = .unknown

    /// When set, resolution is skipped and this URL is executed directly (no
    /// login shell), exactly like the bundled case. Lets tests drive the real
    /// Process/Pipe/timeout machinery against a fake `burn` script. `nil`
    /// preserves the normal bundled→PATH→missing resolution.
    private let explicitBinaryURL: URL?
    /// The `capture` timeout (see `capture`). Injectable so tests can exercise
    /// the timeout/reap path without waiting the production 30s.
    private let captureTimeout: TimeInterval

    /// Serial queue the blocking subprocess work runs on. `capture` awaits a
    /// continuation resumed from here instead of blocking inside the actor, so
    /// the cooperative-pool thread is released for the (up to `captureTimeout`)
    /// duration of the child's run. Because the queue is serial, only one
    /// `burn` subprocess is ever alive at a time — the same invariant the
    /// blocking actor used to provide — even though the actor is reentrant at
    /// the await.
    private let execQueue = DispatchQueue(label: "com.agentworkforce.burn.subprocess", qos: .utility)

    /// - Parameters:
    ///   - explicitBinaryURL: a `burn`-compatible executable to run directly,
    ///     bypassing resolution (default `nil` → normal resolution).
    ///   - timeout: per-invocation capture timeout in seconds (default `30`).
    init(explicitBinaryURL: URL? = nil, timeout: TimeInterval = 30) {
        self.explicitBinaryURL = explicitBinaryURL
        self.captureTimeout = timeout
    }

    func run(_ args: [String]) async -> String? {
        let label = args.first ?? "burn"
        switch await resolveTool() {
        case .bundled(let url):
            // Self-contained Rust binary — exec directly, no shell needed.
            return await capture(label: label) { $0.executableURL = url; $0.arguments = args }
        case .path:
            // Run through a login shell so nvm/Homebrew PATH (and the `node` the
            // npm `burn` shim needs) resolve even when launched from Finder.
            let command = "burn " + args.map(shellQuote).joined(separator: " ")
            return await loginShell(command, label: label)
        case .missing, .unknown:
            return nil
        }
    }

    func bundledBinaryURL() async -> URL? {
        if case .bundled(let url) = await resolveTool() { return url }
        return nil
    }

    private func resolveTool() async -> Tool {
        // Explicit binary short-circuits resolution: run it directly like a
        // bundled helper. Preserves normal resolution when unset.
        if let explicit = explicitBinaryURL {
            return .bundled(explicit)
        }
        if case .unknown = tool {
            if let url = Bundle.main.url(forAuxiliaryExecutable: "burn"),
               FileManager.default.isExecutableFile(atPath: url.path) {
                tool = .bundled(url)
            } else {
                // The actor is reentrant across this await, so two concurrent
                // first calls may both run the PATH probe. That's benign: both
                // converge on the same value and the probe is idempotent.
                let probe = (await loginShell("command -v burn"))?
                    .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
                tool = probe.isEmpty ? .missing : .path
            }
        }
        return tool
    }

    private func loginShell(_ command: String, label: String = "shell") async -> String? {
        await capture(label: label) {
            $0.executableURL = URL(fileURLWithPath: "/bin/zsh")
            $0.arguments = ["-lc", command]
        }
    }

    /// Runs a configured process on the serial exec queue and returns stdout,
    /// or `nil` on failure / nonzero exit / timeout. The timeout
    /// (`captureTimeout`) stops a hung `burn` from wedging the queue and piling
    /// follow-up spend requests behind it. Awaiting the queue keeps the
    /// cooperative pool free while the child runs. `label` names the subprocess
    /// in the Instruments signpost interval, which spans queue wait + run.
    private func capture(label: String, _ configure: @escaping @Sendable (Process) -> Void) async -> String? {
        let signpostID = Signposts.subprocess.makeSignpostID()
        let interval = Signposts.subprocess.beginInterval(
            "capture", id: signpostID, "\(label, privacy: .public)"
        )
        defer { Signposts.subprocess.endInterval("capture", interval) }

        let timeout = captureTimeout
        return await withCheckedContinuation { continuation in
            execQueue.async {
                continuation.resume(returning: Self.blockingCapture(timeout: timeout, configure))
            }
        }
    }

    /// The synchronous Process/Pipe body. Pure — touches no actor state — so it
    /// runs on the exec queue, off the cooperative pool.
    ///
    /// Descriptor hygiene matters here: the parent-side `Pipe`/`FileHandle`
    /// objects are Obj-C autoreleased, and on GCD threads the pool drains
    /// lazily — without the explicit `close()`s and per-call `autoreleasepool`
    /// each spawn leaked ~2 fds long after the child exited (caught by
    /// `SubprocessSoakTests`; with a spawn every 3s on the live tab this is a
    /// real production leak, not test noise).
    private static func blockingCapture(timeout: TimeInterval, _ configure: (Process) -> Void) -> String? {
        autoreleasepool { () -> String? in
            let process = Process()
            configure(process)
            let stdout = Pipe()
            let stderr = Pipe()
            process.standardOutput = stdout
            process.standardError = stderr
            do {
                try process.run()
            } catch {
                return nil
            }
            // Read stdout to EOF (which arrives when the process exits) and reap
            // it on a background queue; bound the wait with a timeout. This
            // blocks the exec queue until the process truly finishes or is
            // killed — which, because that queue is serial, guarantees only one
            // `burn` subprocess can ever be alive at a time (no pile-up). Avoids
            // the `terminationHandler` race that could let capture() return
            // while the child kept running.
            let group = DispatchGroup()
            group.enter()
            let output = DataBox()
            DispatchQueue.global(qos: .utility).async {
                output.data = stdout.fileHandleForReading.readDataToEndOfFile()
                try? stdout.fileHandleForReading.close()
                process.waitUntilExit()
                group.leave()
            }
            // Drain stderr separately: an undrained pipe fills at ~64KB and
            // blocks the child's writes, turning a chatty failure into a timeout.
            DispatchQueue.global(qos: .utility).async {
                _ = stderr.fileHandleForReading.readDataToEndOfFile()
                try? stderr.fileHandleForReading.close()
            }
            if group.wait(timeout: .now() + timeout) == .timedOut {
                process.terminate()                       // SIGTERM…
                usleep(200_000)
                if process.isRunning {                    // …then SIGKILL if it ignores it
                    kill(process.processIdentifier, SIGKILL)
                }
                // The kill closes the child's write ends, so the background
                // readers hit EOF, return, and close their handles. Give them a
                // bounded beat (avoids closing under an in-flight legacy read),
                // then close defensively — double-closes throw and are swallowed.
                _ = group.wait(timeout: .now() + 1)
                try? stdout.fileHandleForReading.close()
                try? stderr.fileHandleForReading.close()
                return nil
            }
            guard process.terminationStatus == 0 else { return nil }
            return String(data: output.data, encoding: .utf8)
        }
    }

    /// Mutable holder the background reader fills in; capturing a `var` for
    /// mutation in concurrently-executing code is an error under Swift 6.
    private final class DataBox: @unchecked Sendable {
        var data = Data()
    }

    private func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}

/// Reads authoritative spend figures from the burn ledger. Cost is *not* stored
/// in the ledger — burn computes it from its pricing table — so we invoke the
/// `burn` binary (via a `BurnRunner`) rather than re-derive pricing here.
///
/// Returns `nil` when burn is unavailable, letting the UI hide the spend line.
actor BurnLedger {
    static let shared = BurnLedger()

    private let runner: BurnRunner

    init(runner: BurnRunner = SystemBurnRunner()) {
        self.runner = runner
    }

    /// burn's provider name for one of our providers.
    static func burnProvider(for name: ProviderName) -> String {
        switch name {
        case .claude: return "anthropic"
        case .codex: return "openai"
        }
    }

    /// Total USD spend for `provider` since `since`, or `nil` if burn is
    /// unavailable or the query fails.
    func cost(provider: String, since: Date) async -> Double? {
        let iso = ISO8601DateFormatter().string(from: since)
        let args = ["summary", "--provider", provider, "--since", iso, "--json"]
        guard let output = await runner.run(args),
              let data = output.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let totalCost = json["totalCost"] as? [String: Any],
              let total = (totalCost["total"] as? NSNumber)?.doubleValue
        else { return nil }
        return total
    }

    /// One `burn summary` reading: cumulative cost and token count since a point.
    struct Summary {
        /// Total USD cost (`totalCost.total`).
        let cost: Double
        /// Total tokens across every model row's usage fields.
        let tokens: Int
    }

    /// Cumulative cost and token totals for `provider` since `since`, or `nil`
    /// when burn is unavailable or the query fails. Cheap enough to poll on a
    /// short interval: `burn summary` only queries the ledger (it no longer runs
    /// an ingest sweep), so freshness comes from a separate `ingest --watch`.
    func summary(provider: String, since: Date) async -> Summary? {
        let iso = ISO8601DateFormatter().string(from: since)
        let args = ["summary", "--provider", provider, "--since", iso, "--json"]
        guard let output = await runner.run(args),
              let data = output.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any]
        else { return nil }

        let cost = ((json["totalCost"] as? [String: Any])?["total"] as? NSNumber)?.doubleValue ?? 0

        // Total tokens = sum of every usage field across model rows.
        var tokens = 0
        if let byModel = json["byModel"] as? [[String: Any]] {
            let fields = ["input", "output", "reasoning", "cacheRead", "cacheCreate5m", "cacheCreate1h"]
            for row in byModel {
                guard let usage = row["usage"] as? [String: Any] else { continue }
                for field in fields {
                    tokens += (usage[field] as? NSNumber)?.intValue ?? 0
                }
            }
        }
        return Summary(cost: cost, tokens: tokens)
    }

    /// One bucket of a `burn summary --bucket` time-series.
    struct TimeseriesPoint {
        let date: Date
        let tokens: Int
        let cost: Double
    }

    /// Per-bucket cost/token totals for `provider` over `[since, now]`, bucketed
    /// by `bucket` (a burn duration like "30s"/"5m"/"1h"). Returns `nil` when
    /// burn is unavailable or the query/parse fails. Runs `burn summary --bucket`
    /// (read-only), so it's cheap to refresh.
    func timeseries(provider: String, since: Date, bucket: String) async -> [TimeseriesPoint]? {
        let iso = ISO8601DateFormatter().string(from: since)
        let args = ["summary", "--provider", provider, "--since", iso, "--bucket", bucket, "--json"]
        guard let output = await runner.run(args),
              let data = output.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let buckets = json["buckets"] as? [[String: Any]]
        else { return nil }

        let parser = ISO8601DateFormatter()
        parser.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        let plain = ISO8601DateFormatter()

        return buckets.compactMap { entry -> TimeseriesPoint? in
            guard let start = entry["start"] as? String,
                  let date = parser.date(from: start) ?? plain.date(from: start)
            else { return nil }
            let tokens = (entry["totalTokens"] as? NSNumber)?.intValue ?? 0
            let cost = ((entry["totalCost"] as? [String: Any])?["total"] as? NSNumber)?.doubleValue ?? 0
            return TimeseriesPoint(date: date, tokens: tokens, cost: cost)
        }
    }

    // MARK: - Long-lived ingest watch

    /// The running `burn ingest --watch` process, if any. `burn summary` is
    /// read-only (~10ms) but a one-shot `burn ingest` sweep is multi-second on a
    /// large ledger — far too slow to run per poll. Instead this long-lived watch
    /// keeps the ledger fresh incrementally (FS-event driven, ~1s poll), so the
    /// live view's summary polls stay fast.
    private var watchProcess: Process?

    /// Starts a background `burn ingest --watch` if one isn't already running.
    /// Only runs with the bundled native helper (a login-shell child can't be
    /// cleanly managed); the live chart still polls either way.
    func startIngestWatch() async {
        guard watchProcess == nil else { return }
        guard let url = await runner.bundledBinaryURL() else { return }
        let process = Process()
        process.executableURL = url
        process.arguments = ["ingest", "--watch", "--quiet"]
        process.standardOutput = Pipe()
        process.standardError = Pipe()
        do {
            try process.run()
            watchProcess = process
        } catch {
            watchProcess = nil
        }
    }

    /// Terminates the background watch process, if running.
    func stopIngestWatch() {
        watchProcess?.terminate()
        watchProcess = nil
    }
}
