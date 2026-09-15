//! LAN discovery for paired devices — the "one computer made of many
//! devices" half of the Personal App Cloud MVP.
//!
//! A device running `pai sync serve --announce` multicasts a *signed*
//! announcement every few seconds. Listeners verify the Ed25519
//! signature against `sync_peers.ed_pubkey` — forged or unpaired
//! announcements are dropped, so a LAN attacker can't impersonate a
//! trusted peer. The datagram carries only the relay **port**; the
//! contact address is always the packet's source IP.
//!
//! Relay auth on the mesh needs no shared token file: the bearer token
//! is `hex(peer_key)` — the pairwise X25519 secret both sides derived
//! at pairing. Only devices that completed pairing can authenticate.

use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use serde::{Deserialize, Serialize};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::path::Path;
use std::time::{Duration, Instant};

/// Admin-scoped multicast group + port for announcements.
pub const MULTICAST_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 71, 77);
pub const MULTICAST_PORT: u16 = 47677;

/// Announcements outside this clock window are dropped — bounds replay
/// of captured datagrams on the LAN.
pub const MAX_SKEW_SECS: i64 = 15 * 60;

/// A signed "I serve a sync relay on this port" datagram.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Announcement {
    pub v: u8,
    pub device_id: String,
    pub name: String,
    pub platform: String,
    /// TCP port the sync relay listens on. The address is implicit —
    /// receivers use the datagram's source IP.
    pub port: u16,
    pub ts: String,
    /// Hex Ed25519 signature over [`signing_payload`](Self::signing_payload).
    pub sig: String,
}

impl Announcement {
    /// Canonical string the signature covers.
    pub fn signing_payload(&self) -> String {
        format!(
            "v{}|{}|{}|{}|{}|{}",
            self.v, self.device_id, self.name, self.platform, self.port, self.ts
        )
    }
}

/// Build + sign an announcement for `device` serving `relay_port`.
pub fn make_announcement(
    ids: &IdentityStore,
    device: &Device,
    key_dir: &Path,
    relay_port: u16,
) -> Result<Announcement> {
    let mut a = Announcement {
        v: 1,
        device_id: device.id.to_string(),
        name: device.name.clone(),
        platform: format!("{:?}", device.platform).to_lowercase(),
        port: relay_port,
        ts: now().to_rfc3339(),
        sig: String::new(),
    };
    let sig = ids.sign(device.id, key_dir, a.signing_payload().as_bytes())?;
    a.sig = hex::encode(sig);
    Ok(a)
}

/// Verify an announcement against a candidate Ed25519 public key:
/// version, signature, and freshness. Returns false on any failure —
/// callers drop the datagram.
pub fn verify_announcement(ids: &IdentityStore, a: &Announcement, ed_pubkey: &[u8; 32]) -> bool {
    if a.v != 1 {
        return false;
    }
    let Ok(sig) = hex::decode(&a.sig) else {
        return false;
    };
    let fresh = chrono::DateTime::parse_from_rfc3339(&a.ts)
        .map(|t| (chrono::Utc::now().timestamp() - t.timestamp()).abs() <= MAX_SKEW_SECS)
        .unwrap_or(false);
    if !fresh {
        return false;
    }
    ids.verify_with_key(ed_pubkey, a.signing_payload().as_bytes(), &sig)
        .unwrap_or(false)
}

/// Send one announcement datagram to `dest` (multicast group, or a
/// unicast addr in tests).
pub fn send_announcement(sock: &UdpSocket, a: &Announcement, dest: SocketAddr) -> Result<()> {
    let raw = serde_json::to_vec(a).map_err(|e| Error::Sync(e.to_string()))?;
    sock.send_to(&raw, dest)
        .map_err(|e| Error::Sync(format!("announce send: {e}")))?;
    Ok(())
}

/// Bind a UDP listener on `bind`, joining `group` when given. For the
/// LAN path use `0.0.0.0:MULTICAST_PORT` + [`MULTICAST_GROUP`]; tests
/// bind loopback with no group and announce by unicast.
pub fn bind_listener(bind: SocketAddr, group: Option<Ipv4Addr>) -> Result<UdpSocket> {
    let sock =
        UdpSocket::bind(bind).map_err(|e| Error::Sync(format!("announce bind {bind}: {e}")))?;
    if let Some(g) = group {
        sock.join_multicast_v4(&g, &Ipv4Addr::UNSPECIFIED)
            .map_err(|e| Error::Sync(format!("join multicast {g}: {e}")))?;
    }
    sock.set_read_timeout(Some(Duration::from_millis(200)))
        .map_err(|e| Error::Sync(format!("announce timeout: {e}")))?;
    Ok(sock)
}

/// Collect announcements for `timeout`, deduplicated by device_id
/// (freshest wins). Malformed datagrams are ignored; signature
/// verification is the caller's job (`verify_announcement`).
pub fn discover(sock: &UdpSocket, timeout: Duration) -> Vec<(Announcement, SocketAddr)> {
    let deadline = Instant::now() + timeout;
    let mut seen: std::collections::HashMap<String, (Announcement, SocketAddr)> =
        std::collections::HashMap::new();
    let mut buf = [0u8; 8192];
    while Instant::now() < deadline {
        match sock.recv_from(&mut buf) {
            Ok((n, src)) => {
                if let Ok(a) = serde_json::from_slice::<Announcement>(&buf[..n]) {
                    seen.insert(a.device_id.clone(), (a, src));
                }
            }
            Err(ref e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => break,
        }
    }
    seen.into_values().collect()
}

/// A discovered device that passed pairing + signature checks.
#[derive(Debug, Clone)]
pub struct PairedPeer {
    pub announcement: Announcement,
    /// `src.ip():announcement.port` — where its sync relay lives.
    pub relay_addr: SocketAddr,
    pub peer: pai_sync::pair::SyncPeer,
}

/// Filter discovered announcements to *paired* devices with valid
/// signatures — the only endpoints worth syncing with.
pub fn paired_announcements(
    store: &Store,
    ids: &IdentityStore,
    found: Vec<(Announcement, SocketAddr)>,
) -> Result<Vec<PairedPeer>> {
    let peers = pai_sync::pair::list_peers(store)?;
    let mut out = Vec::new();
    for (a, src) in found {
        let Some(peer) = peers
            .iter()
            .find(|p| p.device_id.to_string() == a.device_id)
        else {
            continue; // not paired — ignored
        };
        if !verify_announcement(ids, &a, &peer.ed_pubkey) {
            tracing::warn!(device = %a.device_id, "dropping unverifiable announcement");
            continue;
        }
        out.push(PairedPeer {
            relay_addr: SocketAddr::new(src.ip(), a.port),
            announcement: a,
            peer: peer.clone(),
        });
    }
    Ok(out)
}

/// Bearer token this device presents to `peer`'s mesh relay:
/// `hex(peer_key)` — only devices that completed pairing can compute it.
pub fn token_for(secret: &x25519_dalek::StaticSecret, peer: &pai_sync::pair::SyncPeer) -> String {
    hex::encode(pai_sync::crypto::peer_key(
        secret,
        &x25519_dalek::PublicKey::from(peer.agree_pubkey),
    ))
}

/// Every bearer token a mesh relay should accept — one per paired
/// peer. Recompute per request so pairing changes apply live.
pub fn relay_tokens(store: &Store, secret: &x25519_dalek::StaticSecret) -> Result<Vec<String>> {
    Ok(pai_sync::pair::list_peers(store)?
        .iter()
        .map(|p| token_for(secret, p))
        .collect())
}
