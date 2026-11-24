#!/usr/bin/env bash
# Build the BLE-federation neutrino for iOS: NeutrinoFFI.xcframework (device +
# simulator static libs) plus the uniffi-generated Swift for both namespaces,
# laid out as a local Swift package in ./ios. The iOS twin of build-aar.sh.
# Requires: Xcode and the aarch64-apple-ios / aarch64-apple-ios-sim rust targets.
#
# The simulator has no Bluetooth; its slice exists so the app builds and runs
# there, not to federate.

set -euo pipefail
cd "$(dirname "$0")"

TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim)
OUT=./ios
GEN="$OUT/Sources/Neutrino"
HEADERS=./target/ios-headers

# 1. Host build so uniffi-bindgen can load the cdylib (same feature set as the
#    device build, as for the .aar).
cargo build -p neutrino-ffi-ble --release --features ble

# 2. Device and simulator static libraries.
for target in "${TARGETS[@]}"; do
  cargo build -p neutrino-ffi-ble --release --features ble --target "$target"
done

# 3. Swift bindings for both namespaces. The generated .swift files go into the
#    package; the C headers and a single merged modulemap go into the xcframework.
rm -rf "$GEN" "$HEADERS"
mkdir -p "$GEN" "$HEADERS"
cargo run -p uniffi-bindgen -- generate --library ./target/release/libneutrino_ble.dylib \
  --language swift --out-dir "$GEN"
mv "$GEN"/*.h "$HEADERS/"
cat "$GEN"/*.modulemap > "$HEADERS/module.modulemap"
rm "$GEN"/*.modulemap

# 4. Package.
rm -rf "$OUT/NeutrinoFFI.xcframework"
xc_args=()
for target in "${TARGETS[@]}"; do
  xc_args+=(-library "./target/$target/release/libneutrino_ble.a" -headers "$HEADERS")
done
xcodebuild -create-xcframework "${xc_args[@]}" -output "$OUT/NeutrinoFFI.xcframework"
