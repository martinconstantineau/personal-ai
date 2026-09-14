//! Broker RPC over any `SyncTransport` — paired devices ask each other to
//! run workloads ("phone asks desktop to transcribe this").
//!
//! Requests and responses are ordinary sealed `SyncObject`s the sync
//! engine deliberately ignores (unknown key prefix):
//!
//! - `breq/<to-device>/<request-id>` — sealed [`BrokerRequest`]
//! - `bres/<to-device>/<request-id>` — sealed [`BrokerResponse`]
//!
//! Everything travels sealed under the shared vault key, so only paired
//! devices can make or read requests. The `writer`/`from` fields are
//! informational, not per-device proof — the vault is a group secret.
//!
//! Ops are strings (`"stt"`, `"tts"`, `"infer"`, `"describe"`); the
//! server side resolves them through an [`OpHandler`] the caller wires to
//! whatever providers that device actually runs.

use async_trait::async_trait;
use base64::Engine as _;
use pai_core::*;
use pai_sync::{crypto, SyncTransport};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};

const REQ_PREFIX: &str = "breq/";
const RES_PREFIX: &str = "bres/";

/// Sealed request payload.
#[derive(Debug, Serialize, Deserialize)]
struct BrokerRequest {
    v: u8,
    id: String,
    /// Informational sender id — see module docs on authenticity.
    from: String,
    op: String,
    /// Opaque op input (audio bytes, UTF-8 prompt, JSON args, …).
    payload_b64: String,
    created_at: String,
}

/// Sealed response payload.
#[derive(Debug, Serialize, Deserialize)]
struct BrokerResponse {
    v: u8,
    request_id: String,
    /// Informational responder id.
    from: String,
    ok: bool,
    payload_b64: String,
    error: Option<String>,
}

/// Executes one op on the serving device. Return value is opaque to the
/// transport — the caller and handler agree on encoding per op.
#[async_trait]
pub trait OpHandler: Send + Sync {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>>;
}

/// Client side: send one request to a peer and wait for its response.
pub struct BrokerClient<'a, T: SyncTransport + ?Sized> {
    transport: &'a T,
    vault: &'a [u8; 32],
    device: DeviceId,
}

impl<'a, T: SyncTransport + ?Sized> BrokerClient<'a, T> {
    pub fn new(transport: &'a T, vault: &'a [u8; 32], device: DeviceId) -> Self {
        Self {
            transport,
            vault,
            device,
        }
    }

    /// Push `breq/<to>/<id>` then poll `bres/<me>/<id>` until `timeout`.
    pub async fn call(
        &self,
        to: DeviceId,
        op: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let id = uuid::Uuid::new_v4().to_string();
        let req = BrokerRequest {
            v: 1,
            id: id.clone(),
            from: self.device.to_string(),
            op: op.to_string(),
            payload_b64: base64::engine::general_purpose::STANDARD.encode(payload),
            created_at: pai_storage::ts(&now()),
        };
        let key = format!("{REQ_PREFIX}{to}/{id}");
        let raw = serde_json::to_vec(&req).map_err(|e| Error::Sync(e.to_string()))?;
        let obj = SyncObject {
            key: key.clone(),
            ciphertext: crypto::seal(self.vault, key.as_bytes(), &raw)?,
            version: now().timestamp_millis().max(1) as u64,
            writer: self.device,
            updated_at: now(),
            tombstone: false,
        };
        self.transport.push(&obj).await?;

        // Poll for the response object addressed to us.
        let res_key = format!("{RES_PREFIX}{}/{id}", self.device);
        let deadline = Instant::now() + timeout;
        loop {
            if Instant::now() > deadline {
                return Err(Error::Sync(format!(
                    "broker: no response to {id} within {}s",
                    timeout.as_secs()
                )));
            }
            if let Some(obj) = self.transport.pull(&res_key).await? {
                let raw = crypto::open(self.vault, obj.key.as_bytes(), &obj.ciphertext)?;
                let res: BrokerResponse = serde_json::from_slice(&raw)
                    .map_err(|e| Error::Sync(format!("bad broker response: {e}")))?;
                if res.v != 1 {
                    return Err(Error::Sync(format!(
                        "unsupported broker response v{}",
                        res.v
                    )));
                }
                if !res.ok {
                    return Err(Error::Sync(format!(
                        "broker: peer failed {op}: {}",
                        res.error.unwrap_or_else(|| "unknown error".into())
                    )));
                }
                return base64::engine::general_purpose::STANDARD
                    .decode(&res.payload_b64)
                    .map_err(|e| Error::Sync(format!("bad broker payload: {e}")));
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
}

/// Server side: answer requests addressed to this device.
pub struct BrokerServer<'a, T: SyncTransport + ?Sized, H: OpHandler> {
    transport: &'a T,
    vault: &'a [u8; 32],
    device: DeviceId,
    handler: &'a H,
    /// Request ids already served — the transport has no delete op, so we
    /// track seen ids for the process lifetime.
    served: std::collections::HashSet<String>,
}

impl<'a, T: SyncTransport + ?Sized, H: OpHandler> BrokerServer<'a, T, H> {
    pub fn new(transport: &'a T, vault: &'a [u8; 32], device: DeviceId, handler: &'a H) -> Self {
        Self {
            transport,
            vault,
            device,
            handler,
            served: Default::default(),
        }
    }

    /// One pass over pending requests addressed to us. Returns how many
    /// were served — handy for tests and idle-loop logging.
    pub async fn serve_once(&mut self) -> Result<usize> {
        let want = format!("{REQ_PREFIX}{}/", self.device);
        let metas = self.transport.list().await?;
        let mut served = 0usize;
        for meta in metas {
            if !meta.key.starts_with(&want) || meta.tombstone {
                continue;
            }
            let req_id = meta.key[want.len()..].to_string();
            if self.served.contains(&req_id) {
                continue;
            }
            let Some(obj) = self.transport.pull(&meta.key).await? else {
                continue;
            };
            let raw = match crypto::open(self.vault, obj.key.as_bytes(), &obj.ciphertext) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key = %obj.key, error = %e, "broker: unopenable request");
                    continue;
                }
            };
            let req: BrokerRequest = match serde_json::from_slice(&raw) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(key = %obj.key, error = %e, "broker: bad request payload");
                    continue;
                }
            };
            if req.v != 1 {
                continue;
            }
            let from =
                DeviceId(uuid::Uuid::parse_str(&req.from).unwrap_or_else(|_| uuid::Uuid::nil()));
            let (ok, payload_b64, error) =
                match base64::engine::general_purpose::STANDARD.decode(&req.payload_b64) {
                    Ok(bytes) => match self.handler.handle(&req.op, &bytes).await {
                        Ok(out) => (
                            true,
                            base64::engine::general_purpose::STANDARD.encode(&out),
                            None,
                        ),
                        Err(e) => (false, String::new(), Some(e.to_string())),
                    },
                    Err(e) => (
                        false,
                        String::new(),
                        Some(format!("bad request payload: {e}")),
                    ),
                };
            let res = BrokerResponse {
                v: 1,
                request_id: req.id.clone(),
                from: self.device.to_string(),
                ok,
                payload_b64,
                error,
            };
            let key = format!("{RES_PREFIX}{from}/{}", req.id);
            let raw = serde_json::to_vec(&res).map_err(|e| Error::Sync(e.to_string()))?;
            let obj = SyncObject {
                key: key.clone(),
                ciphertext: crypto::seal(self.vault, key.as_bytes(), &raw)?,
                version: now().timestamp_millis().max(1) as u64,
                writer: self.device,
                updated_at: now(),
                tombstone: false,
            };
            self.transport.push(&obj).await?;
            self.served.insert(req_id);
            served += 1;
        }
        Ok(served)
    }

    /// Poll-and-serve forever — the `pai broker serve` loop.
    pub async fn serve(&mut self, poll: Duration) -> ! {
        loop {
            if let Err(e) = self.serve_once().await {
                tracing::warn!(error = %e, "broker serve pass failed");
            }
            tokio::time::sleep(poll).await;
        }
    }
}
