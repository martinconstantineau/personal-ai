//! V2p tests: the process-based vision adapter — placeholder
//! substitution, temp-file handoff, stderr/timeout failure paths, and
//! `vision.json` roundtrip. Uses the platform shell (cmd/sh) as the
//! "VLM runner" — no model needed, the adapter boundary is what counts.

use pai_inference::ImageUnderstandingProvider;
use pai_vision::{ProcessVisionConfig, ProcessVisionProvider, VisionFileConfig};

fn shell_echo_cfg() -> ProcessVisionConfig {
    if cfg!(windows) {
        ProcessVisionConfig {
            command: "cmd".into(),
            args: vec!["/c".into(), "echo described:{prompt}".into()],
            timeout_secs: 10,
        }
    } else {
        ProcessVisionConfig {
            command: "sh".into(),
            args: vec![
                "-c".into(),
                "echo described:\"$0\"".into(),
                "{prompt}".into(),
            ],
            timeout_secs: 10,
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn process_describe_roundtrip() {
    let p = ProcessVisionProvider::detect(shell_echo_cfg()).expect("shell on PATH");
    let text = p
        .describe(b"fake-png-bytes", "image/png", "what is this")
        .await
        .unwrap();
    assert!(text.contains("described:what is this"), "got: {text:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn image_bytes_reach_process_via_tempfile() {
    // `type`/`cat` the {image} temp file — stdout echoes the bytes back,
    // proving the image actually reached the subprocess as a file.
    let cfg = if cfg!(windows) {
        ProcessVisionConfig {
            command: "cmd".into(),
            args: vec!["/c".into(), "type {image}".into()],
            timeout_secs: 10,
        }
    } else {
        ProcessVisionConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "cat \"$0\"".into(), "{image}".into()],
            timeout_secs: 10,
        }
    };
    let p = ProcessVisionProvider::detect(cfg).expect("shell on PATH");
    let text = p
        .describe(b"PNGDATA-12345", "image/png", "unused")
        .await
        .unwrap();
    assert!(text.contains("PNGDATA-12345"), "got: {text:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn nonzero_exit_surfaces_stderr() {
    let cfg = if cfg!(windows) {
        ProcessVisionConfig {
            command: "cmd".into(),
            args: vec!["/c".into(), "echo model blew up 1>&2 & exit /b 3".into()],
            timeout_secs: 10,
        }
    } else {
        ProcessVisionConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "echo model blew up >&2; exit 3".into()],
            timeout_secs: 10,
        }
    };
    let p = ProcessVisionProvider::detect(cfg).expect("shell on PATH");
    let err = p
        .describe(b"x", "image/png", "p")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("model blew up"), "got: {err}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn timeout_kills_slow_runner() {
    let cfg = if cfg!(windows) {
        // ping -n 30 waits ~29s; timeout_secs=1 kills it fast.
        ProcessVisionConfig {
            command: "cmd".into(),
            args: vec!["/c".into(), "ping -n 30 127.0.0.1 >nul".into()],
            timeout_secs: 1,
        }
    } else {
        ProcessVisionConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "sleep 30".into()],
            timeout_secs: 1,
        }
    };
    let p = ProcessVisionProvider::detect(cfg).expect("shell on PATH");
    let err = p
        .describe(b"x", "image/png", "p")
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("timed out"), "got: {err}");
}

#[test]
fn detect_missing_command_is_none() {
    let cfg = ProcessVisionConfig {
        command: "pai-definitely-not-a-real-vlm-xyz".into(),
        args: vec![],
        timeout_secs: 5,
    };
    assert!(ProcessVisionProvider::detect(cfg).is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn args_without_placeholders_pass_through() {
    let cfg = if cfg!(windows) {
        ProcessVisionConfig {
            command: "cmd".into(),
            args: vec!["/c".into(), "echo static-output".into()],
            timeout_secs: 10,
        }
    } else {
        ProcessVisionConfig {
            command: "sh".into(),
            args: vec!["-c".into(), "echo static-output".into()],
            timeout_secs: 10,
        }
    };
    let p = ProcessVisionProvider::detect(cfg).expect("shell on PATH");
    let text = p.describe(b"x", "image/png", "ignored").await.unwrap();
    assert!(text.contains("static-output"), "got: {text:?}");
}

#[test]
fn vision_json_roundtrip() {
    let dir = std::env::temp_dir().join(format!("pai-v2p-cfg-{}", std::process::id()));
    assert!(VisionFileConfig::load(&dir).unwrap().is_none());
    let cfg = VisionFileConfig {
        process: Some(ProcessVisionConfig {
            command: "python".into(),
            args: vec![
                "-m".into(),
                "mlx_vlm.generate".into(),
                "--image".into(),
                "{image}".into(),
                "--prompt".into(),
                "{prompt}".into(),
            ],
            timeout_secs: 300,
        }),
    };
    cfg.save(&dir).unwrap();
    let back = VisionFileConfig::load(&dir).unwrap().unwrap();
    let p = back.process.unwrap();
    assert_eq!(p.command, "python");
    assert_eq!(p.timeout_secs, 300);
    assert!(p.args.iter().any(|a| a == "{image}"));
    let _ = std::fs::remove_dir_all(&dir);
}
