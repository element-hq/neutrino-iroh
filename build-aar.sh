#!/usr/bin/env bash
# Build (and optionally publish) the BLE-federation neutrino .aar — the
# iroh/BLE twin of the main repo's `cargo xtask compile` / `cargo xtask
# publish`. Requires: Android SDK (ANDROID_HOME) + NDK (r27c), cargo-ndk, the
# Android rust targets, and — on Linux hosts — libdbus-1-dev (the host build
# compiles blew's bluer backend for binding generation).
#
# Usage:
#   ./build-aar.sh                                    # assembleRelease only
#   ./build-aar.sh --publish-local --version 0.6.5    # ~ xtask publish --local
#   ./build-aar.sh --publish --version 0.6.5          # GitHub Packages
#                                                     # (needs GITHUB_ACTOR/TOKEN)
#   ABIS="arm64-v8a" ./build-aar.sh                   # limit ABIs (default: all 4)

set -euo pipefail
cd "$(dirname "$0")"

# The exact neutrino commit this .aar is built against, read from the lockfile
# (the resolved sha after '#', not the requested `rev`, which may be short or a
# tag). Baked into the published POM via -PneutrinoCommit and echoed, so every
# build — local or CI — records which upstream neutrino it was compiled with.
NEUTRINO_COMMIT="$(grep -oE 'github\.com/element-hq/neutrino[^#]*#[0-9a-f]{40}' Cargo.lock | grep -oE '[0-9a-f]{40}$' | head -1)"
echo "neutrino commit: ${NEUTRINO_COMMIT:-unknown}"

ABIS=(${ABIS:-armeabi-v7a arm64-v8a x86 x86_64})
GRADLE_TASK=":bindings:assembleRelease"
VERSION=""
while [ $# -gt 0 ]; do
  case "$1" in
    --publish-local) GRADLE_TASK=":bindings:publishToMavenLocal" ;;
    --publish) GRADLE_TASK=":bindings:publish" ;;
    --version) VERSION="$2"; shift ;;
    *) echo "unknown arg: $1" >&2; exit 2 ;;
  esac
  shift
done

# 1. Host build so uniffi-bindgen can load the cdylib. Must match the Android
#    feature set (step 2): bindings are generated from THIS cdylib (step 3), so
#    a feature whose exports are missing here is missing from the Kotlin
#    bindings even though the device .so has it. Hence `ble`.
cargo build -p neutrino-ffi-ble --release --features ble

# 2. Android targets via cargo-ndk → jniLibs.
ndk_args=()
for abi in "${ABIS[@]}"; do ndk_args+=(-t "$abi"); done
cargo ndk -o ./bindings/src/main/jniLibs "${ndk_args[@]}" \
  build -p neutrino-ffi-ble --release --features ble

# 2b. The cdylib target is `neutrino_ble` (it cannot share neutrino-ffi's lib
#     name in one dependency graph), but every generated binding — and the
#     consuming app's NativeBle/JNA lookups — loads "neutrino" (see
#     neutrino-ffi-ble/uniffi.toml). Rename in place.
for abi in "${ABIS[@]}"; do
  mv "./bindings/src/main/jniLibs/$abi/libneutrino_ble.so" \
     "./bindings/src/main/jniLibs/$abi/libneutrino.so"
done

# 3. Generate the Kotlin bindings (both namespaces: `neutrino` = the whole
#    embedded API from neutrino-ffi, in io.element.neutrino; `neutrino_ble` =
#    startBle, in io.element.neutrino.ble) from the host cdylib. Drop any
#    previously generated files first so a layout change can't leave stale
#    redeclarations behind (NativeBle.kt is hand-written and kept).
case "$(uname)" in
  Darwin) host_lib=./target/release/libneutrino_ble.dylib ;;
  *) host_lib=./target/release/libneutrino_ble.so ;;
esac
rm -f ./bindings/src/main/java/io/element/neutrino/neutrino.kt \
      ./bindings/src/main/java/io/element/neutrino/neutrino_ble.kt
rm -rf ./bindings/src/main/java/io/element/neutrino/ble
cargo run -p uniffi-bindgen -- generate --library "$host_lib" \
  --language kotlin --out-dir ./bindings/src/main/java

# 4. Package / publish the .aar (version property matches xtask publish).
gradle_args=("$GRADLE_TASK")
[ -n "$VERSION" ] && gradle_args+=("-PneutrinoVersion=$VERSION")
[ -n "$NEUTRINO_COMMIT" ] && gradle_args+=("-PneutrinoCommit=$NEUTRINO_COMMIT")
# maven-publish / AGP publication tasks aren't configuration-cache compatible;
# with CC enabled the build succeeds but the invocation still exits non-zero.
# Disable the config cache for the publish paths only (assembleRelease keeps it).
case "$GRADLE_TASK" in
  *publish*) gradle_args+=(--no-configuration-cache) ;;
esac
./gradlew "${gradle_args[@]}"
if [ "$GRADLE_TASK" = ":bindings:assembleRelease" ]; then
  echo "aar: bindings/build/outputs/aar/"
fi