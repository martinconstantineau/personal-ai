//! Voice pipeline boundary.
//!
//! ```text
//! Mic → VAD → STT → Agent → TTS → Speaker
//! ```
//!
//! Every stage is a provider trait (defined in `pai-inference`) so Whisper,
//! Piper, Silero, sherpa-onnx, or OS speech APIs all slot in identically.
//! This crate wires the stages into one cancellable pipeline.
//!
//! Status: the orchestration is real; concrete providers live in
//! [`providers`] — whisper.cpp's `whisper-server` for STT, the `piper`
//! binary for TTS, and a dependency-free energy VAD.

use async_trait::async_trait;

pub mod mic;
pub mod providers;
use pai_core::*;
use pai_inference::{SpeechToTextProvider, TextToSpeechProvider, VoiceActivityProvider};
pub use providers::{
    detect, pcm16_to_wav, EnergyVad, PiperTts, VoiceConfig, VoiceSetup, WhisperServerStt,
    DEFAULT_WHISPER_URL,
};
use std::sync::Arc;

/// What happens to spoken text once transcribed.
#[async_trait]
pub trait UtteranceHandler: Send + Sync {
    async fn respond(&self, transcript: &str) -> Result<String>;
}

pub struct VoicePipeline {
    pub vad: Arc<dyn VoiceActivityProvider>,
    pub stt: Arc<dyn SpeechToTextProvider>,
    pub tts: Arc<dyn TextToSpeechProvider>,
}

impl VoicePipeline {
    /// Run one conversational turn: audio in → transcript → handler → audio out.
    pub async fn turn(
        &self,
        audio: &[u8],
        mime: &str,
        handler: &dyn UtteranceHandler,
    ) -> Result<(String, Vec<u8>)> {
        let transcript = self.stt.transcribe(audio, mime).await?;
        let reply = handler.respond(&transcript).await?;
        let speech = self.tts.synthesize(&reply, None).await?;
        Ok((transcript, speech))
    }
}

// ---------------------------------------------------------------------------
// Streaming STT — partial transcripts while the user is still talking.
// ---------------------------------------------------------------------------

/// Transcribe already-segmented PCM (16 kHz mono i16 per piece) through a
/// provider, reporting each partial via `on_partial` as it lands.
/// Silent-only segments are skipped so no request is wasted on the
/// utterance's trailing quiet. Returns the joined transcript.
pub async fn transcribe_pcm_segments(
    stt: &dyn SpeechToTextProvider,
    segments: &[Vec<i16>],
    on_partial: &mut dyn FnMut(&str),
) -> Result<String> {
    let mut parts: Vec<String> = Vec::new();
    for seg in segments {
        if mic::is_quiet(seg) {
            continue;
        }
        let bytes: Vec<u8> = seg.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = pcm16_to_wav(&bytes, mic::TARGET_RATE);
        let text = stt.transcribe(&wav, "audio/wav").await?;
        let text = text.trim().to_string();
        if !text.is_empty() {
            on_partial(&text);
            parts.push(text);
        }
    }
    Ok(parts.join(" "))
}

/// Live streaming listen: capture segmented from the mic, transcribe
/// each ~400 ms-pause-finalized segment as it closes, and report
/// partials through `on_partial` — the UI sees text mid-utterance
/// instead of waiting for the final pause. Returns the full transcript.
/// `on_partial` fires inside the capture loop (audio keeps buffering).
/// The provider call runs on a dedicated current-thread runtime — this
/// function is deliberately synchronous (mic capture is).
pub fn stream_transcribe(
    stt: &dyn SpeechToTextProvider,
    vad: &EnergyVad,
    max_secs: u32,
    on_partial: &mut dyn FnMut(&str),
) -> Result<String> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| Error::Other(format!("voice rt: {e}")))?;
    let mut parts: Vec<String> = Vec::new();
    let mut first_err: Option<Error> = None;
    mic::capture_segmented(vad, max_secs, &mut |pcm| {
        if mic::is_quiet(&pcm) {
            return;
        }
        let bytes: Vec<u8> = pcm.iter().flat_map(|s| s.to_le_bytes()).collect();
        let wav = pcm16_to_wav(&bytes, mic::TARGET_RATE);
        match rt.block_on(stt.transcribe(&wav, "audio/wav")) {
            Ok(text) => {
                let text = text.trim().to_string();
                if !text.is_empty() {
                    on_partial(&text);
                    parts.push(text);
                }
            }
            Err(e) => {
                if first_err.is_none() {
                    first_err = Some(e);
                }
            }
        }
    })?;
    if parts.is_empty() {
        if let Some(e) = first_err {
            return Err(e);
        }
    }
    Ok(parts.join(" "))
}
