import XCTest
@testable import BurnCore

/// Exercises the pure feed-parsing and version-comparison half of
/// `AppUpdater` — the network/install half shells out to codesign/ditto and is
/// exercised on real releases.
final class AppUpdaterTests: XCTestCase {

    // MARK: - Version parsing

    func testVersionComponentsParsesMacosTags() {
        XCTAssertEqual(AppUpdater.versionComponents(ofTag: "macos-v2026.7.2"), [2026, 7, 2])
        XCTAssertEqual(AppUpdater.versionComponents(ofTag: "macos-v2026.12.10"), [2026, 12, 10])
    }

    func testVersionComponentsRejectsOtherTags() {
        // CLI releases, the moving pointer, and malformed versions must never
        // be treated as app updates.
        XCTAssertNil(AppUpdater.versionComponents(ofTag: "relayburn-v4.0.0"))
        XCTAssertNil(AppUpdater.versionComponents(ofTag: "v2.3.0"))
        XCTAssertNil(AppUpdater.versionComponents(ofTag: "macos-latest"))
        XCTAssertNil(AppUpdater.versionComponents(ofTag: "macos-v2026.7.beta"))
        XCTAssertNil(AppUpdater.versionComponents(ofTag: "macos-v"))
    }

    func testIsVersionNewerComparesNumerically() {
        // Numeric, not lexicographic: month 10 beats month 9.
        XCTAssertTrue(AppUpdater.isVersion("2026.10.1", newerThan: "2026.9.9"))
        XCTAssertFalse(AppUpdater.isVersion("2026.9.9", newerThan: "2026.10.1"))
        XCTAssertTrue(AppUpdater.isVersion("2026.7.2", newerThan: "2026.7.1"))
        XCTAssertFalse(AppUpdater.isVersion("2026.7.2", newerThan: "2026.7.2"))
        // Shorter versions are zero-padded.
        XCTAssertFalse(AppUpdater.isVersion("2026.7", newerThan: "2026.7.0"))
        XCTAssertTrue(AppUpdater.isVersion("2026.7.1", newerThan: "2026.7"))
        // The dev placeholder version always loses to a real release…
        XCTAssertTrue(AppUpdater.isVersion("2026.1.1", newerThan: "1.0.0"))
        // …and an unparseable candidate never wins.
        XCTAssertFalse(AppUpdater.isVersion("garbage", newerThan: "1.0.0"))
    }

    // MARK: - Feed selection

    private func release(
        tag: String, draft: Bool = false, prerelease: Bool = false, assets: [String] = []
    ) -> String {
        let assetJSON = assets.map {
            """
            {"name": "\($0)", "browser_download_url": "https://example.com/\(tag)/\($0)"}
            """
        }.joined(separator: ",")
        return """
        {"tag_name": "\(tag)", "draft": \(draft), "prerelease": \(prerelease), "assets": [\(assetJSON)]}
        """
    }

    private func feed(_ releases: [String]) -> Data {
        Data("[\(releases.joined(separator: ","))]".utf8)
    }

    func testLatestUpdatePicksNewestPublishedMacosRelease() {
        // Newest by version, not by list position, with CLI releases mixed in.
        let data = feed([
            release(tag: "relayburn-v4.0.0", assets: ["burn-aarch64-apple-darwin.tar.gz"]),
            release(tag: "macos-v2026.6.3", assets: ["BurnOSX-arm64.dmg", "BurnOSX-arm64.zip"]),
            release(tag: "macos-v2026.7.1", assets: ["BurnOSX-arm64.dmg", "BurnOSX-arm64.zip"]),
            release(tag: "macos-latest", assets: ["BurnOSX-arm64.dmg", "BurnOSX-arm64.zip"]),
        ])
        let update = AppUpdater.latestUpdate(inReleasesJSON: data, assetName: "BurnOSX-arm64.zip")
        XCTAssertEqual(update?.version, "2026.7.1")
        XCTAssertEqual(
            update?.downloadURL.absoluteString,
            "https://example.com/macos-v2026.7.1/BurnOSX-arm64.zip")
    }

    func testLatestUpdateSkipsDraftsAndPrereleases() {
        let data = feed([
            release(tag: "macos-v2026.8.1", draft: true, assets: ["BurnOSX-arm64.zip"]),
            release(tag: "macos-v2026.7.9", prerelease: true, assets: ["BurnOSX-arm64.zip"]),
            release(tag: "macos-v2026.7.1", assets: ["BurnOSX-arm64.zip"]),
        ])
        let update = AppUpdater.latestUpdate(inReleasesJSON: data, assetName: "BurnOSX-arm64.zip")
        XCTAssertEqual(update?.version, "2026.7.1")
    }

    func testLatestUpdateRequiresTheMatchingAsset() {
        // A release without the updater's zip (e.g. DMG-only builds published
        // before the updater existed) can't be offered.
        let data = feed([
            release(tag: "macos-v2026.7.2", assets: ["BurnOSX-arm64.dmg"]),
            release(tag: "macos-v2026.7.1", assets: ["BurnOSX-arm64.dmg", "BurnOSX-arm64.zip"]),
        ])
        let update = AppUpdater.latestUpdate(inReleasesJSON: data, assetName: "BurnOSX-arm64.zip")
        XCTAssertEqual(update?.version, "2026.7.1")
    }

    func testLatestUpdateEmptyOrMalformedFeed() {
        XCTAssertNil(AppUpdater.latestUpdate(inReleasesJSON: feed([]), assetName: "BurnOSX-arm64.zip"))
        XCTAssertNil(AppUpdater.latestUpdate(
            inReleasesJSON: Data("not json".utf8), assetName: "BurnOSX-arm64.zip"))
    }
}
