import Foundation

/// Supported usage providers.
enum ProviderName: String, CaseIterable, Identifiable {
    case codex
    case claude

    var id: String { rawValue }

    var displayName: String {
        switch self {
        case .claude: return "Claude"
        case .codex: return "Codex"
        }
    }

    /// Short description of where the data comes from (shown in the header).
    var sourceLabel: String {
        switch self {
        case .claude: return "Anthropic OAuth"
        case .codex: return "Codex RPC"
        }
    }
}

enum ProviderStatusType {
    case ok
    case warning
    case error
    case unavailable
    case loading
    case rateLimited
}

/// A single usage window (e.g. the 5-hour or weekly limit).
struct UsageMetric: Identifiable {
    let id = UUID()
    /// Human readable window name, e.g. "5-hour" or "Weekly".
    let name: String
    /// Percentage of the limit that has been *used* (0...100).
    let percentage: Double
    /// When this window resets, if known.
    let resetsAt: Date?
    /// Total duration of the window in seconds.
    let periodSeconds: Double
}

struct ProviderStatus {
    let provider: ProviderName
    let status: ProviderStatusType
    var plan: String?
    var metrics: [UsageMetric]
    var message: String?

    static func unavailable(_ provider: ProviderName, _ message: String) -> ProviderStatus {
        ProviderStatus(provider: provider, status: .unavailable, plan: nil, metrics: [], message: message)
    }

    static func failure(_ provider: ProviderName, _ message: String) -> ProviderStatus {
        ProviderStatus(provider: provider, status: .error, plan: nil, metrics: [], message: message)
    }

    /// The usage endpoint rate-limited the poll (HTTP 429). Transient — callers
    /// should keep showing the last known data and back off rather than surface
    /// this as a hard error.
    static func rateLimited(_ provider: ProviderName) -> ProviderStatus {
        ProviderStatus(provider: provider, status: .rateLimited, plan: nil, metrics: [],
                       message: "Usage API is rate-limiting requests. Showing the last update.")
    }
}

/// ISO-8601 parsing that tolerates fractional seconds (Anthropic) and plain
/// internet date-time strings.
///
/// Anthropic returns microsecond precision with an explicit offset, e.g.
/// `2026-06-17T05:19:59.253508+00:00`. `ISO8601DateFormatter` only reliably
/// parses up to 3 fractional digits, so we fall back to stripping the
/// fractional component entirely (sub-second precision is irrelevant for reset
/// times).
enum DateParsing {
    private static let fractional: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return f
    }()

    private static let plain: ISO8601DateFormatter = {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime]
        return f
    }()

    static func date(from string: String) -> Date? {
        if let date = fractional.date(from: string) { return date }
        if let date = plain.date(from: string) { return date }

        // Strip fractional seconds (e.g. ".253508") and retry.
        if let range = string.range(of: #"\.\d+"#, options: .regularExpression) {
            var stripped = string
            stripped.removeSubrange(range)
            if let date = plain.date(from: stripped) { return date }
        }
        return nil
    }
}
