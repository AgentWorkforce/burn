import SwiftUI

/// One bucket of the burn series for a provider: the per-bucket burn rate plus
/// the running cumulative totals across the selected range.
struct LiveBurnSample: Identifiable {
    let id = UUID()
    let date: Date
    /// Cumulative cost (USD) across the range up to and including this bucket.
    let cost: Double
    /// Cumulative token count across the range up to and including this bucket.
    let tokens: Int
    /// Tokens burned per second within this bucket.
    let tokensPerSecond: Double
    /// USD burned per minute within this bucket.
    let dollarsPerMinute: Double
}

/// The time window the live chart covers, with its burn-bucket size and refresh
/// cadence. `bucket` uses burn's `--bucket` grammar where `m` = minutes.
enum LiveRange: String, CaseIterable, Identifiable {
    case m5, h1, h12, d1, d7

    var id: String { rawValue }

    var label: String {
        switch self {
        case .m5: return "5m"
        case .h1: return "1h"
        case .h12: return "12h"
        case .d1: return "1d"
        case .d7: return "7d"
        }
    }

    /// How far back the window reaches.
    var sinceSeconds: TimeInterval {
        switch self {
        case .m5: return 300
        case .h1: return 3_600
        case .h12: return 43_200
        case .d1: return 86_400
        case .d7: return 604_800
        }
    }

    /// `--bucket` argument (burn grammar; `m` = minutes).
    var bucketArg: String {
        switch self {
        case .m5: return "30s"
        case .h1: return "5m"
        case .h12: return "1h"
        case .d1: return "2h"
        case .d7: return "12h"
        }
    }

    /// Bucket width in seconds — the denominator for the per-bucket rate.
    var bucketSeconds: Double {
        switch self {
        case .m5: return 30
        case .h1: return 300
        case .h12: return 3_600
        case .d1: return 7_200
        case .d7: return 43_200
        }
    }

    /// How often to re-query (longer ranges change slowly).
    var refreshInterval: TimeInterval {
        switch self {
        case .m5: return 3
        case .h1: return 15
        case .h12: return 60
        case .d1: return 120
        case .d7: return 300
        }
    }
}

/// Drives the live "Burn rate" tab. For the selected `range`, queries
/// `burn summary --bucket` per provider on a cadence and keeps a per-provider
/// series, so the view can overlay one color-coded line per provider with
/// show/hide toggles. Freshness comes from a background `burn ingest --watch`
/// (summary is read-only); when burn is missing the series stay empty and the
/// view hints.
@MainActor
final class LiveBurnViewModel: ObservableObject {
    /// Per-provider bucketed series (oldest first) for the current range.
    @Published private(set) var series: [ProviderName: [LiveBurnSample]] = [:]
    /// Providers whose line is currently shown (the toggles).
    @Published private(set) var enabled: Set<ProviderName> = Set(ProviderName.allCases)
    /// The selected time range.
    @Published private(set) var range: LiveRange = .m5
    /// True once we've confirmed burn can't be queried at all.
    @Published private(set) var unavailable = false

    private let providers = ProviderName.allCases
    private var timer: Timer?
    private var refreshing = false
    /// Set when a refresh is requested while one is in flight, so the running
    /// one does another pass (for the latest range) instead of being dropped.
    private var refreshAgain = false

    /// Begins the refresh loop and the background ingest watch. Idempotent.
    func start() {
        guard timer == nil else { return }
        Task { await BurnLedger.shared.startIngestWatch() }
        Task { await refresh() }
        scheduleTimer()
    }

    /// Stops the loop and the watch.
    func stop() {
        timer?.invalidate()
        timer = nil
        Task { await BurnLedger.shared.stopIngestWatch() }
    }

    func isEnabled(_ provider: ProviderName) -> Bool { enabled.contains(provider) }

    /// Show/hide a provider's line. Never lets the user hide the last one.
    func toggle(_ provider: ProviderName) {
        if enabled.contains(provider) {
            guard enabled.count > 1 else { return }
            enabled.remove(provider)
        } else {
            enabled.insert(provider)
        }
    }

    /// Switches the chart's time range, re-querying immediately and retiming the
    /// refresh loop to the new range's cadence.
    func setRange(_ newRange: LiveRange) {
        guard newRange != range else { return }
        range = newRange
        series = [:]
        if timer != nil { scheduleTimer() } // only retime when running
        Task { await refresh() }
    }

    private func scheduleTimer() {
        timer?.invalidate()
        timer = Timer.scheduledTimer(withTimeInterval: range.refreshInterval, repeats: true) { [weak self] _ in
            Task { await self?.refresh() }
        }
    }

    private func refresh() async {
        // Coalesce: if a refresh is already running, ask it to do one more pass
        // (for whatever the latest range is) instead of dropping this request.
        // Otherwise a fast range switch clears the series and then has its
        // refresh skipped, leaving the warming spinner until the next timer tick
        // (up to 5 min on the 7d range).
        if refreshing {
            refreshAgain = true
            return
        }
        refreshing = true
        defer { refreshing = false }

        repeat {
            refreshAgain = false
            let range = self.range
            let since = Date().addingTimeInterval(-range.sinceSeconds)
            var next: [ProviderName: [LiveBurnSample]] = [:]
            var gotAny = false

            for provider in providers {
                let burnProvider = BurnLedger.burnProvider(for: provider)
                guard let points = await BurnLedger.shared.timeseries(
                    provider: burnProvider, since: since, bucket: range.bucketArg
                ) else { continue }
                gotAny = true

                var cumulativeTokens = 0
                var cumulativeCost = 0.0
                next[provider] = points.map { point in
                    cumulativeTokens += point.tokens
                    cumulativeCost += point.cost
                    return LiveBurnSample(
                        date: point.date,
                        cost: cumulativeCost,
                        tokens: cumulativeTokens,
                        tokensPerSecond: Double(point.tokens) / range.bucketSeconds,
                        dollarsPerMinute: point.cost / range.bucketSeconds * 60
                    )
                }
            }

            // Only publish if the range still matches what we queried. A switch
            // during the query sets refreshAgain, so the loop reruns for the new
            // range rather than showing stale data or getting stuck empty.
            if range == self.range {
                if gotAny {
                    series = next
                    unavailable = false
                } else {
                    unavailable = true // burn not installed / query failing
                }
            }
        } while refreshAgain
    }
}
