import SwiftUI

/// One polled reading of the live burn series: the cumulative totals at `date`
/// and the per-interval deltas ("burn rate") since the previous sample.
struct LiveBurnSample: Identifiable {
    let id = UUID()
    let date: Date
    /// Cumulative USD cost over the rolling window at this instant.
    let cost: Double
    /// Cumulative token count over the rolling window at this instant.
    let tokens: Int
    /// Tokens burned per second since the previous sample (0 for the first).
    let tokensPerSecond: Double
    /// USD burned per minute since the previous sample (0 for the first).
    let dollarsPerMinute: Double
}

/// Drives the live "Burn rate" tab. Polls `burn summary --json` on a short timer
/// against a rolling window, keeps a bounded in-memory ring buffer of samples,
/// and derives a moving per-interval burn rate. Spawns a long-lived
/// `burn ingest --watch` for the lifetime of the view so the polled numbers
/// actually move (`summary` only queries the ledger; it no longer freshens it).
///
/// Mirrors `UsageViewModel`'s `@MainActor` + `Task`/timer style and the
/// graceful no-op-on-failure behavior — when burn is missing the series simply
/// stays empty and the view shows a hint.
@MainActor
final class LiveBurnViewModel: ObservableObject {
    /// The rolling series of samples (oldest first), capped at `maxSamples`.
    @Published private(set) var samples: [LiveBurnSample] = []
    /// True once we've confirmed burn can't be queried, so the view can hint.
    @Published private(set) var unavailable = false

    /// How often we poll `burn summary`. Cheap now that summary doesn't ingest,
    /// so a tight cadence makes the chart feel live without hammering anything.
    private let pollInterval: TimeInterval = 1.5
    /// Rolling window passed to `--since`: cumulative totals are measured over
    /// the trailing few minutes so the line tracks recent activity, not all time.
    private let windowSeconds: TimeInterval = 5 * 60
    /// Ring-buffer cap. At 1.5s/sample this is ~3.75 minutes of history on screen.
    private let maxSamples = 150

    private var provider: ProviderName
    private var timer: Timer?
    /// Guards against overlapping polls if a `summary` call runs long.
    private var polling = false

    init(provider: ProviderName) {
        self.provider = provider
    }

    /// Begins polling and starts the background ingest watch. Idempotent.
    func start() {
        guard timer == nil else { return }
        Task { await BurnLedger.shared.startIngestWatch() }
        Task { await poll() }
        timer = Timer.scheduledTimer(withTimeInterval: pollInterval, repeats: true) { [weak self] _ in
            Task { await self?.poll() }
        }
    }

    /// Stops polling and tears down the ingest watch. Called when the live view
    /// disappears or the app closes.
    func stop() {
        timer?.invalidate()
        timer = nil
        Task { await BurnLedger.shared.stopIngestWatch() }
    }

    /// Switches the tracked provider, clearing the series so the new provider's
    /// readings start fresh.
    func select(_ provider: ProviderName) {
        guard provider != self.provider else { return }
        self.provider = provider
        samples = []
        unavailable = false
        Task { await poll() }
    }

    private func poll() async {
        guard !polling else { return }
        polling = true
        defer { polling = false }

        let burnProvider = BurnLedger.burnProvider(for: provider)
        let since = Date().addingTimeInterval(-windowSeconds)
        guard let summary = await BurnLedger.shared.summary(provider: burnProvider, since: since) else {
            // Only flip to "unavailable" before we've ever shown data, so a
            // transient query failure doesn't blank an established chart.
            if samples.isEmpty { unavailable = true }
            return
        }
        unavailable = false
        append(summary)
    }

    private func append(_ summary: BurnLedger.Summary) {
        let now = Date()
        let tokensPerSecond: Double
        let dollarsPerMinute: Double
        if let prev = samples.last {
            let dt = now.timeIntervalSince(prev.date)
            if dt > 0 {
                // Deltas can dip negative as the rolling window slides old turns
                // out from under `--since`; clamp so the rate line stays sane.
                tokensPerSecond = max(0, Double(summary.tokens - prev.tokens) / dt)
                dollarsPerMinute = max(0, (summary.cost - prev.cost) / dt * 60)
            } else {
                tokensPerSecond = 0
                dollarsPerMinute = 0
            }
        } else {
            tokensPerSecond = 0
            dollarsPerMinute = 0
        }

        samples.append(LiveBurnSample(
            date: now,
            cost: summary.cost,
            tokens: summary.tokens,
            tokensPerSecond: tokensPerSecond,
            dollarsPerMinute: dollarsPerMinute
        ))
        if samples.count > maxSamples {
            samples.removeFirst(samples.count - maxSamples)
        }
    }
}
