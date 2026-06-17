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
        // Drain stdout on a background queue so a full pipe buffer can't deadlock
        // against waitUntilExit, then bound the wait with a timeout.
        let dataQueue = DispatchQueue(label: "burn-ledger.capture")
        var output = Data()
        dataQueue.async { output = stdout.fileHandleForReading.readDataToEndOfFile() }

        let finished = DispatchSemaphore(value: 0)
        process.terminationHandler = { _ in finished.signal() }
        if finished.wait(timeout: .now() + timeout) == .timedOut {
            process.terminate()
            return nil
        }
        dataQueue.sync {} // ensure the drain finished
        guard process.terminationStatus == 0 else { return nil }
        return String(data: output, encoding: .utf8)
    }

    private func shellQuote(_ value: String) -> String {
        "'" + value.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}
