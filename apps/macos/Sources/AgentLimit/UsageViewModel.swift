import SwiftUI

/// This-period and previous-period spend (USD) for one usage window, from the
/// burn ledger.
struct PeriodSpend {
    let thisPeriod: Double
    let lastPeriod: Double?
}

/// Drives data loading and exposes view state to SwiftUI. Refreshes on a timer
/// so the menu bar label stays current even while the popover is closed.
@MainActor
final class UsageViewModel: ObservableObject {
    @Published private(set) var selectedProvider: ProviderName
    @Published private(set) var status: ProviderStatus?
    @Published private(set) var charts: [(metric: UsageMetric, data: BurndownData?)] = []
    @Published private(set) var lastUpdated: Date?
    @Published private(set) var isLoading = false
    /// Transient banner (e.g. rate-limit notice) shown alongside stale data.
    @Published private(set) var notice: String?
    /// Per-window spend from the burn ledger, keyed by window name (stable
    /// across refreshes, unlike the per-fetch metric id). Empty when burn isn't
    /// installed.
    @Published private(set) var spend: [String: PeriodSpend] = [:]

    let refreshInterval: TimeInterval = 60

    private var timer: Timer?
    /// While set, scheduled (non-forced) refreshes are skipped to let a 429 clear.
    private var backoffUntil: Date?
    private var consecutiveRateLimits = 0
    private let providers: [ProviderName: UsageProvider] = [
        .claude: ClaudeProvider(),
        .codex: CodexProvider(),
    ]

    init() {
        if let raw = UserDefaults.standard.string(forKey: "selectedProvider"),
           let provider = ProviderName(rawValue: raw) {
            selectedProvider = provider
        } else {
            selectedProvider = .codex
        }
        start()
    }

    private func start() {
        Task { await refresh(force: true) }
        timer = Timer.scheduledTimer(withTimeInterval: refreshInterval, repeats: true) { [weak self] _ in
            Task { await self?.refresh() }
        }
    }

    func select(_ provider: ProviderName) {
        guard provider != selectedProvider else { return }
        selectedProvider = provider
        UserDefaults.standard.set(provider.rawValue, forKey: "selectedProvider")
        status = nil
        charts = []
        notice = nil
        spend = [:]
        // New provider has its own rate-limit budget; clear any pending backoff.
        backoffUntil = nil
        consecutiveRateLimits = 0
        Task { await refresh(force: true) }
    }

    /// - Parameter force: bypass the rate-limit backoff (user-initiated refresh).
    func refresh(force: Bool = false) async {
        guard let provider = providers[selectedProvider] else { return }
        if !force, let until = backoffUntil, Date() < until { return }
        isLoading = true
        let result = await provider.fetch()
        let now = Date()

        // Ignore results for a provider the user switched away from mid-flight.
        guard result.provider == selectedProvider else {
            isLoading = false
            return
        }

        // Rate-limited: keep the last good reading on screen and back off so we
        // stop hammering the endpoint while the limit clears.
        if result.status == .rateLimited {
            consecutiveRateLimits += 1
            let delay = min(15 * 60, refreshInterval * pow(2, Double(consecutiveRateLimits)))
            backoffUntil = now.addingTimeInterval(delay)
            if status == nil || (status?.metrics.isEmpty ?? true) {
                status = result   // nothing prior to show; surface the notice itself
            } else {
                notice = result.message
            }
            isLoading = false
            return
        }

        backoffUntil = nil
        consecutiveRateLimits = 0
        notice = nil

        var built: [(metric: UsageMetric, data: BurndownData?)] = []
        if result.status == .ok || result.status == .warning {
            for metric in result.metrics {
                let samples = UsageHistoryStore.shared.record(provider: result.provider, metric: metric, at: now)
                let data = BurndownBuilder.build(metric: metric, samples: samples, now: now)
                built.append((metric, data))
            }
        }

        status = result
        charts = built
        lastUpdated = now
        isLoading = false

        // Spend comes from the burn ledger via a subprocess; load it off the
        // critical path so the usage display isn't held up.
        let metrics = result.metrics
        let resultProvider = result.provider
        Task { await loadSpend(provider: resultProvider, metrics: metrics) }
    }

    /// Loads per-window spend from the burn ledger. No-op (leaves `spend`
    /// unchanged) if burn isn't installed or a query fails. Each `burn summary`
    /// call runs an ingest pass (~5s), so this is throttled — spend changes
    /// slowly and doesn't need the 60s usage cadence.
    private var lastSpendAt: Date?
    private let spendInterval: TimeInterval = 300

    private func loadSpend(provider: ProviderName, metrics: [UsageMetric]) async {
        if !spend.isEmpty, let last = lastSpendAt, Date().timeIntervalSince(last) < spendInterval {
            return
        }
        let burnProvider = BurnLedger.burnProvider(for: provider)
        var result: [String: PeriodSpend] = [:]
        for metric in metrics {
            guard let resetsAt = metric.resetsAt, metric.periodSeconds > 0 else { continue }
            let thisStart = resetsAt.addingTimeInterval(-metric.periodSeconds)
            let lastStart = resetsAt.addingTimeInterval(-2 * metric.periodSeconds)
            guard let thisCost = await BurnLedger.shared.cost(provider: burnProvider, since: thisStart) else {
                return // burn unavailable — keep whatever we had
            }
            // "Last period" = [lastStart, thisStart): spend since lastStart minus
            // spend since thisStart (burn has no --until).
            let lastCost = await BurnLedger.shared.cost(provider: burnProvider, since: lastStart)
                .map { max(0, $0 - thisCost) }
            result[metric.name] = PeriodSpend(thisPeriod: thisCost, lastPeriod: lastCost)
        }
        guard provider == selectedProvider else { return }
        spend = result
        lastSpendAt = Date()
    }

    /// The busiest window (highest used percentage), driving the menu bar label.
    private var headlineMetric: UsageMetric? {
        status?.metrics.max(by: { $0.percentage < $1.percentage })
    }

    /// Highest used percentage across windows, for the menu bar label.
    var headlineUsage: Int? {
        guard let metric = headlineMetric else { return nil }
        return Int(metric.percentage.rounded())
    }

    /// True when the busiest window is burning faster than its ideal pace
    /// ("off target"). Falls back to a high-usage threshold when the window has
    /// no burndown data (no known reset time).
    var headlineOffTarget: Bool {
        guard let metric = headlineMetric else { return false }
        if let data = charts.first(where: { $0.metric.id == metric.id })?.data {
            return data.isOverPace
        }
        return metric.percentage >= 80
    }
}
