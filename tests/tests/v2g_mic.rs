//! V2g tests: mic pipeline's hardware-independent parts — the VAD-driven
//! utterance gate, PCM resampling, and WAV decode. Real device capture/
//! playback can't run headless, so cpal paths are exercised manually.

use pai_voice::mic::{resample, wav_to_pcm16, UtteranceGate};
use pai_voice::pcm16_to_wav;

fn le_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

#[test]
fn resample_passthrough_and_halves() {
    let src: Vec<i16> = (0..100).map(|i| i as i16 * 10).collect();
    assert_eq!(resample(&src, 16000, 16000), src);
    let half = resample(&src, 48000, 24000);
    assert_eq!(half.len(), 50);
    assert_eq!(half[0], src[0]);
    // Endpoints survive the linear interp.
    assert!((half[49] as i32 - src[98] as i32).abs() <= 10);
    // Empty input is safe.
    assert!(resample(&[], 48000, 16000).is_empty());
}

#[test]
fn wav_roundtrip() {
    let samples: Vec<i16> = (0..1600)
        .map(|i| ((i * 37) % 20000) as i16 - 10000)
        .collect();
    let wav = pcm16_to_wav(&le_bytes(&samples), 16000);
    let (rate, back) = wav_to_pcm16(&wav).unwrap();
    assert_eq!(rate, 16000);
    assert_eq!(back, samples);
}

#[test]
fn wav_downmixes_stereo() {
    // Two-channel frames: L=100, R=300 → mono 200.
    let mut bytes = Vec::new();
    for _ in 0..10 {
        bytes.extend_from_slice(&100i16.to_le_bytes());
        bytes.extend_from_slice(&300i16.to_le_bytes());
    }
    // Hand-build a stereo RIFF: fmt declares 2 channels.
    let mut wav = Vec::new();
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + bytes.len() as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&2u16.to_le_bytes()); // channels
    wav.extend_from_slice(&22050u32.to_le_bytes()); // rate
    wav.extend_from_slice(&(22050u32 * 2 * 2).to_le_bytes());
    wav.extend_from_slice(&(2u16 * 2).to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    wav.extend_from_slice(&bytes);

    let (rate, mono) = wav_to_pcm16(&wav).unwrap();
    assert_eq!(rate, 22050);
    assert_eq!(mono.len(), 10);
    assert!(mono.iter().all(|&s| s == 200));
}

#[test]
fn wav_rejects_non_riff() {
    assert!(wav_to_pcm16(b"not a wav").is_err());
    assert!(wav_to_pcm16(&[]).is_err());
}

#[test]
fn utterance_gate_endpointing() {
    let frame = |v: i16| vec![v; 480]; // 30 ms @ 16kHz
    let mut g = UtteranceGate::new();

    // Silence before speech → pre-roll fills, nothing recorded.
    for _ in 0..5 {
        assert!(!g.feed(&frame(10), false));
    }
    assert!(g.pcm().is_empty());

    // Speech starts → pre-roll (last 5 frames) flushes into the buffer.
    assert!(!g.feed(&frame(5000), true));
    assert!(g.heard_speech());
    assert_eq!(g.pcm().len(), 6 * 480);

    // More speech keeps it open.
    for _ in 0..3 {
        assert!(!g.feed(&frame(5000), true));
    }

    // Trailing silence: SILENCE_LIMIT+1 silent frames end the utterance.
    let mut done = false;
    for _ in 0..30 {
        done = g.feed(&frame(10), false);
        if done {
            break;
        }
    }
    assert!(done, "gate never closed on trailing silence");
    assert!(g.pcm().len() >= 9 * 480);
}

#[test]
fn gate_never_heard_returns_empty() {
    let mut g = UtteranceGate::new();
    for _ in 0..50 {
        assert!(!g.feed(&vec![0i16; 480], false));
    }
    assert!(!g.heard_speech());
    assert!(g.pcm().is_empty());
}

// -- hardware smoke (manual): run with `cargo test -- --ignored` --------

/// Opens the real input stream for up to 3s and captures ambient audio.
/// Passes when a device exists and the stream opens — content is
/// discarded (nothing leaves the machine).
#[test]
#[ignore = "needs a microphone"]
fn capture_opens_default_mic() {
    if !pai_voice::mic::input_available() {
        eprintln!("no input device — skipping");
        return;
    }
    let vad = pai_voice::EnergyVad::new(1.0, 0); // any noise counts
    let pcm = pai_voice::mic::capture_utterance(&vad, 3).unwrap();
    assert!(!pcm.is_empty(), "stream opened but captured nothing");
    eprintln!("captured {} samples @16kHz", pcm.len());
}
