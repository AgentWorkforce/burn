import SwiftUI

/// One polled reading of the live burn series for a provider: the running
/// session totals and the moving per-interval burn rate.
struct LiveBurnSample: Identifiable {
    let id = UUID()
    let date: Date
    /// Running session cost (USD) — integral of the rate.
    let cost: Double
    /// Running session token count — integral of the rate.
    let tokens: Int
    /// Moving-average tokens burned per second over the trailing rate window.
    let tokensPerSecond: Double
    /// Moving-average USD burned per minute over the trailing rate window.
    let dollarsPerMinute: Double
}

/// Drives the live "Burn rate" tab. Polls `burn summary --json` for every
/// provider on a short timer and keeps a bounded per-provider ring buffer, so
/// the view can overlay one color-coded line per provider with show/hide
/// toggles. Freshness comes from a background `burn ingest --watch` (summary is
/// read-only); when burn is missing the series stay empty and the view hints.
@MainActor
final class LiveBurnViewModel: ObservableObject {
    /// Per-provider rolling sample series (oldest first), capped at `maxSamples`.
    @Published private(set) var series: [ProviderName: [LiveBurnSample]] = [:]
    /// Providers whose line is currently shown (the toggles).
    @Published private(set) var enabled: Set<ProviderName> = Set(ProviderName.allCases)
    /// True once we've confirmed burn can't be queried at all.
    @Published private(set) var unavailable = false

    /// Poll `burn summary` (read-only, ~10ms). Freshness is handled by the watch.
    private let pollInterval: TimeInterval = 1.5
    /// Trailing window the burn rate is averaged over — robust to window slide
    /// and late ingests (unlike an inter-sample delta, which dips negative).
    private let rateWindow: TimeInterval = 60
    /// Ring-buffer cap on samples kept on screen, per provider.
    private let maxSamples = 150

    private let providers = ProviderName.allCases
    private var timer: Timer?
    private var polling = false
    /// Running session totals (integral of the rate) per provider.
    private var session: [ProviderName: (tokens: Double, cost: Double)] = [:]

    /// Begins the poll loop and the background ingest watch. Idempotent.
    func start() {
        guard timer == nil else { return }
        Task { await BurnLedger.shared.startIngestWatch() }
        Task { await poll() }
        timer = Timer.scheduledTimer(withTimeInterval: pollInterval, repeats: true) { [weak self] _ in
            Task { await self?.poll() }
        }
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

    private func poll() async {
        guard !polling else { return }
        polling = true
        defer { polling = false }

        let now = Date()
        var gotAny = false
        for provider in providers {
            let burnProvider = BurnLedger.burnProvider(for: provider)
            guard let window = await BurnLedger.shared.summary(
                provider: burnProvider, since: now.addingTimeInterval(-rateWindow)
            ) else { continue }
            gotAny = true

            let tokensPerSecond = Double(window.tokens) / rateWindow
            let dollarsPerMinute = window.cost / rateWindow * 60
            let dt = series[provider]?.last.map { now.timeIntervalSince($0.date) } ?? pollInterval
            var totals = session[provider] ?? (tokens: 0, cost: 0)
            totals.tokens += tokensPerSecond * dt
            totals.cost += dollarsPerMinute / 60 * dt
            session[provider] = totals

            var samples = series[provider] ?? []
            samples.append(LiveBurnSample(
                date: now,
                cost: totals.cost,
                tokens: Int(totals.tokens),
                tokensPerSecond: tokensPerSecond,
                dollarsPerMinute: dollarsPerMinute
            ))
            if samples.count > maxSamples {
                samples.removeFirst(samples.count - maxSamples)
            }
            series[provider] = samples
        }

        if gotAny {
            unavailable = false
        } else if series.values.allSatisfy({ $0.isEmpty }) {
            unavailable = true // burn not installed / query failing
        }
    }
}
