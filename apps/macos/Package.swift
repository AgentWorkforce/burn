// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "Burn",
    platforms: [
        .macOS(.v13)
    ],
    targets: [
        .executableTarget(
            name: "Burn",
            path: "Sources/Burn",
            resources: [
                .process("Resources")
            ]
        )
    ]
)
