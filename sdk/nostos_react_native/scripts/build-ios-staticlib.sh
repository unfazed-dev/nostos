#!/usr/bin/env bash
# build-ios-staticlib.sh — regenerate the nostos-swift iOS artifacts the RN pod
# links against, INTO ios/ (the gitignored vendored cache the podspec points
# at). Run by the podspec `prepare_command` at `pod install` so the pod is
# SELF-CONTAINED: a fresh checkout + `pod install` (with the Rust toolchain
# present) produces a working fat sim staticlib + the UniFFI Swift sources +
# the nostos_swiftFFI module — no pre-vendored artifacts shipped in the repo.
#
# Produces, all under sdk/nostos_react_native/ios/:
#   libnostos_swift.a            — FAT arm64-sim + x86_64-sim (lipo'd)
#   nostos_swift.swift           — UniFFI-generated Swift bindings (copied)
#   ffi/nostos_swiftFFI.h        — the C ABI header (copied)
#   ffi/nostos_swiftFFI.modulemap — exposes nostos_swiftFFI as a module
#
# Mirrors sdk/nostos_swift/ios-test/build.sh (which links the thin arm64-sim .a
# directly) but adds the x86_64 slice + lipo so an Intel-hosted sim links too.
#
# ponytail: debug profile by default (fast; the .a links into a release app
# fine — object code is config-agnostic). Set NOSTOS_PROFILE=release for a
# smaller/optimized shippable binary. A device (arm64) slice + an xcframework
# are the production upgrade (out of scope: sim verification only).
set -euo pipefail

LEGACY_PROFILE="${CAIRN_PROFILE:-}" # rename:hold — pre-rename env name, read as fallback until 1.0 (ADR-0046)
if [ -z "${NOSTOS_PROFILE:-}" ] && [ -n "$LEGACY_PROFILE" ]; then
  echo "warning: the pre-rename profile env var is deprecated, set NOSTOS_PROFILE instead (read as a fallback until 1.0)" >&2
fi
PROFILE="${NOSTOS_PROFILE:-${LEGACY_PROFILE:-debug}}"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
RN_SDK_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"          # sdk/nostos_react_native
NOSTOS_SWIFT_DIR="$(cd "$RN_SDK_DIR/../nostos_swift" && pwd)"  # sdk/nostos_swift
IOS_DIR="$RN_SDK_DIR/ios"
FFI_DIR="$IOS_DIR/ffi"

# nostos_swift is a STANDALONE workspace → its own target/ dir.
SIM_ARM64="aarch64-apple-ios-sim"
SIM_X86="x86_64-apple-ios"

echo "[build-ios] 1/5 cargo build $SIM_ARM64 ($PROFILE)"
(cd "$NOSTOS_SWIFT_DIR" && cargo build --target "$SIM_ARM64" $([ "$PROFILE" = release ] && echo --release))
echo "[build-ios] 2/5 cargo build $SIM_X86 ($PROFILE)"
(cd "$NOSTOS_SWIFT_DIR" && cargo build --target "$SIM_X86" $([ "$PROFILE" = release ] && echo --release))

ARM64_A="$NOSTOS_SWIFT_DIR/target/$SIM_ARM64/$PROFILE/libnostos_swift.a"
X86_A="$NOSTOS_SWIFT_DIR/target/$SIM_X86/$PROFILE/libnostos_swift.a"
for a in "$ARM64_A" "$X86_A"; do
  [[ -f "$a" ]] || { echo "[build-ios] FAIL: expected artifact not found: $a" >&2; exit 1; }
done

mkdir -p "$FFI_DIR"
echo "[build-ios] 3/5 lipo -> fat $IOS_DIR/libnostos_swift.a"
lipo -create "$ARM64_A" "$X86_A" -output "$IOS_DIR/libnostos_swift.a"
lipo -info "$IOS_DIR/libnostos_swift.a"

echo "[build-ios] 4/5 copy UniFFI Swift sources + FFI header"
cp "$NOSTOS_SWIFT_DIR/swift-sources/nostos_swift.swift" "$IOS_DIR/nostos_swift.swift"
cp "$NOSTOS_SWIFT_DIR/swift-sources/nostos_swiftFFI.h" "$FFI_DIR/nostos_swiftFFI.h"

echo "[build-ios] 5/5 write nostos_swiftFFI modulemap"
# Exposes the C ABI as a module so nostos_swift.swift's `import nostos_swiftFFI`
# resolves under the framework build (RN 0.86 builds the pod as a framework,
# where a Swift bridging header is unsupported — the Xcode-16+/Swift-6 fix).
cat > "$FFI_DIR/nostos_swiftFFI.modulemap" <<'EOF'
module nostos_swiftFFI {
    header "nostos_swiftFFI.h"
    export *
}
EOF

echo "[build-ios] DONE — fat .a + sources regenerated under $IOS_DIR"
