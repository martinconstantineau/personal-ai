//! Guest requests: capability-token-authenticated ops from devices
//! that are NOT vault members, over any `SyncTransport`.
//!
//! Objects (unknown prefixes — the sync engine ignores them):
//!
//! - `greq/<to-device>/<request-id>` — unsealed [`GuestRequest`]: the
//!   embedded [`Capability`] token is the authorization, plus an
//!   ephemeral X25519 pubkey for the response channel.
//! - `gres/<reply-tag>/<request-id>` — [`GuestResponse`] sealed to
//!   that ephemeral key (the server generates its own ephemeral pair;
//!   `srv_eph_pub` rides the unsealed envelope). Reply-tag is a
//!   truncated hash of token+eph so the poll key reveals neither.
//!
//! Grantee-bound tokens (`grantee_key` set) also require `sig`: an
//! Ed25519 signature by the grantee over the canonical request —
//! args/op/app tamper-evident. Bearer tokens carry no signature: dir
//! write access plus token possession is the (documented) auth
//! surface, matching the transport-dir trust boundary.

use crate::{Action, Capability, ShareError, ShareResult, ShareStore};
use pai_core::{Device, DeviceId, Error, Result, SyncObject};
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_sync::{crypto, pair, SyncTransport};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Guest-side request signer — a closure over the grantee's device
/// key (e.g. `|m| ids.sign(dev, key_dir, m)`).
pub type RequestSigner<'a> = dyn Fn(&[u8]) -> ShareResult<Vec<u8>> + 'a;

/// Server-side guest op executor — the caller wires `app-run` to its
/// sandbox (`pai_apps::app_run_op`-shaped).
pub type GuestHandler<'a> = dyn Fn(&str, &str, &[String]) -> Result<Vec<u8>> + 'a;

const REQ_PREFIX: &str = "greq/";
const RES_PREFIX: &str = "gres/";

/// Unsealed guest request object (v1).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuestRequest {
    pub v: u8,
    pub id: String,
    /// `app-run`/`app-read`/`app-write` — each maps to a required
    /// token action via [`op_action`].
    pub op: String,
    pub app_id: String,
    pub args: Vec<String>,
    /// The capability token authorizing this request.
    pub capability: Capability,
    /// Guest's ephemeral X25519 pubkey (hex) — response sealed to it.
    pub eph_pub: String,
    /// Grantee Ed25519 signature (hex) over `signing_payload` —
    /// required when the token is grantee-bound, absent for bearer.
    #[serde(default)]
    pub sig: Option<String>,
    /// ms epoch after which the server skips the request; 0 = never.
    #[serde(default)]
    pub expires_at_ms: u64,
}

impl GuestRequest {
    /// Bytes the grantee signature covers — everything but the token
    /// (already signed by the issuer) and `sig` itself.
    fn signing_payload(&self) -> Vec<u8> {
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            self.id,
            self.op,
            self.app_id,
            self.args.join("\0"),
            self.eph_pub,
            self.capability.token_id,
        )
        .into_bytes()
    }
}

/// Sealed response body (decrypts to this JSON).
#[derive(Debug, Serialize, Deserialize)]
pub struct GuestResponse {
    pub v: u8,
    pub request_id: String,
    pub ok: bool,
    pub payload_b64: String,
    pub error: Option<String>,
}

/// Unsealed `gres` envelope — `sealed` opens only with the request's
/// ephemeral secret.
#[derive(Debug, Serialize, Deserialize)]
struct GuestEnvelope {
    v: u8,
    request_id: String,
    srv_eph_pub: String,
    sealed_b64: String,
}

/// Poll key for the guest: `gres/t<hash>/<id>` — reveals neither the
/// token id nor the ephemeral pub to other readers of the transport.
fn reply_tag(token_id: &str, eph_pub: &[u8; 32]) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(token_id.as_bytes());
    h.update(eph_pub);
    format!("t{}", hex::encode(&h.finalize()[..8]))
}

fn obj(key: &str, writer: DeviceId, raw: &[u8]) -> SyncObject {
    SyncObject {
        key: key.to_string(),
        ciphertext: raw.to_vec(),
        version: pai_core::now().timestamp_millis().max(1) as u64,
        writer,
        updated_at: pai_core::now(),
        tombstone: false,
    }
}

/// Guest side: present a capability token to a device and wait for the
/// sealed response. `signer` signs the request when the token is
/// grantee-bound (the caller's device key, e.g. a closure over
/// `IdentityStore::sign`); pass `None` for bearer tokens.
/// Ops a guest may request and the [`Action`] each requires — the
/// server maps the same table, so a token covering `read` can never
/// run `app-run`.
pub fn op_action(op: &str) -> Option<Action> {
    match op {
        "app-run" => Some(Action::Exec),
        "app-read" => Some(Action::Read),
        "app-write" => Some(Action::Write),
        _ => None,
    }
}

pub async fn call_guest<T: SyncTransport + ?Sized>(
    transport: &T,
    to: DeviceId,
    capability: Capability,
    op: &str,
    args: &[String],
    timeout: Duration,
    signer: Option<&RequestSigner<'_>>,
) -> Result<Vec<u8>> {
    if op_action(op).is_none() {
        return Err(Error::InvalidInput(format!("unknown guest op '{op}'")));
    }
    use base64::Engine as _;
    let b64 = base64::engine::general_purpose::STANDARD;

    let eph_secret = x25519_dalek::StaticSecret::random_from_rng(rand_core::OsRng);
    let eph_pub = x25519_dalek::PublicKey::from(&eph_secret);
    let id = uuid::Uuid::new_v4().to_string();
    let mut req = GuestRequest {
        v: 1,
        id: id.clone(),
        op: op.to_string(),
        app_id: capability.app_id.clone(),
        args: args.to_vec(),
        capability,
        eph_pub: hex::encode(eph_pub.as_bytes()),
        sig: None,
        expires_at_ms: (pai_core::now()
            + chrono::Duration::from_std(timeout + Duration::from_secs(60)).unwrap_or_default())
        .timestamp_millis()
        .max(1) as u64,
    };
    if req.capability.grantee_key.is_some() {
        let sign = signer.ok_or_else(|| {
            Error::InvalidInput(
                "grantee-bound token needs a request signature — pass `signer`".into(),
            )
        })?;
        req.sig = Some(hex::encode(
            sign(&req.signing_payload()).map_err(|e| Error::Other(e.to_string()))?,
        ));
    }
    let tag = reply_tag(&req.capability.token_id, eph_pub.as_bytes());
    let key = format!("{REQ_PREFIX}{to}/{id}");
    let raw = serde_json::to_vec(&req).map_err(|e| Error::Sync(e.to_string()))?;
    transport
        .push(&obj(&key, req.capability.issued_by, &raw))
        .await?;

    let res_key = format!("{RES_PREFIX}{tag}/{id}");
    let deadline = Instant::now() + timeout;
    loop {
        if Instant::now() > deadline {
            return Err(Error::Sync(format!(
                "guest: no response to {id} within {}s",
                timeout.as_secs()
            )));
        }
        if let Some(obj) = transport.pull(&res_key).await? {
            transport.delete(&res_key).await.ok();
            let env: GuestEnvelope = serde_json::from_slice(&obj.ciphertext)
                .map_err(|e| Error::Sync(format!("bad guest envelope: {e}")))?;
            if env.v != 1 {
                return Err(Error::Sync(format!("guest envelope v{}", env.v)));
            }
            let srv_pub = hex::decode(&env.srv_eph_pub)
                .map_err(|e| Error::Sync(format!("bad srv_eph_pub: {e}")))?;
            let srv_pub: [u8; 32] = srv_pub
                .try_into()
                .map_err(|_| Error::Sync("srv_eph_pub not 32 bytes".into()))?;
            let shared = crypto::peer_key(&eph_secret, &x25519_dalek::PublicKey::from(srv_pub));
            let sealed = b64
                .decode(&env.sealed_b64)
                .map_err(|e| Error::Sync(format!("bad sealed b64: {e}")))?;
            let raw = crypto::open(&shared, res_key.as_bytes(), &sealed)?;
            let res: GuestResponse = serde_json::from_slice(&raw)
                .map_err(|e| Error::Sync(format!("bad guest response: {e}")))?;
            if !res.ok {
                return Err(Error::Sync(format!(
                    "guest: peer failed {}: {}",
                    req.op,
                    res.error.unwrap_or_else(|| "unknown error".into())
                )));
            }
            return b64
                .decode(&res.payload_b64)
                .map_err(|e| Error::Sync(format!("bad guest payload: {e}")));
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

/// Server side for guest requests. Resolves issuer pubkeys from this
/// device (self) plus paired peers (`sync_peers`), verifies the token
/// against [`ShareStore`], runs `handler(op, app_id, args)`, and seals
/// the response back to the request's ephemeral key.
pub struct GuestServer {
    device: Device,
    store: Arc<Store>,
    ids: IdentityStore,
    shares: ShareStore,
}

impl GuestServer {
    pub fn new(device: Device, store: Arc<Store>, data_dir: &std::path::Path) -> Self {
        Self {
            device,
            ids: IdentityStore::new(store.clone()),
            store,
            shares: ShareStore::new(data_dir),
        }
    }

    /// Issuer pubkey for `issued_by`: own device or a paired peer.
    fn issuer_pubkey(&self, issued_by: DeviceId) -> Option<[u8; 32]> {
        if issued_by == self.device.id {
            return self.device.public_key.as_slice().try_into().ok();
        }
        pair::list_peers(&self.store)
            .ok()?
            .iter()
            .find(|p| p.device_id == issued_by)
            .map(|p| p.ed_pubkey)
    }

    /// Verify one parsed request: issuer resolvable, token signature +
    /// expiry + revocation + `exec` action, app binding, and the
    /// grantee request-signature when the token is bound.
    fn verify(&self, req: &GuestRequest) -> ShareResult<()> {
        let action = op_action(&req.op).ok_or_else(|| {
            ShareError::InvalidInput(format!("unsupported guest op {:?}", req.op))
        })?;
        if req.v != 1 {
            return Err(ShareError::InvalidInput(format!(
                "unsupported guest request v{}",
                req.v
            )));
        }
        let cap = &req.capability;
        if cap.app_id != req.app_id {
            return Err(ShareError::InvalidInput(
                "token app_id does not match request".into(),
            ));
        }
        let issuer = self
            .issuer_pubkey(cap.issued_by)
            .ok_or_else(|| ShareError::InvalidInput(format!("unknown issuer {}", cap.issued_by)))?;
        if let Some(grantee_hex) = &cap.grantee_key {
            let sig_hex = req
                .sig
                .as_deref()
                .ok_or_else(|| ShareError::InvalidInput("grantee-bound request unsigned".into()))?;
            let gk = hex::decode(grantee_hex)
                .map_err(|e| ShareError::InvalidInput(format!("grantee_key: {e}")))?;
            let gk: [u8; 32] = gk
                .try_into()
                .map_err(|_| ShareError::InvalidInput("grantee_key not 32 bytes".into()))?;
            let sig =
                hex::decode(sig_hex).map_err(|e| ShareError::InvalidInput(format!("sig: {e}")))?;
            let ok = self
                .ids
                .verify_with_key(&gk, &req.signing_payload(), &sig)?;
            if !ok {
                return Err(ShareError::BadSignature(format!(
                    "grantee request sig (token {})",
                    cap.token_id
                )));
            }
        }
        self.shares.verify(&self.ids, cap, &issuer, action)?;
        Ok(())
    }

    /// One pass over `greq/<me>/`. Each request is verified, run through
    /// `handler(op, app_id, args)`, sealed to the guest's ephemeral key
    /// as `gres/<tag>/<id>`, and deleted. Returns how many were served.
    pub async fn serve_once<T: SyncTransport + ?Sized>(
        &self,
        transport: &T,
        handler: &GuestHandler<'_>,
    ) -> Result<usize> {
        use base64::Engine as _;
        let b64 = base64::engine::general_purpose::STANDARD;
        let want = format!("{REQ_PREFIX}{}/", self.device.id);
        let mut served = 0usize;
        for meta in transport.list().await? {
            if !meta.key.starts_with(&want) || meta.tombstone {
                continue;
            }
            let Some(o) = transport.pull(&meta.key).await? else {
                continue;
            };
            let req: GuestRequest = match serde_json::from_slice(&o.ciphertext) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key = %o.key, error = %e, "guest: bad request");
                    transport.delete(&meta.key).await.ok();
                    continue;
                }
            };
            if req.expires_at_ms > 0
                && pai_core::now().timestamp_millis() as u64 > req.expires_at_ms
            {
                transport.delete(&meta.key).await.ok();
                continue;
            }
            // Verify → run → always produce a response object so the
            // guest gets a real error instead of a timeout.
            let (ok, payload_b64, error) = match self.verify(&req) {
                Ok(()) => match handler(&req.op, &req.app_id, &req.args) {
                    Ok(out) => (true, b64.encode(&out), None),
                    Err(e) => (false, String::new(), Some(e.to_string())),
                },
                Err(e) => (false, String::new(), Some(e.to_string())),
            };
            let res = GuestResponse {
                v: 1,
                request_id: req.id.clone(),
                ok,
                payload_b64,
                error,
            };
            let res_raw = serde_json::to_vec(&res).map_err(|e| Error::Sync(e.to_string()))?;
            let res_key = format!(
                "{RES_PREFIX}{}/{}",
                reply_tag(
                    &req.capability.token_id,
                    &hex::decode(&req.eph_pub)
                        .ok()
                        .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
                        .unwrap_or_default()
                ),
                req.id
            );
            let srv_secret = x25519_dalek::StaticSecret::random_from_rng(rand_core::OsRng);
            let srv_pub = x25519_dalek::PublicKey::from(&srv_secret);
            let guest_pub = hex::decode(&req.eph_pub)
                .ok()
                .and_then(|v| <[u8; 32]>::try_from(v.as_slice()).ok())
                .map(x25519_dalek::PublicKey::from)
                .unwrap_or_else(|| x25519_dalek::PublicKey::from([0u8; 32]));
            let shared = crypto::peer_key(&srv_secret, &guest_pub);
            let sealed = crypto::seal(&shared, res_key.as_bytes(), &res_raw)?;
            let env = GuestEnvelope {
                v: 1,
                request_id: req.id.clone(),
                srv_eph_pub: hex::encode(srv_pub.as_bytes()),
                sealed_b64: b64.encode(sealed),
            };
            let env_raw = serde_json::to_vec(&env).map_err(|e| Error::Sync(e.to_string()))?;
            transport
                .push(&obj(&res_key, self.device.id, &env_raw))
                .await?;
            transport.delete(&meta.key).await.ok();
            served += 1;
        }
        Ok(served)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Action, GrantSpec};
    use pai_core::{DeviceCapabilities, NetworkState, Platform};
    use pai_sync::FolderTransport;
    use std::path::{Path, PathBuf};

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pai-guest-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// A device with an identity store + key dir (the serving side, and
    /// any grantee that signs requests with a raw key).
    fn device(root: &Path, name: &str) -> (IdentityStore, Device, PathBuf) {
        let ids = IdentityStore::new(Arc::new(Store::in_memory().unwrap()));
        let user = ids.create_user("t").unwrap();
        let key_dir = root.join(format!("keys-{name}"));
        let dev = ids
            .register_device(
                user.id,
                name,
                Platform::Linux,
                DeviceCapabilities {
                    cpu_cores: 4,
                    ram_bytes: 1 << 30,
                    gpu_vram_bytes: None,
                    gpu_name: None,
                    npu_available: false,
                    on_battery: None,
                    thermal_throttled: None,
                    network: NetworkState::Unknown,
                    available_models: vec![],
                    supported_capabilities: vec![],
                },
                &key_dir,
            )
            .unwrap();
        (ids, dev, key_dir)
    }

    fn echo_handler(op: &str, app: &str, args: &[String]) -> Result<Vec<u8>> {
        Ok(format!("{op}:{app}:{}", args.join(",")).into_bytes())
    }

    /// serve_once is one pass — poll it like `broker serve` does.
    async fn serve_loop(server: &GuestServer, transport: &FolderTransport) {
        for _ in 0..60 {
            let n = server.serve_once(transport, &echo_handler).await.unwrap();
            if n > 0 {
                // keep serving briefly so a second request lands too
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn bearer_token_roundtrip() {
        let srv_dir = tmp();
        let shared = tmp();
        let (ids, dev, key_dir) = device(&srv_dir, "srv");
        let store = Arc::new(Store::in_memory().unwrap());
        let shares = ShareStore::new(&srv_dir);
        let cap = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("echo-app", vec![Action::Exec]),
            )
            .unwrap();
        let dev_id = dev.id;
        let server = GuestServer::new(dev, store, &srv_dir);
        // GuestServer's ids is a *different* store — verification needs
        // only the issuer pubkey + verify_with_key, which is stateless.
        let transport = FolderTransport::new(shared).unwrap();
        let srv = serve_loop(&server, &transport);

        let args = vec!["a".to_string(), "b".to_string()];
        let call = call_guest(
            &transport,
            dev_id,
            cap,
            "app-run",
            &args,
            Duration::from_secs(5),
            None,
        );
        let (_, out) = tokio::join!(srv, call);
        assert_eq!(out.unwrap(), b"app-run:echo-app:a,b");
        let _ = std::fs::remove_dir_all(&srv_dir);
    }

    #[tokio::test]
    async fn grantee_bound_requires_request_sig() {
        let srv_dir = tmp();
        let shared = tmp();
        let (ids, dev, key_dir) = device(&srv_dir, "srv");
        let store = Arc::new(Store::in_memory().unwrap());
        let shares = ShareStore::new(&srv_dir);
        let grantee_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let grantee_pub: [u8; 32] = grantee_key.verifying_key().to_bytes();
        let cap = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec {
                    grantee_key: Some(grantee_pub),
                    ..GrantSpec::for_app("echo-app", vec![Action::Exec])
                },
            )
            .unwrap();
        let dev_id = dev.id;
        let server = GuestServer::new(dev.clone(), store, &srv_dir);
        let transport = FolderTransport::new(shared).unwrap();

        // call_guest refuses client-side (a bound token needs the
        // grantee key to sign). The server-side rejection is covered
        // by a forged request in app_binding_and_unknown_issuer.
        let bad = call_guest(
            &transport,
            dev_id,
            cap.clone(),
            "app-run",
            &[],
            Duration::from_secs(5),
            None,
        )
        .await;
        assert!(bad.err().unwrap().to_string().contains("signer"));

        // Signed request → ok.
        let signer = move |msg: &[u8]| -> ShareResult<Vec<u8>> {
            use ed25519_dalek::Signer;
            Ok(grantee_key.sign(msg).to_bytes().to_vec())
        };
        let srv = serve_loop(&server, &transport);
        let args = vec!["x".to_string()];
        let good = call_guest(
            &transport,
            dev_id,
            cap,
            "app-run",
            &args,
            Duration::from_secs(5),
            Some(&signer),
        );
        let (_, out) = tokio::join!(srv, good);
        assert_eq!(out.unwrap(), b"app-run:echo-app:x");
        let _ = std::fs::remove_dir_all(&srv_dir);
    }

    #[tokio::test]
    async fn revoked_token_rejected() {
        let srv_dir = tmp();
        let shared = tmp();
        let (ids, dev, key_dir) = device(&srv_dir, "srv");
        let store = Arc::new(Store::in_memory().unwrap());
        let shares = ShareStore::new(&srv_dir);
        let cap = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("a", vec![Action::Exec]),
            )
            .unwrap();
        let dev_id = dev.id;
        let server = GuestServer::new(dev, store, &srv_dir);
        let transport = FolderTransport::new(shared).unwrap();
        shares.revoke(&cap.token_id).unwrap();
        let srv = serve_loop(&server, &transport);
        let args: Vec<String> = vec![];
        let call = call_guest(
            &transport,
            dev_id,
            cap,
            "app-run",
            &args,
            Duration::from_secs(5),
            None,
        );
        let (_, out) = tokio::join!(srv, call);
        assert!(out.err().unwrap().to_string().contains("revoked"));
        let _ = std::fs::remove_dir_all(&srv_dir);
    }

    #[tokio::test]
    async fn op_maps_to_required_action() {
        let srv_dir = tmp();
        let shared = tmp();
        let (ids, dev, key_dir) = device(&srv_dir, "srv");
        let store = Arc::new(Store::in_memory().unwrap());
        let shares = ShareStore::new(&srv_dir);
        // exec-only token: app-read must be refused even though the
        // token is otherwise valid.
        let cap = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("echo-app", vec![Action::Exec]),
            )
            .unwrap();
        let dev_id = dev.id;
        let server = GuestServer::new(dev.clone(), store.clone(), &srv_dir);
        let transport = FolderTransport::new(shared).unwrap();

        let srv = serve_loop(&server, &transport);
        let path_args = vec!["files/seed.txt".to_string()];
        let call = call_guest(
            &transport,
            dev_id,
            cap.clone(),
            "app-read",
            &path_args,
            Duration::from_secs(5),
            None,
        );
        let (_, out) = tokio::join!(srv, call);
        let err = out.err().unwrap().to_string();
        assert!(err.contains("does not grant"), "unexpected: {err}");

        // A read-capable token passes the same op.
        let cap2 = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("echo-app", vec![Action::Read]),
            )
            .unwrap();
        let srv = serve_loop(&server, &transport);
        let call = call_guest(
            &transport,
            dev_id,
            cap2,
            "app-read",
            &path_args,
            Duration::from_secs(5),
            None,
        );
        let (_, out) = tokio::join!(srv, call);
        assert_eq!(out.unwrap(), b"app-read:echo-app:files/seed.txt");

        // Unknown ops never reach the handler.
        assert!(call_guest(
            &transport,
            dev_id,
            cap,
            "app-delete",
            &[],
            Duration::from_secs(1),
            None
        )
        .await
        .is_err());
        let _ = std::fs::remove_dir_all(&srv_dir);
    }

    #[tokio::test]
    async fn app_binding_and_unknown_issuer_rejected() {
        let srv_dir = tmp();
        let shared = tmp();
        let (ids, dev, key_dir) = device(&srv_dir, "srv");
        let store = Arc::new(Store::in_memory().unwrap());
        let shares = ShareStore::new(&srv_dir);
        let cap = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("a", vec![Action::Exec]),
            )
            .unwrap();
        // Grantee-bound token minted by the same issuer — replayed
        // unsigned below, server must refuse (verify before handler).
        let grantee_key = ed25519_dalek::SigningKey::generate(&mut rand_core::OsRng);
        let bound = shares
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec {
                    grantee_key: Some(grantee_key.verifying_key().to_bytes()),
                    ..GrantSpec::for_app("a", vec![Action::Exec])
                },
            )
            .unwrap();
        let dev_id = dev.id;
        let server = GuestServer::new(dev, store, &srv_dir);
        let transport = FolderTransport::new(shared).unwrap();

        // Forged request: token for app "a", request for "other" —
        // hand-built since call_guest always binds correctly.
        let forged = GuestRequest {
            v: 1,
            id: uuid::Uuid::new_v4().to_string(),
            op: "app-run".into(),
            app_id: "other".into(),
            args: vec![],
            capability: cap.clone(),
            eph_pub: hex::encode([7u8; 32]),
            sig: None,
            expires_at_ms: 0,
        };
        let key = format!("{REQ_PREFIX}{dev_id}/{}", forged.id);
        let raw = serde_json::to_vec(&forged).unwrap();
        transport
            .push(&obj(&key, forged.capability.issued_by, &raw))
            .await
            .unwrap();
        let n = server.serve_once(&transport, &echo_handler).await.unwrap();
        assert_eq!(n, 1); // served — as an error response
                          // The forged reply lands under the tag derived from token+eph.
        let tag = reply_tag(&forged.capability.token_id, &[7u8; 32]);
        let res = transport
            .pull(&format!("{RES_PREFIX}{tag}/{}", forged.id))
            .await
            .unwrap()
            .unwrap();
        let env: GuestEnvelope = serde_json::from_slice(&res.ciphertext).unwrap();
        assert_eq!(env.request_id, forged.id);

        let forged2 = GuestRequest {
            v: 1,
            id: uuid::Uuid::new_v4().to_string(),
            op: "app-run".into(),
            app_id: "a".into(),
            args: vec![],
            capability: bound.clone(),
            eph_pub: hex::encode([9u8; 32]),
            sig: None,
            expires_at_ms: 0,
        };
        let key = format!("{REQ_PREFIX}{dev_id}/{}", forged2.id);
        let raw = serde_json::to_vec(&forged2).unwrap();
        transport
            .push(&obj(&key, bound.issued_by, &raw))
            .await
            .unwrap();
        let n = server.serve_once(&transport, &echo_handler).await.unwrap();
        assert_eq!(n, 1);
        let tag = reply_tag(&bound.token_id, &[9u8; 32]);
        let res = transport
            .pull(&format!("{RES_PREFIX}{tag}/{}", forged2.id))
            .await
            .unwrap()
            .unwrap();
        let env: GuestEnvelope = serde_json::from_slice(&res.ciphertext).unwrap();
        assert_eq!(env.request_id, forged2.id);
        let _ = std::fs::remove_dir_all(&srv_dir);
    }
}
