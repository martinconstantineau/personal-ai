//! Live audio: mic capture (VAD-endpointed) + speaker playback via cpal.
//!
//! `cpal` is the only OS-audio dependency. It stays on the `windows` 0.54
//! chain — `windows-sys` 0.52 ships prebuilt import libs, so the
//! windows-gnu toolchain needs no binutils (0.60+ would invoke dlltool).

use crate::EnergyVad;
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use pai_core::*;
use pai_inference::VoiceActivityProvider;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

/// WASAPI (cpal's Windows backend) caches its `IMMDeviceEnumerator` in a
/// process-wide `OnceLock`: it is created on whichever thread first
/// touches cpal, then shared with every later caller. COM objects are
/// apartment-bound, so when that first thread exits and its apartment
/// is torn down (`CoUninitialize`), the cached enumerator dangles and
/// the next call access-violates inside `GetDefaultAudioEndpoint` —
/// the crash `pai serve` request threads trigger on the second audio
/// call. Every cpal touch therefore runs on one long-lived worker
/// thread whose COM apartment (and thus the cached enumerator) stays
/// valid for the process lifetime.
#[cfg(windows)]
mod com {
    #[link(name = "ole32")]
    extern "system" {
        pub fn CoInitializeEx(reserved: *mut std::ffi::c_void, coinit: u32) -> i32;
    }
}

#[cfg(windows)]
fn com_thread<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    use std::sync::{mpsc, OnceLock};
    type Task = Box<dyn FnOnce() + Send>;
    static SENDER: OnceLock<mpsc::SyncSender<Task>> = OnceLock::new();
    let tx = SENDER.get_or_init(|| {
        let (tx, rx) = mpsc::sync_channel::<Task>(8);
        std::thread::spawn(move || {
            const COINIT_MULTITHREADED: u32 = 0x0;
            unsafe { com::CoInitializeEx(std::ptr::null_mut(), COINIT_MULTITHREADED) };
            while let Ok(task) = rx.recv() {
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(task));
            }
        });
        tx
    });
    let (rtx, rrx) = mpsc::channel();
    let task: Box<dyn FnOnce() + Send + '_> = Box::new(move || drop(rtx.send(f())));
    // SAFETY: the caller blocks on `rrx.recv()` until the worker has run
    // the task to completion, so every borrow captured in `f` outlives
    // the call — the same guarantee `std::thread::scope` gives.
    let task: Box<dyn FnOnce() + Send + 'static> = unsafe { std::mem::transmute(task) };
    if tx.send(task).is_err() {
        panic!("audio worker thread dead");
    }
    rrx.recv().expect("audio worker panicked")
}

#[cfg(not(windows))]
fn com_thread<T: Send>(f: impl FnOnce() -> T + Send) -> T {
    f()
}

/// What whisper wants: mono 16 kHz i16.
pub const TARGET_RATE: u32 = 16_000;
const FRAME_MS: usize = 30;
/// Frames of trailing post-hangover silence that end an utterance.
const SILENCE_LIMIT: u32 = 25;
/// Pre-roll kept so the first phoneme isn't clipped (~10 frames).
const PRE_ROLL: usize = 10;

/// VAD-driven endpointing, factored out of the cpal loop for testability.
/// Feed 30 ms mono frames; once speech starts it buffers every frame and
/// reports `done` after `SILENCE_LIMIT` consecutive non-speech frames.
pub struct UtteranceGate {
    pre_roll: VecDeque<Vec<i16>>,
    out: Vec<i16>,
    heard: bool,
    silent: u32,
}

impl Default for UtteranceGate {
    fn default() -> Self {
        Self::new()
    }
}

impl UtteranceGate {
    pub fn new() -> Self {
        Self {
            pre_roll: VecDeque::new(),
            out: Vec::new(),
            heard: false,
            silent: 0,
        }
    }

    /// Returns `true` when the utterance is complete (trailing silence).
    pub fn feed(&mut self, frame: &[i16], speech: bool) -> bool {
        if speech {
            if !self.heard {
                self.heard = true;
                for f in self.pre_roll.drain(..) {
                    self.out.extend_from_slice(&f);
                }
            }
            self.out.extend_from_slice(frame);
            self.silent = 0;
            false
        } else if self.heard {
            self.silent += 1;
            self.silent > SILENCE_LIMIT
        } else {
            self.pre_roll.push_back(frame.to_vec());
            if self.pre_roll.len() > PRE_ROLL {
                self.pre_roll.pop_front();
            }
            false
        }
    }

    /// Samples captured so far (empty until speech is heard).
    pub fn pcm(&self) -> &[i16] {
        &self.out
    }

    pub fn heard_speech(&self) -> bool {
        self.heard
    }
}

/// Frames of pause that close a streaming segment (~400 ms) — shorter
/// than the utterance-ending SILENCE_LIMIT so partials emit mid-speech.
const SEG_SILENCE: u32 = 13;

/// What a `SegmentGate` feed produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateEvent {
    /// Keep capturing.
    Continue,
    /// A pause-finalized segment is ready in `take_segment`.
    Segment,
    /// The utterance is over (trailing silence); remainder in `take_segment`.
    Done,
}

/// Streaming variant of `UtteranceGate`: a pause ≥ ~400 ms closes the
/// current segment (emitted for partial transcription) while the
/// utterance continues; ~750 ms of trailing silence still ends it.
/// Factored out of the cpal loop for testability.
pub struct SegmentGate {
    pre_roll: VecDeque<Vec<i16>>,
    seg: Vec<i16>,
    heard: bool,
    silent: u32,
    emitted: bool,
}

impl Default for SegmentGate {
    fn default() -> Self {
        Self::new()
    }
}

impl SegmentGate {
    pub fn new() -> Self {
        Self {
            pre_roll: VecDeque::new(),
            seg: Vec::new(),
            heard: false,
            silent: 0,
            emitted: false,
        }
    }

    pub fn feed(&mut self, frame: &[i16], speech: bool) -> GateEvent {
        if speech {
            if !self.heard {
                self.heard = true;
                for f in self.pre_roll.drain(..) {
                    self.seg.extend_from_slice(&f);
                }
            }
            self.seg.extend_from_slice(frame);
            self.silent = 0;
            self.emitted = false;
            GateEvent::Continue
        } else if self.heard {
            self.silent += 1;
            self.seg.extend_from_slice(frame);
            if self.silent > SILENCE_LIMIT {
                GateEvent::Done
            } else if self.silent == SEG_SILENCE && !self.emitted {
                self.emitted = true;
                GateEvent::Segment
            } else {
                GateEvent::Continue
            }
        } else {
            self.pre_roll.push_back(frame.to_vec());
            if self.pre_roll.len() > PRE_ROLL {
                self.pre_roll.pop_front();
            }
            GateEvent::Continue
        }
    }

    /// Drain the current segment (pause frames included — a short
    /// silence tail is harmless to whisper).
    pub fn take_segment(&mut self) -> Vec<i16> {
        std::mem::take(&mut self.seg)
    }

    pub fn heard_speech(&self) -> bool {
        self.heard
    }
}

/// Cheap quiet check on mono i16 — skips whisper calls on pure-silence
/// trailing segments. ~RMS 200 is well below normal speech.
pub fn is_quiet(pcm: &[i16]) -> bool {
    if pcm.is_empty() {
        return true;
    }
    let energy: f64 = pcm.iter().map(|s| (*s as f64) * (*s as f64)).sum::<f64>() / pcm.len() as f64;
    energy.sqrt() < 200.0
}

/// True when a default input device exists (doesn't open it — OS
/// permission prompts only fire on stream start).
pub fn input_available() -> bool {
    com_thread(|| cpal::default_host().default_input_device().is_some())
}

/// True when a default output device exists.
pub fn output_available() -> bool {
    com_thread(|| cpal::default_host().default_output_device().is_some())
}

/// Minimal block_on — the VAD's async method is pure CPU; no IO drivers
/// needed (same trick as `pai-sync`'s relay). NOT for reqwest futures —
/// those need a real runtime (see `stream_transcribe`).
fn block_on<F: std::future::Future>(f: F) -> F::Output {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut f = std::pin::pin!(f);
    loop {
        if let std::task::Poll::Ready(v) = f.as_mut().poll(&mut cx) {
            return v;
        }
        std::thread::yield_now();
    }
}

/// Linear-interp resample mono i16 between rates. Speech-band audio is
/// tolerant; this keeps the dep list at cpal-only.
pub fn resample(src: &[i16], src_rate: u32, dst_rate: u32) -> Vec<i16> {
    if src_rate == dst_rate || src.is_empty() {
        return src.to_vec();
    }
    let ratio = src_rate as f64 / dst_rate as f64;
    let n = (src.len() as f64 / ratio) as usize;
    (0..n)
        .map(|i| {
            let pos = i as f64 * ratio;
            let j = pos as usize;
            let frac = pos - j as f64;
            let a = src.get(j).copied().unwrap_or(0) as f64;
            let b = src.get(j + 1).copied().unwrap_or(a as i16) as f64;
            (a + (b - a) * frac) as i16
        })
        .collect()
}

/// Parse a RIFF/WAVE buffer back to (sample_rate, mono i16 samples).
/// Handles the canonical 44-byte layout `pcm16_to_wav` writes plus other
/// chunk layouts by scanning for `fmt `/`data`.
pub fn wav_to_pcm16(wav: &[u8]) -> Result<(u32, Vec<i16>)> {
    if wav.len() < 12 || &wav[0..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(Error::InvalidInput("not a WAVE file".into()));
    }
    let mut rate = 0u32;
    let mut channels = 1usize;
    let mut data: Option<&[u8]> = None;
    let mut off = 12;
    while off + 8 <= wav.len() {
        let tag = &wav[off..off + 4];
        let size = u32::from_le_bytes(wav[off + 4..off + 8].try_into().unwrap()) as usize;
        let body = &wav[off + 8..(off + 8 + size).min(wav.len())];
        if tag == b"fmt " && body.len() >= 16 {
            channels = u16::from_le_bytes(body[2..4].try_into().unwrap()) as usize;
            rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
        } else if tag == b"data" {
            data = Some(body);
        }
        off += 8 + size + (size & 1); // chunks are 2-byte aligned
    }
    let data = data.ok_or_else(|| Error::InvalidInput("WAVE has no data chunk".into()))?;
    if rate == 0 {
        return Err(Error::InvalidInput("WAVE has no fmt chunk".into()));
    }
    // Downmix to mono, i16 LE assumed (what we write).
    let ch = channels.max(1);
    let mut out = Vec::with_capacity(data.len() / 2 / ch);
    for frame in data.chunks_exact(2 * ch) {
        let mut acc = 0i32;
        for c in 0..ch {
            acc += i16::from_le_bytes(frame[2 * c..2 * c + 2].try_into().unwrap()) as i32;
        }
        out.push((acc / ch as i32) as i16);
    }
    Ok((rate, out))
}

/// Capture one utterance from the default input device: records while the
/// VAD sees speech (+ hangover), ends on ~750 ms of trailing silence or
/// `max_secs`. Returns mono 16 kHz i16 — empty when nothing was heard.
pub fn capture_utterance(vad: &EnergyVad, max_secs: u32) -> Result<Vec<i16>> {
    com_thread(|| capture_utterance_inner(vad, max_secs))
}

fn capture_utterance_inner(vad: &EnergyVad, max_secs: u32) -> Result<Vec<i16>> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| Error::InvalidInput("no audio input device".into()))?;
    let supported = device
        .default_input_config()
        .map_err(|e| Error::InvalidInput(format!("input device: {e}")))?;
    let src_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let cfg = supported.config();

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let err_fn = |e| tracing::warn!(error = %e, "mic stream error");
    let tx2 = tx.clone();
    let tx3 = tx.clone();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &cfg,
            move |d: &[f32], _| {
                let _ = tx.send(d.to_vec());
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &cfg,
            move |d: &[i16], _| {
                let _ = tx2.send(d.iter().map(|s| *s as f32 / 32768.0).collect());
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &cfg,
            move |d: &[u16], _| {
                let _ = tx3.send(d.iter().map(|s| (*s as f32 - 32768.0) / 32768.0).collect());
            },
            err_fn,
            None,
        ),
        other => {
            return Err(Error::InvalidInput(format!(
                "unsupported mic sample format {other:?}"
            )))
        }
    }
    .map_err(|e| Error::InvalidInput(format!("open mic: {e}")))?;
    stream
        .play()
        .map_err(|e| Error::InvalidInput(format!("start mic: {e}")))?;

    let frame_len = src_rate as usize * FRAME_MS / 1000;
    let mut mono: Vec<f32> = Vec::new();
    let mut gate = UtteranceGate::new();
    let deadline = Instant::now() + Duration::from_secs(max_secs.into());
    let mut done = false;
    while !done && Instant::now() < deadline {
        let chunk = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        for f in chunk.chunks(channels.max(1)) {
            mono.push(f.iter().sum::<f32>() / channels.max(1) as f32);
        }
        while mono.len() >= frame_len {
            let frame: Vec<i16> = mono
                .drain(..frame_len)
                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let speech = block_on(vad.is_speech(&frame)).unwrap_or(false);
            done = gate.feed(&frame, speech);
        }
    }
    drop(stream);
    if !gate.heard_speech() {
        return Ok(Vec::new());
    }
    Ok(resample(gate.pcm(), src_rate, TARGET_RATE))
}

/// Segmented capture: same device loop as `capture_utterance`, but every
/// ~400 ms pause hands the finalized segment (resampled to 16 kHz) to
/// `on_segment` for partial transcription while capture continues.
/// Returns the full utterance (all segments concatenated), empty when
/// nothing was heard. `on_segment` runs inline — the cpal channel keeps
/// buffering so no audio is lost while it works.
pub fn capture_segmented(
    vad: &EnergyVad,
    max_secs: u32,
    on_segment: &mut (dyn FnMut(Vec<i16>) + Send),
) -> Result<Vec<i16>> {
    com_thread(|| capture_segmented_inner(vad, max_secs, on_segment))
}

fn capture_segmented_inner(
    vad: &EnergyVad,
    max_secs: u32,
    on_segment: &mut dyn FnMut(Vec<i16>),
) -> Result<Vec<i16>> {
    let host = cpal::default_host();
    let device = host
        .default_input_device()
        .ok_or_else(|| Error::InvalidInput("no audio input device".into()))?;
    let supported = device
        .default_input_config()
        .map_err(|e| Error::InvalidInput(format!("input device: {e}")))?;
    let src_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    let cfg = supported.config();

    let (tx, rx) = mpsc::channel::<Vec<f32>>();
    let err_fn = |e| tracing::warn!(error = %e, "mic stream error");
    let tx2 = tx.clone();
    let tx3 = tx.clone();
    let stream = match supported.sample_format() {
        cpal::SampleFormat::F32 => device.build_input_stream(
            &cfg,
            move |d: &[f32], _| {
                let _ = tx.send(d.to_vec());
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::I16 => device.build_input_stream(
            &cfg,
            move |d: &[i16], _| {
                let _ = tx2.send(d.iter().map(|s| *s as f32 / 32768.0).collect());
            },
            err_fn,
            None,
        ),
        cpal::SampleFormat::U16 => device.build_input_stream(
            &cfg,
            move |d: &[u16], _| {
                let _ = tx3.send(d.iter().map(|s| (*s as f32 - 32768.0) / 32768.0).collect());
            },
            err_fn,
            None,
        ),
        other => {
            return Err(Error::InvalidInput(format!(
                "unsupported mic sample format {other:?}"
            )))
        }
    }
    .map_err(|e| Error::InvalidInput(format!("open mic: {e}")))?;
    stream
        .play()
        .map_err(|e| Error::InvalidInput(format!("start mic: {e}")))?;

    let frame_len = src_rate as usize * FRAME_MS / 1000;
    let mut mono: Vec<f32> = Vec::new();
    let mut gate = SegmentGate::new();
    let mut all: Vec<i16> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(max_secs.into());
    let mut done = false;
    while !done && Instant::now() < deadline {
        let chunk = match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(c) => c,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        for f in chunk.chunks(channels.max(1)) {
            mono.push(f.iter().sum::<f32>() / channels.max(1) as f32);
        }
        while mono.len() >= frame_len {
            let frame: Vec<i16> = mono
                .drain(..frame_len)
                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0) as i16)
                .collect();
            let speech = block_on(vad.is_speech(&frame)).unwrap_or(false);
            match gate.feed(&frame, speech) {
                GateEvent::Segment => {
                    let seg = resample(&gate.take_segment(), src_rate, TARGET_RATE);
                    all.extend_from_slice(&seg);
                    on_segment(seg);
                }
                GateEvent::Done => {
                    let seg = resample(&gate.take_segment(), src_rate, TARGET_RATE);
                    all.extend_from_slice(&seg);
                    on_segment(seg);
                    done = true;
                }
                GateEvent::Continue => {}
            }
        }
    }
    drop(stream);
    // Deadline/disconnect mid-utterance: flush whatever speech is still
    // buffered so the tail isn't lost (a pure-silence remainder reaches
    // the callback too — callers filter with `is_quiet`).
    let tail = gate.take_segment();
    if gate.heard_speech() && !tail.is_empty() {
        let seg = resample(&tail, src_rate, TARGET_RATE);
        all.extend_from_slice(&seg);
        on_segment(seg);
    }
    if !gate.heard_speech() {
        return Ok(Vec::new());
    }
    Ok(all)
}

/// Play mono i16 PCM through the default output device; blocks until the
/// buffer is consumed. Resamples when the device can't take the rate.
pub fn play(pcm: &[i16], sample_rate: u32) -> Result<()> {
    com_thread(|| play_inner(pcm, sample_rate))
}

fn play_inner(pcm: &[i16], sample_rate: u32) -> Result<()> {
    if pcm.is_empty() {
        return Ok(());
    }
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| Error::InvalidInput("no audio output device".into()))?;

    // Prefer an output config at the PCM's own rate; else resample to the
    // device default.
    let supported: Vec<_> = device
        .supported_output_configs()
        .map_err(|e| Error::InvalidInput(format!("output configs: {e}")))?
        .collect();
    let native = supported.iter().find(|r| {
        r.channels() == 1
            && r.min_sample_rate().0 <= sample_rate
            && sample_rate <= r.max_sample_rate().0
    });
    let any_rate = supported
        .iter()
        .find(|r| r.min_sample_rate().0 <= sample_rate && sample_rate <= r.max_sample_rate().0);
    let (cfg, samples) = if let Some(r) = native.or(any_rate) {
        let cfg = r.with_sample_rate(cpal::SampleRate(sample_rate)).config();
        let pcm16 = pcm.to_vec();
        (cfg, pcm16)
    } else {
        let cfg = device
            .default_output_config()
            .map_err(|e| Error::InvalidInput(format!("output device: {e}")))?
            .config();
        (cfg.clone(), resample(pcm, sample_rate, cfg.sample_rate.0))
    };
    let channels = cfg.channels as usize;
    let f32_samples: Arc<Vec<f32>> =
        Arc::new(samples.iter().map(|s| *s as f32 / 32768.0).collect());
    let total = f32_samples.len();
    let pos = Arc::new(AtomicUsize::new(0));
    let err_fn = |e| tracing::warn!(error = %e, "playback stream error");

    let src = f32_samples.clone();
    let p = pos.clone();
    let write = move |out: &mut [f32], _: &cpal::OutputCallbackInfo| {
        for frame in out.chunks_mut(channels.max(1)) {
            let i = p.fetch_add(1, Relaxed);
            let s = src.get(i).copied().unwrap_or(0.0);
            for ch in frame.iter_mut() {
                *ch = s;
            }
        }
    };
    let stream = device
        .build_output_stream(&cfg, write, err_fn, None)
        .map_err(|e| Error::InvalidInput(format!("open speaker: {e}")))?;
    stream
        .play()
        .map_err(|e| Error::InvalidInput(format!("start playback: {e}")))?;
    while pos.load(Relaxed) < total {
        std::thread::sleep(Duration::from_millis(10));
    }
    // Let the last buffer drain.
    std::thread::sleep(Duration::from_millis(80));
    Ok(())
}
