// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Burn",
    platforms: [
        .macOS(.v13)
    ],
    targets: [
        // All app logic lives here as a library so it can be unit-tested via
        // `@testable import BurnCore`. Resources move with it, so `Bundle.module`
        // references keep resolving.
        .target(
            name: "BurnCore",
            path: "Sources/Burn",
            resources: [
                .process("Resources")
            ]
        ),
        // Thin `@main` entry point. Keeps the executable product named `Burn`
        // (build.sh/release.sh copy `$BIN_PATH/Burn`).
        .executableTarget(
            name: "Burn",
            dependencies: ["BurnCore"],
            path: "Sources/Main"
        ),
        .testTarget(
            name: "BurnTests",
            dependencies: ["BurnCore"],
            path: "Tests/BurnTests"
        )
    ]
)
