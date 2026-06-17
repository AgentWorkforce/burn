import Foundation

/// One recorded observation of a usage window.
struct UsageSample: Codable {
    let date: Date
    /// Percentage *used* at the time of the sample (0...100).
    let percentage: Double
}

/// Persists a rolling history of usage samples so the burndown chart can draw
/// the real, jagged usage curve instead of a single point. Samples are keyed by
/// provider + window + reset time, so each limit window gets its own series and
/// old windows are pruned after they reset.
final class UsageHistoryStore {
    static let shared = UsageHistoryStore()

    private let queue = DispatchQueue(label: "com.agentworkforce.burn.history")
    private var cache: [String: [UsageSample]] = [:]
    private let fileURL: URL

    private init() {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Burn", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        fileURL = dir.appendingPathComponent("history.json")

        if let data = try? Data(contentsOf: fileURL),
           let decoded = try? JSONDecoder().decode([String: [UsageSample]].self, from: data) {
            cache = decoded
        }
    }

    private func key(provider: ProviderName, metric: UsageMetric) -> String {
        let reset = metric.resetsAt.map { String(Int($0.timeIntervalSince1970)) } ?? "none"
        return "\(provider.rawValue)|\(metric.name)|\(reset)"
    }

    /// Records the current value of a metric and returns the full sample series
    /// for the metric's current window.
    @discardableResult
    func record(provider: ProviderName, metric: UsageMetric, at date: Date = Date()) -> [UsageSample] {
        queue.sync {
            let k = key(provider: provider, metric: metric)
            var samples = cache[k] ?? []

            // Collapse rapid duplicate samples (e.g. manual refreshes).
            if let last = samples.last, date.timeIntervalSince(last.date) < 1 {
                samples[samples.count - 1] = UsageSample(date: date, percentage: metric.percentage)
            } else {
                samples.append(UsageSample(date: date, percentage: metric.percentage))
            }

            cache[k] = samples
            pruneStaleWindows(reference: date)
            persist()
            return samples
        }
    }

    /// Drops series for windows that reset more than an hour ago.
    private func pruneStaleWindows(reference: Date) {
        for k in cache.keys {
            // The reset timestamp is always the final segment and never contains
            // "|", so read it from the end — robust to a "|" in a metric name.
            guard let last = k.split(separator: "|").last, let ts = Double(last), ts != 0 else { continue }
            let resetDate = Date(timeIntervalSince1970: ts)
            if resetDate < reference.addingTimeInterval(-3600) {
                cache.removeValue(forKey: k)
            }
        }
    }

    private func persist() {
        guard let data = try? JSONEncoder().encode(cache) else { return }
        try? data.write(to: fileURL)
    }
}
