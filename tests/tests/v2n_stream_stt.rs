//! V2n tests: streaming STT — the SegmentGate state machine and the
//! per-segment transcription layer. Real mic capture stays behind the
//! ignored hardware smoke test; everything else is deterministic.

use async_trait::async_trait;
use pai_core::{Error, Result};
use pai_inference::SpeechToTextProvider;
use pai_voice::mic::{is_quiet, GateEvent, SegmentGate};
use pai_voice::transcribe_pcm_segments;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

fn frame(v: i16) -> Vec<i16> {
    vec![v; 480] // 30 ms @ 16kHz
}

const SILENCE: i16 = 10;
const SPEECH: i16 = 5000;

// -- SegmentGate ------------------------------------------------------

#[test]
fn segment_gate_emits_at_pause_and_continues() {
    let mut g = SegmentGate::new();
    for _ in 0..5 {
        assert_eq!(g.feed(&frame(SILENCE), false), GateEvent::Continue);
    }
    // Speech starts — pre-roll included in the first segment.
    assert_eq!(g.feed(&frame(SPEECH), true), GateEvent::Continue);
    for _ in 0..9 {
        assert_eq!(g.feed(&frame(SPEECH), true), GateEvent::Continue);
    }
    // ~400 ms pause (13 silent frames) → Segment event.
    let mut ev = GateEvent::Continue;
    for _ in 0..13 {
        ev = g.feed(&frame(SILENCE), false);
    }
    assert_eq!(ev, GateEvent::Segment);
    let seg1 = g.take_segment();
    // 5 pre-roll + 10 speech + 13 pause frames.
    assert_eq!(seg1.len(), 28 * 480);

    // Speech resumes → the utterance continues, new segment accumulates.
    for _ in 0..6 {
        assert_eq!(g.feed(&frame(SPEECH), true), GateEvent::Continue);
    }
    // Long trailing silence → Done with the remainder.
    let mut ev = GateEvent::Continue;
    for _ in 0..30 {
        ev = g.feed(&frame(SILENCE), false);
        if ev == GateEvent::Done {
            break;
        }
    }
    assert_eq!(ev, GateEvent::Done);
    let seg2 = g.take_segment();
    assert!(!seg2.is_empty());
}

#[test]
fn segment_gate_no_segment_before_speech() {
    let mut g = SegmentGate::new();
    for _ in 0..50 {
        assert_eq!(g.feed(&frame(SILENCE), false), GateEvent::Continue);
    }
    assert!(!g.heard_speech());
    assert!(g.take_segment().is_empty());
}

#[test]
fn segment_gate_segment_fires_once_per_pause() {
    let mut g = SegmentGate::new();
    g.feed(&frame(SPEECH), true);
    // Drift through the pause window and beyond: exactly one Segment
    // event, then Continue until Done.
    let mut events = Vec::new();
    for _ in 0..40 {
        events.push(g.feed(&frame(SILENCE), false));
        if events.last() == Some(&GateEvent::Done) {
            break;
        }
    }
    assert_eq!(
        events.iter().filter(|e| **e == GateEvent::Segment).count(),
        1
    );
    assert_eq!(events.last(), Some(&GateEvent::Done));
}

#[test]
fn segment_gate_preserves_preroll_on_first_speech() {
    let mut g = SegmentGate::new();
    // 20 quiet frames → only the last ~10 are kept as pre-roll.
    for i in 0..20i16 {
        g.feed(&vec![i; 480], false);
    }
    g.feed(&frame(SPEECH), true);
    let mut ev = GateEvent::Continue;
    for _ in 0..13 {
        ev = g.feed(&frame(SILENCE), false);
    }
    assert_eq!(ev, GateEvent::Segment);
    let seg = g.take_segment();
    // 10 pre-roll + 1 speech + 13 pause.
    assert_eq!(seg.len(), 24 * 480);
    // The pre-roll kept is the *most recent* quiet frames (10..19).
    assert_eq!(seg[0], 10);
}

#[test]
fn is_quiet_filters_silence_keeps_speech() {
    assert!(is_quiet(&[]));
    assert!(is_quiet(&frame(SILENCE)));
    assert!(is_quiet(&vec![100i16; 480]));
    assert!(!is_quiet(&frame(SPEECH)));
}

// -- transcribe_pcm_segments -------------------------------------------

struct MockStt {
    calls: AtomicUsize,
    fail_at: Mutex<Option<usize>>,
}

impl MockStt {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            fail_at: Mutex::new(None),
        }
    }
}

#[async_trait]
impl SpeechToTextProvider for MockStt {
    fn id(&self) -> &'static str {
        "mock"
    }
    async fn transcribe(&self, audio: &[u8], mime: &str) -> Result<String> {
        assert_eq!(mime, "audio/wav");
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        if *self.fail_at.lock().unwrap() == Some(n) {
            return Err(Error::Provider("mock stt failure".into()));
        }
        // Decodes the WAV payload size → echoes a deterministic marker.
        Ok(format!("seg{n}:{}bytes", audio.len()))
    }
}

fn loud_seg(n: usize) -> Vec<i16> {
    vec![SPEECH; 480 * n]
}

#[tokio::test]
async fn segments_transcribe_in_order() {
    let stt = MockStt::new();
    let segs = vec![loud_seg(10), loud_seg(20), loud_seg(5)];
    let mut partials: Vec<String> = Vec::new();
    let mut cb = |p: &str| partials.push(p.to_string());
    let text = transcribe_pcm_segments(&stt, &segs, &mut cb).await.unwrap();
    assert_eq!(stt.calls.load(Ordering::SeqCst), 3);
    assert_eq!(partials.len(), 3);
    assert!(partials[0].starts_with("seg0:"));
    assert!(partials[1].starts_with("seg1:"));
    assert!(partials[2].starts_with("seg2:"));
    assert!(text.contains("seg0:") && text.contains("seg2:"));
}

#[tokio::test]
async fn silent_segments_skip_provider_calls() {
    let stt = MockStt::new();
    let segs = vec![loud_seg(10), frame(SILENCE), loud_seg(10)];
    let mut partials: Vec<String> = Vec::new();
    let mut cb = |p: &str| partials.push(p.to_string());
    transcribe_pcm_segments(&stt, &segs, &mut cb).await.unwrap();
    // The middle silence-only segment never reaches whisper.
    assert_eq!(stt.calls.load(Ordering::SeqCst), 2);
    assert_eq!(partials.len(), 2);
}

#[tokio::test]
async fn stt_error_propagates_deterministically() {
    let stt = MockStt::new();
    *stt.fail_at.lock().unwrap() = Some(1);
    let segs = vec![loud_seg(5), loud_seg(5), loud_seg(5)];
    let mut cb = |_: &str| {};
    let err = transcribe_pcm_segments(&stt, &segs, &mut cb)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("mock stt failure"));
    // Calls before the failure landed; the failing call aborts the rest.
    assert_eq!(stt.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn empty_and_all_silent_transcribe_to_empty() {
    let stt = MockStt::new();
    let mut cb = |_: &str| panic!("no partial expected");
    let text = transcribe_pcm_segments(&stt, &[], &mut cb).await.unwrap();
    assert!(text.is_empty());
    let text = transcribe_pcm_segments(&stt, &[frame(SILENCE)], &mut cb)
        .await
        .unwrap();
    assert!(text.is_empty());
    assert_eq!(stt.calls.load(Ordering::SeqCst), 0);
}

// -- hardware smoke (manual) -------------------------------------------

/// End-to-end streaming listen on the real mic + configured whisper
/// server. Ignored by default — needs hardware + `pai voice configure`.
#[test]
#[ignore = "needs mic + whisper-server"]
fn stream_transcribe_live() {
    if !pai_voice::mic::input_available() {
        eprintln!("no input device — skipping");
        return;
    }
    // Any-noise VAD so ambient sound exercises the segmentation path.
    let vad = pai_voice::EnergyVad::new(1.0, 0);
    let whisper =
        std::env::var("PAI_WHISPER_URL").unwrap_or_else(|_| "http://127.0.0.1:8080".to_string());
    let stt = pai_voice::providers::WhisperServerStt::new(&whisper);
    let mut partials = Vec::new();
    match pai_voice::stream_transcribe(&stt, &vad, 8, &mut |p| {
        eprintln!("partial: {p}");
        partials.push(p.to_string());
    }) {
        Ok(text) => eprintln!("final: {text} ({} partials)", partials.len()),
        Err(e) => eprintln!("stt unavailable: {e}"),
    }
}
