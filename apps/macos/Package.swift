// swift-tools-version:5.9
import PackageDescription

let package = Package(
    name: "AgentLimit",
    platforms: [
        .macOS(.v13)
    ],
    targets: [
        .executableTarget(
            name: "AgentLimit",
            path: "Sources/AgentLimit",
            resources: [
                .process("Resources")
            ]
        )
    ]
)
