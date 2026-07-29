// Copyright (c) 2026 Element Creations Ltd.
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Element-Commercial.

//! iroh-backed [`DatagramLink`] for the low-bandwidth federation transport.
//!
//! Carries one CoAP/CBOR datagram per unreliable QUIC datagram between nodes,
//! keyed by the peer's link address — the lowercase 64-hex ASCII of its
//! 32-byte node id, exactly the `server_name` bytes the egress resolver
//! renders — no OS socket, no TUN, no virtual IPs. iroh is confined to this
//! layer; `neutrino-lb` stays iroh-free and speaks only the [`DatagramLink`]
//! seam (opaque `LinkAddr` byte strings). The hex translation happens only at
//! that trait boundary: internally everything stays keyed by the raw
//! `[u8; 32]` node id. QUIC datagrams are per-connection, so the transport
//! keeps a `[u8; 32] → Connection` send-side table (populated by dialing on
//! egress and by accepting — a connection is bidirectional, so one accepted
//! from a peer is reused to send back). Every connection (dialed or accepted)
//! gets a reader task that tags each inbound datagram with the lowercase hex
//! of the cryptographically-authenticated remote node id — the ingress
//! origin↔source gate compares those bytes against the claimed `X-Matrix`
//! origin verbatim; a reader removes its own send-side entry when its
//! connection dies, so the next send re-dials instead of reusing a dead
//! connection.
//!
//! The endpoint still binds its own ephemeral loopback UDP socket for QUIC
//! transport (see [`RELAY_BIND`]); that is iroh-internal and not a peer data
//! path.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use iroh::endpoint::presets::Minimal;
use iroh::endpoint::{Connection, IdleTimeout, QuicTransportConfig, VarInt};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, SecretKey};
use neutrino_main::{DatagramLink, LinkAddr, LinkContext, LinkProfile};
use tokio::sync::Mutex as AsyncMutex;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::warn;

/// A node's stable cryptographic identity, as raw public-key bytes. iroh's
/// endpoint id IS these bytes; the [`DatagramLink`] address is their lowercase
/// hex (see [`hex32`]/[`unhex32`]).
type NodeKey = [u8; 32];

/// UDP bind for the iroh endpoint's QUIC transport. Ephemeral loopback port; on
/// device the BLE custom transport carries packets to peers and discovery
/// advertises reachability, so the UDP socket is only iroh's local plumbing.
pub(crate) const RELAY_BIND: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0);

/// ALPN for the federation datagram link.
const RELAY_ALPN: &[u8] = b"neutrino/iroh-relay/0";

/// Forward BLE-discovered peers into the homeserver's discovery registry. Each
/// transport snapshot replaces the registry set: peers are keyed by
/// `server_name` (= lowercase hex of the node id, matching how the resolver
/// derives it) and stamped with the fixed [`crate::DISCOVERY_LOCALPART`].
///
/// When a snapshot introduces a peer the previous one didn't have, pulse
/// [`neutrino_main::Command::KickBackoff`] through the server's command fan-in:
/// a destination that backed off while that peer was out of range retries
/// promptly instead of waiting out the rest of its backoff (the same edge the
/// LAN mDNS browser kicks on).
#[cfg(feature = "ble")]
fn spawn_discovery_drain(
    mut rx: tokio::sync::watch::Receiver<Vec<iroh_ble_transport::discovery::DiscoveredPeer>>,
    registry: Arc<neutrino_main::DiscoveryRegistry>,
    kick: mpsc::UnboundedSender<neutrino_main::Command>,
) {
    tokio::spawn(async move {
        let mut known = std::collections::HashSet::new();
        while rx.changed().await.is_ok() {
            let snapshot = rx.borrow_and_update().clone();
            let last_seen_ms = now_ms();
            let map: HashMap<_, _> = snapshot
                .into_iter()
                .map(|p| {
                    (
                        hex32(&p.node_id),
                        neutrino_main::DiscoveredPeer {
                            localpart: crate::DISCOVERY_LOCALPART.to_string(),
                            display_name: p.display_name,
                            last_seen_ms,
                        },
                    )
                })
                .collect();
            let appeared = map.keys().any(|k| !known.contains(k));
            known = map.keys().cloned().collect();
            registry.replace(map);
            if appeared {
                // Send failure = server stopped; the drain ends when the watch
                // closes, so no need to bail early here.
                let _ = kick.send(neutrino_main::Command::KickBackoff);
            }
        }
    });
}

/// Re-advertise the local display name whenever it changes (`PUT
/// /profile/.../displayname` pulses the watch).
#[cfg(feature = "ble")]
fn spawn_readvertise(
    ble: Arc<iroh_ble_transport::transport::BleTransport>,
    mut name_rx: tokio::sync::watch::Receiver<String>,
) {
    tokio::spawn(async move {
        while name_rx.changed().await.is_ok() {
            let name = name_rx.borrow_and_update().clone();
            if let Err(e) = ble.set_display_name(Some(name)).await {
                warn!(error = %e, "re-advertise after display-name change failed");
            }
        }
    });
}

/// Lowercase-hex a 32-byte node id → its `server_name` string (whose bytes are
/// the node's [`DatagramLink`] address).
fn hex32(bytes: &[u8; 32]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(64);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// The inverse of [`hex32`]: parse a [`DatagramLink`] address — exactly 64
/// lowercase hex ASCII chars — back into the 32-byte node key. Anything else
/// (wrong length, uppercase, non-hex) is rejected: addresses are canonical
/// lowercase and compared as exact bytes, so this must not be lenient.
fn unhex32(addr: &[u8]) -> std::io::Result<NodeKey> {
    fn nibble(b: u8) -> Option<u8> {
        match b {
            b'0'..=b'9' => Some(b - b'0'),
            b'a'..=b'f' => Some(b - b'a' + 10),
            _ => None,
        }
    }
    if addr.len() != 64 {
        return Err(std::io::Error::other(format!(
            "link: peer address must be the 64-char lowercase hex of a 32-byte node id, got {} bytes",
            addr.len()
        )));
    }
    let mut key = [0u8; 32];
    for (byte, pair) in key.iter_mut().zip(addr.chunks_exact(2)) {
        match (nibble(pair[0]), nibble(pair[1])) {
            (Some(hi), Some(lo)) => *byte = (hi << 4) | lo,
            _ => {
                return Err(std::io::Error::other(
                    "link: peer address must be lowercase hex (a 32-byte node id)",
                ));
            }
        }
    }
    Ok(key)
}

/// Wall-clock milliseconds since the Unix epoch (0 if the clock is before it).
#[cfg(feature = "ble")]
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Upper bound on a single dial (id-only `connect`, resolved via BLE discovery).
/// Without it `endpoint.connect` waits forever when discovery never finds the peer
/// (peer not advertising / out of range / BLE unpaired), so a federation request
/// would hang until the coap-layer timeout with no indication why. Generous enough
/// for a real BLE discovery + QUIC handshake, short enough to surface a dead peer
/// well before the coap request timeout.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Connection-level QUIC idle timeout. iroh's default is 30s (noq-proto,
/// RFC 9308 §3.2); iroh overrides the *path* idle (15s) and keepalive (5s) but
/// leaves this connection-level one at the default. On a BLE peer restart the
/// old connection otherwise lingers the full 30s: iroh won't migrate to the
/// freshly-reconnected pipe because the peer's custom address is prefix-keyed
/// off its (unchanged) node id, so it looks like the same, still-live path —
/// iroh only re-resolves and re-handshakes once *this* timer closes the dead
/// connection. 10s is >2x the 5s keepalive (a healthy link survives a single
/// lost keepalive) yet well under the 30s default, collapsing peer-restart
/// recovery from ~30s+ to ~10s.
const CONN_MAX_IDLE: Duration = Duration::from_secs(10);

/// Bound on buffered inbound datagrams before the per-connection readers block
/// (back-pressure onto the wire, which is acceptable for a best-effort link).
const INBOUND_CAPACITY: usize = 256;

/// Send-side route table: the connection to use for sending to each peer.
type ConnMap = Arc<AsyncMutex<HashMap<NodeKey, Connection>>>;

pub(crate) struct IrohTransport {
    endpoint: Endpoint,
    conns: ConnMap,
    /// Where to reach a peer. Seeded out of band — service discovery on device,
    /// the test seeds loopback addresses. A peer with no entry that has never
    /// dialed us cannot be reached.
    addrs: Mutex<HashMap<NodeKey, EndpointAddr>>,
    inbound_tx: mpsc::Sender<(LinkAddr, Vec<u8>)>,
    inbound_rx: AsyncMutex<mpsc::Receiver<(LinkAddr, Vec<u8>)>>,
    /// The accept-loop task. Aborted on drop so the endpoint (and its UDP
    /// socket) can close — the loop captures only clones, never an `Arc<Self>`,
    /// so it doesn't keep this transport alive (which would leak an endpoint per
    /// rebind).
    accept_task: Mutex<Option<JoinHandle<()>>>,
}

impl IrohTransport {
    /// Bind an endpoint whose identity is derived from the context's `secret`
    /// (the persisted node secret the server's `server_name` is derived from,
    /// so this endpoint's id equals that node id — the invariant `LinkContext`
    /// documents), and start accepting connections. The context's display-name
    /// watch drives the BLE advertisement (+ re-advertise on change), its
    /// registry receives scanned peers, and its command sender is pulsed on
    /// peer appearance; all three are unused off the `ble` feature.
    pub(crate) async fn bind(
        ctx: LinkContext,
        bind_addr: SocketAddr,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error + Send + Sync>> {
        let LinkContext {
            secret,
            display_name: name_rx,
            discovery,
            commands,
        } = ctx;
        #[cfg(not(feature = "ble"))]
        let _ = (name_rx, discovery, commands);
        let secret_key = SecretKey::from_bytes(&secret);
        // The BLE transport needs our public key; capture it before the key is
        // moved into the builder.
        #[cfg(feature = "ble")]
        let public = secret_key.public();
        // Offline BLE-mesh homeserver: no relay AND no n0 DNS discovery. The
        // `N0`/`N0DisableRelay` presets silently append a `PkarrPublisher` +
        // `DnsAddressLookup`, both pointing at `dns.iroh.link`. With no network
        // those repeatedly fail/block, and on our single-threaded (`current_thread`)
        // runtime that stalls the executor — starving the C-S `/sync` long-poll
        // timers so the client's room list never updates. `Minimal` sets only the
        // crypto provider; we disable the relay explicitly and resolve peers
        // solely via the BLE `address_lookup` wired below (LAN peers are seeded
        // via `add_peer`), so nothing ever touches the network for discovery.
        // Shorten the connection-level idle timeout so a dead BLE connection is
        // abandoned (and re-established over a fresh pipe) in seconds rather than
        // the 30s default. Built from `QuicTransportConfig::builder()`, which
        // seeds iroh's own defaults (5s keepalive, 15s path idle) — we override
        // only the connection idle. See `CONN_MAX_IDLE`.
        let transport_config = QuicTransportConfig::builder()
            .max_idle_timeout(Some(IdleTimeout::try_from(CONN_MAX_IDLE).map_err(
                |e| -> Box<dyn std::error::Error + Send + Sync> { Box::new(e) },
            )?))
            .build();
        let builder = Endpoint::builder(Minimal)
            .relay_mode(RelayMode::Disabled)
            .secret_key(secret_key)
            .alpns(vec![RELAY_ALPN.to_vec()])
            .transport_config(transport_config);

        // On the embedded (Android) target, add the BLE custom transport
        // *alongside* IP, so federation reaches peers over both LAN and BLE
        // (phones); the transport's `address_lookup` resolves peers over the BLE
        // mesh, while LAN peers are seeded via `add_peer`. Desktop/CI is IP-only.
        #[cfg(feature = "ble")]
        let endpoint = {
            // Bootstrap blew's Android JNI layer (no-op off Android), else
            // `Central::new` panics with "JVM not initialized".
            crate::ble_android::ensure_initialised()
                .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;
            let central = Arc::new(iroh_ble_transport::Central::new().await?);
            let peripheral = Arc::new(iroh_ble_transport::Peripheral::new().await?);
            // Advertise `node_id ‖ display_name` for peer discovery (current name
            // from the watch; re-advertised on change below).
            //
            // `verified_rx` + the `BleDedupHook` installed on the builder below
            // are one mechanism: the hook forwards each QUIC-verified peer
            // endpoint into the transport's registry. Both connection dedup AND
            // the GATT→L2CAP upgrade trigger only on those events — without
            // this wiring the registry never learns a peer is verified, so no
            // L2CAP upgrade is ever attempted and every link stays on GATT.
            let (verified_tx, verified_rx) = mpsc::unbounded_channel();
            let config = iroh_ble_transport::transport::BleTransportConfig {
                display_name: Some(name_rx.borrow().clone()),
                verified_rx: Some(verified_rx),
                // L2CAP upgrade enabled (PreferL2cap): the verified_rx wiring
                // above feeds QUIC-verified peers into the registry, which is
                // what triggers the GATT->L2CAP upgrade. Instrumented with the
                // hop-by-hop logging in vendor/blew l2cap_state.rs +
                // L2capSocketManager.kt to validate the Android JNI data bridge
                // under blew::L2capChannel across a swap.
                l2cap_policy: iroh_ble_transport::transport::L2capPolicy::PreferL2cap,
                ..Default::default()
            };
            let ble = Arc::new(
                iroh_ble_transport::transport::BleTransport::with_config(
                    public, central, peripheral, config,
                )
                .await?,
            );
            let lookup = ble.address_lookup();
            // Publish peers the transport scans into the homeserver's registry
            // (kicking backed-off destinations when one appears), and
            // re-advertise our name when it changes.
            spawn_discovery_drain(ble.discovered_peers(), discovery, commands);
            spawn_readvertise(Arc::clone(&ble), name_rx);
            let ble: Arc<dyn iroh::endpoint::transports::CustomTransport> = ble;
            tracing::info!("BLE dedup hook wired: verified-endpoint events -> registry");
            builder
                .hooks(iroh_ble_transport::BleDedupHook::new(verified_tx))
                .add_custom_transport(ble)
                .address_lookup(lookup)
                .bind_addr(bind_addr)?
                .bind()
                .await?
        };
        #[cfg(not(feature = "ble"))]
        let endpoint = builder.bind_addr(bind_addr)?.bind().await?;

        tracing::info!(
            node_id = %endpoint.id(),
            sockets = ?endpoint.bound_sockets(),
            ble = cfg!(feature = "ble"),
            "datagram link: iroh endpoint bound"
        );

        let (inbound_tx, inbound_rx) = mpsc::channel(INBOUND_CAPACITY);
        let conns: ConnMap = Arc::new(AsyncMutex::new(HashMap::new()));
        // Spawn the accept loop with clones (endpoint/conns/tx), NOT an
        // `Arc<Self>` — so the transport isn't kept alive by its own loop.
        let accept_task = tokio::spawn(accept_loop(
            endpoint.clone(),
            conns.clone(),
            inbound_tx.clone(),
        ));
        Ok(Arc::new(Self {
            endpoint,
            conns,
            addrs: Mutex::new(HashMap::new()),
            inbound_tx,
            inbound_rx: AsyncMutex::new(inbound_rx),
            accept_task: Mutex::new(Some(accept_task)),
        }))
    }

    /// This node's identity (its iroh endpoint id) as a [`NodeKey`].
    #[allow(dead_code)]
    pub(crate) fn node_key(&self) -> NodeKey {
        *self.endpoint.id().as_bytes()
    }

    // The discovery accessors below are wired when service discovery (mDNS/BLE)
    // lands; until then they're used only via the test seeders.
    /// This endpoint's id, for handing to peers as part of an [`EndpointAddr`].
    #[allow(dead_code)]
    pub(crate) fn endpoint_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// The sockets this endpoint is bound to.
    #[allow(dead_code)]
    pub(crate) fn bound_sockets(&self) -> Vec<SocketAddr> {
        self.endpoint.bound_sockets()
    }

    /// Teach the transport how to reach a peer. Keyed by the address's own
    /// endpoint id, so the mapping has a single source of truth.
    #[allow(dead_code)]
    pub(crate) fn add_peer(&self, addr: EndpointAddr) {
        let key = *addr.id.as_bytes();
        self.addrs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key, addr);
    }

    /// Drain a connection's datagrams into the inbound queue until it closes,
    /// then drop its send-side entry iff the map still points to *this*
    /// connection — so the next send re-dials and we never evict a newer one.
    /// Each datagram is tagged with the connection's authenticated remote node
    /// id as a [`LinkAddr`] (lowercase hex ASCII), encoded once per connection.
    fn spawn_reader(
        peer: NodeKey,
        conn: Connection,
        conns: ConnMap,
        tx: mpsc::Sender<(LinkAddr, Vec<u8>)>,
    ) {
        let conn_id = conn.stable_id();
        tokio::spawn(async move {
            let src: LinkAddr = hex32(&peer).into_bytes();
            // `read_datagram` errors only on connection close — terminal.
            while let Ok(bytes) = conn.read_datagram().await {
                // Best-effort: drop rather than block this reader (which would
                // stall every peer's inbound — head-of-line) when the consumer is
                // slow to drain. Closed channel = the link is gone → stop.
                match tx.try_send((src.clone(), bytes.to_vec())) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => {
                        tracing::trace!("datagram link: inbound queue full, dropping datagram");
                    }
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
            let mut map = conns.lock().await;
            if map.get(&peer).map(Connection::stable_id) == Some(conn_id) {
                map.remove(&peer);
            }
        });
    }

    /// A live connection to `dst`, dialing (and starting its reader) on a miss.
    async fn connection(&self, dst: NodeKey) -> std::io::Result<Connection> {
        {
            let conns = self.conns.lock().await;
            if let Some(conn) = conns.get(&dst)
                && conn.close_reason().is_none()
            {
                return Ok(conn.clone());
            }
        }
        let seeded = self
            .addrs
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&dst)
            .cloned();
        let addr = match seeded {
            Some(addr) => addr,
            // No seeded IP address. With the BLE transport present, dial by
            // endpoint id alone and let the endpoint's `address_lookup` (the BLE
            // mesh discovery wired in `bind`) resolve a path — this is the device
            // path: nothing seeds `addrs` on a phone (`add_peer` is the test/LAN
            // seam only). Without BLE (desktop/CI) an unseeded peer is genuinely
            // unreachable, so keep failing fast.
            #[cfg(feature = "ble")]
            None => EndpointAddr::new(
                EndpointId::from_bytes(&dst)
                    .map_err(|e| std::io::Error::other(format!("link: invalid peer id: {e}")))?,
            ),
            #[cfg(not(feature = "ble"))]
            None => {
                return Err(std::io::Error::other("link: no known address for peer"));
            }
        };
        // Info, not debug: this is the load-bearing federation hop, and a dial that
        // never resolves is the failure we most need to see. `peer` is the id we
        // dial; `addrs` is empty on device (id-only, resolved via BLE discovery).
        let peer = addr.id;
        tracing::info!(%peer, addrs = ?addr.addrs, "datagram link: dialing peer (id-only over BLE discovery if no addrs)");
        let conn = match tokio::time::timeout(
            CONNECT_TIMEOUT,
            self.endpoint.connect(addr, RELAY_ALPN),
        )
        .await
        {
            Ok(Ok(conn)) => conn,
            Ok(Err(e)) => {
                tracing::warn!(%peer, error = %e, "datagram link: connect to peer failed");
                return Err(std::io::Error::other(format!(
                    "link connect to {peer}: {e}"
                )));
            }
            Err(_) => {
                tracing::warn!(
                    %peer,
                    timeout = ?CONNECT_TIMEOUT,
                    "datagram link: connect to peer timed out — peer unreachable (not advertising / out of BLE range / discovery found no path)"
                );
                return Err(std::io::Error::other(format!(
                    "link connect to {peer} timed out after {CONNECT_TIMEOUT:?}"
                )));
            }
        };
        tracing::info!(%peer, "datagram link: connection established");
        {
            let mut conns = self.conns.lock().await;
            // A live connection may have appeared while we dialed (a concurrent
            // dial, or an accepted one): keep it and close our loser.
            if let Some(existing) = conns.get(&dst)
                && existing.close_reason().is_none()
            {
                let existing = existing.clone();
                conn.close(VarInt::from_u32(0), b"superseded");
                return Ok(existing);
            }
            conns.insert(dst, conn.clone());
        }
        Self::spawn_reader(
            dst,
            conn.clone(),
            self.conns.clone(),
            self.inbound_tx.clone(),
        );
        Ok(conn)
    }
}

impl Drop for IrohTransport {
    fn drop(&mut self) {
        // Stop accepting so the endpoint (and its UDP socket) can close. The
        // accept loop holds only clones, so this Drop fires once the last
        // `Arc<Self>` drops — without this, an endpoint would leak per rebind.
        if let Some(task) = self
            .accept_task
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
        {
            task.abort();
        }
    }
}

/// Accept inbound connections (with clones of the shared state, not an
/// `Arc<IrohTransport>`, so the loop never keeps the transport alive).
async fn accept_loop(
    endpoint: Endpoint,
    conns: ConnMap,
    inbound_tx: mpsc::Sender<(LinkAddr, Vec<u8>)>,
) {
    while let Some(incoming) = endpoint.accept().await {
        let conns = conns.clone();
        let inbound_tx = inbound_tx.clone();
        tokio::spawn(async move {
            match incoming.await {
                Ok(conn) => adopt(conn, conns, inbound_tx).await,
                Err(err) => warn!(?err, "datagram link: inbound connection failed"),
            }
        });
    }
}

/// Adopt an accepted connection: always read from it (the peer may send on it),
/// but only take over the send-side route if we have no live connection to this
/// peer yet — don't clobber an existing one (glare).
async fn adopt(conn: Connection, conns: ConnMap, inbound_tx: mpsc::Sender<(LinkAddr, Vec<u8>)>) {
    let peer = *conn.remote_id().as_bytes();
    {
        let mut map = conns.lock().await;
        let vacant = match map.get(&peer) {
            Some(existing) => existing.close_reason().is_some(),
            None => true,
        };
        if vacant {
            map.insert(peer, conn.clone());
        }
    }
    IrohTransport::spawn_reader(peer, conn, conns, inbound_tx);
}

#[async_trait]
impl DatagramLink for IrohTransport {
    async fn send(&self, dst: &[u8], datagram: &[u8]) -> std::io::Result<()> {
        // The seam's address is the peer's `server_name` bytes — for this
        // medium the lowercase hex of its node id. Decode at the boundary;
        // everything below stays keyed by the raw 32 bytes.
        let conn = self.connection(unhex32(dst)?).await?;
        conn.send_datagram(Bytes::copy_from_slice(datagram))
            .map_err(|e| {
                // Loud: a datagram larger than the connection's max datagram size is
                // rejected here, which would otherwise silently drop a federation
                // block.
                tracing::warn!(error = %e, len = datagram.len(), "datagram link: send_datagram failed (block exceeds path max datagram size?)");
                std::io::Error::other(format!("send_datagram: {e}"))
            })
    }

    async fn recv(&self) -> Option<(LinkAddr, Vec<u8>)> {
        self.inbound_rx.lock().await.recv().await
    }

    fn profile(&self) -> LinkProfile {
        LinkProfile {
            // Declared so block derivation yields 512 B Q-Blocks — NOT the
            // link's true datagram capability. This medium has no LinkCodec,
            // so the full uncompressed federation options ride every block:
            // with the default 1280 B profile the derivation picks 1024 B
            // blocks, and 1024 + real options overflows coap-lite's 1280 B
            // message cap — the historical silent send stall. 512 + options
            // stays comfortably under it. (1024 derives 512 both before and
            // after neutrino dropped its fixed option budget.)
            max_datagram: 1024,
            ..LinkProfile::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use tokio::time::timeout;

    /// A [`LinkContext`] with test doubles for the BLE-only members: an empty
    /// display-name watch (its sender is dropped immediately — the channel is
    /// unused off the `ble` feature, and a closed channel just makes the
    /// re-advertise task exit cleanly under it), a fresh empty discovery
    /// registry, and a command sender whose receiver is dropped (sends are
    /// best-effort no-ops).
    fn test_ctx(secret: [u8; 32]) -> LinkContext {
        LinkContext {
            secret,
            display_name: tokio::sync::watch::channel(String::new()).1,
            discovery: Arc::new(neutrino_main::DiscoveryRegistry::new()),
            commands: mpsc::unbounded_channel().0,
        }
    }

    /// Pick a node's loopback dialing address from its bound sockets.
    fn loopback_addr(tp: &IrohTransport) -> EndpointAddr {
        let sock = tp
            .bound_sockets()
            .into_iter()
            .find(|s| s.ip().is_loopback())
            .expect("a loopback bound socket");
        EndpointAddr::new(tp.endpoint_id()).with_ip_addr(sock)
    }

    // Full link flow over real iroh, driving the `DatagramLink` seam directly:
    // A sends a datagram to B's link address (the lowercase hex of B's node
    // id), B receives it tagged with the hex of A's authenticated node id, then
    // B replies A over the reused (accepted) connection. Exercises dial,
    // accept, bidirectional reuse, the inbound source tagging, and
    // identity-from-secret.
    #[tokio::test]
    async fn datagram_relays_a_to_b_to_a_over_iroh() {
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
        let a_tp = IrohTransport::bind(test_ctx([1u8; 32]), loopback)
            .await
            .expect("bind A");
        let b_tp = IrohTransport::bind(test_ctx([2u8; 32]), loopback)
            .await
            .expect("bind B");

        let a_addr = hex32(&a_tp.node_key()).into_bytes();
        let b_addr = hex32(&b_tp.node_key()).into_bytes();
        assert_ne!(a_addr, b_addr);

        // A can reach B (the egress/dial path); B learns A on inbound.
        a_tp.add_peer(loopback_addr(&b_tp));

        // A → B.
        let to_b = b"hello-b";
        a_tp.send(&b_addr, to_b).await.expect("send a->b");
        let (src, got) = timeout(Duration::from_secs(10), b_tp.recv())
            .await
            .expect("B receives in time")
            .expect("B link open");
        assert_eq!(
            src, a_addr,
            "datagram tagged with the hex of A's authenticated node id"
        );
        assert_eq!(got, to_b);

        // B → A, routed via the reused (accepted) connection — B never seeded A.
        let to_a = b"hello-a";
        b_tp.send(&a_addr, to_a).await.expect("send b->a");
        let (src, got) = timeout(Duration::from_secs(10), a_tp.recv())
            .await
            .expect("A receives in time")
            .expect("A link open");
        assert_eq!(src, b_addr);
        assert_eq!(got, to_a);
    }

    // The one error the transport itself originates: a destination with no
    // seeded address that has never dialed us is unroutable.
    #[tokio::test]
    async fn send_to_unknown_peer_is_an_error() {
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
        let tp = IrohTransport::bind(test_ctx([3u8; 32]), loopback)
            .await
            .expect("bind");
        assert!(tp.send(hex32(&[9u8; 32]).as_bytes(), b"x").await.is_err());
    }

    // The seam's address is exactly 64 lowercase hex chars; anything else is
    // rejected at the trait boundary, before any dial is attempted.
    #[tokio::test]
    async fn send_to_malformed_address_is_an_error() {
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
        let tp = IrohTransport::bind(test_ctx([4u8; 32]), loopback)
            .await
            .expect("bind");
        let uppercase = hex32(&[9u8; 32]).to_uppercase();
        for addr in [
            &b""[..],             // empty
            b"abc123",            // too short
            &[b'a'; 63],          // one short
            &[b'a'; 65],          // one long
            uppercase.as_bytes(), // not canonical lowercase
            &[b'g'; 64],          // right length, not hex
        ] {
            assert!(
                tp.send(addr, b"x").await.is_err(),
                "address {addr:?} must be rejected"
            );
        }
    }

    // `unhex32` is the exact inverse of `hex32` on every byte value.
    #[test]
    fn unhex32_inverts_hex32() {
        let mut key = [0u8; 32];
        for (i, b) in key.iter_mut().enumerate() {
            *b = (i * 8 + 7) as u8; // covers low/high nibble variety incl. 0xff
        }
        assert_eq!(unhex32(hex32(&key).as_bytes()).expect("round-trip"), key);
    }

    // Load-bearing cross-layer invariant: the link's `node_key` (iroh endpoint
    // id) must equal the ed25519 public key that neutrino-main derives the
    // server_name from for the same secret — otherwise the host would advertise
    // one node id while the link answers on another, silently breaking
    // federation. iroh's node id IS the raw ed25519 pubkey today; this pins it so
    // a future iroh key-derivation change fails loudly here.
    #[tokio::test]
    async fn node_key_matches_ed25519_public_key() {
        let secret = [7u8; 32];
        let loopback: SocketAddr = "127.0.0.1:0".parse().expect("loopback");
        let tp = IrohTransport::bind(test_ctx(secret), loopback)
            .await
            .expect("bind");
        let expected = ed25519_dalek::SigningKey::from_bytes(&secret)
            .verifying_key()
            .to_bytes();
        assert_eq!(tp.node_key(), expected);
    }
}
