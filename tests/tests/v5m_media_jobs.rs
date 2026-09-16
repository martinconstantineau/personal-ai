//! V5m tests: broker-routed media jobs — `media-run` op executes on a
//! capable device, the requester/worker each record a `media_jobs` row,
//! and the result lands as a content-addressed blob on both sides.

use pai_broker::rpc::{BrokerClient, BrokerServer, OpHandler};
use pai_core::*;
use pai_identity::IdentityStore;
use pai_media::providers::MediaConfig;
use pai_media::{jobs, JobState, MediaJobKind};
use pai_storage::Store;
use pai_sync::{crypto, pair, FolderTransport};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("pai-v5m-{tag}-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Dev {
    dir: PathBuf,
    store: Arc<Store>,
    ids: IdentityStore,
    key_dir: PathBuf,
    device: Device,
}

fn dev(tag: &str) -> Dev {
    let dir = tmpdir(tag);
    let store = Arc::new(Store::open(&dir, None).unwrap());
    let ids = IdentityStore::new(store.clone());
    let key_dir = dir.join("keys");
    let user = ids.create_user("u").unwrap();
    let device = ids
        .register_device(
            user.id,
            tag,
            Platform::Linux,
            DeviceCapabilities::default(),
            &key_dir,
        )
        .unwrap();
    Dev {
        dir,
        store,
        ids,
        key_dir,
        device,
    }
}

fn pair_devices(offerer: &Dev, acceptor: &Dev) {
    let agree_a = crypto::agreement_key(offerer.device.id, &offerer.dir).unwrap();
    let agree_b = crypto::agreement_key(acceptor.device.id, &acceptor.dir).unwrap();
    let offer =
        pair::make_offer(&offerer.device, &agree_a, &offerer.ids, &offerer.key_dir).unwrap();
    let accept = pair::accept_offer(
        &acceptor.store,
        &offer,
        &acceptor.device,
        &agree_b,
        &acceptor.ids,
        &acceptor.key_dir,
        &acceptor.dir,
    )
    .unwrap();
    pair::complete_pairing(&offerer.store, &accept, &agree_a, &offerer.dir).unwrap();
}

/// HTTP stub: serves `status` + `body` to every request — `detect()`
/// probes with a GET before the real POST, so it must accept repeatedly.
fn stub_audio(status: &'static str, body: &'static [u8]) -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || loop {
        let Ok((mut s, _)) = l.accept() else { break };
        use std::io::{Read, Write};
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let mut len = None;
            loop {
                let n = s.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&chunk[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4) {
                    if len.is_none() {
                        let head = String::from_utf8_lossy(&buf[..p]);
                        len = Some(
                            head.lines()
                                .find_map(|l| {
                                    l.to_ascii_lowercase()
                                        .strip_prefix("content-length:")
                                        .and_then(|v| v.trim().parse::<usize>().ok())
                                })
                                .unwrap_or(0),
                        );
                    }
                    if buf.len() >= p + len.unwrap() {
                        break;
                    }
                }
            }
            let resp = format!(
                "HTTP/1.1 {status}\r\nContent-Type: audio/wav\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = s.write_all(resp.as_bytes());
            let _ = s.write_all(body);
        });
    });
    format!("http://127.0.0.1:{port}")
}

fn state_of(store: &Store, id: &str) -> String {
    jobs::get(store, TaskId(uuid::Uuid::parse_str(id).unwrap()))
        .unwrap()
        .unwrap()["state"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn queue_lifecycle() {
    let d = dev("q");
    let mut job = jobs::new_job(MediaJobKind::TextToAudio, "rain on glass");
    let params = serde_json::json!({"duration_seconds": 8}).to_string();

    jobs::record(&d.store, &job, Some(&params), Some(d.device.id), None).unwrap();
    let row = jobs::get(&d.store, job.id).unwrap().unwrap();
    assert_eq!(row["kind"], "text_to_audio");
    assert_eq!(row["state"], "queued");
    assert_eq!(row["prompt"], "rain on glass");
    assert_eq!(row["params"], serde_json::json!(params));
    assert_eq!(row["requester"], d.device.id.to_string());

    job.state = JobState::Running;
    job.placement_device = Some(d.device.id);
    jobs::record(&d.store, &job, Some(&params), Some(d.device.id), None).unwrap();
    assert_eq!(state_of(&d.store, &job.id.to_string()), "running");

    job.state = JobState::Done;
    job.result_blob = Some("deadbeef".into());
    jobs::record(&d.store, &job, Some(&params), Some(d.device.id), None).unwrap();
    let row = jobs::get(&d.store, job.id).unwrap().unwrap();
    assert_eq!(row["state"], "done");
    assert_eq!(row["result_blob"], "deadbeef");
    assert_eq!(jobs::list(&d.store, 10).unwrap().len(), 1);
}

#[tokio::test]
async fn media_run_op_end_to_end() {
    let w = dev("worker");
    let url = stub_audio("200 OK", b"RIFF-worked-bytes");
    MediaConfig {
        audio_gen_url: Some(url),
        ..MediaConfig::default()
    }
    .save(&w.dir)
    .unwrap();

    let payload = serde_json::json!({
        "prompt": "tape hiss and vinyl crackle",
        "duration_seconds": 12,
    })
    .to_string()
    .into_bytes();
    let resp = jobs::media_run_op(&w.dir, &w.store, w.device.id, &payload)
        .await
        .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&resp).unwrap();
    use base64::Engine as _;
    let audio = base64::engine::general_purpose::STANDARD
        .decode(v["audio_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(audio, b"RIFF-worked-bytes");
    assert_eq!(v["mime"], "audio/wav");
    assert_eq!(v["bytes"], 17);

    // Blob + job row landed on the worker.
    let blob = v["blob"].as_str().unwrap();
    assert_eq!(w.store.get_blob(blob).unwrap(), audio);
    let row = jobs::get(
        &w.store,
        TaskId(uuid::Uuid::parse_str(v["job_id"].as_str().unwrap()).unwrap()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(row["state"], "done");
    assert_eq!(row["worker"], w.device.id.to_string());
    assert_eq!(row["result_blob"], blob);
}

#[tokio::test]
async fn media_run_op_failure_records_job() {
    let w = dev("fail");
    let url = stub_audio("500 Internal Server Error", b"");
    MediaConfig {
        audio_gen_url: Some(url),
        ..MediaConfig::default()
    }
    .save(&w.dir)
    .unwrap();
    let payload = serde_json::json!({"prompt": "x"}).to_string().into_bytes();
    let e = jobs::media_run_op(&w.dir, &w.store, w.device.id, &payload)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("500"), "{e}");
    let rows = jobs::list(&w.store, 10).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "failed");
    assert!(rows[0]["error"].as_str().unwrap().contains("500"));
}

#[tokio::test]
async fn media_run_op_no_provider() {
    let w = dev("noprov");
    MediaConfig {
        audio_gen_url: Some("http://127.0.0.1:1".into()), // never listening
        ..MediaConfig::default()
    }
    .save(&w.dir)
    .unwrap();
    let payload = serde_json::json!({"prompt": "x"}).to_string().into_bytes();
    let e = jobs::media_run_op(&w.dir, &w.store, w.device.id, &payload)
        .await
        .unwrap_err();
    assert!(e.to_string().contains("no audio-gen server"), "{e}");
    // No provider → no job row at all (nothing to claim).
    assert!(jobs::list(&w.store, 10).unwrap().is_empty());
}

// --- real broker round-trip ----------------------------------------------

struct MediaHandler {
    dir: PathBuf,
    store: Arc<Store>,
    device: DeviceId,
}

#[async_trait::async_trait]
impl OpHandler for MediaHandler {
    async fn handle(&self, op: &str, payload: &[u8]) -> Result<Vec<u8>> {
        assert_eq!(op, "media-run");
        jobs::media_run_op(&self.dir, &self.store, self.device, payload).await
    }
}

#[tokio::test]
async fn remote_media_run_roundtrip() {
    let a = dev("caller");
    let b = dev("runner");
    pair_devices(&a, &b);

    // B's provider is a stub server — B advertises media-run via BrokerOps
    // in production; the op handler is the same function either way.
    let url = stub_audio("200 OK", b"RIFF-remote-audio");
    MediaConfig {
        audio_gen_url: Some(url),
        ..MediaConfig::default()
    }
    .save(&b.dir)
    .unwrap();

    let shared = tmpdir("xport");
    let transport = FolderTransport::new(shared).unwrap();
    let vault = crypto::vault_key(&a.dir).unwrap().unwrap();
    let handler = MediaHandler {
        dir: b.dir.clone(),
        store: b.store.clone(),
        device: b.device.id,
    };
    let mut server = BrokerServer::new(&transport, &vault, b.device.id, &handler);
    let client = BrokerClient::new(&transport, &vault, a.device.id);

    // Requester side mirrors `pai audio gen --device <b>` bookkeeping.
    let mut job = jobs::new_job(MediaJobKind::TextToAudio, "distant fog horn");
    let params = serde_json::json!({"duration_seconds": 6}).to_string();
    jobs::record(&a.store, &job, Some(&params), Some(a.device.id), None).unwrap();
    job.state = JobState::Running;
    job.placement_device = Some(b.device.id);
    jobs::record(&a.store, &job, Some(&params), Some(a.device.id), None).unwrap();

    let payload = serde_json::json!({"prompt": job.prompt, "duration_seconds": 6})
        .to_string()
        .into_bytes();
    let call = client.call(b.device.id, "media-run", &payload, Duration::from_secs(15));
    let serve = async {
        loop {
            if server.serve_once().await.unwrap() > 0 {
                break;
            }
        }
    };
    let (resp, _) = tokio::join!(call, serve);
    let v: serde_json::Value = serde_json::from_slice(&resp.unwrap()).unwrap();
    use base64::Engine as _;
    let audio = base64::engine::general_purpose::STANDARD
        .decode(v["audio_b64"].as_str().unwrap())
        .unwrap();
    assert_eq!(audio, b"RIFF-remote-audio");

    // Requester completes its intent row; worker has its execution row.
    job.state = JobState::Done;
    job.result_blob = Some(a.store.put_blob(&audio).unwrap());
    jobs::record(&a.store, &job, Some(&params), Some(a.device.id), None).unwrap();
    assert_eq!(state_of(&a.store, &job.id.to_string()), "done");
    assert_eq!(state_of(&b.store, v["job_id"].as_str().unwrap()), "done");
    assert_eq!(
        jobs::list(&b.store, 10).unwrap()[0]["worker"],
        b.device.id.to_string()
    );
}
