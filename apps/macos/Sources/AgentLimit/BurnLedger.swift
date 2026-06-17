import Foundation

/// Reads authoritative spend figures from the locally-installed `burn` CLI
/// (relayburn) ledger. Cost is *not* stored in the ledger — burn computes it
/// from its pricing table — so we shell out to `burn` rather than re-derive
/// pricing here. Every method returns `nil` when burn isn't available, letting
/// the UI simply hide the spend line.
actor BurnLedger {
    static let shared = BurnLedger()

    /// burn's provider name for one of our providers.
    static func burnProvider(for name: ProviderName) -> String {
        switch name {
        case .claude: return "anthropic"
        case .codex: return "openai"
        }
    }

    private enum Availability { case unknown, missing, present }
    private var availability: Availability = .unknown

    /// Total USD spend for `provider` since `since`, or `nil` if burn is
    /// unavailable or the query fails.
    func cost(provider: String, since: Date) async -> Double? {
        guard isAvailable() else { return nil }
        let iso = ISO8601DateFormatter().string(from: since)
        let command = "burn summary --provider \(provider) --since \(iso) --json"
        guard let output = run(command),
              let data = output.data(using: .utf8),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let totalCost = json["totalCost"] as? [String: Any],
              let total = (totalCost["total"] as? NSNumber)?.doubleValue
        else { return nil }
        return total
    }

    // MARK: - Subprocess

    private func isAvailable() -> Bool {
        switch availability {
        case .present: return true
        case .missing: return false
        case .unknown:
            let found = !(run("command -v burn")?
                .trimmingCharacters(in: .whitespacesAndNewlines) ?? "").isEmpty
            availability = found ? .present : .missing
            return found
        }
    }

    /// Runs a command in a login shell — so nvm/Homebrew PATH (and the `node`
    /// the `burn` shim needs) resolve even when launched from Finder — and
    /// returns stdout, or `nil` on failure / nonzero exit.
    private func run(_ command: String) -> String? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/zsh")
        process.arguments = ["-lc", command]
        let stdout = Pipe()
        process.standardOutput = stdout
        process.standardError = Pipe()
        do {
            try process.run()
        } catch {
            return nil
        }
        let data = stdout.fileHandleForReading.readDataToEndOfFile()
        process.waitUntilExit()
        guard process.terminationStatus == 0 else { return nil }
        return String(data: data, encoding: .utf8)
    }
}
