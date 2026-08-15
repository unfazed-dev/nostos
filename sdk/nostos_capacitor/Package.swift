// swift-tools-version: 5.9
import PackageDescription

// SPM manifest (Capacitor 8 app template shape). The package and product
// names must be `NostosCapacitor` — `npx cap sync ios` derives that name from
// the npm name (@nostos-sync/capacitor) and references it verbatim from the app's
// generated CapApp-SPM/Package.swift. CocoaPods apps use NostosCapacitor.podspec
// instead; keep both in step.
let package = Package(
    name: "NostosCapacitor",
    platforms: [.iOS(.v15)],
    products: [
        .library(
            name: "NostosCapacitor",
            targets: ["NostosPlugin"])
    ],
    dependencies: [
        .package(url: "https://github.com/ionic-team/capacitor-swift-pm.git", from: "8.0.0")
    ],
    targets: [
        .target(
            name: "NostosPlugin",
            dependencies: [
                .product(name: "Capacitor", package: "capacitor-swift-pm"),
                .product(name: "Cordova", package: "capacitor-swift-pm")
            ],
            path: "ios/Nostos")
    ]
)
