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

    /// How often we poll `burn summary` (read-only, ~10ms). Freshness comes from
    /// the background `ingest --watch`, so this can be a tight, smooth cadence.
    private let pollInterval: TimeInterval = 1.5
    /// Trailing window the burn rate is averaged over. Each poll re-queries the
    /// last `rateWindow` seconds and divides — a moving average that's robust to
    /// the window sliding and to late ingests, unlike an inter-sample delta
    /// (which dips negative as old turns age out and reads as "no usage").
    private let rateWindow: TimeInterval = 60
    /// Ring-buffer cap on samples kept on screen.
    private let maxSamples = 150

    private var provider: ProviderName
    private var timer: Timer?
    /// Guards against overlapping polls if a `summary` call runs long.
    private var polling = false
    /// Running session totals (integral of the rate) for the cumulative line.
    private var sessionTokens = 0.0
    private var sessionCost = 0.0

    init(provider: ProviderName) {
        self.provider = provider
    }

    /// Begins the poll loop and the background ingest watch. Idempotent.
    func start() {
        guard timer == nil else { return }
        Task { await BurnLedger.shared.startIngestWatch() }
        Task { await poll() }
        timer = Timer.scheduledTimer(withTimeInterval: pollInterval, repeats: true) { [weak self] _ in
            Task { await self?.poll() }
        }
    }

    /// Stops the loop and the watch. Called when the live view disappears or the
    /// app closes.
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
        sessionTokens = 0
        sessionCost = 0
        unavailable = false
        Task { await poll() }
    }

    private func poll() async {
        guard !polling else { return }
        polling = true
        defer { polling = false }

        let burnProvider = BurnLedger.burnProvider(for: provider)
        let now = Date()
        guard let window = await BurnLedger.shared.summary(
            provider: burnProvider, since: now.addingTimeInterval(-rateWindow)
        ) else {
            // Only flip to "unavailable" before we've ever shown data, so a
            // transient query failure doesn't blank an established chart.
            if samples.isEmpty { unavailable = true }
            return
        }
        unavailable = false

        // Burn rate = tokens/cost over the trailing `rateWindow`, divided by it.
        let tokensPerSecond = Double(window.tokens) / rateWindow
        let dollarsPerMinute = window.cost / rateWindow * 60

        // Integrate the rate into a monotonic session total for the second line.
        let dt = samples.last.map { now.timeIntervalSince($0.date) } ?? pollInterval
        sessionTokens += tokensPerSecond * dt
        sessionCost += dollarsPerMinute / 60 * dt

        samples.append(LiveBurnSample(
            date: now,
            cost: sessionCost,
            tokens: Int(sessionTokens),
            tokensPerSecond: tokensPerSecond,
            dollarsPerMinute: dollarsPerMinute
        ))
        if samples.count > maxSamples {
            samples.removeFirst(samples.count - maxSamples)
        }
    }
}
