//! In-process bridge — `pai serve --bridge` hosts a [`PaiRuntime`] and
//! answers the web build's ops over HTTP instead of `dart:ffi`.
//! [`bridge_dispatch`] mirrors `PaiClient`'s op → extern-fn mapping
//! exactly: one optional string arg per op (raw or JSON), JSON out.

use crate::*;
use std::ffi::{c_char, CStr, CString};

/// Ops that only touch lock-guarded state (`pending`, `cancel`, the event
/// queue). The bridge may run these while a `send` holds the dispatch
/// lock — every other op serializes on it.
pub fn bridge_op_concurrent(op: &str) -> bool {
    matches!(op, "approve" | "cancel" | "pollEvents")
}

fn c_arg(s: &str) -> Result<CString> {
    CString::new(s).map_err(|_| Error::InvalidInput("arg contains NUL".into()))
}

unsafe fn take_json(p: *mut c_char) -> String {
    if p.is_null() {
        return "{\"error\":\"null result\"}".into();
    }
    let s = CStr::from_ptr(p).to_string_lossy().into_owned();
    pai_free_string(p);
    s
}

fn arg_json(arg: Option<&str>) -> serde_json::Value {
    arg.and_then(|s| serde_json::from_str(s).ok())
        .unwrap_or(serde_json::Value::Null)
}

fn json_arg<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    v.get(key)
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::InvalidInput(format!("missing '{key}'")))
}

/// Initialize a runtime for bridging — identical to `pai_init` (JSON
/// `InitConfig` in) but keeps the `unsafe` at the FFI boundary.
/// # Safety
/// Returned handle must be freed with `pai_free`.
pub unsafe fn bridge_init(config_json: &str) -> Result<*mut PaiRuntime> {
    let c = c_arg(config_json)?;
    let h = pai_init(c.as_ptr());
    if h.is_null() {
        Err(Error::Other("pai_init failed".into()))
    } else {
        Ok(h)
    }
}

/// Push every emitted event JSON onto `sink` — the web bridge drains it
/// via the `pollEvents` op for live run progress + approval requests.
/// # Safety
/// `handle` must come from `bridge_init`; `sink` is intentionally leaked
/// for the runtime's lifetime (bridge state outlives it).
pub unsafe fn bridge_install_event_sink(
    handle: *mut PaiRuntime,
    sink: Arc<Mutex<Vec<serde_json::Value>>>,
) {
    extern "C" fn push(evt: *const c_char, user: *mut std::ffi::c_void) {
        if evt.is_null() || user.is_null() {
            return;
        }
        let sink = unsafe { &*(user as *const Mutex<Vec<serde_json::Value>>) };
        if let Ok(s) = unsafe { CStr::from_ptr(evt) }.to_str() {
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
                sink.lock().unwrap_or_else(|e| e.into_inner()).push(v);
            }
        }
    }
    let user = Arc::into_raw(sink) as *mut std::ffi::c_void;
    pai_set_event_callback(handle, Some(push), user);
}

/// Dispatch one bridge op. `op` is the Dart `_Op` name; `arg` is its
/// single string payload (raw or JSON-encoded). Returns the JSON text
/// the corresponding `pai_*` function produced.
/// # Safety
/// `handle` must come from `bridge_init`. Ops not marked concurrent by
/// [`bridge_op_concurrent`] must be serialized by the caller.
pub unsafe fn bridge_dispatch(
    handle: *mut PaiRuntime,
    op: &str,
    arg: Option<&str>,
) -> Result<String> {
    macro_rules! str_arg {
        () => {
            arg.ok_or_else(|| Error::InvalidInput(format!("{op} needs an arg")))?
        };
    }
    // Optional-arg ops bind the CString in the match arm so it outlives
    // the extern call — a helper returning a bare pointer would dangle.
    macro_rules! opt_call {
        ($f:expr) => {{
            let a = arg.map(c_arg).transpose()?;
            take_json($f(
                handle,
                a.as_ref().map_or(std::ptr::null(), |c| c.as_ptr()),
            ))
        }};
    }
    let out = match op {
        "send" => take_json(pai_send(handle, c_arg(str_arg!())?.as_ptr())),
        "resume" => take_json(pai_resume(handle, c_arg(str_arg!())?.as_ptr())),
        "approve" => {
            let v = arg_json(arg);
            let id = c_arg(json_arg(&v, "id")?)?;
            let granted = v["granted"].as_bool().unwrap_or(false);
            if pai_approve(handle, id.as_ptr(), granted as i32) == 1 {
                "true".into()
            } else {
                "false".into()
            }
        }
        "cancel" => {
            pai_cancel(handle);
            "null".into()
        }
        "memories" => take_json(pai_memories(handle)),
        "modelsList" => take_json(pai_models_list(handle)),
        "modelsScan" => take_json(pai_models_scan(handle)),
        "modelsServe" => {
            let v = arg_json(arg);
            let port = v["port"].as_i64().unwrap_or(0) as i32;
            take_json(pai_models_serve(
                handle,
                c_arg(json_arg(&v, "slug")?)?.as_ptr(),
                port,
            ))
        }
        "modelsCatalog" => take_json(pai_models_catalog(handle)),
        "modelsInstall" => take_json(pai_models_install(handle, c_arg(str_arg!())?.as_ptr())),
        "audit" => take_json(pai_audit(handle, 50)),
        "runs" => take_json(pai_runs(handle)),
        "conversations" => take_json(pai_conversations(handle)),
        "history" => take_json(pai_history(handle)),
        "policies" => take_json(pai_policies(handle)),
        "detect" => take_json(pai_detect()),
        "convNew" => {
            let scope = str_arg!();
            let cfg = serde_json::json!({"memory": scope}).to_string();
            take_json(pai_conversation_new(handle, c_arg(&cfg)?.as_ptr()))
        }
        "convSelect" => take_json(pai_conversation_select(handle, c_arg(str_arg!())?.as_ptr())),
        "convDelete" => take_json(pai_conversation_delete(handle, c_arg(str_arg!())?.as_ptr())),
        "convRename" => {
            let v = arg_json(arg);
            take_json(pai_conversation_rename(
                handle,
                c_arg(json_arg(&v, "id")?)?.as_ptr(),
                c_arg(json_arg(&v, "title")?)?.as_ptr(),
            ))
        }
        "convSetMemory" => {
            let v = arg_json(arg);
            take_json(pai_conversation_set_memory(
                handle,
                c_arg(json_arg(&v, "id")?)?.as_ptr(),
                c_arg(json_arg(&v, "mode")?)?.as_ptr(),
            ))
        }
        "forget" => take_json(pai_forget(handle, c_arg(str_arg!())?.as_ptr())),
        "setPolicy" => {
            let v = arg_json(arg);
            take_json(pai_set_policy(
                handle,
                c_arg(json_arg(&v, "p")?)?.as_ptr(),
                c_arg(json_arg(&v, "x")?)?.as_ptr(),
            ))
        }
        "docs" => take_json(pai_docs(handle)),
        "docsIngest" => take_json(pai_docs_ingest(handle, c_arg(str_arg!())?.as_ptr())),
        "docsSearch" => take_json(pai_docs_search(handle, c_arg(str_arg!())?.as_ptr())),
        "docsDelete" => take_json(pai_docs_delete(handle, c_arg(str_arg!())?.as_ptr())),
        "emailSearch" => opt_call!(pai_email_search),
        "emailRead" => take_json(pai_email_read(handle, c_arg(str_arg!())?.as_ptr())),
        "emailDraft" => take_json(pai_email_draft(handle, c_arg(str_arg!())?.as_ptr())),
        "emailSend" => take_json(pai_email_send(handle, c_arg(str_arg!())?.as_ptr())),
        "emailConfigure" => take_json(pai_email_configure(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabStatus" => take_json(pai_gitlab_status(handle)),
        "gitlabProjects" => opt_call!(pai_gitlab_projects),
        "gitlabIssues" => opt_call!(pai_gitlab_issues),
        "gitlabIssue" => take_json(pai_gitlab_issue(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabIssueCreate" => {
            take_json(pai_gitlab_issue_create(handle, c_arg(str_arg!())?.as_ptr()))
        }
        "gitlabComment" => take_json(pai_gitlab_comment(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabMrs" => opt_call!(pai_gitlab_mrs),
        "gitlabMr" => take_json(pai_gitlab_mr(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabMrCreate" => take_json(pai_gitlab_mr_create(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabMrMerge" => take_json(pai_gitlab_mr_merge(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabPipelines" => opt_call!(pai_gitlab_pipelines),
        "gitlabFile" => take_json(pai_gitlab_file(handle, c_arg(str_arg!())?.as_ptr())),
        "gitlabConfigure" => take_json(pai_gitlab_configure(handle, c_arg(str_arg!())?.as_ptr())),
        "notifyList" => take_json(pai_notify_list(handle, arg == Some("unread"))),
        "notifyMarkRead" => take_json(pai_notify_mark_read(handle, c_arg(str_arg!())?.as_ptr())),
        "status" => take_json(pai_status(handle)),
        "setProvider" => take_json(pai_set_provider(handle, c_arg(str_arg!())?.as_ptr())),
        "voiceStatus" => take_json(pai_voice_status(handle)),
        "voiceListen" => {
            let secs = str_arg!().parse::<u32>().unwrap_or(30);
            take_json(pai_voice_listen(handle, secs))
        }
        "voiceListenStream" => {
            let secs = str_arg!().parse::<u32>().unwrap_or(30);
            take_json(pai_voice_listen_stream(handle, secs))
        }
        "voiceTranscribe" => take_json(pai_voice_transcribe(handle, c_arg(str_arg!())?.as_ptr())),
        "voiceSay" => take_json(pai_voice_say(handle, c_arg(str_arg!())?.as_ptr())),
        "appsList" => take_json(pai_apps_list(handle)),
        "appsRun" => {
            let v = arg_json(arg);
            let args = v["args"].to_string();
            take_json(pai_apps_run(
                handle,
                c_arg(json_arg(&v, "id")?)?.as_ptr(),
                c_arg(&args)?.as_ptr(),
            ))
        }
        "appsMigrate" => {
            let v = arg_json(arg);
            take_json(pai_apps_migrate(
                handle,
                c_arg(json_arg(&v, "id")?)?.as_ptr(),
                c_arg(json_arg(&v, "to")?)?.as_ptr(),
            ))
        }
        "peersList" => take_json(pai_peers_list(handle)),
        "devicesPlacement" => take_json(pai_devices_placement(handle)),
        "appsShareGrant" => {
            let v = arg_json(arg);
            take_json(pai_share_grant(
                handle,
                c_arg(json_arg(&v, "id")?)?.as_ptr(),
                c_arg(json_arg(&v, "actions")?)?.as_ptr(),
                v["days"].as_i64().unwrap_or(30),
                c_arg(json_arg(&v, "for")?)?.as_ptr(),
            ))
        }
        "shareDelegate" => {
            let v = arg_json(arg);
            take_json(pai_share_delegate(
                handle,
                c_arg(json_arg(&v, "parent")?)?.as_ptr(),
                c_arg(json_arg(&v, "actions")?)?.as_ptr(),
                v["days"].as_i64().unwrap_or(30),
                c_arg(json_arg(&v, "for")?)?.as_ptr(),
            ))
        }
        "guestCall" => take_json(pai_guest_call(handle, c_arg(str_arg!())?.as_ptr())),
        "shareList" => take_json(pai_share_list(handle)),
        "shareRevoke" => take_json(pai_share_revoke(handle, c_arg(str_arg!())?.as_ptr())),
        "mediaList" => take_json(pai_media_list(handle)),
        "mediaGen" => take_json(pai_media_gen(handle, c_arg(str_arg!())?.as_ptr())),
        "mediaExport" => {
            let v = arg_json(arg);
            take_json(pai_media_export(
                handle,
                c_arg(json_arg(&v, "id")?)?.as_ptr(),
                c_arg(json_arg(&v, "dest")?)?.as_ptr(),
            ))
        }
        "syncNow" => {
            let s = arg.unwrap_or("{}");
            take_json(pai_sync_now(handle, c_arg(s)?.as_ptr()))
        }
        "syncStatus" => take_json(pai_sync_status(handle)),
        "pairOffer" => take_json(pai_pair_offer(handle, c_arg(str_arg!())?.as_ptr())),
        "pairAccept" => {
            let v = arg_json(arg);
            take_json(pai_pair_accept(
                handle,
                c_arg(json_arg(&v, "offer")?)?.as_ptr(),
                c_arg(json_arg(&v, "out")?)?.as_ptr(),
            ))
        }
        "pairComplete" => take_json(pai_pair_complete(handle, c_arg(str_arg!())?.as_ptr())),
        "pairFolder" => take_json(pai_pair_folder(handle)),
        "pairQr" => take_json(pai_pair_qr(handle, c_arg(str_arg!())?.as_ptr())),
        _ => return Err(Error::InvalidInput(format!("unknown bridge op '{op}'"))),
    };
    Ok(out)
}
