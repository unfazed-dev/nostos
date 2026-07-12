// swift-tools-version:5.9
//
// Nostos Swift Package — wraps the UniFFI-generated `nostos_swift.swift` + the
// Rust staticlib (`libnostos_swift.a`) produced by `cargo build --release`.
//
// SCOPE (scaffold): the UniFFI-generated Swift is verified by
// `swiftc -typecheck -I swift-sources swift-sources/nostos_swift.swift` — see
// the parent README/ponytail notes in sdk/nostos_swift/src/lib.rs. SPM
// `.binaryTarget` linking of the `.a` (and the `.xcframework` for iOS) is the
// NEXT increment; this Package.swift declares the target shape a future
// xcframework would slot into.
//
// ponytail: this Package currently exposes Nostos as a regular target whose
// sources are the UniFFI-generated `nostos_swift.swift` (and a thin re-export
// shim under Sources/Nostos/). To ship, replace the `.target` with a
// `.binaryTarget(path: "../xcframework/Nostos.xcframework")` once the
// xcframework is built (cargo build --release for macos + ios targets, then
// xcodebuild -create-xcframework).

import PackageDescription

let package = Package(
    name: "Nostos",
    platforms: [
        .iOS(.v15),
        .macOS(.v12),
    ],
    products: [
        .library(name: "Nostos", targets: ["Nostos"]),
    ],
    targets: [
        // The UniFFI-generated Swift sources live in ../swift-sources/ after
        // `uniffi-bindgen generate`. The Sources/Nostos/ shim re-exports them
        // so SPM sees a single module. The C FFI module
        // (nostos_swiftFFI.modulemap + .h) must be reachable via -I — wired in
        // by the binary-target increment below when the .a ships.
        .target(
            name: "Nostos",
            path: "Sources/Nostos"
        ),
    ]
)
