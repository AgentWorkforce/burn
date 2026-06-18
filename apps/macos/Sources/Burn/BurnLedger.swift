import Foundation

/// Reads authoritative spend figures from the burn ledger. Cost is *not* stored
/// in the ledger — burn computes it from its pricing table — so we invoke the
/// `burn` binary rather than re-derive pricing here.
///
/// Prefers the native `burn` helper bundled inside the app (so spend works with
/// no separate install), and falls back to a `burn` on `PATH` for dev builds
/// run via `swift run`. Returns `nil` when neither is available, letting the UI
/// hide the spend line.
actor BurnLedger {
    static let shared = BurnLedger()

    /// burn's provider name for one of our providers.
    static func burnProvider(for name: ProviderName) -> String {
        switch name {
        case .claude: return "anthropic"
        case .codex: return "openai"
        }
    }

    private enum Tool {
        case unknown
        case bundled(URL) // self-contained native binary in the app bundle
        case path         // a `burn` on PATH (resolved via a login shell)
        case missing
    }
    private var tool: Tool = .unknown

    /// Total USD spend for `provider` since `since`, or `nil` if burn is
    /// unavailable or the query fails.
    func cost(provider: String, since: Date) async -> Double? {
        let iso = ISO8601DateFormatter().string(from: since)
        let args = ["summary", "--provider", provider, "--since", iso, "--json"]
        guard let output = runBurn(args),
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
        guard let output = runBurn(args),
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
    func startIngestWatch() {
        guard watchProcess == nil else { return }
        guard case .bundled(let url) = resolveTool() else { return }
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

    // MARK: - Resolution & invocation

    private func resolveTool() -> Tool {
        if case .unknown = tool {
            if let url = Bundle.main.url(forAuxiliaryExecutable: "burn"),
               FileManager.default.isExecutableFile(atPath: url.path) {
                tool = .bundled(url)
            } else if !(loginShell("command -v burn")?
                .trimmingCharacters(in: .whitespacesAndNewlines) ?? "").isEmpty {
                tool = .path
            } else {
                tool = .missing
            }
        }
        return tool
    }

    private func runBurn(_ args: [String]) -> String? {
        switch resolveTool() {
        case .bundled(let url):
            // Self-contained Rust binary — exec directly, no shell needed.
            return capture { $0.executableURL = url; $0.arguments = args }
        case .path:
            // Run through a login shell so nvm/Homebrew PATH (and the `node` the
            // npm `burn` shim needs) resolve even when launched from Finder.
            let command = "burn " + args.map(shellQuote).joined(separator: " ")
            return loginShell(command)
        case .missing, .unknown:
            return nil
        }
    }

    private func loginShell(_ command: String) -> String? {
        capture {
            $0.executableURL = URL(fileURLWithPath: "/bin/zsh")
            $0.arguments = ["-lc", command]
        }
    }

    /// Runs a configured process and returns stdout, or `nil` on failure /
    /// nonzero exit / timeout. The timeout stops a hung `burn` from wedging the
    /// actor and queuing follow-up spend requests behind it.
    private func capture(_ configure: (Process) -> Void, timeout: TimeInterval = 30) -> String? {
        let process = Process()
        configure(process)
        let stdout = Pipe()
        process.standardOutput = stdout
        process.standardError = Pipe()
        do {
            try process.run()
        } catch {
            return nil
        }
        // Read stdout to EOF (which arrives when the process exits) and reap it
        // on a background queue; bound the wait with a timeout. This blocks the
        // BurnLedger actor until the process truly finishes or is killed — which,
        // because the actor serializes calls, guarantees only one `burn`
        // subprocess can ever be alive at a time (no pile-up). Avoids the
        // `terminationHandler` race that could let capture() return while the
        // child kept running.
        let group = DispatchGroup()
        group.enter()
        var output = Data()
        DispatchQueue.global(qos: .utility).async {
            output = stdout.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            group.leave()
        }
        if group.wait(timeout: .now() + timeout) == .timedOut {
            process.terminate()                       // SIGTERM…
            usleep(200_000)
            if process.isRunning {                    // …then SIGKILL if it ignores it
                kill(process.processIdentifier, SIGKILL)
            }
            return nil
        }
        guard process.terminationStatus == 0 else { return nil }
        return String(data: output, encoding: .utf8)
    }

    private func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}
