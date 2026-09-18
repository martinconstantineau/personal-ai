//! `pai media …` / `pai audio …` — media generation (audio/image/video)
//! via configured providers or a paired `media-run` device.

use crate::ctx::Base;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;
use pai_storage::Store;
use std::sync::Arc;

#[derive(Subcommand)]
pub(crate) enum AudioCmd {
    /// Show the configured audio-generation provider and reachability.
    Status,
    /// Write media.json: the audio-generation server URL.
    Configure {
        /// Base URL, e.g. http://127.0.0.1:8179 (a MusicGen/stable-audio
        /// wrapper — any server accepting POST /generate {prompt,
        /// duration_seconds} → audio bytes).
        #[arg(long)]
        audio_gen_url: Option<String>,
    },
    /// Generate audio from a text prompt → writes a WAV file.
    Gen {
        prompt: String,
        #[arg(long, default_value = "10")]
        seconds: u32,
        /// Output path; default <data_dir>/media/audio-<ts>.wav
        #[arg(long)]
        out: Option<String>,
        /// Run on a paired device (id prefix or "any" = best advertised
        /// media-run peer). Needs --dir or --relay for the transport.
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// List recent media jobs (local + executed-for-peers rows).
    Jobs,
}

#[derive(Subcommand)]
pub(crate) enum MediaCmd {
    /// Show configured media providers (audio/image/video) and reachability.
    Status,
    /// Write media.json: generation server URLs.
    Configure {
        /// Audio-gen base URL (POST /generate {prompt,duration_seconds}
        /// → WAV), e.g. services/media-gen or a MusicGen/stable-audio
        /// wrapper. Also PAI_AUDIO_GEN_URL.
        #[arg(long)]
        audio_gen_url: Option<String>,
        /// Image-gen base URL. Also PAI_IMAGE_GEN_URL.
        #[arg(long)]
        image_gen_url: Option<String>,
        /// Image adapter: `sdcpp` (default — stable-diffusion.cpp's
        /// OpenAI /v1/images/generations) or `onnx` (minimal
        /// POST /generate → PNG convention).
        #[arg(long)]
        image_backend: Option<String>,
        /// Video-gen base URL (async job contract: POST /generate →
        /// {job_id}, GET /jobs/{id} → {status,result_b64}).
        /// Also PAI_VIDEO_GEN_URL.
        #[arg(long)]
        video_gen_url: Option<String>,
    },
    /// Generate media from a text prompt → writes a file.
    Gen {
        /// audio | image | image-edit | upscale | video
        #[arg(long, default_value = "audio")]
        kind: String,
        prompt: String,
        /// Audio/video duration, seconds (clamped 1–300).
        #[arg(long, default_value = "10")]
        seconds: u32,
        #[arg(long)]
        width: Option<u32>,
        #[arg(long)]
        height: Option<u32>,
        /// Source image file (image-edit / upscale kinds).
        #[arg(long)]
        input: Option<String>,
        /// Output path; default <data_dir>/media/<kind>-<ts>.<ext>
        #[arg(long)]
        out: Option<String>,
        /// Run on a paired device (id prefix or "any" = best advertised
        /// media-run peer). Needs --dir or --relay for the transport.
        #[arg(long)]
        device: Option<String>,
        #[arg(long)]
        dir: Option<String>,
        #[arg(long)]
        relay: Option<String>,
        #[arg(long)]
        token: Option<String>,
    },
    /// List recent media jobs (local + executed-for-peers rows).
    Jobs,
}

/// `pai audio …` — audio-generation provider config + generation.
/// Needs only config + the provider endpoint (no inference stack), so it
/// lives on the light command path. Sugar over `pai media`.
/// `pai audio` — audio generation (subset of `pai media`).
pub(crate) async fn run_audio(cmd: &AudioCmd, b: &Base) -> Result<()> {
    run_audio_cmds(cmd, &b.cfg, &b.store, &b.device).await
}

async fn run_audio_cmds(
    cmd: &AudioCmd,
    cfg: &pai_config::Config,
    store: &Arc<Store>,
    device: &Device,
) -> Result<()> {
    let m = match cmd {
        AudioCmd::Status => MediaCmd::Status,
        AudioCmd::Configure { audio_gen_url } => MediaCmd::Configure {
            audio_gen_url: audio_gen_url.clone(),
            image_gen_url: None,
            image_backend: None,
            video_gen_url: None,
        },
        AudioCmd::Gen {
            prompt,
            seconds,
            out,
            device: on,
            dir,
            relay,
            token,
        } => MediaCmd::Gen {
            kind: "audio".into(),
            prompt: prompt.clone(),
            seconds: *seconds,
            width: None,
            height: None,
            input: None,
            out: out.clone(),
            device: on.clone(),
            dir: dir.clone(),
            relay: relay.clone(),
            token: token.clone(),
        },
        AudioCmd::Jobs => MediaCmd::Jobs,
    };
    run_media_cmds(&m, cfg, store, device).await
}

/// `pai media …` — media generation (audio/image/video) provider
/// config + generation. Needs only config + the provider endpoints,
/// so it lives on the light command path.
/// `pai media` — audio/image/video generation + provider status.
pub(crate) async fn run(cmd: &MediaCmd, b: &Base) -> Result<()> {
    run_media_cmds(cmd, &b.cfg, &b.store, &b.device).await
}

async fn run_media_cmds(
    cmd: &MediaCmd,
    cfg: &pai_config::Config,
    store: &Arc<Store>,
    device: &Device,
) -> Result<()> {
    use base64::Engine as _;
    match cmd {
        MediaCmd::Status => {
            let c = pai_media::providers::MediaConfig::load(&cfg.data_dir)?;
            let t = std::time::Duration::from_secs(2);
            let audio = pai_media::providers::detect(&cfg.data_dir, t).await;
            let image = pai_media::providers::detect_image(&cfg.data_dir, t).await;
            let video = pai_media::providers::detect_video(&cfg.data_dir, t).await;
            let report = |name: &str, url: Option<String>, up: bool| {
                let at = url.unwrap_or_else(|| "(unset)".into());
                println!(
                    "{name}: {}",
                    if up {
                        format!("reachable at {at}")
                    } else {
                        format!("not reachable ({at})")
                    }
                );
            };
            report("audio", c.audio_gen_url.clone(), audio.is_some());
            report("image", c.image_gen_url.clone(), image.is_some());
            report("video", c.video_gen_url.clone(), video.is_some());
            if audio.is_none() && image.is_none() && video.is_none() {
                println!(
                    "none reachable — `pai media configure` sets URLs (defaults from env); \
                     services/media-gen is the bundled reference backend"
                );
            }
        }
        MediaCmd::Configure {
            audio_gen_url,
            image_gen_url,
            image_backend,
            video_gen_url,
        } => {
            let mut c = pai_media::providers::MediaConfig::load(&cfg.data_dir)?;
            if let Some(u) = audio_gen_url {
                c.audio_gen_url = Some(u.clone());
            }
            if let Some(u) = image_gen_url {
                c.image_gen_url = Some(u.clone());
            }
            if let Some(b) = image_backend {
                if b != "sdcpp" && b != "onnx" {
                    return Err(Error::InvalidInput(
                        "--image-backend must be `sdcpp` or `onnx`".into(),
                    ));
                }
                c.image_backend = Some(b.clone());
            }
            if let Some(u) = video_gen_url {
                c.video_gen_url = Some(u.clone());
            }
            c.save(&cfg.data_dir)?;
            println!("media.json written — `pai media status` to verify");
        }
        MediaCmd::Gen {
            kind,
            prompt,
            seconds,
            width,
            height,
            input,
            out,
            device: on,
            dir,
            relay,
            token,
        } => {
            let k = pai_media::jobs::kind_from_str(kind)
                .map_err(|_| Error::InvalidInput(format!("unknown media kind: {kind}")))?;
            let secs = (*seconds).clamp(1, 300);
            let size = (width.unwrap_or(512), height.unwrap_or(512));
            let input_bytes = match input {
                Some(p) => {
                    let bytes = std::fs::read(p).map_err(|e| Error::Storage(e.to_string()))?;
                    let mime = match std::path::Path::new(p)
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_lowercase()
                        .as_str()
                    {
                        "png" => "image/png",
                        "jpg" | "jpeg" => "image/jpeg",
                        "webp" => "image/webp",
                        _ => "application/octet-stream",
                    };
                    Some((bytes, mime.to_string()))
                }
                None => None,
            };
            let out_path = |data_dir: &std::path::Path, mime: &str| -> Result<std::path::PathBuf> {
                match out {
                    Some(o) => Ok(std::path::PathBuf::from(o)),
                    None => {
                        let dir = data_dir.join("media");
                        std::fs::create_dir_all(&dir).map_err(|e| Error::Storage(e.to_string()))?;
                        let ext = match mime {
                            "image/png" => "png",
                            "video/mp4" => "mp4",
                            _ => "wav",
                        };
                        Ok(dir.join(format!(
                            "{}-{}.{}",
                            kind,
                            pai_core::now().timestamp_millis(),
                            ext
                        )))
                    }
                }
            };
            let mut job = pai_media::jobs::new_job(k, prompt);
            let params = serde_json::json!({
                "kind": kind,
                "duration_seconds": secs,
                "size": [size.0, size.1],
                "has_input": input_bytes.is_some(),
            })
            .to_string();
            pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;

            if let Some(dev) = on {
                // Remote: dispatch media-run to a paired device.
                let result = async {
                    let t = sync_transport(dir, relay, token)?;
                    let vault = pai_sync::crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                        Error::Sync("no vault key — pair a device first (pai pair)".into())
                    })?;
                    let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id)
                        .with_weights(place_weights(store));
                    let to = if dev == "any" {
                        client.find_peer("media-run").await?.ok_or_else(|| {
                            Error::NotFound(
                                "no paired device advertises media-run — \
                                 `pai broker serve` running on the worker?"
                                    .into(),
                            )
                        })?
                    } else {
                        resolve_peer(store, dev)?
                    };
                    job.state = pai_media::JobState::Running;
                    job.placement_device = Some(to);
                    pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;
                    let mut payload = serde_json::json!({
                        "prompt": prompt,
                        "kind": kind,
                        "duration_seconds": secs,
                        "width": size.0,
                        "height": size.1,
                    });
                    if let Some((bytes, mime)) = &input_bytes {
                        payload["input_b64"] = serde_json::Value::String(
                            base64::engine::general_purpose::STANDARD.encode(bytes),
                        );
                        payload["input_mime"] = serde_json::Value::String(mime.clone());
                    }
                    let payload = payload.to_string().into_bytes();
                    println!("generating {kind} on {to} — this can take a while…");
                    client
                        .call(
                            to,
                            "media-run",
                            &payload,
                            std::time::Duration::from_secs(660),
                        )
                        .await
                }
                .await;

                match result {
                    Ok(resp) => {
                        let v: serde_json::Value = serde_json::from_slice(&resp)
                            .map_err(|e| Error::Other(format!("bad media-run reply: {e}")))?;
                        let b64 = v["result_b64"]
                            .as_str()
                            .or_else(|| v["audio_b64"].as_str())
                            .ok_or_else(|| {
                                Error::Other("media-run reply missing result_b64".into())
                            })?;
                        let bytes = base64::engine::general_purpose::STANDARD
                            .decode(b64)
                            .map_err(|e| Error::Other(e.to_string()))?;
                        let mime = v["mime"].as_str().unwrap_or("audio/wav").to_string();
                        job.state = pai_media::JobState::Done;
                        job.result_blob = Some(store.put_blob(&bytes)?);
                        pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;
                        let path = out_path(&cfg.data_dir, &mime)?;
                        std::fs::write(&path, &bytes).map_err(|e| Error::Storage(e.to_string()))?;
                        println!(
                            "{} → {} bytes (job {} on {})",
                            path.display(),
                            bytes.len(),
                            job.id,
                            job.placement_device.unwrap()
                        );
                    }
                    Err(e) => {
                        job.state = pai_media::JobState::Failed;
                        pai_media::jobs::record(
                            store,
                            &job,
                            Some(&params),
                            Some(device.id),
                            Some(&e.to_string()),
                        )?;
                        return Err(e);
                    }
                }
            } else {
                job.state = pai_media::JobState::Running;
                job.placement_device = Some(device.id);
                println!("generating {kind} — this can take a while…");
                match pai_media::providers::generate(
                    &cfg.data_dir,
                    k,
                    prompt,
                    secs,
                    size,
                    input_bytes,
                )
                .await
                {
                    Ok((bytes, mime)) => {
                        job.state = pai_media::JobState::Done;
                        job.result_blob = Some(store.put_blob(&bytes)?);
                        pai_media::jobs::record(store, &job, Some(&params), Some(device.id), None)?;
                        let path = out_path(&cfg.data_dir, mime)?;
                        std::fs::write(&path, &bytes).map_err(|e| Error::Storage(e.to_string()))?;
                        println!("{} → {} bytes", path.display(), bytes.len());
                    }
                    Err(e) => {
                        job.state = pai_media::JobState::Failed;
                        pai_media::jobs::record(
                            store,
                            &job,
                            Some(&params),
                            Some(device.id),
                            Some(&e.to_string()),
                        )?;
                        return Err(e);
                    }
                }
            }
        }
        MediaCmd::Jobs => {
            for j in pai_media::jobs::list(store, 20)? {
                println!(
                    "{} {} [{}] {} {}",
                    j["id"].as_str().unwrap_or("?"),
                    j["kind"].as_str().unwrap_or("?"),
                    j["prompt"].as_str().unwrap_or("?"),
                    j["state"].as_str().unwrap_or("?"),
                    j["worker"]
                        .as_str()
                        .map(|w| format!("on {:.8}", w))
                        .unwrap_or_default(),
                );
                if let Some(e) = j["error"].as_str() {
                    println!("    error: {e}");
                }
            }
        }
    }
    Ok(())
}
