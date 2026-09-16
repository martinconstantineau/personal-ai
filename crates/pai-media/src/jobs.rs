//! Media job bookkeeping + the `media-run` broker op handler.
//!
//! A requester logs intent (`enqueue`), the worker logs execution inside
//! `media_run_op`; results are content-addressed blobs on whichever
//! device ran the job (and on the requester once the response lands).
//! Rows are local bookkeeping — they never sync.

use crate::providers;
use crate::{JobState, MediaJob, MediaJobKind};
use base64::Engine as _;
use pai_core::*;
use pai_storage::Store;
use rusqlite::params;
use std::path::Path;
use std::sync::Arc;

fn kind_str(k: MediaJobKind) -> &'static str {
    match k {
        MediaJobKind::TextToImage => "text_to_image",
        MediaJobKind::ImageEdit => "image_edit",
        MediaJobKind::Upscale => "upscale",
        MediaJobKind::TextToVideo => "text_to_video",
        MediaJobKind::TextToAudio => "text_to_audio",
    }
}

fn state_str(s: JobState) -> &'static str {
    match s {
        JobState::Queued => "queued",
        JobState::Running => "running",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

/// Insert or replace a job row. `requester`/`worker` are device ids —
/// `job.placement_device` carries the worker.
pub fn record(
    store: &Store,
    job: &MediaJob,
    params_json: Option<&str>,
    requester: Option<DeviceId>,
    err: Option<&str>,
) -> Result<()> {
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO media_jobs(id, kind, prompt, params_json, state,
                    requester, worker, result_blob, error, created_at, updated_at)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
             ON CONFLICT(id) DO UPDATE SET state=excluded.state,
                    worker=excluded.worker, result_blob=excluded.result_blob,
                    error=excluded.error, updated_at=excluded.updated_at",
            params![
                job.id.to_string(),
                kind_str(job.kind),
                job.prompt,
                params_json,
                state_str(job.state),
                requester.map(|d| d.to_string()),
                job.placement_device.map(|d| d.to_string()),
                job.result_blob,
                err,
                job.created_at.to_rfc3339(),
                now().to_rfc3339(),
            ],
        )
        .map(|_| ())
    })
}

pub fn get(store: &Store, id: TaskId) -> Result<Option<serde_json::Value>> {
    store.with_conn(|c| {
        Ok(c.query_row(
            "SELECT id, kind, prompt, params_json, state, requester,
                        worker, result_blob, error, created_at, updated_at
                 FROM media_jobs WHERE id=?1",
            params![id.to_string()],
            |r| Ok(row_json(r)),
        )
        .ok())
    })
}

/// Newest-first job listing.
pub fn list(store: &Store, limit: usize) -> Result<Vec<serde_json::Value>> {
    store.with_conn(|c| {
        let mut stmt = c.prepare(
            "SELECT id, kind, prompt, params_json, state, requester,
                    worker, result_blob, error, created_at, updated_at
             FROM media_jobs ORDER BY created_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| Ok(row_json(r)))?;
        rows.collect()
    })
}

fn row_json(r: &rusqlite::Row<'_>) -> serde_json::Value {
    serde_json::json!({
        "id": r.get::<_, String>(0).unwrap_or_default(),
        "kind": r.get::<_, String>(1).unwrap_or_default(),
        "prompt": r.get::<_, String>(2).unwrap_or_default(),
        "params": r.get::<_, Option<String>>(3).unwrap_or_default(),
        "state": r.get::<_, String>(4).unwrap_or_default(),
        "requester": r.get::<_, Option<String>>(5).unwrap_or_default(),
        "worker": r.get::<_, Option<String>>(6).unwrap_or_default(),
        "result_blob": r.get::<_, Option<String>>(7).unwrap_or_default(),
        "error": r.get::<_, Option<String>>(8).unwrap_or_default(),
        "created_at": r.get::<_, String>(9).unwrap_or_default(),
        "updated_at": r.get::<_, String>(10).unwrap_or_default(),
    })
}

/// Create a fresh queued job (doesn't store it — `record` does).
pub fn new_job(kind: MediaJobKind, prompt: &str) -> MediaJob {
    MediaJob {
        id: TaskId::new(),
        kind,
        prompt: prompt.to_string(),
        placement_device: None,
        state: JobState::Queued,
        created_at: now(),
        result_blob: None,
    }
}

// ---------------------------------------------------------------------------
// `media-run` op — the worker side. Payload: {"prompt", "kind"?,
// "duration_seconds"?, "width"?, "height"?, "input_b64"?, "input_mime"?}.
// `kind` defaults to text_to_audio (the original contract). Response:
// {"job_id","result_b64","mime","bytes","blob"} — `audio_b64` is also set
// for audio results, and the result lands in the worker's blob store +
// media_jobs row for `pai media jobs`.
// ---------------------------------------------------------------------------

/// Parse a media-job kind string — accepts the snake_case serde spelling
/// plus short aliases (`audio`, `image`, `video`).
pub fn kind_from_str(s: &str) -> Result<MediaJobKind> {
    match s {
        "text_to_audio" | "audio" => Ok(MediaJobKind::TextToAudio),
        "text_to_image" | "image" => Ok(MediaJobKind::TextToImage),
        "image_edit" => Ok(MediaJobKind::ImageEdit),
        "upscale" => Ok(MediaJobKind::Upscale),
        "text_to_video" | "video" => Ok(MediaJobKind::TextToVideo),
        other => Err(Error::InvalidInput(format!(
            "media-run: unknown kind '{other}'"
        ))),
    }
}

pub async fn media_run_op(
    data_dir: &Path,
    store: &Arc<Store>,
    worker: DeviceId,
    payload: &[u8],
) -> Result<Vec<u8>> {
    #[derive(serde::Deserialize)]
    struct Req {
        prompt: String,
        /// Job kind — absent means text_to_audio (the original contract).
        #[serde(default)]
        kind: Option<String>,
        #[serde(default)]
        duration_seconds: Option<u32>,
        #[serde(default)]
        width: Option<u32>,
        #[serde(default)]
        height: Option<u32>,
        /// Source image (edit/upscale kinds) — base64 + optional mime.
        #[serde(default)]
        input_b64: Option<String>,
        #[serde(default)]
        input_mime: Option<String>,
    }
    let req: Req = serde_json::from_slice(payload)
        .map_err(|e| Error::InvalidInput(format!("media-run payload JSON: {e}")))?;
    if req.prompt.trim().is_empty() {
        return Err(Error::InvalidInput("media-run: empty prompt".into()));
    }
    let kind = req
        .kind
        .as_deref()
        .map(kind_from_str)
        .transpose()?
        .unwrap_or(MediaJobKind::TextToAudio);

    let secs = req.duration_seconds.unwrap_or(10).clamp(1, 300);
    let size = (req.width.unwrap_or(512), req.height.unwrap_or(512));
    let input = match req.input_b64 {
        Some(b64) => Some((
            base64::engine::general_purpose::STANDARD
                .decode(&b64)
                .map_err(|e| Error::InvalidInput(format!("media-run input_b64: {e}")))?,
            req.input_mime.unwrap_or_else(|| "image/png".into()),
        )),
        None => None,
    };
    // No reachable backend → fail before recording — nothing to claim.
    providers::detect_for_kind(data_dir, kind, std::time::Duration::from_secs(2)).await?;

    let mut job = new_job(kind, &req.prompt);
    job.state = JobState::Running;
    job.placement_device = Some(worker);
    let params = serde_json::json!({
        "kind": kind_str(kind),
        "duration_seconds": secs,
        "size": [size.0, size.1],
        "has_input": input.is_some(),
    })
    .to_string();
    record(store, &job, Some(&params), None, None)?;

    let out = match providers::generate(data_dir, kind, &req.prompt, secs, size, input).await {
        Ok((bytes, mime)) => {
            let blob = store.put_blob(&bytes)?;
            job.state = JobState::Done;
            job.result_blob = Some(blob.clone());
            record(store, &job, Some(&params), None, None)?;
            let mut v = serde_json::json!({
                "job_id": job.id.to_string(),
                "result_b64": base64::engine::general_purpose::STANDARD.encode(&bytes),
                "mime": mime,
                "bytes": bytes.len(),
                "blob": blob,
            });
            if kind == MediaJobKind::TextToAudio {
                // original response field name — older requesters read it
                v["audio_b64"] = v["result_b64"].clone();
            }
            v
        }
        Err(e) => {
            job.state = JobState::Failed;
            record(store, &job, Some(&params), None, Some(&e.to_string()))?;
            return Err(e);
        }
    };
    Ok(out.to_string().into_bytes())
}
