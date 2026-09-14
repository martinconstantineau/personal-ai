//! Sandboxed WASM execution for installed apps.
//!
//! Runs `runtime = "wasm"` packages under wasmi with WASI preview1.
//! Deny-by-default: no env, no network (sockets are never preopened),
//! and filesystem access limited to the app's own `files/` and `data/`
//! directories via preopens. Execution is bounded by fuel and a
//! memory limiter.

use crate::{AppError, AppPackage, AppResult, AppRuntime, StorageKind};
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
}

impl AppPackage {
    /// Run the package's entrypoint in a WASI sandbox rooted at
    /// `app_dir` (the installed package directory). Only wasm packages
    /// are runnable — native is never executed.
    pub fn run(&self, app_dir: &Path, args: &[String], limits: RunLimits) -> AppResult<RunOutput> {
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
        ctx.stdin(Box::new(ReadPipe::from("")))
            .stdout(Box::new(stdout))
            .stderr(Box::new(stderr))
            .arg(&self.manifest.app_id())
            .map_err(|e| AppError::Layout(format!("argv0: {e}")))?;
        for a in args {
            ctx.arg(a)
                .map_err(|e| AppError::Layout(format!("arg: {e}")))?;
        }
        // No inherited env/args/stdio. `network` is a no-op for preview1:
        // sockets only exist if explicitly preopened, which we never do.

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
        let out = pkg.run(&dir, &[], RunLimits::default()).unwrap();
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
        let out = pkg.run(&dir, &[], RunLimits::default()).unwrap();
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
        assert!(pkg.run(&dir, &[], limits).is_err());
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
        assert!(pkg.run(&root, &[], RunLimits::default()).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn manifest_still_parses() {
        // Guard: run.rs touches manifest fields the tests above rely on.
        let m = AppManifest::parse("[app]\nname=\"t\"\nversion=\"1\"").unwrap();
        assert_eq!(m.app.runtime, AppRuntime::Wasm);
    }
}
