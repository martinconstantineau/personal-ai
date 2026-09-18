//! V3c tests: process-boundary inference — the on-device adapter that
//! lets ExecuTorch/MLC/llama-cli (or any local generator) plug in via
//! inference.json without linking a runtime. Tests run real subprocesses
//! through the platform shell, which resolves via PATH on Windows
//! (cmd.exe) and Unix (sh) alike.

use futures::StreamExt;
use pai_core::*;
use pai_inference::process::{
    render_prompt, InferenceFileConfig, ProcessInferenceConfig, ProcessInferenceProvider,
};
use pai_inference::{AIRequest, InferenceProvider, ModelAction, StreamEvent};
use std::sync::Arc;

fn shell() -> &'static str {
    if cfg!(windows) {
        "cmd"
    } else {
        "sh"
    }
}

fn cfg(args: Vec<&str>) -> ProcessInferenceConfig {
    ProcessInferenceConfig {
        command: shell().into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        model: Some("test-model".into()),
        timeout_secs: 30,
    }
}

fn req(text: &str) -> AIRequest {
    AIRequest {
        messages: vec![Message {
            id: MessageId::new(),
            conversation: ConversationId(uuid::Uuid::new_v4()),
            role: Role::User,
            created_at: pai_core::now(),
            content: vec![Content::text(text)],
            trust: TrustLevel::User,
        }],
        tools: vec![],
        model: Some("test-model".into()),
        temperature: None,
        max_tokens: None,
        require_structured: false,
        system: None,
    }
}

#[test]
fn render_prompt_lists_roles_and_tools() {
    let mut r = req("hello there");
    r.tools = vec![pai_inference::ToolSpec {
        name: "memory.remember".into(),
        description: "store a fact".into(),
        input_schema: serde_json::json!({"type": "object"}),
    }];
    let p = render_prompt(&r);
    assert!(p.contains("tools:"), "{p}");
    assert!(p.contains("memory.remember"), "{p}");
    assert!(p.contains("User: hello there"), "{p}");
}

#[tokio::test]
async fn argv_substitution_delivers_prompt() {
    // cmd /c echo <arg> / sh -c echo — stdout must be the rendered prompt.
    let args = if cfg!(windows) {
        vec!["/c", "echo", "{prompt}"]
    } else {
        vec!["-c", "echo \"$1\"", "--", "{prompt}"]
    };
    let p = ProcessInferenceProvider::detect(cfg(args)).unwrap();
    let out = p.generate(&req("one-line-prompt")).await.unwrap();
    assert!(out.text.contains("one-line-prompt"), "{}", out.text);
    assert_eq!(out.model.as_deref(), Some("test-model"));
}

#[tokio::test]
async fn stdin_delivery_when_no_placeholder() {
    // No {prompt} arg → prompt is piped to stdin. `more` (Windows) and
    // `cat` (Unix) both copy stdin to stdout.
    let args = if cfg!(windows) {
        vec!["/c", "more"]
    } else {
        vec!["-c", "cat"]
    };
    let p = ProcessInferenceProvider::detect(ProcessInferenceConfig {
        command: shell().into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        model: None,
        timeout_secs: 30,
    })
    .unwrap();
    let out = p.generate(&req("stdin-prompt")).await.unwrap();
    assert!(out.text.contains("stdin-prompt"), "{}", out.text);
}

#[tokio::test]
async fn nonzero_exit_surfaces_error() {
    let args = if cfg!(windows) {
        vec!["/c", "exit", "7"]
    } else {
        vec!["-c", "exit 7"]
    };
    let p = ProcessInferenceProvider::detect(cfg(args)).unwrap();
    let err = p.generate(&req("x")).await.unwrap_err();
    assert!(err.to_string().contains("exited"), "{err}");
}

#[tokio::test]
async fn timeout_kills_the_process() {
    // ping -n 30 / sleep 30 — both stall past the 1s bound.
    let (command, args) = if cfg!(windows) {
        ("ping", vec!["-n", "30", "127.0.0.1"])
    } else {
        ("sleep", vec!["30"])
    };
    let p = ProcessInferenceProvider::detect(ProcessInferenceConfig {
        command: command.into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        model: None,
        timeout_secs: 1,
    })
    .unwrap();
    let err = p.generate(&req("x")).await.unwrap_err();
    assert!(matches!(err, Error::Timeout), "{err}");
}

#[tokio::test]
async fn action_protocol_parsed_from_stdout() {
    // A runner that emits the action JSON gets it parsed — the agent
    // loop can drive tools through a process backend.
    let dir = std::env::temp_dir().join(format!("pai-v3c-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let f = dir.join("action.json");
    std::fs::write(&f, r#"{"type":"final","content":"proc says hi"}"#).unwrap();
    let args = if cfg!(windows) {
        vec![
            "/c".to_string(),
            "type".to_string(),
            f.to_str().unwrap().to_string(),
        ]
    } else {
        vec!["-c".to_string(), format!("cat {}", f.display())]
    };
    let c = ProcessInferenceConfig {
        command: shell().into(),
        args,
        model: Some("test-model".into()),
        timeout_secs: 30,
    };
    let p = ProcessInferenceProvider::detect(c).unwrap();
    let out = p.generate(&req("ignored")).await.unwrap();
    match out.action {
        Some(ModelAction::Final { content }) => assert_eq!(content, "proc says hi"),
        other => panic!("expected parsed Final action, got {other:?}"),
    }
}

#[tokio::test]
async fn stream_emits_deltas_then_done() {
    // Two echoed lines → Delta per line, Done carries the joined text.
    let args = if cfg!(windows) {
        vec!["/c", "echo", "one", "&", "echo", "two"]
    } else {
        vec!["-c", "echo one; echo two"]
    };
    let p = ProcessInferenceProvider::detect(cfg(args)).unwrap();
    let mut deltas = Vec::new();
    let mut done_text = None;
    let mut s = p.stream(req("x"));
    while let Some(item) = s.next().await {
        match item.unwrap() {
            StreamEvent::Delta(d) => deltas.push(d),
            StreamEvent::Done(r) => done_text = Some(r.text),
            StreamEvent::Error(e) => panic!("stream error: {e}"),
        }
    }
    assert_eq!(deltas.len(), 2, "{deltas:?}");
    let text = done_text.expect("Done event required");
    assert!(text.contains("one") && text.contains("two"), "{text}");
}

#[tokio::test]
async fn stream_timeout_kills_silent_process() {
    // A runner that hangs without writing must still hit the deadline —
    // the waiter thread owns it, independent of stdout EOF.
    let args = if cfg!(windows) {
        // `>nul` keeps the pipe silent; ping outlives the 1s bound.
        // (kill() takes cmd; the ping grandchild exits ~4s later.)
        vec!["/c", "ping", "-n", "5", "127.0.0.1", ">nul"]
    } else {
        vec!["-c", "exec sleep 30"]
    };
    let p = ProcessInferenceProvider::detect(ProcessInferenceConfig {
        command: shell().into(),
        args: args.iter().map(|s| s.to_string()).collect(),
        model: None,
        timeout_secs: 1,
    })
    .unwrap();
    let mut s = p.stream(req("x"));
    let mut saw_err = false;
    while let Some(item) = s.next().await {
        if let Ok(StreamEvent::Error(e)) = item {
            assert!(e.contains("timed out"), "{e}");
            saw_err = true;
        }
    }
    assert!(saw_err, "expected a timed-out Error event");
}

#[test]
fn detect_returns_none_for_missing_command() {
    assert!(ProcessInferenceProvider::detect(ProcessInferenceConfig {
        command: "pai-nonexistent-bin-xyz".into(),
        args: vec![],
        model: None,
        timeout_secs: 1,
    })
    .is_none());
}

#[test]
fn inference_json_roundtrips() {
    let dir = std::env::temp_dir().join(format!("pai-v3c-cfg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("inference.json"),
        r#"{"process": {"command": "llama-cli", "args": ["-m", "m.gguf", "-p", "{prompt}"], "timeout_secs": 120}}"#,
    )
    .unwrap();
    let c = InferenceFileConfig::load(&dir).unwrap().unwrap();
    let pc = c.process.unwrap();
    assert_eq!(pc.command, "llama-cli");
    assert_eq!(pc.timeout_secs, 120);
    assert!(pc.args.contains(&"{prompt}".to_string()));
}

#[tokio::test]
async fn empty_config_loads_none() {
    let dir = std::env::temp_dir().join(format!("pai-v3c-none-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    assert!(InferenceFileConfig::load(&dir).unwrap().is_none());
}

#[tokio::test]
async fn provider_declares_text_only_capability() {
    let p = ProcessInferenceProvider::detect(cfg(vec!["/c", "echo", "hi"])).unwrap();
    let _ = &p as &dyn InferenceProvider; // object-safe
    assert_eq!(p.id(), "process");
    assert!(p
        .available_models()
        .await
        .unwrap()
        .contains(&"test-model".to_string()));
    let _unused: Arc<dyn InferenceProvider> =
        Arc::new(ProcessInferenceProvider::detect(cfg(vec!["/c", "echo", "x"])).unwrap());
}
