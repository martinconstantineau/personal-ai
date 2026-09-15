//! Media generation boundary: image + video + audio jobs.
//!
//! Media jobs are asynchronous and broker-routable — a phone can request a
//! video that a home GPU server renders. Traits are in `pai-inference`
//! (`ImageGenerationProvider`, `VideoGenerationProvider`,
//! `AudioGenerationProvider`); this crate adds the job record + queue
//! semantics.
//!
//! Local targets (all free): stable-diffusion.cpp / diffusers-onnx for
//! images; video is declared now, implemented later. Audio generation is
//! live via [`providers::HttpAudioGen`] — any local HTTP server that
//! accepts `{prompt, duration_seconds}` and returns audio bytes
//! (MusicGen/stable-audio wrappers).

use pai_core::*;
use serde::{Deserialize, Serialize};

pub mod providers;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaJob {
    pub id: TaskId,
    pub kind: MediaJobKind,
    pub prompt: String,
    /// Device the broker routed the job to.
    pub placement_device: Option<DeviceId>,
    pub state: JobState,
    pub created_at: Timestamp,
    /// Result blob id once complete.
    pub result_blob: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaJobKind {
    TextToImage,
    ImageEdit,
    Upscale,
    TextToVideo,
    /// Text-to-audio: music, sound effects, ambience.
    TextToAudio,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}
