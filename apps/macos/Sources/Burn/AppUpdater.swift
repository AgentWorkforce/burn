import AppKit
import SwiftUI

/// A newer app build discovered on GitHub Releases: the version to advertise
/// and the notarized zip asset the updater downloads.
struct AppUpdate: Equatable {
    let version: String
    let downloadURL: URL
}

/// In-app updates for the menu bar app, backed by the `macos-v*` GitHub
/// Releases that `release-macos.yml` publishes (mirroring Pear's updater UX:
/// silent periodic checks, then "Update Now" → download → "Restart Now").
///
/// There is no Sparkle dependency — the release pipeline already gives us a
/// signed, notarized, stapled zip at a predictable URL, so the updater is:
/// check the releases feed, download the zip, verify its Developer ID
/// signature matches the running app's team, swap the installed bundle, and
/// relaunch. All state is surfaced through `phase` for the Settings tab.
@MainActor
final class AppUpdater: ObservableObject {

    /// The updater's lifecycle, rendered by the Settings tab. `upToDate` and
    /// `failed` only appear after a user-initiated check — silent scheduled
    /// checks either surface `available` or leave the phase alone.
    enum Phase: Equatable {
        case idle
        case checking
        case upToDate
        case available(AppUpdate)
        case downloading
        case readyToRestart(AppUpdate)
        case failed(String)
    }

    @Published private(set) var phase: Phase = .idle

    /// The running build's version (`CFBundleShortVersionString`), stamped by
    /// release.sh at release time. Dev builds carry the committed `1.0.0`
    /// placeholder, which is why scheduled checks are gated on a signed install.
    let currentVersion: String

    /// The zip asset release.sh publishes for this machine's architecture.
    static var assetName: String {
        #if arch(x86_64)
        return "BurnOSX-x86_64.zip"
        #else
        return "BurnOSX-arm64.zip"
        #endif
    }

    private static let checkInterval: TimeInterval = 6 * 60 * 60

    /// Held outside the actor so `deinit` can tear it down (see `TimerBox`).
    private let timerBox = TimerBox()

    /// The verified, extracted .app staged by `beginDownload()`, consumed by
    /// `restart()`.
    private var stagedAppURL: URL?

    init(currentVersion: String? = nil) {
        self.currentVersion = currentVersion
            ?? Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String
            ?? "0"
    }

    // MARK: Scheduling

    /// Launch-time entry point: one silent check shortly after startup, then
    /// every 6 hours. Skipped entirely for unsigned builds (`swift run`, local
    /// build.sh output) so dev builds don't nag about "updates" the placeholder
    /// version can never install; the Settings button still allows a manual
    /// check anywhere.
    func startPeriodicChecks() {
        guard !timerBox.isActive else { return }
        Task { [weak self] in
            let bundleURL = Bundle.main.bundleURL
            guard bundleURL.pathExtension == "app" else { return }
            let team = await Self.teamIdentifier(ofCodeAt: bundleURL)
            guard team != nil, let self else { return }
            self.timerBox.timer = Timer.scheduledTimer(
                withTimeInterval: Self.checkInterval, repeats: true
            ) { [weak self] _ in
                // Rebind strongly before hopping into the task: capturing the
                // weak `self` var inside the concurrent closure is a Swift 6
                // isolation error.
                guard let self else { return }
                Task { @MainActor in await self.check(silently: true) }
            }
            await self.check(silently: true)
        }
    }

    // MARK: Checking

    /// Non-async wrapper for SwiftUI buttons.
    func checkNow() {
        Task { await check() }
    }

    /// Fetches the releases feed and compares the newest published `macos-v*`
    /// build against the running version. Silent checks (the scheduled path)
    /// only ever surface a new `available` phase; interactive checks also
    /// report `upToDate` / `failed`.
    func check(silently: Bool = false) async {
        switch phase {
        case .checking, .downloading, .readyToRestart:
            return
        default:
            break
        }
        if !silently { phase = .checking }
        do {
            let latest = try await Self.fetchLatestUpdate(assetName: Self.assetName)
            if let latest, Self.isVersion(latest.version, newerThan: currentVersion) {
                phase = .available(latest)
            } else if !silently {
                phase = .upToDate
            }
        } catch {
            if !silently {
                phase = .failed("Update check failed: \(error.localizedDescription)")
            }
        }
    }

    // MARK: Download & install

    /// "Update Now": downloads the zip, extracts it, and verifies the payload's
    /// Developer ID signature before offering the restart. The installed bundle
    /// is not touched until the user confirms the restart.
    func beginDownload() {
        guard case .available(let update) = phase else { return }
        phase = .downloading
        Task { [weak self] in
            do {
                let staged = try await Self.downloadAndStage(update)
                self?.stagedAppURL = staged
                self?.phase = .readyToRestart(update)
            } catch {
                self?.phase = .failed("Update failed: \(error.localizedDescription)")
            }
        }
    }

    /// "Restart Now": swaps the staged bundle over the installed one, spawns a
    /// relauncher that waits for this process to exit, and terminates the app.
    func restart() {
        guard case .readyToRestart = phase, let staged = stagedAppURL else { return }
        do {
            let installed = Bundle.main.bundleURL
            try Self.install(staged: staged, over: installed)
            Self.relaunch(appAt: installed)
            NSApp.terminate(nil)
        } catch {
            stagedAppURL = nil
            phase = .failed("Update failed: \(error.localizedDescription)")
        }
    }

    // MARK: Feed parsing (pure — unit-tested)

    /// One entry of the GitHub releases list; only the fields the updater reads.
    struct Release: Decodable {
        struct Asset: Decodable {
            let name: String
            let browserDownloadUrl: URL
        }

        let tagName: String
        let draft: Bool
        let prerelease: Bool
        let assets: [Asset]
    }

    /// `"macos-v2026.7.2"` → `[2026, 7, 2]`; nil for CLI tags (`relayburn-v*`,
    /// `v*`), the `macos-latest` pointer, and anything non-numeric.
    nonisolated static func versionComponents(ofTag tag: String) -> [Int]? {
        guard tag.hasPrefix("macos-v") else { return nil }
        return versionComponents(of: String(tag.dropFirst("macos-v".count)))
    }

    /// `"2026.7.2"` → `[2026, 7, 2]`; nil unless every dot component is numeric.
    nonisolated static func versionComponents(of version: String) -> [Int]? {
        let parts = version.split(separator: ".", omittingEmptySubsequences: false)
        guard !parts.isEmpty else { return nil }
        var components: [Int] = []
        for part in parts {
            guard let value = Int(part), value >= 0 else { return nil }
            components.append(value)
        }
        return components
    }

    /// Numeric component-wise comparison (so `2026.10.1` beats `2026.9.9`),
    /// padding the shorter version with zeros. Unparseable versions never win.
    nonisolated static func isVersion(_ candidate: String, newerThan current: String) -> Bool {
        guard let a = versionComponents(of: candidate) else { return false }
        guard let b = versionComponents(of: current) else { return true }
        for i in 0..<max(a.count, b.count) {
            let x = i < a.count ? a[i] : 0
            let y = i < b.count ? b[i] : 0
            if x != y { return x > y }
        }
        return false
    }

    /// Picks the highest-versioned published (non-draft, non-prerelease)
    /// `macos-v*` release that carries the named zip asset.
    nonisolated static func latestUpdate(inReleasesJSON data: Data, assetName: String) -> AppUpdate? {
        let decoder = JSONDecoder()
        decoder.keyDecodingStrategy = .convertFromSnakeCase
        guard let releases = try? decoder.decode([Release].self, from: data) else { return nil }

        var best: (components: [Int], update: AppUpdate)?
        for release in releases where !release.draft && !release.prerelease {
            guard let components = versionComponents(ofTag: release.tagName),
                  let asset = release.assets.first(where: { $0.name == assetName })
            else { continue }
            if let current = best, !componentsAreOrderedBefore(current.components, components) {
                continue
            }
            let version = String(release.tagName.dropFirst("macos-v".count))
            best = (components, AppUpdate(version: version, downloadURL: asset.browserDownloadUrl))
        }
        return best?.update
    }

    /// Component-wise `<` with zero-padding, mirroring `isVersion(_:newerThan:)`.
    nonisolated private static func componentsAreOrderedBefore(_ a: [Int], _ b: [Int]) -> Bool {
        for i in 0..<max(a.count, b.count) {
            let x = i < a.count ? a[i] : 0
            let y = i < b.count ? b[i] : 0
            if x != y { return x < y }
        }
        return false
    }

    // MARK: Networking

    enum UpdateError: LocalizedError {
        case badStatus(Int)
        case noAppInArchive
        case unsignedDownload(String)
        case unsignedInstall
        case notInstalledAsApp

        var errorDescription: String? {
            switch self {
            case .badStatus(let code):
                return "GitHub returned HTTP \(code)."
            case .noAppInArchive:
                return "the downloaded archive did not contain an app."
            case .unsignedDownload(let detail):
                return "the downloaded app failed signature verification (\(detail))."
            case .unsignedInstall:
                return "this build is unsigned, so in-app updates are disabled. Download the DMG instead."
            case .notInstalledAsApp:
                return "the app is not running from an installed bundle."
            }
        }
    }

    nonisolated private static func fetchLatestUpdate(assetName: String) async throws -> AppUpdate? {
        // GitHub releases feed for this repo. `per_page=100` comfortably covers
        // the newest `macos-v*` release even with CLI releases interleaved.
        // (Local so it stays off the @MainActor class — a nonisolated context
        // can't read the class's isolated statics.)
        let releasesURL = URL(
            string: "https://api.github.com/repos/AgentWorkforce/burn/releases?per_page=100")!
        var request = URLRequest(url: releasesURL)
        request.setValue("application/vnd.github+json", forHTTPHeaderField: "Accept")
        let (data, response) = try await URLSession.shared.data(for: request)
        if let http = response as? HTTPURLResponse, http.statusCode != 200 {
            throw UpdateError.badStatus(http.statusCode)
        }
        return latestUpdate(inReleasesJSON: data, assetName: assetName)
    }

    /// Downloads and extracts the update zip, then verifies the payload:
    /// `codesign --verify` must pass and the archive's team identifier must
    /// match the running app's. Refuses to proceed from an unsigned build —
    /// there is no team to pin the download against.
    nonisolated private static func downloadAndStage(_ update: AppUpdate) async throws -> URL {
        guard let expectedTeam = await teamIdentifier(ofCodeAt: Bundle.main.bundleURL) else {
            throw UpdateError.unsignedInstall
        }

        let (zipURL, response) = try await URLSession.shared.download(from: update.downloadURL)
        if let http = response as? HTTPURLResponse, http.statusCode != 200 {
            throw UpdateError.badStatus(http.statusCode)
        }

        let extractDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("burn-update-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: extractDir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: zipURL) }

        let unzip = await run("/usr/bin/ditto", ["-x", "-k", zipURL.path, extractDir.path])
        guard unzip.status == 0 else { throw UpdateError.noAppInArchive }

        let contents = try FileManager.default.contentsOfDirectory(
            at: extractDir, includingPropertiesForKeys: nil)
        guard let app = contents.first(where: { $0.pathExtension == "app" }) else {
            throw UpdateError.noAppInArchive
        }

        // Belt and braces: the zip is notarized and stapled, but strip any
        // quarantine attribute so the relaunch can't trip over translocation.
        _ = await run("/usr/bin/xattr", ["-dr", "com.apple.quarantine", app.path])

        let verify = await run("/usr/bin/codesign", ["--verify", "--deep", "--strict", app.path])
        guard verify.status == 0 else {
            throw UpdateError.unsignedDownload(verify.output.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        let downloadedTeam = await teamIdentifier(ofCodeAt: app)
        guard downloadedTeam == expectedTeam else {
            throw UpdateError.unsignedDownload("unexpected signing team")
        }
        return app
    }

    // MARK: Bundle swap & relaunch

    /// Moves the installed bundle aside and puts the staged one in its place,
    /// rolling the old bundle back if the swap fails halfway.
    nonisolated private static func install(staged: URL, over installed: URL) throws {
        guard installed.pathExtension == "app" else { throw UpdateError.notInstalledAsApp }
        let fm = FileManager.default
        let aside = fm.temporaryDirectory
            .appendingPathComponent("burn-previous-\(UUID().uuidString)", isDirectory: true)
            .appendingPathComponent(installed.lastPathComponent)
        try fm.createDirectory(
            at: aside.deletingLastPathComponent(), withIntermediateDirectories: true)
        try fm.moveItem(at: installed, to: aside)
        do {
            // moveItem fails across volumes (temp vs. /Applications on some
            // setups); fall back to a copy, which preserves the signature.
            do {
                try fm.moveItem(at: staged, to: installed)
            } catch {
                try fm.copyItem(at: staged, to: installed)
            }
        } catch {
            try? fm.moveItem(at: aside, to: installed)
            throw error
        }
        try? fm.removeItem(at: aside.deletingLastPathComponent())
    }

    /// Spawns a detached shell that waits for this process to exit, then opens
    /// the (now updated) bundle. The path travels as `$1`, so no quoting games.
    nonisolated private static func relaunch(appAt url: URL) {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/bin/sh")
        process.arguments = [
            "-c",
            "while /bin/kill -0 \(ProcessInfo.processInfo.processIdentifier) 2>/dev/null; do /bin/sleep 0.2; done; /usr/bin/open \"$1\"",
            "burn-relaunch",
            url.path,
        ]
        try? process.run()
    }

    // MARK: Subprocess helpers

    /// `codesign -dvv`'s `TeamIdentifier=` line, or nil for unsigned/ad-hoc
    /// code. Used both to gate scheduled checks (dev builds are unsigned) and
    /// to pin downloads to the running app's signing team.
    nonisolated static func teamIdentifier(ofCodeAt url: URL) async -> String? {
        let result = await run("/usr/bin/codesign", ["-dvv", url.path])
        guard result.status == 0 else { return nil }
        for line in result.output.split(separator: "\n") {
            if line.hasPrefix("TeamIdentifier=") {
                let team = String(line.dropFirst("TeamIdentifier=".count))
                return (team.isEmpty || team == "not set") ? nil : team
            }
        }
        return nil
    }

    /// Runs a tool to completion off the cooperative pool, returning its exit
    /// status and combined stdout+stderr (one shared pipe — codesign reports on
    /// stderr; outputs here are tiny, far below the 64KB pipe buffer).
    nonisolated private static func run(
        _ tool: String, _ arguments: [String]
    ) async -> (status: Int32, output: String) {
        await withCheckedContinuation { continuation in
            DispatchQueue.global(qos: .utility).async {
                autoreleasepool {
                    let process = Process()
                    process.executableURL = URL(fileURLWithPath: tool)
                    process.arguments = arguments
                    let pipe = Pipe()
                    process.standardOutput = pipe
                    process.standardError = pipe
                    do {
                        try process.run()
                    } catch {
                        continuation.resume(returning: (-1, ""))
                        return
                    }
                    let data = pipe.fileHandleForReading.readDataToEndOfFile()
                    try? pipe.fileHandleForReading.close()
                    process.waitUntilExit()
                    let output = String(data: data, encoding: .utf8) ?? ""
                    continuation.resume(returning: (process.terminationStatus, output))
                }
            }
        }
    }
}
