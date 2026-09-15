//! Sandboxed WASM execution for installed apps.
//!
//! Runs `runtime = "wasm"` packages under wasmi with WASI preview1.
//! Deny-by-default: no env, no network (sockets are never preopened),
//! and filesystem access limited to the app's own `files/` and `data/`
//! directories via preopens. Execution is bounded by fuel and a
//! memory limiter.

use crate::{AppError, AppPackage, AppResult, AppRuntime, NetworkSpec, StorageKind};
use std::path::{Path, PathBuf};
use wasmi::{Config, Engine, Linker, Module, Store, StoreLimits, StoreLimitsBuilder};
use wasmi_wasi::sync::{ambient_authority, Dir, WasiCtxBuilder};
use wasmi_wasi::wasi_common::pipe::{ReadPipe, WritePipe};
use wasmi_wasi::WasiCtx;

/// Bounds on a single app run.
#[derive(Debug, Clone, Copy)]
pub struct RunLimits {
    /// Fuel budget — roughly one unit per wasm instruction.
    pub fuel: u64,
    /// Maximum guest linear-memory size in bytes.
    pub memory_bytes: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            fuel: 1_000_000_000,
            memory_bytes: 256 * 1024 * 1024,
        }
    }
}

/// Result of a sandboxed run.
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `Some(code)` when the app called `proc_exit`; `None` when the
    /// entrypoint returned normally.
    pub exit_code: Option<u32>,
    pub fuel_consumed: u64,
}

struct Host {
    wasi: WasiCtx,
    limits: StoreLimits,
    /// `network = "outbound"` — `pai_fetch` is gated on this.
    outbound: bool,
    /// Manifest's `allowed_hosts` — the fetch scope.
    allowed_hosts: Vec<String>,
    /// `app_dir/logs/fetch.log` — append-only audit of every fetch.
    fetch_log: PathBuf,
}

impl AppPackage {
    /// Run the package's entrypoint in a WASI sandbox rooted at
    /// `app_dir` (the installed package directory). Only wasm packages
    /// are runnable — native is never executed. `envs` are the only
    /// environment the guest sees (deny-by-default stays intact) — the
    /// auth layer injects `PAI_OAUTH_*` access tokens through it.
    pub fn run(
        &self,
        app_dir: &Path,
        args: &[String],
        limits: RunLimits,
        envs: &[(String, String)],
        input: &[u8],
    ) -> AppResult<RunOutput> {
        if self.manifest.app.runtime != AppRuntime::Wasm {
            return Err(AppError::Layout(
                "native runtime is not executable — wasm only".into(),
            ));
        }
        let wasm_path = app_dir.join(&self.manifest.app.entrypoint);
        let wasm = std::fs::read(&wasm_path)
            .map_err(|e| AppError::Layout(format!("entrypoint {:?}: {e}", wasm_path)))?;

        // --- WASI context: deny-by-default, then grant what the manifest
        // asked for — nothing else is reachable. ---
        let stdout = WritePipe::new_in_memory();
        let stderr = WritePipe::new_in_memory();
        let out_pipe = stdout.clone();
        let err_pipe = stderr.clone();

        let mut ctx = WasiCtxBuilder::new();
        ctx.stdin(Box::new(ReadPipe::from(input.to_vec())))
            .stdout(Box::new(stdout))
            .stderr(Box::new(stderr))
            .arg(&self.manifest.app_id())
            .map_err(|e| AppError::Layout(format!("argv0: {e}")))?;
        for a in args {
            ctx.arg(a)
                .map_err(|e| AppError::Layout(format!("arg: {e}")))?;
        }
        // No inherited env/args/stdio — only caller-supplied pairs
        // (resolved oauth tokens). `network` is a no-op for preview1:
        // sockets only exist if explicitly preopened, which we never do.
        ctx.envs(envs)
            .map_err(|e| AppError::Layout(format!("envs: {e}")))?;

        // `files` permission: each declared package-relative dir is
        // preopened at the same guest path, rooted inside app_dir.
        for rel in &self.manifest.permissions.files {
            let host = app_dir.join(rel);
            if !host.is_dir() {
                return Err(AppError::Layout(format!(
                    "permissioned dir {rel:?} missing in package"
                )));
            }
            let dir = Dir::open_ambient_dir(&host, ambient_authority())
                .map_err(|e| AppError::Layout(format!("preopen {rel:?}: {e}")))?;
            ctx.preopened_dir(dir, rel)
                .map_err(|e| AppError::Layout(format!("preopen {rel:?}: {e}")))?;
        }
        // Storage permission: `data/` exists for sqlite/kv/files apps.
        if self.manifest.storage.r#type != StorageKind::None {
            let data = app_dir.join("data");
            std::fs::create_dir_all(&data)?;
            let dir = Dir::open_ambient_dir(&data, ambient_authority())
                .map_err(|e| AppError::Layout(format!("preopen data: {e}")))?;
            ctx.preopened_dir(dir, "data")
                .map_err(|e| AppError::Layout(format!("preopen data: {e}")))?;
        }

        // --- Engine: fuel metering + memory limiter bound the run. ---
        let mut config = Config::default();
        config.consume_fuel(true);
        let engine = Engine::new(&config);
        let host = Host {
            wasi: ctx.build(),
            limits: StoreLimitsBuilder::new()
                .memory_size(limits.memory_bytes)
                .build(),
            outbound: matches!(self.manifest.permissions.network, NetworkSpec::Outbound),
            allowed_hosts: self.manifest.permissions.allowed_hosts.clone(),
            fetch_log: app_dir.join("logs").join("fetch.log"),
        };
        let mut store = Store::new(&engine, host);
        store
            .set_fuel(limits.fuel)
            .map_err(|e| AppError::Layout(format!("fuel metering unavailable: {e}")))?;
        store.limiter(|h| &mut h.limits);

        let module = Module::new(&engine, &wasm[..])
            .map_err(|e| AppError::Layout(format!("bad wasm module: {e}")))?;
        let mut linker = <Linker<Host>>::new(&engine);
        wasmi_wasi::sync::add_to_linker(&mut linker, |h: &mut Host| &mut h.wasi)
            .map_err(|e| AppError::Layout(format!("wasi linker: {e}")))?;
        // `env::pai_fetch(req_ptr, req_len, resp_ptr, resp_cap)` — the
        // app's only network: a host-mediated JSON request/response.
        // req JSON: {"method","url","headers":{},"body_b64"}; resp JSON:
        // {"status","headers":{},"body_b64"}. Return: >=0 bytes written;
        // -1..-9 fixed errors; <=-10 means "needs -ret resp bytes".
        linker
            .func_wrap("env", "pai_fetch", host_fetch)
            .map_err(|e| AppError::Layout(format!("pai_fetch linker: {e}")))?;
        let instance = linker
            .instantiate_and_start(&mut store, &module)
            .map_err(|e| AppError::Layout(format!("instantiate: {e}")))?;

        // Entry resolution: WASI `_start`, else a plain `main`/`run` export.
        let func = ["_start", "main", "run"]
            .iter()
            .find_map(|n| instance.get_func(&store, n))
            .ok_or_else(|| AppError::Layout("no _start/main/run export in wasm module".into()))?;
        let fuel_before = store.get_fuel().unwrap_or(0);
        let result = func.call(&mut store, &[], &mut []);

        let fuel_consumed = fuel_before.saturating_sub(store.get_fuel().unwrap_or(0));
        let mut exit_code = None;
        if let Err(e) = result {
            // proc_exit(N) surfaces as an i32-exit status on the wasmi error.
            match e.i32_exit_status() {
                Some(code) => exit_code = Some(code as u32),
                None => return Err(AppError::Layout(format!("run trapped: {e}"))),
            }
        }
        // Extract captured pipes back out of the store.
        drop(store);
        let stdout = out_pipe
            .try_into_inner()
            .map(|c| c.into_inner())
            .unwrap_or_default();
        let stderr = err_pipe
            .try_into_inner()
            .map(|c| c.into_inner())
            .unwrap_or_default();
        Ok(RunOutput {
            stdout,
            stderr,
            exit_code,
            fuel_consumed,
        })
    }
}

/// Host-side path for an installed app dir inside the registry.
pub fn installed_dir(data_dir: &Path, app_id: &str) -> PathBuf {
    data_dir.join("apps").join(app_id)
}

/// Broker op body for `app-run`: look up `app_id` in the registry, run
/// it in the sandbox, return the result as a JSON object
/// (`{"stdout_b64","stderr_b64","exit_code","fuel"}`). Lives in the
/// crate (not the CLI) so it's testable and the binary's handler stays
/// a thin guard + delegation.
/// The logging run itself — `envs` are the guest's whole environment.
pub(crate) fn run_logged_inner(
    data_dir: &Path,
    app_id: &str,
    args: &[String],
    envs: &[(String, String)],
    input: &[u8],
) -> AppResult<RunOutput> {
    let reg = crate::AppRegistry::new(data_dir);
    let pkg = reg
        .get(app_id)?
        .ok_or_else(|| AppError::Layout(format!("app {app_id} not installed")))?;
    let dir = installed_dir(data_dir, app_id);
    let entry = |out: Result<&RunOutput, &AppError>| crate::logs::RunLog {
        at: pai_core::now().to_rfc3339(),
        app_id: app_id.into(),
        args: args.to_vec(),
        exit_code: out.ok().and_then(|o| o.exit_code),
        fuel: out.map(|o| o.fuel_consumed).unwrap_or(0),
        trap: out.err().map(|e| e.to_string()),
        stdout: out
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default(),
        stderr: out
            .map(|o| String::from_utf8_lossy(&o.stderr).into_owned())
            .unwrap_or_default(),
    };
    match pkg.run(&dir, args, RunLimits::default(), envs, input) {
        Ok(out) => {
            crate::logs::record(data_dir, app_id, &entry(Ok(&out)));
            Ok(out)
        }
        // A trapped run is exactly what "why is my app broken" needs —
        // record the error text, then propagate.
        Err(e) => {
            crate::logs::record(data_dir, app_id, &entry(Err(&e)));
            Err(e)
        }
    }
}

/// Run an installed app and record the run log — shared by the local
/// CLI path and the broker `app-run` op so every run lands in
/// `apps/<id>/logs/` regardless of caller. Resolves `PAI_OAUTH_*`
/// access tokens for configured providers before the run.
pub async fn run_logged(data_dir: &Path, app_id: &str, args: &[String]) -> AppResult<RunOutput> {
    let envs = crate::auth::resolve_envs(data_dir, app_id).await;
    run_logged_inner(data_dir, app_id, args, &envs, &[])
}

fn encode_run(out: &RunOutput) -> Vec<u8> {
    use base64::Engine;
    serde_json::json!({
        "stdout_b64": base64::engine::general_purpose::STANDARD.encode(&out.stdout),
        "stderr_b64": base64::engine::general_purpose::STANDARD.encode(&out.stderr),
        "exit_code": out.exit_code,
        "fuel": out.fuel_consumed,
    })
    .to_string()
    .into_bytes()
}

pub async fn app_run_op(data_dir: &Path, app_id: &str, args: &[String]) -> AppResult<Vec<u8>> {
    let out = run_logged(data_dir, app_id, args).await?;
    Ok(encode_run(&out))
}

/// `greq/` guest runs: deliberately **no** OAuth injection — the
/// request comes from an outside capability holder, so the owner's
/// access tokens must not ride along. Everything else (sandbox,
/// logging) is identical.
pub fn app_run_op_guest(data_dir: &Path, app_id: &str, args: &[String]) -> AppResult<Vec<u8>> {
    let out = run_logged_inner(data_dir, app_id, args, &[], &[])?;
    Ok(encode_run(&out))
}

/// `app-serve` op (stable app URLs, V5j): `args[0]` is a JSON
/// [`crate::serve::ServeRequest`]. Runs the app CGI-style — request
/// metadata arrives as env vars, the body on stdin — and returns the
/// same `encode_run` envelope; the caller parses stdout as a CGI
/// response. No OAuth envs: the HTTP caller is not the owner.
pub fn app_serve_op(data_dir: &Path, app_id: &str, args: &[String]) -> AppResult<Vec<u8>> {
    let req_json = args
        .first()
        .ok_or_else(|| AppError::Layout("app-serve needs a ServeRequest JSON arg".into()))?;
    let req: crate::serve::ServeRequest = serde_json::from_str(req_json)
        .map_err(|e| AppError::Layout(format!("app-serve request: {e}")))?;
    let out = crate::serve::run_request(data_dir, app_id, &req)?;
    Ok(encode_run(&out))
}

/// Guest/local file ops jail a request path under the app's dir.
/// `tops` names the allowed first components (`files`, `data`);
/// `..`, absolute prefixes, and anything else are refused. The app
/// must be installed — the jail anchors at its registry dir.
fn jailed_path(data_dir: &Path, app_id: &str, rel: &str, tops: &[&str]) -> AppResult<PathBuf> {
    let reg = crate::AppRegistry::new(data_dir);
    reg.get(app_id)?
        .ok_or_else(|| AppError::Layout(format!("app {app_id} not installed")))?;
    let rel_path = Path::new(rel);
    if rel_path.is_absolute() {
        return Err(AppError::Layout(format!("path {rel} must be relative")));
    }
    let mut parts = rel_path.components();
    let top = match parts.next() {
        Some(std::path::Component::Normal(c)) => c,
        _ => {
            return Err(AppError::Layout(format!(
                "path {rel} must start with files/ or data/"
            )))
        }
    };
    if !tops.iter().any(|t| top == std::ffi::OsStr::new(t)) {
        return Err(AppError::Layout(format!(
            "path {rel} outside {}",
            tops.join("/ or ")
        )));
    }
    for c in parts {
        if !matches!(c, std::path::Component::Normal(_)) {
            return Err(AppError::Layout(format!("path {rel} escapes the app dir")));
        }
    }
    Ok(installed_dir(data_dir, app_id).join(rel_path))
}

/// Max file size read/written through the guest/local ops — keeps a
/// single sync object bounded on the transport.
pub const APP_IO_MAX: u64 = 8 * 1024 * 1024;

/// Broker op body for `app-read`: `args = [relpath]` under `files/` or
/// `data/` — returns `{"path","data_b64","bytes"}`.
pub fn app_read_op(data_dir: &Path, app_id: &str, args: &[String]) -> AppResult<Vec<u8>> {
    let rel = args
        .first()
        .ok_or_else(|| AppError::Layout("app-read needs a path argument".into()))?;
    let path = jailed_path(data_dir, app_id, rel, &["files", "data"])?;
    let meta = path
        .metadata()
        .map_err(|_| AppError::Layout(format!("{rel}: not found")))?;
    if !meta.is_file() {
        return Err(AppError::Layout(format!("{rel}: not a file")));
    }
    if meta.len() > APP_IO_MAX {
        return Err(AppError::Layout(format!("{rel}: exceeds 8 MiB cap")));
    }
    let data = std::fs::read(&path)?;
    use base64::Engine;
    Ok(serde_json::json!({
        "path": rel,
        "bytes": data.len(),
        "data_b64": base64::engine::general_purpose::STANDARD.encode(&data),
    })
    .to_string()
    .into_bytes())
}

/// Broker op body for `app-write`: `args = [relpath, data_b64]` —
/// writes under `data/` only (package files are signed content, not
/// guest-writable). Returns `{"path","bytes"}`.
pub fn app_write_op(data_dir: &Path, app_id: &str, args: &[String]) -> AppResult<Vec<u8>> {
    let (rel, data_b64) = match (args.first(), args.get(1)) {
        (Some(r), Some(d)) => (r, d),
        _ => return Err(AppError::Layout("app-write needs path + data_b64".into())),
    };
    let path = jailed_path(data_dir, app_id, rel, &["data"])?;
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD
        .decode(data_b64)
        .map_err(|e| AppError::Layout(format!("data_b64: {e}")))?;
    if data.len() as u64 > APP_IO_MAX {
        return Err(AppError::Layout(format!("{rel}: exceeds 8 MiB cap")));
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, &data)?;
    Ok(serde_json::json!({"path": rel, "bytes": data.len()})
        .to_string()
        .into_bytes())
}

// --- pai_fetch: host-mediated HTTP for sandboxed apps ----------------
//
// The app's only network: a host function it imports as
// `env::pai_fetch`. WASI preview1 has no sockets, so this is how an app
// uses its injected OAuth tokens — the host performs the request, the
// manifest's `[permissions] network = "outbound"` + `allowed_hosts`
// scope it. Redirects are never followed automatically (a 3xx could
// hop outside the allowlist); the app re-requests the Location itself,
// which is host-checked again. Every call appends to `logs/fetch.log`.

/// Error codes returned by `pai_fetch` (negative, -1..=-9).
const FETCH_ERR_DENIED: i32 = -2;
const FETCH_ERR_TRANSPORT: i32 = -3;
const FETCH_ERR_BAD_REQ: i32 = -4;
const FETCH_ERR_NO_MEM: i32 = -5;

#[derive(serde::Deserialize)]
struct FetchReq {
    #[serde(default = "default_get")]
    method: String,
    url: String,
    #[serde(default)]
    headers: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    body_b64: String,
}

fn default_get() -> String {
    "GET".into()
}

/// `*.example.com` matches `a.example.com` and `example.com`; a bare
/// pattern matches exactly. Case-insensitive.
pub fn host_match(pattern: &str, host: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix("*.") {
        host.eq_ignore_ascii_case(suffix)
            || host
                .to_ascii_lowercase()
                .ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
    } else {
        host.eq_ignore_ascii_case(pattern)
    }
}

/// Loopback HTTP is allowed for dev/test; anything else must be https.
fn scheme_ok(url: &reqwest::Url, host: &str) -> bool {
    url.scheme() == "https"
        || (url.scheme() == "http"
            && (host == "localhost" || host == "::1" || host.starts_with("127.")))
}

fn log_fetch(log: &PathBuf, line: &str) {
    if let Some(parent) = log.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
    {
        let _ = writeln!(f, "{} {line}", pai_core::now().to_rfc3339());
    }
}

fn host_fetch(
    mut caller: wasmi::Caller<'_, Host>,
    req_ptr: i32,
    req_len: i32,
    resp_ptr: i32,
    resp_cap: i32,
) -> i32 {
    let Some(wasmi::Extern::Memory(mem)) = caller.get_export("memory") else {
        return FETCH_ERR_NO_MEM;
    };
    let (req_ptr, req_len) = (req_ptr as usize, req_len as usize);
    let (resp_ptr, resp_cap) = (resp_ptr as usize, resp_cap as usize);
    let Some(req_raw) = mem
        .data(&caller)
        .get(req_ptr..req_ptr.saturating_add(req_len))
        .map(<[u8]>::to_vec)
    else {
        return FETCH_ERR_BAD_REQ;
    };
    let Ok(freq) = serde_json::from_slice::<FetchReq>(&req_raw) else {
        return FETCH_ERR_BAD_REQ;
    };
    let Ok(url) = reqwest::Url::parse(&freq.url) else {
        return FETCH_ERR_BAD_REQ;
    };
    let host_str = url.host_str().unwrap_or_default().to_string();

    // Gate: manifest grants outbound AND the host is allowlisted AND
    // the scheme is safe. Denials are logged like successes.
    let allowed = caller.data().outbound
        && caller
            .data()
            .allowed_hosts
            .iter()
            .any(|h| host_match(h, &host_str))
        && scheme_ok(&url, &host_str);
    if !allowed {
        log_fetch(
            &caller.data().fetch_log,
            &format!("DENIED {} {}", freq.method, freq.url),
        );
        return FETCH_ERR_DENIED;
    }

    let method =
        reqwest::Method::from_bytes(freq.method.as_bytes()).unwrap_or(reqwest::Method::GET);
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
    {
        Ok(c) => c,
        Err(_) => return FETCH_ERR_TRANSPORT,
    };
    let mut b = client.request(method, url);
    for (k, v) in &freq.headers {
        if let (Ok(name), Ok(value)) = (
            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
            reqwest::header::HeaderValue::from_str(v),
        ) {
            b = b.header(name, value);
        }
    }
    if !freq.body_b64.is_empty() {
        use base64::Engine;
        match base64::engine::general_purpose::STANDARD.decode(&freq.body_b64) {
            Ok(body) if body.len() <= crate::serve::BODY_CAP => b = b.body(body),
            Ok(_) => return FETCH_ERR_BAD_REQ,
            Err(_) => return FETCH_ERR_BAD_REQ,
        }
    }
    let out = match b.send() {
        Ok(resp) => {
            let status = resp.status().as_u16();
            let headers: std::collections::BTreeMap<String, String> = resp
                .headers()
                .iter()
                .filter_map(|(k, v)| {
                    v.to_str()
                        .ok()
                        .map(|s| (k.as_str().to_string(), s.to_string()))
                })
                .collect();
            let body = resp.bytes().unwrap_or_default();
            use base64::Engine;
            let body_b64 = if body.len() <= crate::serve::BODY_CAP {
                base64::engine::general_purpose::STANDARD.encode(&body)
            } else {
                base64::engine::general_purpose::STANDARD.encode(&body[..crate::serve::BODY_CAP])
            };
            log_fetch(
                &caller.data().fetch_log,
                &format!("{} {} → {status}", freq.method, freq.url),
            );
            serde_json::json!({
                "status": status,
                "headers": headers,
                "body_b64": body_b64,
            })
        }
        Err(e) => {
            log_fetch(
                &caller.data().fetch_log,
                &format!("ERROR {} {} — {e}", freq.method, freq.url),
            );
            return FETCH_ERR_TRANSPORT;
        }
    };
    let out = serde_json::to_vec(&out).unwrap_or_default();
    if out.len() > resp_cap {
        // Size probe / undersized buffer: return -needed (always ≤ -10
        // since a response JSON is never shorter than that).
        return -(out.len() as i32);
    }
    match mem
        .data_mut(&mut caller)
        .get_mut(resp_ptr..resp_ptr.saturating_add(out.len()))
    {
        Some(dst) => {
            dst.copy_from_slice(&out);
            out.len() as i32
        }
        None => FETCH_ERR_NO_MEM,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AppManifest, AppPackage};

    /// Package dir with the given wasm (WAT text is accepted in tests).
    fn pkg_with(wat: &str) -> (PathBuf, AppPackage) {
        let root = std::env::temp_dir().join(format!("pai-apps-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("manifest.toml"),
            "[app]\nname=\"t\"\nversion=\"1\"\nruntime=\"wasm\"\n",
        )
        .unwrap();
        std::fs::write(root.join("app.wasm"), wat).unwrap();
        let pkg = AppPackage::load(&root).unwrap();
        (root, pkg)
    }

    #[test]
    fn runs_trivial_module() {
        let (dir, pkg) = pkg_with("(module (func (export \"run\")))");
        let out = pkg.run(&dir, &[], RunLimits::default(), &[], &[]).unwrap();
        assert!(out.exit_code.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn proc_exit_sets_exit_code() {
        let (dir, pkg) = pkg_with(
            r#"(module
                (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
                (memory (export "memory") 1)
                (func (export "_start") i32.const 7 call $exit))"#,
        );
        let out = pkg.run(&dir, &[], RunLimits::default(), &[], &[]).unwrap();
        assert_eq!(out.exit_code, Some(7));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fuel_limit_traps() {
        let (dir, pkg) = pkg_with(r#"(module (func (export "run") (loop $l (br $l))))"#);
        let limits = RunLimits {
            fuel: 10_000,
            ..RunLimits::default()
        };
        assert!(pkg.run(&dir, &[], limits, &[], &[]).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn native_runtime_not_runnable() {
        let root = std::env::temp_dir().join(format!("pai-apps-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("manifest.toml"),
            "[app]\nname=\"t\"\nversion=\"1\"\nruntime=\"native\"\nentrypoint=\"bin/x\"\n",
        )
        .unwrap();
        let pkg = AppPackage::load(&root).unwrap();
        assert!(pkg.run(&root, &[], RunLimits::default(), &[], &[]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// WAT that opens `hello.txt` under the first preopen (fd 3), writes
    /// 16 bytes, and exits 0. path_open args:
    /// (fd, dirflags, path_ptr, path_len, oflags, rights, rights_inh,
    ///  fdflags, result_ptr). rights 9286 = FD_READ|FD_SEEK|FD_WRITE|
    /// PATH_CREATE_FILE|PATH_OPEN.
    const WRITE_THROUGH_PREOPEN: &str = r#"(module
        (import "wasi_snapshot_preview1" "path_open"
          (func $path_open (param i32 i32 i32 i32 i32 i64 i64 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_write"
          (func $fd_write (param i32 i32 i32 i32) (result i32)))
        (import "wasi_snapshot_preview1" "fd_close"
          (func $fd_close (param i32) (result i32)))
        (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
        (memory (export "memory") 1)
        (data (i32.const 200) "hello.txt")
        (data (i32.const 220) "sandbox-write-ok")
        (func (export "_start")
          (local $errno i32)
          (local.set $errno
            (call $path_open (i32.const 3) (i32.const 0) (i32.const 200)
              (i32.const 9) (i32.const 1) (i64.const 9286) (i64.const 9286)
              (i32.const 0) (i32.const 100)))
          (if (i32.ne (local.get $errno) (i32.const 0))
            (then (call $exit (local.get $errno))))
          (i32.store (i32.const 300) (i32.const 220))
          (i32.store (i32.const 304) (i32.const 16))
          (local.set $errno
            (call $fd_write (i32.load (i32.const 100)) (i32.const 300)
              (i32.const 1) (i32.const 320)))
          (if (i32.ne (local.get $errno) (i32.const 0))
            (then (call $exit (local.get $errno))))
          (drop (call $fd_close (i32.load (i32.const 100))))
          (call $exit (i32.const 0))))"#;

    fn pkg_with_perms(wat: &str, manifest_extra: &str) -> (PathBuf, AppPackage) {
        let root = std::env::temp_dir().join(format!("pai-apps-run-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("manifest.toml"),
            format!("[app]\nname=\"t\"\nversion=\"1\"\nruntime=\"wasm\"\n{manifest_extra}"),
        )
        .unwrap();
        std::fs::write(root.join("app.wasm"), wat).unwrap();
        let pkg = AppPackage::load(&root).unwrap();
        (root, pkg)
    }

    #[test]
    fn writes_through_files_preopen() {
        let (dir, pkg) =
            pkg_with_perms(WRITE_THROUGH_PREOPEN, "[permissions]\nfiles = [\"files/\"]");
        std::fs::create_dir_all(dir.join("files")).unwrap();
        let out = pkg.run(&dir, &[], RunLimits::default(), &[], &[]).unwrap();
        assert_eq!(out.exit_code, Some(0));
        assert_eq!(
            std::fs::read(dir.join("files/hello.txt")).unwrap(),
            b"sandbox-write-ok"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn no_permissions_no_preopens() {
        // Same app, but the manifest grants nothing — fd 3 is not a
        // preopen, so path_open fails with EBADF (errno 8). storage must
        // be "none": the sqlite default would preopen data/ at fd 3.
        let (dir, pkg) = pkg_with_perms(WRITE_THROUGH_PREOPEN, "[storage]\ntype = \"none\"");
        let out = pkg.run(&dir, &[], RunLimits::default(), &[], &[]).unwrap();
        assert_eq!(out.exit_code, Some(8));
        assert!(!dir.join("hello.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn manifest_still_parses() {
        // Guard: run.rs touches manifest fields the tests above rely on.
        let m = AppManifest::parse("[app]\nname=\"t\"\nversion=\"1\"").unwrap();
        assert_eq!(m.app.runtime, AppRuntime::Wasm);
    }
}
