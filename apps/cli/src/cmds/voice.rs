//! `pai voice …` — whisper-server STT, piper TTS, energy-VAD mic capture.

use crate::ctx::*;
use clap::Subcommand;
use pai_agent::AgentDefinition;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum VoiceCmd {
    /// Show detected voice providers (whisper-server reachability, piper
    /// binary/model, VAD) and the effective config.
    Status,
    /// Write voice.json: whisper-server URL, piper binary + voice model.
    Configure {
        #[arg(long)]
        whisper_url: Option<String>,
        #[arg(long)]
        piper_bin: Option<String>,
        #[arg(long)]
        piper_model: Option<String>,
    },
    /// Transcribe an audio file (WAV) via whisper-server.
    Transcribe { file: String },
    /// Synthesize text to a WAV file via piper.
    Say {
        text: String,
        #[arg(long, default_value = "reply.wav")]
        out: String,
    },
    /// One conversational turn: WAV in → transcript → agent → reply WAV.
    Turn {
        /// WAV file to transcribe; omit with --mic to capture instead.
        file: Option<String>,
        /// Capture the utterance from the default microphone and play the
        /// spoken reply through the speakers.
        #[arg(long)]
        mic: bool,
        #[arg(long, default_value = "reply.wav")]
        out: String,
    },
    /// Capture one utterance from the mic (VAD-endpointed) and transcribe.
    Listen {
        /// Hard cap on capture length, seconds.
        #[arg(long, default_value = "30")]
        max_secs: u32,
        /// Stream partial transcripts — each pause-finalized segment
        /// transcribes while you keep talking.
        #[arg(long)]
        stream: bool,
    },
}

/// Voice ops. `transcribe`/`say` use one provider each; `turn` runs the
/// full Mic→VAD→STT→Agent→TTS→Speaker pipeline (file-based I/O — live mic
/// capture is the next step, needs OS audio permissions).
/// `pai voice` — whisper-server STT, piper TTS, energy-VAD mic capture.
pub(crate) async fn run(cmd: &VoiceCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    run_voice_cmds(cmd, ctx, cfg).await
}

async fn run_voice_cmds(cmd: &VoiceCmd, ctx: &Ctx, cfg: &pai_config::Config) -> Result<()> {
    use pai_inference::{SpeechToTextProvider, TextToSpeechProvider};
    use pai_voice::{UtteranceHandler, VoicePipeline};
    match cmd {
        VoiceCmd::Status => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            println!(
                "whisper-server: {}",
                if s.stt.is_some() {
                    format!("reachable at {}", s.cfg.whisper_url())
                } else {
                    format!("NOT reachable ({})", s.cfg.whisper_url())
                }
            );
            println!(
                "piper: {}",
                if s.tts.is_some() {
                    "found"
                } else {
                    "NOT found (set `pai voice configure --piper-bin/--piper-model` or PAI_PIPER_MODEL)"
                }
            );
            println!("vad: energy-vad (always available)");
            println!(
                "mic: {} | speaker: {}",
                if pai_voice::mic::input_available() {
                    "default input found"
                } else {
                    "NONE"
                },
                if pai_voice::mic::output_available() {
                    "default output found"
                } else {
                    "NONE"
                }
            );
        }
        VoiceCmd::Configure {
            whisper_url,
            piper_bin,
            piper_model,
        } => {
            let mut c = pai_voice::VoiceConfig::load(&cfg.data_dir)?;
            if let Some(u) = whisper_url {
                c.whisper_url = Some(u.clone());
            }
            if let Some(b) = piper_bin {
                c.piper_bin = Some(b.into());
            }
            if let Some(m) = piper_model {
                c.piper_model = Some(m.into());
            }
            c.save(&cfg.data_dir)?;
            println!("voice.json written — `pai voice status` to verify");
        }
        VoiceCmd::Transcribe { file } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let stt = s.stt.ok_or_else(|| {
                Error::Provider(
                    "whisper-server unreachable — start it or `pai voice configure --whisper-url`"
                        .into(),
                )
            })?;
            let audio =
                std::fs::read(file).map_err(|e| Error::InvalidInput(format!("{file}: {e}")))?;
            let text = stt.transcribe(&audio, "audio/wav").await?;
            println!("{text}");
        }
        VoiceCmd::Say { text, out } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let tts = s.tts.ok_or_else(|| {
                Error::Provider(
                    "piper not found — `pai voice configure --piper-bin/--piper-model`".into(),
                )
            })?;
            let wav = tts.synthesize(text, None).await?;
            std::fs::write(out, &wav).map_err(|e| Error::Storage(e.to_string()))?;
            println!("wrote {out} ({} bytes)", wav.len());
        }
        VoiceCmd::Turn { file, mic, out } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let pipeline: VoicePipeline = s.pipeline().ok_or_else(|| {
                Error::Provider(
                    "voice turn needs both whisper-server AND piper — `pai voice status`".into(),
                )
            })?;
            let audio = if *mic {
                println!("listening… (speak, then pause)");
                let pcm = pai_voice::mic::capture_utterance(&pai_voice::EnergyVad::default(), 30)?;
                if pcm.is_empty() {
                    println!("(nothing heard)");
                    return Ok(());
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE)
            } else {
                let f = file
                    .as_deref()
                    .ok_or_else(|| Error::InvalidInput("pass a WAV file or --mic".into()))?;
                std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
            };
            let def = agent_def(&ctx.provider_name, ctx.model.clone());
            struct AgentVoice<'a> {
                ctx: &'a Ctx,
                def: &'a AgentDefinition,
            }
            #[async_trait::async_trait]
            impl UtteranceHandler for AgentVoice<'_> {
                async fn respond(&self, transcript: &str) -> Result<String> {
                    let out =
                        send(self.ctx, self.def, transcript, None, None, &CliApproval).await?;
                    Ok(out.answer.unwrap_or_else(|| "(no reply)".into()))
                }
            }
            let handler = AgentVoice { ctx, def: &def };
            let (transcript, speech) = pipeline.turn(&audio, "audio/wav", &handler).await?;
            println!("you said: {transcript}");
            if *mic {
                let (rate, pcm) = pai_voice::mic::wav_to_pcm16(&speech)?;
                pai_voice::mic::play(&pcm, rate)?;
                println!("reply spoken");
            } else {
                std::fs::write(out, &speech).map_err(|e| Error::Storage(e.to_string()))?;
                println!("reply → {out} ({} bytes)", speech.len());
            }
        }
        VoiceCmd::Listen { max_secs, stream } => {
            let s = pai_voice::detect(&cfg.data_dir, std::time::Duration::from_secs(2)).await?;
            let stt = s.stt.ok_or_else(|| {
                Error::Provider(
                    "whisper-server unreachable — start it or `pai voice configure --whisper-url`"
                        .into(),
                )
            })?;
            if *stream {
                println!("listening… (partials print as segments close)");
                let mut n = 0usize;
                let text = pai_voice::stream_transcribe(
                    &stt,
                    &pai_voice::EnergyVad::default(),
                    *max_secs,
                    &mut |part| {
                        n += 1;
                        println!("  [{n}] {part}");
                    },
                )?;
                if text.is_empty() {
                    println!("(nothing heard)");
                } else {
                    println!("final: {text}");
                }
            } else {
                println!("listening… (speak, then pause)");
                let pcm =
                    pai_voice::mic::capture_utterance(&pai_voice::EnergyVad::default(), *max_secs)?;
                if pcm.is_empty() {
                    println!("(nothing heard)");
                    return Ok(());
                }
                let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
                let wav = pai_voice::pcm16_to_wav(&bytes, pai_voice::mic::TARGET_RATE);
                let text = stt.transcribe(&wav, "audio/wav").await?;
                println!("{text}");
            }
        }
    }
    Ok(())
}
