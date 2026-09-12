//! Vision boundary: image understanding + VLM access.
//!
//! The trait lives in `pai-inference` (`ImageUnderstandingProvider` joins
//! the capability model). This crate holds vision-side request types.
//! Local runtimes targeted: llama.cpp VLMs (LLaVA-family), ONNX, MLX-VLM —
//! all free/open-weight.

use serde::{Deserialize, Serialize};

/// A question about an image blob.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionRequest {
    pub image_blob: String,
    pub prompt: String,
    pub model: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VisionResult {
    pub text: String,
    /// Detected objects/regions when the model supports grounding.
    pub detections: Vec<Detection>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Detection {
    pub label: String,
    pub confidence: f32,
    /// Normalized (x, y, w, h).
    pub bbox: Option<[f32; 4]>,
}
