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
