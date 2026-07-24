// Copyright (c) 2026 Element Creations Ltd.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial.

//! The iroh/BLE federation medium for the embedded neutrino homeserver.
//!
//! This crate is a composition root, not an API: it provides the concrete
//! [`neutrino_main::DatagramLink`] (an iroh QUIC endpoint carrying one CoAP
//! datagram per unreliable QUIC datagram, over BLE on device — see
//! `relay_transport`) and injects it into the transport-agnostic FFI surface
//! via [`neutrino::start_with`]. The one export, [`start_ble`], is the
//! BLE-mesh twin of `neutrino::start` (the LAN/UDP build).
//!
//! The cdylib built from this crate carries both uniffi namespaces —
//! `neutrino_ble` (this file) and `neutrino` (the whole embedded API:
//! `NeutrinoConfig`, `NeutrinoHandle`, ...) — so uniffi-bindgen in library
//! mode over `libneutrino_ble.so` generates the complete Kotlin surface.

uniffi::setup_scaffolding!("neutrino_ble");

#[cfg(feature = "ble")]
mod ble_android;
mod relay_transport;

use relay_transport::{IrohTransport, RELAY_BIND};

/// Fixed localpart for every embedded peer's user: user ids are
/// `@n:{node_id}`. The discovery registry is localpart-agnostic — this is the
/// BLE medium's convention, applied by its discovery drain (see
/// `relay_transport`) where the node id is known.
#[cfg(feature = "ble")]
pub(crate) const DISCOVERY_LOCALPART: &str = "n";

/// Start the embedded homeserver with the iroh/BLE federation medium.
///
/// Identical contract to `neutrino::start` (spawned runtime, returned control
/// handle) with the datagram link factory injected: once the entrypoint has
/// resolved the node secret it binds an iroh endpoint whose id IS that
/// secret's ed25519 public key, dials peers by their link address (the
/// lowercase hex of their 32-byte node id — the peer's `server_name` bytes),
/// and (with the `ble` feature, i.e. on device) discovers + reaches them over
/// the BLE mesh.
#[uniffi::export]
pub fn start_ble(config: neutrino::NeutrinoConfig) -> neutrino::NeutrinoHandle {
    // Announce which upstream neutrino this .aar was compiled against (baked in
    // by build.rs from the lockfile). Install the logcat subscriber first so the
    // line is not dropped — idempotent, and start_with installs it again.
    neutrino_main::init_tracing();
    tracing::info!(
        neutrino_commit = env!("NEUTRINO_COMMIT"),
        "neutrino BLE medium starting"
    );
    // iroh unifies reqwest's TLS backend to rustls with no default crypto
    // provider, so building the federation client would panic ("No rustls
    // crypto provider is configured"). The provider is a process-global the
    // composition root must install before the server (or iroh) builds any
    // client. Idempotent: `install_default` errs if one is set; ignored.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let factory: neutrino_main::FederationLinkFactory = Box::new(move |ctx| {
        Box::pin(async move {
            let transport = IrohTransport::bind(ctx, RELAY_BIND).await?;
            Ok(neutrino_main::FederationLink {
                link: transport as std::sync::Arc<dyn neutrino_main::DatagramLink>,
                key_resolver: Some(std::sync::Arc::new(neutrino_main::NodeIdKeyResolver)),
            })
        })
    });
    neutrino::start_with(config, Some(factory))
}
