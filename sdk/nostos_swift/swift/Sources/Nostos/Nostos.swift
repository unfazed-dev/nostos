// Nostos Swift shim — re-export entry point.
//
// The UniFFI-generated Swift (`nostos_swift.swift`, produced by
// `uniffi-bindgen generate --library ../target/debug/libnostos_swift.dylib
// --language swift --out-dir ../swift-sources/`) defines the `NostosClient`
// class on the Swift side. This file is a placeholder so `Sources/Nostos/` is
// non-empty for the SPM `.target(name: "Nostos", path: "Sources/Nostos")`
// declaration in Package.swift; the real Swift surface lives in the generated
// file and is wired in by the binary-target increment (see Package.swift
// ponytail).

import Foundation

/// Marker for the scaffold: the generated bindings are not yet compiled into
/// this target. Once `swift-sources/nostos_swift.swift` is added as a source,
/// callers use `NostosClient` directly.
public enum NostosSDK {
    public static let name = "nostos-swift"
    public static let isLinked = false
}
