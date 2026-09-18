//! `pai broker …` — trusted-device compute: serve ops to paired peers,
//! place work on them, and the guest endpoint for capability-token runs.

use crate::ctx::Base;
use crate::util::*;
use crate::Cli;
use clap::Subcommand;
use pai_core::*;
use pai_inference::LlamaServerProvider;
use pai_storage::Store;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

#[derive(Subcommand)]
pub(crate) enum BrokerCmd {
    /// List paired peer devices (potential trusted executors).
    Devices,
    /// Prefer (or deprioritize) a device when `--on any` routes a call:
    /// the weight adds to the device's announced load score. Positive
    /// prefers, negative avoids, 0 clears. Local-only — never announced.
    Prefer {
        /// Peer device id or unambiguous prefix (see `broker devices`).
        peer: String,
        /// Signed weight — e.g. 500 strongly prefers, -500 avoids.
        weight: i64,
    },
    /// Answer broker requests addressed to this device, forever.
    /// Ops are served by this device's providers: stt (whisper-server),
    /// tts (piper), infer/describe (llama-server).
    Serve {
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
        /// Poll interval, seconds.
        #[arg(long, default_value = "2")]
        poll_secs: u64,
    },
    /// Send one request to a paired peer and wait for its response.
    Call {
        /// Peer device id or unambiguous prefix (see `broker devices`),
        /// or the literal `any` to route by announced capability.
        device: String,
        /// Operation: stt | tts | infer | describe.
        op: String,
        /// Stream the response — chunks print as they arrive (infer).
        #[arg(long)]
        stream: bool,
        /// UTF-8 payload (tts/infer/describe prompt) — or --file for bytes.
        #[arg(long)]
        text: Option<String>,
        /// Binary payload file (stt WAV, describe image).
        #[arg(long)]
        file: Option<String>,
        /// Write response bytes to a file instead of stdout.
        #[arg(long)]
        out: Option<String>,
        #[arg(long, default_value = "60")]
        timeout_secs: u64,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
}

/// Broker op dispatch for `pai broker serve`: each op resolves through
/// whatever this device actually runs — whisper-server (stt), piper
/// (tts), llama-server (infer/describe).
struct BrokerOps {
    stt: Option<pai_voice::WhisperServerStt>,
    tts: Option<pai_voice::PiperTts>,
    /// Any media-generation backend reachable — the device advertises
    /// `media-run` and executes generation jobs for vault members
    /// (the op itself dispatches to audio/image/video by `kind`).
    has_media: bool,
    server_url: String,
    model: String,
    data_dir: std::path::PathBuf,
    store: Arc<Store>,
    device: DeviceId,
    /// Ops currently executing — announced as `DeviceLoad.busy` so
    /// peers can place work on the least-loaded device.
    busy: Arc<AtomicUsize>,
}

/// Decrements the busy counter when the op finishes — covers early
/// returns without restructuring `handle`'s match.
struct BusyGuard<'a>(&'a AtomicUsize);

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl BrokerOps {
    fn ops(&self) -> Vec<String> {
        // app-run is announced unconditionally: installed apps sync to
        // every paired device, so any peer may serve the run.
        let mut v = vec![
            "infer".to_string(),
            "describe".to_string(),
            "app-run".to_string(),
        ];
        if self.stt.is_some() {
            v.push("stt".into());
        }
        if self.tts.is_some() {
            v.push("tts".into());
        }
        if self.has_media {
            v.push("media-run".into());
        }
        v
    }

    fn describe(&self) -> String {
        self.ops().join(", ")
    }
}

#[async_trait::async_trait]
impl pai_broker::rpc::OpHandler for BrokerOps {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        use base64::Engine as _;
        use pai_inference::{
            ImageUnderstandingProvider, InferenceProvider, SpeechToTextProvider,
            TextToSpeechProvider,
        };
        self.busy.fetch_add(1, Ordering::Relaxed);
        let _busy = BusyGuard(&self.busy);
        match op {
            "stt" => {
                let stt = self
                    .stt
                    .as_ref()
                    .ok_or_else(|| Error::Provider("no whisper-server here".into()))?;
                Ok(stt.transcribe(payload, "audio/wav").await?.into_bytes())
            }
            "tts" => {
                let tts = self
                    .tts
                    .as_ref()
                    .ok_or_else(|| Error::Provider("no piper here".into()))?;
                let text = String::from_utf8(payload.to_vec())
                    .map_err(|_| Error::InvalidInput("tts payload must be UTF-8".into()))?;
                tts.synthesize(&text, None).await
            }
            "infer" => {
                let prompt = String::from_utf8(payload.to_vec())
                    .map_err(|_| Error::InvalidInput("infer payload must be UTF-8".into()))?;
                let p = LlamaServerProvider::new(&self.server_url, self.model.clone());
                let req = pai_inference::AIRequest {
                    messages: vec![Message {
                        id: MessageId::new(),
                        conversation: ConversationId::new(),
                        role: Role::User,
                        created_at: now(),
                        content: vec![Content::Text { text: prompt }],
                        trust: TrustLevel::User,
                    }],
                    tools: vec![],
                    model: Some(self.model.clone()),
                    temperature: None,
                    max_tokens: None,
                    require_structured: false,
                };
                Ok(p.generate(&req).await?.text.into_bytes())
            }
            "app-run" => {
                #[derive(serde::Deserialize)]
                struct RunArgs {
                    id: String,
                    #[serde(default)]
                    args: Vec<String>,
                }
                let a: RunArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("app-run payload JSON: {e}")))?;
                if let Some(other) =
                    pai_sync::backup::active_elsewhere(&self.store, self.device, &a.id)?
                {
                    return Err(Error::InvalidInput(format!(
                        "app {} is active on {other} — not runnable here",
                        a.id
                    )));
                }
                pai_apps::app_run_op(&self.data_dir, &a.id, &a.args)
                    .await
                    .map_err(|e| Error::Other(e.to_string()))
            }
            // Stable app URLs: the gateway forwards the CGI request;
            // the manifest's `serve` flag is re-checked here (the
            // opt-in lives in the package, not the gateway).
            "app-serve" => {
                #[derive(serde::Deserialize)]
                struct ServeArgs {
                    id: String,
                    #[serde(default)]
                    args: Vec<String>,
                }
                let a: ServeArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("app-serve payload JSON: {e}")))?;
                pai_apps::app_serve_op(&self.data_dir, &a.id, &a.args)
                    .map_err(|e| Error::Other(e.to_string()))
            }
            // Broker-routed media generation: the requester picks this
            // device via find_peer("media-run"); execution + job record
            // live in pai_media::jobs::media_run_op.
            "media-run" => {
                pai_media::jobs::media_run_op(&self.data_dir, &self.store, self.device, payload)
                    .await
                    .map_err(|e| Error::Other(e.to_string()))
            }
            "describe" => {
                #[derive(serde::Deserialize)]
                struct DescribeArgs {
                    image_b64: String,
                    mime: String,
                    prompt: String,
                }
                let args: DescribeArgs = serde_json::from_slice(payload)
                    .map_err(|e| Error::InvalidInput(format!("describe payload JSON: {e}")))?;
                let img = base64::engine::general_purpose::STANDARD
                    .decode(&args.image_b64)
                    .map_err(|e| Error::InvalidInput(format!("describe image_b64: {e}")))?;
                let p = pai_vision::LlamaVisionProvider::new(&self.server_url, self.model.clone());
                Ok(p.describe(&img, &args.mime, &args.prompt)
                    .await?
                    .into_bytes())
            }
            other => Err(Error::InvalidInput(format!("unknown broker op {other}"))),
        }
    }

    /// Streaming infer: llama-server deltas become broker chunks.
    /// Everything else falls back to one-chunk `handle`.
    async fn handle_stream(
        &self,
        op: &str,
        payload: &[u8],
        tx: tokio::sync::mpsc::Sender<Vec<u8>>,
    ) -> Result<()> {
        use futures::StreamExt;
        use pai_inference::InferenceProvider;
        if op != "infer" {
            let out = self.handle(op, payload).await?;
            let _ = tx.send(out).await;
            return Ok(());
        }
        self.busy.fetch_add(1, Ordering::Relaxed);
        let _busy = BusyGuard(&self.busy);
        let prompt = String::from_utf8(payload.to_vec())
            .map_err(|_| Error::InvalidInput("infer payload must be UTF-8".into()))?;
        let p = LlamaServerProvider::new(&self.server_url, self.model.clone());
        let req = pai_inference::AIRequest {
            messages: vec![Message {
                id: MessageId::new(),
                conversation: ConversationId::new(),
                role: Role::User,
                created_at: now(),
                content: vec![Content::Text { text: prompt }],
                trust: TrustLevel::User,
            }],
            tools: vec![],
            model: Some(self.model.clone()),
            temperature: None,
            max_tokens: None,
            require_structured: false,
        };
        let mut stream = p.stream(req);
        while let Some(ev) = stream.next().await {
            match ev? {
                pai_inference::StreamEvent::Delta(text) => {
                    if tx.send(text.into_bytes()).await.is_err() {
                        break;
                    }
                }
                pai_inference::StreamEvent::Done(_) => break,
                pai_inference::StreamEvent::Error(e) => {
                    return Err(Error::Provider(e));
                }
            }
        }
        Ok(())
    }
}

/// Detect this device's serveable ops (whisper/piper via voice config;
/// llama-server from inference config).
async fn broker_ops(
    cfg: &pai_config::Config,
    cli: &Cli,
    store: Arc<Store>,
    device: DeviceId,
) -> BrokerOps {
    let (stt, tts) = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2))
        .await
        .map(|v| (v.stt, v.tts))
        .unwrap_or((None, None));
    let has_media =
        pai_media::providers::any_media_backend(&cfg.data_dir, std::time::Duration::from_secs(2))
            .await;
    let model = cli
        .model
        .clone()
        .unwrap_or_else(|| cfg.inference.default_model.clone());
    BrokerOps {
        stt,
        tts,
        has_media,
        server_url: cfg.inference.local_server_url.clone(),
        model,
        data_dir: cfg.data_dir.clone(),
        store,
        device,
        busy: Default::default(),
    }
}

pub(crate) async fn run(cmd: &BrokerCmd, b: &Base, cli: &Cli) -> Result<()> {
    use pai_sync::{crypto, pair};
    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let device = b.device.clone();
    match cmd {
        BrokerCmd::Devices => {
            let peers = pair::list_peers(&store)?;
            if peers.is_empty() {
                println!("(no paired devices — `pai pair` first)");
            }
            for p in peers {
                let w = store
                    .meta_get(&format!("place_weight.{}", p.device_id))?
                    .and_then(|v| v.parse::<i64>().ok())
                    .unwrap_or(0);
                let pref = if w != 0 {
                    format!("  weight={w:+}")
                } else {
                    String::new()
                };
                println!(
                    "  {}  {:<20} {:<10} paired {}{}",
                    p.device_id,
                    p.name,
                    p.platform,
                    p.paired_at.format("%Y-%m-%d"),
                    pref
                );
            }
        }
        BrokerCmd::Prefer { peer, weight } => {
            let id = resolve_peer(&store, peer)?;
            store.meta_set(&format!("place_weight.{id}"), &weight.to_string())?;
            let how = match weight.signum() {
                1 => "preferred",
                -1 => "deprioritized",
                _ => "cleared",
            };
            println!("{id} {how} — placement weight {weight:+} (local only)");
        }
        BrokerCmd::Serve {
            dir,
            relay,
            token,
            poll_secs,
        } => {
            let t = sync_transport(dir, relay, token)?;
            // Guest endpoint: greq/ requests carrying a capability
            // token minted by `pai apps share` — verified, then run
            // through the same sandboxed app_run_op. It does not
            // need a vault: the token is the guest's auth.
            let guest_data = cfg.data_dir.clone();
            let guest_handler: Box<pai_share::guest::GuestHandler<'static>> =
                Box::new(move |op, app_id, args| match op {
                    "app-run" => pai_apps::app_run_op_guest(&guest_data, app_id, args)
                        .map_err(|e| Error::Other(e.to_string())),
                    // Guests may hit the CGI surface too — the
                    // request is theirs, and no owner tokens ride.
                    "app-serve" => pai_apps::app_serve_op(&guest_data, app_id, args)
                        .map_err(|e| Error::Other(e.to_string())),
                    "app-read" => pai_apps::app_read_op(&guest_data, app_id, args)
                        .map_err(|e| Error::Other(e.to_string())),
                    "app-write" => pai_apps::app_write_op(&guest_data, app_id, args)
                        .map_err(|e| Error::Other(e.to_string())),
                    other => Err(Error::InvalidInput(format!("unknown guest op '{other}'"))),
                });
            let guests =
                pai_share::guest::GuestServer::new(device.clone(), store.clone(), &cfg.data_dir);
            match crypto::vault_key(&cfg.data_dir)? {
                Some(vault) => {
                    let ops = broker_ops(&cfg, cli, store.clone(), device.id).await;
                    let busy = ops.busy.clone();
                    let caps = device.capabilities.clone();
                    let mut srv = pai_broker::rpc::BrokerServer::new(&*t, &vault, device.id, &ops)
                        .with_ops(ops.ops())
                        .with_guest_handler(guests, guest_handler)
                        .with_load_probe(Box::new(move || {
                            // Battery/thermal are re-sampled live
                            // each announce; hardware fields are
                            // registration-time.
                            let (bat, th) = pai_identity::probe_power();
                            pai_broker::rpc::DeviceLoad {
                                busy: busy.load(Ordering::Relaxed) as u32,
                                on_battery: bat.or(caps.on_battery),
                                thermal_throttled: th.or(caps.thermal_throttled),
                                ram_bytes: caps.ram_bytes,
                                cpu_cores: caps.cpu_cores,
                            }
                        }));
                    println!(
                        "broker serving {} on {} — ops: {} (+ guest endpoint)",
                        device.id,
                        t.id(),
                        ops.describe()
                    );
                    srv.serve(std::time::Duration::from_secs(*poll_secs)).await;
                }
                None => {
                    // No vault — this device shares to guests only.
                    let poll = std::time::Duration::from_secs(*poll_secs);
                    println!(
                        "no vault key — serving {} guest endpoint only on {}",
                        device.id,
                        t.id()
                    );
                    loop {
                        if let Err(e) = guests.serve_once(&*t, &guest_handler).await {
                            eprintln!("guest serve pass failed: {e}");
                        }
                        tokio::time::sleep(poll).await;
                    }
                }
            }
        }
        BrokerCmd::Call {
            device: dev,
            op,
            stream,
            text,
            file,
            out,
            timeout_secs,
            dir,
            relay,
            token,
        } => {
            let t = sync_transport(dir, relay, token)?;
            let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                Error::Sync("no vault key — pair a device first (pai pair)".into())
            })?;
            let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id)
                .with_weights(place_weights(&store));
            let to = if dev == "any" {
                client.find_peer(op).await?.ok_or_else(|| {
                    Error::NotFound(format!(
                        "no paired device advertises '{op}' — is `pai broker serve` running?"
                    ))
                })?
            } else {
                resolve_peer(&store, dev)?
            };
            let payload = if let Some(f) = file {
                std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
            } else if let Some(t) = text {
                t.clone().into_bytes()
            } else {
                return Err(Error::InvalidInput("pass --text or --file".into()));
            };
            let timeout = std::time::Duration::from_secs(*timeout_secs);
            let resp = if *stream {
                client
                    .call_stream(to, op, &payload, timeout, &mut |chunk| {
                        if let Ok(s) = std::str::from_utf8(chunk) {
                            print!("{s}");
                            std::io::Write::flush(&mut std::io::stdout()).ok();
                        }
                    })
                    .await?
            } else {
                client.call(to, op, &payload, timeout).await?
            };
            if let Some(f) = out {
                std::fs::write(f, &resp).map_err(|e| Error::Storage(e.to_string()))?;
                println!("wrote {f} ({} bytes)", resp.len());
            } else {
                match String::from_utf8(resp.clone()) {
                    Ok(s) => println!("{s}"),
                    Err(_) => println!("({} bytes, binary — use --out)", resp.len()),
                }
            }
        }
    }
    Ok(())
}
