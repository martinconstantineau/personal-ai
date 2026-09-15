//! Broker RPC over any `SyncTransport` — paired devices ask each other to
//! run workloads ("phone asks desktop to transcribe this").
//!
//! Requests and responses are ordinary sealed `SyncObject`s the sync
//! engine deliberately ignores (unknown key prefix):
//!
//! - `breq/<to-device>/<request-id>` — sealed [`BrokerRequest`]
//! - `bres/<to-device>/<request-id>` — sealed final [`BrokerResponse`]
//! - `bres/<to-device>/<request-id>/<seq>` — sealed [`BrokerChunk`]
//!   (streaming calls only; the exact `bres/<to>/<id>` key still lands
//!   last with ok/error)
//! - `bcap/<device>` — sealed [`BrokerCaps`] capability announcement,
//!   refreshed by `serve`; lets callers pick a peer by op instead of id
//!
//! Everything travels sealed under the shared vault key, so only paired
//! devices can make or read requests. The `writer`/`from` fields are
//! informational, not per-device proof — the vault is a group secret.
//!
//! Requests carry `expires_at_ms`: a worker won't execute work the
//! caller already gave up on, and expired `breq`s are deleted on sight.
//! Served requests, consumed responses, and stream chunks are deleted
//! too — transports without `delete` just accumulate as before.
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
const CAP_PREFIX: &str = "bcap/";
/// How long a capability announcement stays authoritative — `serve`
/// re-announces well inside this window.
const CAP_TTL: Duration = Duration::from_secs(300);
/// Periodic re-announce cadence inside `serve`.
const CAP_REANNOUNCE: Duration = Duration::from_secs(60);
/// Margin added to the caller's timeout when deriving `expires_at_ms` —
/// a response that lands shortly after the caller gave up is still
/// allowed to be produced (and GC'd unread), just not started late.
const EXPIRY_MARGIN: Duration = Duration::from_secs(60);

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
    /// Ask for a streamed response (chunk objects before the final one).
    #[serde(default)]
    stream: bool,
    /// ms epoch after which workers skip (and delete) this request.
    /// 0 = never expires.
    #[serde(default)]
    expires_at_ms: u64,
    created_at: String,
}

/// Sealed response payload — the final `bres/<to>/<id>` object.
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

/// One streamed chunk — `bres/<to>/<id>/<seq>` objects, in seq order.
#[derive(Debug, Serialize, Deserialize)]
struct BrokerChunk {
    v: u8,
    seq: u32,
    payload_b64: String,
}

/// Self-reported placement hint inside `bcap` — registered device
/// capabilities plus the live in-flight op count. Absent on pre-V5
/// announcements; `Option` fields deserialize as `None` so the wire
/// stays compatible.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DeviceLoad {
    /// Ops currently executing on the device.
    pub busy: u32,
    pub on_battery: Option<bool>,
    pub thermal_throttled: Option<bool>,
    pub ram_bytes: u64,
    pub cpu_cores: u32,
}

impl DeviceLoad {
    /// Higher = better placement candidate. Battery and thermal state
    /// dominate — an idle low-RAM wall-powered device beats a beefy
    /// one that's unplugged or throttled. `None` fields are neutral.
    fn score(&self) -> i64 {
        let mut s = (self.ram_bytes / (1 << 30)) as i64 + self.cpu_cores as i64 * 2;
        s -= self.busy as i64 * 10;
        if self.on_battery == Some(true) {
            s -= 1000;
        }
        if self.thermal_throttled == Some(true) {
            s -= 500;
        }
        s
    }
}

/// Capability announcement — `bcap/<device>`, sealed like everything
/// else. `ts` is the announcer's clock; stale announcements are ignored.
#[derive(Debug, Serialize, Deserialize)]
struct BrokerCaps {
    v: u8,
    device: String,
    ops: Vec<String>,
    ts: String,
    load: Option<DeviceLoad>,
}

/// Executes one op on the serving device. Return value is opaque to the
/// transport — the caller and handler agree on encoding per op.
#[async_trait]
pub trait OpHandler: Send + Sync {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>>;

    /// Streaming variant — emit chunks through `tx` as they're produced.
    /// Default: run `handle` and emit the whole result as one chunk.
    async fn handle_stream(
        &self,
        op: &str,
        payload: &[u8],
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        let out = self.handle(op, payload).await?;
        let _ = tx.send(out).await;
        Ok(())
    }
}

/// Client side: send one request to a peer and wait for its response.
pub struct BrokerClient<'a, T: SyncTransport + ?Sized> {
    transport: &'a T,
    vault: &'a [u8; 32],
    device: DeviceId,
}

fn seal_obj(vault: &[u8; 32], writer: DeviceId, key: &str, raw: &[u8]) -> Result<SyncObject> {
    Ok(SyncObject {
        key: key.to_string(),
        ciphertext: crypto::seal(vault, key.as_bytes(), raw)?,
        version: now().timestamp_millis().max(1) as u64,
        writer,
        updated_at: now(),
        tombstone: false,
    })
}

impl<'a, T: SyncTransport + ?Sized> BrokerClient<'a, T> {
    pub fn new(transport: &'a T, vault: &'a [u8; 32], device: DeviceId) -> Self {
        Self {
            transport,
            vault,
            device,
        }
    }

    /// Find the best device that recently announced it can run `op`.
    /// Scores on the announcer's [`DeviceLoad`] — battery/thermal
    /// dominate, then in-flight ops, then RAM/cores — with the lowest
    /// device id as the deterministic tiebreak so callers agree.
    pub async fn find_peer(&self, op: &str) -> Result<Option<DeviceId>> {
        let mut best: Option<(i64, DeviceId)> = None;
        for meta in self.transport.list().await? {
            if !meta.key.starts_with(CAP_PREFIX) || meta.tombstone {
                continue;
            }
            let Some(obj) = self.transport.pull(&meta.key).await? else {
                continue;
            };
            let Ok(raw) = crypto::open(self.vault, obj.key.as_bytes(), &obj.ciphertext) else {
                continue;
            };
            let Ok(caps) = serde_json::from_slice::<BrokerCaps>(&raw) else {
                continue;
            };
            if caps.v != 1 || !caps.ops.iter().any(|o| o == op) {
                continue;
            }
            // Freshness: announcement must be within CAP_TTL.
            let ts = pai_storage::parse_ts(&caps.ts);
            if now() - ts > chrono::Duration::from_std(CAP_TTL).unwrap_or_default() {
                continue;
            }
            let Ok(dev) = uuid::Uuid::parse_str(&caps.device).map(DeviceId) else {
                continue;
            };
            let score = caps.load.unwrap_or_default().score();
            let better = match best {
                None => true,
                Some((bs, bd)) => score > bs || (score == bs && dev.0 < bd.0),
            };
            if better {
                best = Some((score, dev));
            }
        }
        Ok(best.map(|(_, d)| d))
    }

    async fn push_request(
        &self,
        to: DeviceId,
        op: &str,
        payload: &[u8],
        timeout: Duration,
        stream: bool,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let req = BrokerRequest {
            v: 1,
            id: id.clone(),
            from: self.device.to_string(),
            op: op.to_string(),
            payload_b64: base64::engine::general_purpose::STANDARD.encode(payload),
            stream,
            expires_at_ms: (now()
                + chrono::Duration::from_std(timeout + EXPIRY_MARGIN).unwrap_or_default())
            .timestamp_millis()
            .max(1) as u64,
            created_at: pai_storage::ts(&now()),
        };
        let key = format!("{REQ_PREFIX}{to}/{id}");
        let raw = serde_json::to_vec(&req).map_err(|e| Error::Sync(e.to_string()))?;
        self.transport
            .push(&seal_obj(self.vault, self.device, &key, &raw)?)
            .await?;
        Ok(id)
    }

    /// Push `breq/<to>/<id>` then poll `bres/<me>/<id>` until `timeout`.
    /// The response object is deleted once consumed (best-effort GC).
    pub async fn call(
        &self,
        to: DeviceId,
        op: &str,
        payload: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>> {
        let id = self.push_request(to, op, payload, timeout, false).await?;
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
                let res = self.open_response(&obj)?;
                self.transport.delete(&res_key).await.ok();
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

    /// Streaming call — chunks land at `bres/<me>/<id>/<seq>` and are
    /// handed to `on_chunk` in order as they arrive; the final
    /// `bres/<me>/<id>` still carries ok/error (+ payload for handlers
    /// that put a summary there). Consumed objects are deleted.
    pub async fn call_stream(
        &self,
        to: DeviceId,
        op: &str,
        payload: &[u8],
        timeout: Duration,
        on_chunk: &mut dyn FnMut(&[u8]),
    ) -> Result<Vec<u8>> {
        let id = self.push_request(to, op, payload, timeout, true).await?;
        let final_key = format!("{RES_PREFIX}{}/{id}", self.device);
        let chunk_prefix = format!("{final_key}/");
        let deadline = Instant::now() + timeout;
        let mut next_seq = 0u32;
        loop {
            if Instant::now() > deadline {
                return Err(Error::Sync(format!(
                    "broker: no response to {id} within {}s",
                    timeout.as_secs()
                )));
            }
            // Deliver contiguous chunks starting at next_seq.
            for meta in self.transport.list().await? {
                if !meta.key.starts_with(&chunk_prefix) || meta.tombstone {
                    continue;
                }
                let Some(obj) = self.transport.pull(&meta.key).await? else {
                    continue;
                };
                let Ok(raw) = crypto::open(self.vault, obj.key.as_bytes(), &obj.ciphertext) else {
                    continue;
                };
                let Ok(chunk) = serde_json::from_slice::<BrokerChunk>(&raw) else {
                    continue;
                };
                if chunk.v != 1 || chunk.seq != next_seq {
                    continue; // wait for the gap to fill
                }
                if let Ok(bytes) =
                    base64::engine::general_purpose::STANDARD.decode(&chunk.payload_b64)
                {
                    on_chunk(&bytes);
                }
                self.transport.delete(&meta.key).await.ok();
                next_seq += 1;
            }
            if let Some(obj) = self.transport.pull(&final_key).await? {
                let res = self.open_response(&obj)?;
                self.transport.delete(&final_key).await.ok();
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
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }

    fn open_response(&self, obj: &SyncObject) -> Result<BrokerResponse> {
        let raw = crypto::open(self.vault, obj.key.as_bytes(), &obj.ciphertext)?;
        let res: BrokerResponse = serde_json::from_slice(&raw)
            .map_err(|e| Error::Sync(format!("bad broker response: {e}")))?;
        if res.v != 1 {
            return Err(Error::Sync(format!(
                "unsupported broker response v{}",
                res.v
            )));
        }
        Ok(res)
    }
}

/// Server side: answer requests addressed to this device.
pub struct BrokerServer<'a, T: SyncTransport + ?Sized, H: OpHandler> {
    transport: &'a T,
    vault: &'a [u8; 32],
    device: DeviceId,
    handler: &'a H,
    /// Request ids already served — a same-process dedup guard on top of
    /// transport deletes (which are best-effort).
    served: std::collections::HashSet<String>,
    /// Ops this device advertises via `bcap` — set by `serve`.
    ops: Vec<String>,
    /// Optional guest-request endpoint: token-authenticated `greq`
    /// objects (non-vault requesters) verified + executed each pass.
    guest: Option<(
        pai_share::guest::GuestServer,
        Box<pai_share::guest::GuestHandler<'static>>,
    )>,
    /// Sampled at each announce — feeds the `load` placement hint.
    load_probe: Option<Box<dyn Fn() -> DeviceLoad + Send + Sync + 'a>>,
}

impl<'a, T: SyncTransport + ?Sized, H: OpHandler> BrokerServer<'a, T, H> {
    pub fn new(transport: &'a T, vault: &'a [u8; 32], device: DeviceId, handler: &'a H) -> Self {
        Self {
            transport,
            vault,
            device,
            handler,
            served: Default::default(),
            ops: vec![],
            guest: None,
            load_probe: None,
        }
    }

    /// Sample device load/capabilities at each `bcap` announce — feeds
    /// the placement hint `find_peer` scores on.
    pub fn with_load_probe(
        mut self,
        probe: Box<dyn Fn() -> DeviceLoad + Send + Sync + 'a>,
    ) -> Self {
        self.load_probe = Some(probe);
        self
    }

    /// Ops this server will advertise + answer to.
    pub fn with_ops(mut self, ops: Vec<String>) -> Self {
        self.ops = ops;
        self
    }

    /// Also answer guest requests: `greq/<me>/` objects carrying a
    /// [`pai_share::Capability`] are verified and run through `handler`
    /// each serve pass; responses seal to the request's ephemeral key.
    pub fn with_guest_handler(
        mut self,
        guests: pai_share::guest::GuestServer,
        handler: Box<pai_share::guest::GuestHandler<'static>>,
    ) -> Self {
        self.guest = Some((guests, handler));
        self
    }

    /// Publish `bcap/<me>` — the ops this device serves right now.
    pub async fn announce(&self) -> Result<()> {
        let caps = BrokerCaps {
            v: 1,
            device: self.device.to_string(),
            ops: self.ops.clone(),
            ts: pai_storage::ts(&now()),
            load: self.load_probe.as_ref().map(|p| p()),
        };
        let key = format!("{CAP_PREFIX}{}", self.device);
        let raw = serde_json::to_vec(&caps).map_err(|e| Error::Sync(e.to_string()))?;
        self.transport
            .push(&seal_obj(self.vault, self.device, &key, &raw)?)
            .await
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
            // Expiry: the caller already gave up — delete and move on.
            if req.expires_at_ms > 0 && now().timestamp_millis() as u64 > req.expires_at_ms {
                self.transport.delete(&meta.key).await.ok();
                self.served.insert(req_id);
                continue;
            }
            let from =
                DeviceId(uuid::Uuid::parse_str(&req.from).unwrap_or_else(|_| uuid::Uuid::nil()));
            let res_key = format!("{RES_PREFIX}{from}/{}", req.id);
            match base64::engine::general_purpose::STANDARD.decode(&req.payload_b64) {
                Ok(bytes) if req.stream => {
                    self.serve_stream(&req, &res_key, &bytes).await?;
                }
                Ok(bytes) => {
                    let (ok, payload_b64, error) = match self.handler.handle(&req.op, &bytes).await
                    {
                        Ok(out) => (
                            true,
                            base64::engine::general_purpose::STANDARD.encode(&out),
                            None,
                        ),
                        Err(e) => (false, String::new(), Some(e.to_string())),
                    };
                    self.push_final(&req, &res_key, ok, payload_b64, error)
                        .await?;
                }
                Err(e) => {
                    self.push_final(
                        &req,
                        &res_key,
                        false,
                        String::new(),
                        Some(format!("bad request payload: {e}")),
                    )
                    .await?;
                }
            }
            self.transport.delete(&meta.key).await.ok();
            self.served.insert(req_id);
            served += 1;
        }
        Ok(served)
    }

    /// Streamed op: drain the handler's channel into numbered chunk
    /// objects, then the usual final response (ok + empty payload — the
    /// chunks carried the data).
    async fn serve_stream(
        &mut self,
        req: &BrokerRequest,
        res_key: &str,
        bytes: &[u8],
    ) -> Result<()> {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let handle = self.handler.handle_stream(&req.op, bytes, tx);
        let mut seq = 0u32;
        let mut fut = std::pin::pin!(handle);
        let mut done = false;
        while !done {
            tokio::select! {
                item = rx.recv() => {
                    match item {
                        Some(chunk_bytes) => {
                            let chunk = BrokerChunk {
                                v: 1,
                                seq,
                                payload_b64: base64::engine::general_purpose::STANDARD
                                    .encode(&chunk_bytes),
                            };
                            let key = format!("{res_key}/{seq}");
                            let raw = serde_json::to_vec(&chunk)
                                .map_err(|e| Error::Sync(e.to_string()))?;
                            self.transport
                                .push(&seal_obj(self.vault, self.device, &key, &raw)?)
                                .await?;
                            seq += 1;
                        }
                        None => done = true,
                    }
                }
                out = &mut fut => {
                    match out {
                        Ok(()) => {}
                        Err(e) => {
                            self.push_final(req, res_key, false, String::new(),
                                Some(e.to_string())).await?;
                            return Ok(());
                        }
                    }
                    // Drain any remaining chunks after completion.
                    while let Ok(chunk_bytes) = rx.try_recv() {
                        let chunk = BrokerChunk {
                            v: 1,
                            seq,
                            payload_b64: base64::engine::general_purpose::STANDARD
                                .encode(&chunk_bytes),
                        };
                        let key = format!("{res_key}/{seq}");
                        let raw = serde_json::to_vec(&chunk)
                            .map_err(|e| Error::Sync(e.to_string()))?;
                        self.transport
                            .push(&seal_obj(self.vault, self.device, &key, &raw)?)
                            .await?;
                        seq += 1;
                    }
                    done = true;
                }
            }
        }
        self.push_final(req, res_key, true, String::new(), None)
            .await
    }

    async fn push_final(
        &self,
        req: &BrokerRequest,
        res_key: &str,
        ok: bool,
        payload_b64: String,
        error: Option<String>,
    ) -> Result<()> {
        let res = BrokerResponse {
            v: 1,
            request_id: req.id.clone(),
            from: self.device.to_string(),
            ok,
            payload_b64,
            error,
        };
        let raw = serde_json::to_vec(&res).map_err(|e| Error::Sync(e.to_string()))?;
        self.transport
            .push(&seal_obj(self.vault, self.device, res_key, &raw)?)
            .await
    }

    /// One pass over the guest endpoint (`greq/<me>/`). No-ops when no
    /// guest handler is attached — exposed for guest-only serving and
    /// tests.
    pub async fn serve_guests_once(&self) -> Result<usize> {
        match &self.guest {
            Some((g, h)) => g.serve_once(self.transport, h.as_ref()).await,
            None => Ok(0),
        }
    }

    /// Poll-and-serve forever — the `pai broker serve` loop. Announces
    /// capabilities up front and re-announces periodically.
    pub async fn serve(&mut self, poll: Duration) -> ! {
        if !self.ops.is_empty() {
            if let Err(e) = self.announce().await {
                tracing::warn!(error = %e, "broker capability announce failed");
            }
        }
        let mut last_announce = Instant::now();
        loop {
            if let Err(e) = self.serve_once().await {
                tracing::warn!(error = %e, "broker serve pass failed");
            }
            if let Err(e) = self.serve_guests_once().await {
                tracing::warn!(error = %e, "broker guest serve pass failed");
            }
            if !self.ops.is_empty() && last_announce.elapsed() >= CAP_REANNOUNCE {
                if let Err(e) = self.announce().await {
                    tracing::warn!(error = %e, "broker capability re-announce failed");
                }
                last_announce = Instant::now();
            }
            tokio::time::sleep(poll).await;
        }
    }
}
