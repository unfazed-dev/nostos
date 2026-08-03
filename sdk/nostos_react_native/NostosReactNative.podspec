# NostosReactNative.podspec — iOS TurboModule pod for @nostos-sync/react-native.
#
# MINIMAL (slice 1): just enough for RN autolinking + @react-native/codegen to
# emit the NativeNostosSpec Obj-C protocol at `pod install`. The vendored
# nostos_swift xcframework + Swift TurboModule source are layered in slice 2
# (after inspecting the generated NativeNostosSpec.h so the Swift impl matches the
# exact codegen contract). See docs/plans/nostos-rn-ios-turbomodule-2026-08-03.md.
Pod::Spec.new do |s|
  s.name         = "NostosReactNative"
  s.version      = "0.1.0"
  s.summary      = "Nostos local-first sync — React Native Codegen TurboModule over nostos-swift UniFFI."
  s.description  = "React Native facade over nostos-swift / nostos-kotlin UniFFI bindings."
  s.license      = "Apache-2.0"
  s.homepage     = "https://github.com/unfazed-dev/nostos"
  s.author       = { "unfazed-dev" => "https://github.com/unfazed-dev" }
  s.source       = { :git => "https://github.com/unfazed-dev/nostos", :tag => "#{s.version}" }

  s.platforms    = { :ios => "15.1" }
  s.swift_version = "5.0"

  # nostos_swift.swift + the TurboModule sources. nostos_swiftFFI.h lives in
  # ios/ffi/ (NOT a source header) so it can't collide with the framework's
  # umbrella modulemap — it's exposed as its own module via the modulemap below.
  s.source_files = "ios/*.{swift,m,mm}"
  s.preserve_paths = "ios/ffi/nostos_swiftFFI.h", "ios/ffi/nostos_swiftFFI.modulemap"

  # nostos_swift UniFFI bindings: vendored ios-sim staticlib + Swift marshalling
  # (nostos_swift.swift) + the C FFI module. NostosReactNative builds as a
  # FRAMEWORK (RN 0.86 default), where Swift bridging headers are unsupported —
  # so nostos_swiftFFI is exposed as a real module instead: a modulemap in
  # ios/ffi/ found via -fmodule-map-file makes canImport(nostos_swiftFFI) true,
  # so nostos_swift.swift's `import nostos_swiftFFI` resolves (the Xcode-16+/Swift-6
  # UniFFI-in-a-module gotcha). ponytail: ios/{nostos_swift.swift,ffi/*,
  # libnostos_swift.a} are VENDORED COPIES for local RN-iOS verification
  # (gitignored — regen from sdk/nostos_swift); a publishable podspec would
  # build+copy them via a prepare_command.
  s.vendored_libraries = "ios/libnostos_swift.a"
  s.frameworks = "Security", "CoreFoundation", "Foundation"
  s.libraries = "resolv"
  s.pod_target_xcconfig = {
    "HEADER_SEARCH_PATHS" => "$(PODS_TARGET_SRCROOT)/ios/ffi",
    "OTHER_SWIFT_FLAGS" => "-Xcc -fmodule-map-file=$(PODS_TARGET_SRCROOT)/ios/ffi/nostos_swiftFFI.modulemap",
  }

  # Wires React-Core + the @react-native/codegen-generated `NativeNostosSpec.h`
  # (emitted at `pod install` now that package.json declares `codegenConfig` —
  # type "modules" = TurboModule). The Swift impl in ios/ conforms to that
  # Obj-C protocol. See docs/plans/nostos-rn-ios-turbomodule-2026-08-03.md.
  install_modules_dependencies(s)
end
