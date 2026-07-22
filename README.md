# neutrino-ffi-ble — the iroh/BLE federation medium

The concrete iroh-backed federation medium for the embedded
[neutrino](https://github.com/element-hq/neutrino) homeserver, split out of the main repo so that
repo stays free of iroh and of the AGPL-3.0-**or-later** vendored crates
(`vendor/blew`, `vendor/iroh-ble-transport`), which are incompatible with the
main repo's `AGPL-3.0-only OR LicenseRef-Element-Commercial` dual licence.

## What it is

- `neutrino-ffi-ble` — a composition root, not an API. It implements
  `neutrino_main::DatagramLink` over an iroh QUIC endpoint
  (`src/relay_transport.rs`: one CoAP datagram per unreliable QUIC datagram,
  keyed by 32-byte node id; BLE mesh via the `ble` feature) and injects it
  through `neutrino::start_with(config, factory)`. One `#[uniffi::export]`:
  `start_ble(config)` — the BLE twin of `neutrino::start` (the LAN/UDP build).
  The medium's contract (identity = ed25519 pubkey of the node secret,
  transport-authenticated source ids, discovery-registry feed, KickBackoff on
  peer appearance) is documented on `neutrino_main::LinkContext`.
- `vendor/blew` — fork of the crates.io BLE stack (extended advertising +
  manufacturer data for `node_id ‖ display_name` adverts). AGPL-3.0-or-later.
- `vendor/iroh-ble-transport` — BLE custom transport for iroh (GATT pipe,
  L2CAP upgrade, discovery). AGPL-3.0-or-later.
- `bindings/` — the Android library (.aar): blew's Kotlin companion managers
  (`org.jakebot.blew.*`, invoked from Rust by JNI name lookup), the
  `NativeBle` JNI bootstrap, the BLE permissions manifest, and the
  uniffi-generated Kotlin (both namespaces) + `libneutrino.so` per ABI.

## Building

- `cargo check` / `cargo test` — default members only, no Bluetooth stack
  needed (the BLE medium is feature-gated; the loopback tests drive iroh over
  UDP).
- `./build-aar.sh` — the Android .aar. See the script header for prerequisites
  (Android SDK + NDK r27c, `cargo-ndk`, JDK 17, and — on Linux — `libdbus-1-dev`
  for the host binding-generation build). The cdylib target is `neutrino_ble`
  but ships renamed to `libneutrino.so`: uniffi takes each namespace's load name
  from its crate's `uniffi.toml`, so the ffi namespace loads "neutrino" no
  matter which file bindgen ran over — see `neutrino-ffi-ble/uniffi.toml`.
  Each namespace gets its own Kotlin package (`io.element.neutrino` /
  `io.element.neutrino.ble`, bridged via `external_packages`) — sharing one
  package would redeclare uniffi's per-file runtime and break kotlinc.

## Publishing

This repository produces the UniFFI binding artifact (`.aar`) that Element X
Android embeds — a single `.aar` carrying both uniffi namespaces (the whole
embedded neutrino API in `io.element.neutrino`, plus `startBle` in
`io.element.neutrino.ble`) in one `libneutrino.so` per ABI. The Maven
coordinate is `io.element.neutrino:bindings:<version>`. The exact upstream
[neutrino](https://github.com/element-hq/neutrino) commit each build was
compiled against is baked into the published POM (and logged at runtime by
`start_ble`) — see `build-aar.sh` / `bindings/build.gradle.kts`.

The [`element-x-android-neutrino`](https://github.com/element-hq/element-x-android-neutrino)
fork resolves the bindings from **your local Maven repository (`~/.m2`) first,
falling back to GitHub Packages** (`element-hq/neutrino-iroh`). Since `~/.m2`
takes priority, a GitHub Packages version is only resolved when no local build
of that version is present.

### Local development

Publish to `~/.m2`, then re-sync your `element-x-android-neutrino` checkout to
pick up the freshly-built bindings:

```
$ ./build-aar.sh --publish-local --version 0.6.5
```

`--version` sets the `-PneutrinoVersion` gradle property; make sure the fork's
`gradle/libs.versions.toml` `neutrino` version block requests the matching
version. A snapshot (an in-development build rather than a tagged release)
conventionally carries a `-SNAPSHOT` suffix, e.g. `--version 0.6.5-SNAPSHOT`.

### Tagged production releases

Pushing a `v*` tag triggers the [`release`](.github/workflows/release.yml)
workflow, which runs `./build-aar.sh --publish --version "${tag#v}"` — building
all four Android ABIs and publishing the bindings to
[GitHub Packages](https://github.com/orgs/element-hq/packages?repo_name=neutrino-iroh)
under the tag version (leading `v` stripped, so `v0.6.5` publishes as `0.6.5`).
Publishing needs `GITHUB_ACTOR`/`GITHUB_TOKEN`; CI supplies them automatically.
Do not run the GitHub Packages path by hand.
