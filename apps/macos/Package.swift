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
    dependencies: [
        // Self-update. Deliberately a dependency of the EXECUTABLE only:
        // `TcrBarCore` stays free of it so the test target keeps linking a
        // framework-free library, and `swift test` never pulls in an updater.
        .package(url: "https://github.com/sparkle-project/Sparkle", from: "2.9.5")
    ],
    targets: [
        .target(name: "TcrBarCore", path: "Sources/TcrBarCore"),
        .executableTarget(
            name: "TcrBar",
            dependencies: [
                "TcrBarCore",
                .product(name: "Sparkle", package: "Sparkle"),
            ],
            path: "Sources/TcrBar",
            // Sparkle links as `@rpath/Sparkle.framework/...`, and SPM only ever
            // adds `@loader_path` — which resolves inside `.build`, not inside an
            // assembled bundle. Without this the app builds and links cleanly and
            // then fails to launch from `TcrBar.app` with a dyld "Library not
            // loaded" the moment the framework moves to `Contents/Frameworks`.
            linkerSettings: [
                .unsafeFlags(["-Xlinker", "-rpath", "-Xlinker", "@executable_path/../Frameworks"])
            ]
        ),
        .testTarget(
            name: "TcrBarTests",
            dependencies: ["TcrBarCore"],
            path: "Tests/TcrBarTests"
        ),
    ]
)
