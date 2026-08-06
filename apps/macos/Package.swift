// swift-tools-version:5.9
import PackageDescription

// TcrBar — a menu-bar front end for the `tcr` CLI.
//
// Split into a library (`TcrBarCore`) plus a thin `@main` executable so the
// decoding and supervision logic is testable without linking a test bundle
// against an `@main` entry point.
let package = Package(
    name: "TcrBar",
    platforms: [.macOS(.v13)],
    products: [
        .executable(name: "TcrBar", targets: ["TcrBar"])
    ],
    targets: [
        .target(name: "TcrBarCore", path: "Sources/TcrBarCore"),
        .executableTarget(
            name: "TcrBar",
            dependencies: ["TcrBarCore"],
            path: "Sources/TcrBar"
        ),
        .testTarget(
            name: "TcrBarTests",
            dependencies: ["TcrBarCore"],
            path: "Tests/TcrBarTests"
        ),
    ]
)
