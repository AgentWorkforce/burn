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

    /// Upper bound on samples retained per series. A weekly window sampled every
    /// ~60s would otherwise accumulate ~10k samples; once a series crosses this
    /// bound it is decimated (see `decimated(_:)`) to stay bounded while keeping
    /// the newest data at full resolution.
    static let maxSamplesPerSeries = 2_000

    private let queue = DispatchQueue(label: "com.agentworkforce.burn.history")
    /// Dedicated serial I/O queue for the actual disk writes. Writes must not
    /// run on `queue`: `record()` enters `queue` synchronously from the main
    /// actor, so a slow write there would block the next refresh.
    private static let writeQueue = DispatchQueue(
        label: "com.agentworkforce.burn.history.write", qos: .utility)
    private var cache: [String: [UsageSample]] = [:]
    private let fileURL: URL

    /// Debounce window for disk writes. Bursts of `record()` calls coalesce into
    /// a single async write instead of re-encoding the whole cache each time.
    private let persistDebounce: TimeInterval = 2
    private var pendingWrite: DispatchWorkItem?

    private convenience init() {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Burn", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        self.init(fileURL: dir.appendingPathComponent("history.json"))
    }

    /// Backs the store with an explicit file (tests point this at a temp file).
    init(fileURL: URL) {
        self.fileURL = fileURL
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

            // Keep each series bounded so long-lived windows can't grow forever.
            // One decimation pass only trims to ~75%, so loop until under the
            // cap — an oversized legacy history must be bounded immediately, not
            // over many future appends. Each pass strictly shrinks the series,
            // so this terminates.
            while samples.count > Self.maxSamplesPerSeries {
                samples = decimated(samples)
            }

            cache[k] = samples
            pruneStaleWindows(reference: date)
            schedulePersist()
            return samples
        }
    }

    /// Synchronously flushes any pending debounced write; the file is guaranteed
    /// to be on disk when this returns. Exposed for tests that need deterministic
    /// on-disk state without waiting for the debounce timer.
    func flushForTesting() {
        queue.sync {
            pendingWrite?.cancel()
            pendingWrite = nil
            persist(synchronously: true)
        }
    }

    /// Reduces an over-long series while preserving its shape: the first sample
    /// (the window's origin) is always kept, the newest half is kept at full
    /// resolution, and the older half is thinned by dropping every second sample.
    /// Deterministic, and structured so repeated appends keep the series bounded
    /// without discarding recent, high-value data.
    private func decimated(_ samples: [UsageSample]) -> [UsageSample] {
        guard samples.count > 2 else { return samples }
        let mid = samples.count / 2
        var result: [UsageSample] = []
        result.reserveCapacity(samples.count)
        // Older half: keep the first sample, then keep every second sample.
        for (offset, sample) in samples[0..<mid].enumerated() where offset % 2 == 0 {
            result.append(sample)
        }
        // Newer half: kept intact.
        result.append(contentsOf: samples[mid...])
        return result
    }

    /// Drops series whose window has aged out.
    ///
    /// - Timestamped keys are dropped once their reset time is more than an hour
    ///   in the past.
    /// - Nil-reset keys (`…|none`) have no reset time to age out on, so they are
    ///   dropped once their newest sample is older than 24h; otherwise they would
    ///   accumulate forever.
    private func pruneStaleWindows(reference: Date) {
        for k in Array(cache.keys) {
            // The reset segment is always the final segment and never contains
            // "|", so read it from the end — robust to a "|" in a metric name.
            guard let last = k.split(separator: "|").last else { continue }

            if last == "none" {
                // Use the max date, not `.last`, so pruning stays correct even
                // if a series is ever not strictly chronological.
                if let newest = cache[k]?.map(\.date).max(),
                   newest < reference.addingTimeInterval(-86_400) {
                    cache.removeValue(forKey: k)
                }
                continue
            }

            guard let ts = Double(last), ts != 0 else { continue }
            let resetDate = Date(timeIntervalSince1970: ts)
            if resetDate < reference.addingTimeInterval(-3600) {
                cache.removeValue(forKey: k)
            }
        }
    }

    /// Coalesces disk writes: cancels any pending write and schedules a fresh one
    /// `persistDebounce` seconds out, so a burst of records results in a single
    /// encode (on `queue`) whose write is handed off to `writeQueue`.
    private func schedulePersist() {
        pendingWrite?.cancel()
        let item = DispatchWorkItem { [weak self] in
            guard let self else { return }
            self.pendingWrite = nil
            self.persist()
        }
        pendingWrite = item
        queue.asyncAfter(deadline: .now() + persistDebounce, execute: item)
    }

    /// Encodes the cache (on `queue`, for thread safety) and hands the bytes to
    /// the dedicated I/O queue so the disk write never blocks `record()`'s
    /// `queue.sync`. Writes atomically so a crash mid-write can't corrupt the
    /// existing history file.
    ///
    /// Always invoked on `queue`. `writeQueue` is serial, so writes land in the
    /// order they were encoded and a `synchronously: true` flush both waits for
    /// any in-flight async write and then supersedes it with the newest data
    /// before returning.
    private func persist(synchronously: Bool = false) {
        guard let data = try? JSONEncoder().encode(cache) else { return }
        let url = fileURL
        if synchronously {
            Self.writeQueue.sync {
                try? data.write(to: url, options: .atomic)
            }
        } else {
            Self.writeQueue.async {
                try? data.write(to: url, options: .atomic)
            }
        }
    }
}
