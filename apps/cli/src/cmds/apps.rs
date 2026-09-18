//! `pai apps …` — package lifecycle (init/build/sign/verify), install
//! registry, remote run via broker or guest capability tokens, backups,
//! migration/rescue, OAuth auth config, and capability grants.

use crate::ctx::Base;
use crate::util::*;
use clap::Subcommand;
use pai_core::*;

#[derive(Subcommand)]
pub(crate) enum AppsCmd {
    /// Scaffold a new app source project (manifest.toml + Rust wasm
    /// skeleton) — then `pai apps build` it.
    Init {
        /// App name; the app id is slugified from it.
        name: String,
        /// Parent directory (default: current dir).
        #[arg(long)]
        dir: Option<String>,
        /// Static web bundle instead of wasm: index.html + app.js +
        /// manifest.webmanifest, `runtime = "web"`, `serve = true` —
        /// `pai serve` installs it as an offline-capable PWA.
        #[arg(long)]
        web: bool,
    },
    /// Build a source dir into a verifiable package: Rust crate →
    /// wasm32-wasip1, or an existing package dir → copy + validate.
    Build {
        /// Source directory (Cargo.toml and/or manifest.toml).
        dir: String,
        /// Output package dir (default: <dir>/pkg).
        #[arg(long)]
        out: Option<String>,
        /// Sign the built package with this device's key.
        #[arg(long)]
        sign: bool,
    },
    /// List installed apps.
    List,
    /// Emit hosts-file lines: `<ip> <app>.<user>.devices` for every
    /// serve-enabled app — append to /etc/hosts (or feed a Tailscale
    /// nameserver) so `http://app.user.devices` URLs resolve here.
    Names {
        /// IP the names should point at (default 127.0.0.1 — the local
        /// `pai serve` gateway). For a shared/LAN gateway pass its IP.
        #[arg(long)]
        ip: Option<String>,
    },
    /// Sign a package in place with this device's key (writes
    /// signature.bin over manifest + content digest).
    Sign {
        /// Package directory containing manifest.toml.
        path: String,
    },
    /// Verify a package's signature without installing it.
    Verify {
        /// Package directory containing manifest.toml.
        path: String,
    },
    /// Run an installed app's wasm entrypoint in the sandbox.
    Run {
        /// Installed app id (see `pai apps list`).
        id: String,
        /// Run on a paired device instead of locally — a device-id
        /// prefix, or `any` to route to a peer announcing the op.
        /// Installed apps sync to every paired device.
        #[arg(long)]
        on: Option<String>,
        /// Transport for --on: shared sync directory.
        #[arg(long)]
        dir: Option<String>,
        /// Transport for --on: relay URL.
        #[arg(long)]
        relay: Option<String>,
        /// Transport for --on: relay bearer token.
        #[arg(long)]
        token: Option<String>,
        /// Run as a guest: path to a capability token JSON file issued
        /// by `pai apps share` on the target device. The request is
        /// signed with this device's key when the grant is bound to it.
        #[arg(long)]
        cap: Option<String>,
        /// Arguments passed to the app.
        args: Vec<String>,
    },
    /// Remove an installed app.
    Remove {
        /// Installed app id.
        id: String,
    },
    /// Snapshot an installed app (package + live data) into a backup
    /// that ships to paired devices on the next `pai sync push`.
    Backup {
        /// Installed app id.
        id: String,
    },
    /// List known app backups — own snapshots and ones received from
    /// paired devices.
    Backups,
    /// Restore an app's package + data from a backup snapshot.
    Restore {
        /// Installed app id.
        id: String,
        /// Backup writer device-id prefix (default: newest backup).
        #[arg(long)]
        from: Option<String>,
    },
    /// Delete my backup for an app — ships a tombstone so paired
    /// devices drop their copy too.
    BackupDelete {
        /// Installed app id.
        id: String,
    },
    /// Move an app to a paired device: snapshot package + data into a
    /// migration-flagged backup, hand over active_device, deactivate
    /// local data. Ships on next `pai sync push`; the target restores
    /// inline on its next pull.
    Migrate {
        /// Installed app id.
        id: String,
        /// Destination device — a paired device-id prefix.
        #[arg(long)]
        to: String,
    },
    /// Rescue an app whose home device is dead/lost: claim
    /// active_device here and restore the newest backup's data.
    /// `--all --from <dev>` rescues every app that lived on that
    /// device. The claim ships on next `pai sync push`; if the old
    /// device returns, its pull parks its stale data.
    Rescue {
        /// Installed app id — or use --all --from.
        id: Option<String>,
        /// Rescue every app whose active_device is --from's device.
        #[arg(long)]
        all: bool,
        /// The dead device's id or prefix (required with --all).
        #[arg(long)]
        from: Option<String>,
    },
    /// Diagnose an installed app: install state, placement, storage,
    /// backups, share tokens, and recent audit outcomes.
    Status {
        /// Installed app id (see `pai apps list`).
        id: String,
    },
    /// Show an app's recent run logs (exit code, trap, stdout/stderr).
    Logs {
        /// Installed app id.
        id: String,
        /// How many entries to show (newest first, max 20).
        #[arg(short = 'n', long, default_value = "5")]
        limit: usize,
    },
    /// Configure an OAuth provider for an app — "add Google login".
    /// Writes the provider config (`auth.json`, synced via `app/`),
    /// runs the RFC 8628 device-authorization flow, and stores the
    /// refresh token in the OS keystore. At run time the app receives
    /// a fresh access token as `PAI_OAUTH_<PROVIDER>` — the refresh
    /// token never enters the sandbox.
    Auth {
        /// Installed app id.
        id: String,
        /// Provider preset: `google` | `microsoft` | `custom`.
        provider: Option<String>,
        /// OAuth public client_id (from your app registration).
        #[arg(long)]
        client_id: Option<String>,
        /// Scope to request — repeatable. Defaults per provider.
        #[arg(long)]
        scope: Vec<String>,
        /// Custom-IdP device-code endpoint (provider `custom`).
        #[arg(long)]
        device_url: Option<String>,
        /// Custom-IdP token endpoint (provider `custom`).
        #[arg(long)]
        token_url: Option<String>,
        /// Microsoft tenant (default `common`).
        #[arg(long)]
        tenant: Option<String>,
        /// Remove the named provider instead of adding one.
        #[arg(long)]
        remove: bool,
        /// List configured providers + local token state.
        #[arg(long)]
        status: bool,
    },
    /// Issue a capability token for an installed app — a signed,
    /// expiring grant a guest device uses with `pai apps run --cap`.
    Share {
        /// Installed app id to share.
        id: String,
        /// Action(s) granted: `exec`, `read`, `write`, `share` —
        /// repeat or comma-separate (`--action read,write`).
        #[arg(long, default_value = "exec", value_delimiter = ',')]
        action: Vec<String>,
        /// Token lifetime in days.
        #[arg(long, default_value_t = 30)]
        days: i64,
        /// Bind the grant to a paired device's id prefix — requests
        /// must then be signed by that device's key. Omit for a
        /// bearer token (anyone holding it may run the app).
        #[arg(long, name = "for")]
        for_device: Option<String>,
        /// Write the token JSON here instead of share/<app>/<id>.json.
        #[arg(long)]
        out: Option<String>,
    },
    /// Re-grant a narrower sub-token from a capability this device
    /// holds — the parent token must carry `share` and be bound to
    /// this device's key. The child embeds the parent chain.
    Delegate {
        /// Parent capability token JSON (from `pai apps share`).
        #[arg(long)]
        parent: String,
        /// Action(s) for the sub-token — must be a subset of the
        /// parent's (`--action read,exec`).
        #[arg(long, default_value = "exec", value_delimiter = ',')]
        action: Vec<String>,
        /// Sub-token lifetime in days — may not outlive the parent.
        #[arg(long)]
        days: Option<i64>,
        /// Bind the sub-token: a paired device-id prefix or a raw
        /// 64-hex Ed25519 pubkey. Omit for a bearer sub-token.
        #[arg(long, name = "for")]
        for_device: Option<String>,
        /// Write the sub-token JSON here (default: share/tokens/).
        #[arg(long)]
        out: Option<String>,
    },
    /// List capability grants this device has issued.
    Grants,
    /// Revoke a capability grant by token id — revoking a parent
    /// also kills every sub-token delegated from it.
    Revoke {
        /// Installed app id the grant belongs to.
        id: String,
        /// Token id (see `pai apps grants`).
        token: String,
    },
    /// Read a file under an installed app's `files/` or `data/` —
    /// locally, or on a host as a guest with --cap + --on.
    Read {
        /// Installed app id.
        id: String,
        /// Path relative to the app dir (e.g. data/state.txt).
        path: String,
        /// Guest capability token JSON (`share --action read`).
        #[arg(long)]
        cap: Option<String>,
        /// Host device id or paired prefix — required with --cap.
        #[arg(long)]
        on: Option<String>,
        /// Guest transport: shared sync directory.
        #[arg(long)]
        dir: Option<String>,
        /// Guest transport: relay URL.
        #[arg(long)]
        relay: Option<String>,
        /// Guest transport: relay bearer token.
        #[arg(long)]
        token: Option<String>,
        /// Write the bytes here instead of stdout.
        #[arg(long)]
        out: Option<String>,
    },
    /// Write bytes into an installed app's `data/` — locally, or on a
    /// host as a guest with --cap + --on (`share --action write`).
    Write {
        /// Installed app id.
        id: String,
        /// Path relative to the app dir; must start with data/.
        path: String,
        /// File whose bytes to write.
        #[arg(long)]
        file: Option<String>,
        /// Literal text to write.
        #[arg(long)]
        text: Option<String>,
        /// Guest capability token JSON (`share --action write`).
        #[arg(long)]
        cap: Option<String>,
        /// Host device id or paired prefix — required with --cap.
        #[arg(long)]
        on: Option<String>,
        /// Guest transport: shared sync directory.
        #[arg(long)]
        dir: Option<String>,
        /// Guest transport: relay URL.
        #[arg(long)]
        relay: Option<String>,
        /// Guest transport: relay bearer token.
        #[arg(long)]
        token: Option<String>,
    },
}

/// `pai deploy <path>` — verify a signed package and install it; the
/// `apps` row then ships the package to paired devices on sync.
pub(crate) async fn deploy(path: &str, upgrade: &bool, b: &Base) -> Result<()> {
    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let ids = b.ids();
    let user = b.user.clone();
    let device = b.device.clone();
    let pkg = load_package(path)?;
    let devices = ids.list_devices(user.id)?;
    let signer = pkg
        .verify_any(&ids, &devices)
        .map_err(|e| Error::InvalidInput(e.to_string()))?;
    let signer_dev = devices
        .iter()
        .find(|d| d.id == signer)
        .expect("verify_any returned a listed device");
    let dest = pai_apps::AppRegistry::new(&cfg.data_dir)
        .install(&pkg, &ids, signer_dev, *upgrade)
        .map_err(|e| Error::Storage(e.to_string()))?;
    // Registry row: `app/<id>` sync objects are built from this
    // table — deployed packages roam to every paired device.
    let now_s = pai_storage::ts(&now());
    store.with_conn(|c| {
        c.execute(
            "INSERT INTO apps(id, name, version, runtime, installed_at,
                        updated_at, deleted) VALUES(?1,?2,?3,?4,?5,?6,0)
                     ON CONFLICT(id) DO UPDATE SET name=excluded.name,
                        version=excluded.version, runtime=excluded.runtime,
                        updated_at=excluded.updated_at, deleted=0",
            rusqlite::params![
                pkg.manifest.app_id(),
                pkg.manifest.app.name,
                pkg.manifest.app.version,
                format!("{:?}", pkg.manifest.app.runtime).to_lowercase(),
                now_s,
                now_s,
            ],
        )?;
        Ok(())
    })?;
    record_audit(
        &store,
        device.id,
        AuditKind::AppDeployed,
        serde_json::json!({
            "app_id": pkg.manifest.app_id(),
            "version": pkg.manifest.app.version,
            "signer": signer.to_string(),
        }),
    )?;
    println!(
        "deployed {} {} ({:?}) -> {}",
        pkg.manifest.app_id(),
        pkg.manifest.app.version,
        pkg.manifest.app.runtime,
        dest.display()
    );
    println!("signed by device {:.8}", signer.to_string());
    println!("run it: `pai apps run {}`", pkg.manifest.app_id());
    Ok(())
}

pub(crate) async fn run(cmd: &AppsCmd, b: &Base) -> Result<()> {
    use pai_sync::crypto;
    let cfg = b.cfg.clone();
    let store = b.store.clone();
    let ids = b.ids();
    let key_dir = b.key_dir.clone();
    let user = b.user.clone();
    let device = b.device.clone();
    match cmd {
        AppsCmd::Init { name, dir, web } => {
            let base = match dir {
                Some(d) => std::path::PathBuf::from(d),
                None => std::env::current_dir().map_err(|e| Error::Other(e.to_string()))?,
            };
            let root = pai_apps::AppPackage::init(&base, name, *web)
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
            println!("created {}", root.display());
            println!("next:    pai apps build {}", root.display());
            if *web {
                println!(
                    "         (static web app — `pai serve` answers it as an installable PWA)"
                );
            }
        }
        AppsCmd::Build { dir, out, sign } => {
            let out_dir = out
                .clone()
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|| std::path::PathBuf::from(dir).join("pkg"));
            let pkg_dir = pai_apps::AppPackage::build(&std::path::PathBuf::from(dir), &out_dir)
                .map_err(|e| Error::InvalidInput(e.to_string()))?;
            println!("package: {}", pkg_dir.display());
            if *sign {
                let pkg = load_package(&pkg_dir.to_string_lossy())?;
                pkg.sign(&ids, &device, &key_dir)
                    .map_err(|e| Error::Other(e.to_string()))?;
                println!("signed by device {:.8}", device.id);
            }
            println!("deploy:  pai deploy {}", pkg_dir.display());
        }
        AppsCmd::Names { ip } => {
            // `app.user.devices` hosts-file lines — one per
            // serve-enabled app. Append to /etc/hosts (Linux/macOS),
            // C:\Windows\System32\drivers\etc\hosts, or the lines a
            // Tailscale/MagicDNS-style nameserver would serve.
            let ip = ip.as_deref().unwrap_or("127.0.0.1");
            let slug = name_slug(&user.display_name);
            let mut any = false;
            for (id, m) in pai_apps::AppRegistry::new(&cfg.data_dir)
                .list()
                .map_err(|e| Error::Storage(e.to_string()))?
            {
                if m.app.serve {
                    println!("{ip:<15}  {id}.{slug}.devices");
                    any = true;
                }
            }
            if !any {
                println!("(no serve-enabled apps — mark `serve = true` in manifest.toml)");
            } else {
                eprintln!(
                    "# append to your hosts file, then open http://<app>.{slug}.devices[:port]"
                );
            }
        }
        AppsCmd::List => {
            let apps = pai_apps::AppRegistry::new(&cfg.data_dir)
                .list()
                .map_err(|e| Error::Storage(e.to_string()))?;
            if apps.is_empty() {
                println!("(no apps installed - `pai deploy <dir>`)");
            }
            // Placement from the sync table — `apps` rows only
            // exist once the app has been synced or migrated.
            let placement: std::collections::HashMap<String, Option<String>> = store
                .with_conn(|c| {
                    let mut s = c.prepare("SELECT id, active_device FROM apps")?;
                    let rows = s.query_map([], |r| {
                        Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
                    })?;
                    rows.collect::<rusqlite::Result<std::collections::HashMap<_, _>>>()
                })
                .unwrap_or_default();
            for (id, m) in apps {
                let where_ = match placement.get(&id).and_then(|p| p.as_deref()) {
                    Some(d) if d == device.id.to_string() => " here".to_string(),
                    Some(d) => format!(" → {:.8}", d),
                    None => " everywhere".to_string(),
                };
                println!(
                    "  {:<28} {:<10} {:<6} {}{}",
                    id,
                    m.app.version,
                    format!("{:?}", m.app.runtime).to_lowercase(),
                    m.app.name,
                    where_
                );
            }
        }
        AppsCmd::Sign { path } => {
            let pkg = load_package(path)?;
            pkg.sign(&ids, &device, &key_dir)
                .map_err(|e| Error::Other(e.to_string()))?;
            println!(
                "signed {} with device {:.8}",
                pkg.manifest.app_id(),
                device.id
            );
        }
        AppsCmd::Verify { path } => {
            let pkg = load_package(path)?;
            let devices = ids.list_devices(user.id)?;
            match pkg.verify_any(&ids, &devices) {
                Ok(signer) => println!(
                    "{} {} - signature ok (device {:.8})",
                    pkg.manifest.app_id(),
                    pkg.manifest.app.version,
                    signer.to_string()
                ),
                Err(e) => return Err(Error::InvalidInput(e.to_string())),
            }
        }
        AppsCmd::Run {
            id,
            on,
            dir,
            relay,
            token,
            cap,
            args,
        } if on.is_some() => {
            let dev = on.as_deref().unwrap();
            let t = sync_transport(dir, relay, token)?;
            let (to, resp) = if let Some(cap_path) = cap {
                // Guest path: a capability token stands in for
                // vault membership — request goes out unsealed,
                // response seals to an ephemeral key in it.
                let cx = GuestCtx {
                    store: &store,
                    ids: &ids,
                    key_dir: &key_dir,
                    device: &device,
                };
                guest_call(&*t, &cx, cap_path, dev, id, "app-run", args).await?
            } else {
                let vault = crypto::vault_key(&cfg.data_dir)?.ok_or_else(|| {
                    Error::Sync("no vault key — pair a device first (pai pair)".into())
                })?;
                let client = pai_broker::rpc::BrokerClient::new(&*t, &vault, device.id)
                    .with_weights(place_weights(&store));
                let to = if dev == "any" {
                    client.find_peer("app-run").await?.ok_or_else(|| {
                        Error::NotFound(
                            "no paired device advertises app-run — `pai broker serve` running?"
                                .into(),
                        )
                    })?
                } else {
                    resolve_peer(&store, dev)?
                };
                let payload = serde_json::json!({"id": id, "args": args})
                    .to_string()
                    .into_bytes();
                let resp = client
                    .call(to, "app-run", &payload, std::time::Duration::from_secs(120))
                    .await?;
                (to, resp)
            };
            let v: serde_json::Value = serde_json::from_slice(&resp)
                .map_err(|e| Error::Other(format!("bad app-run response: {e}")))?;
            use base64::Engine as _;
            let b64 = base64::engine::general_purpose::STANDARD;
            for (key, err) in [("stdout_b64", false), ("stderr_b64", true)] {
                let bytes = v[key]
                    .as_str()
                    .and_then(|x| b64.decode(x).ok())
                    .unwrap_or_default();
                if err && !bytes.is_empty() {
                    eprint!("{}", String::from_utf8_lossy(&bytes));
                } else if !err {
                    print!("{}", String::from_utf8_lossy(&bytes));
                }
            }
            record_audit(
                &store,
                device.id,
                AuditKind::AppRun,
                serde_json::json!({
                    "app_id": id,
                    "on": to.to_string(),
                    "exit_code": v["exit_code"],
                }),
            )?;
            let short = &to.to_string()[..8.min(to.to_string().len())];
            match v["exit_code"].as_i64() {
                Some(c) => println!("(remote {short} — exit {c})"),
                None => println!("(remote {short})"),
            }
        }
        AppsCmd::Run { id, args, .. } => {
            if let Some(other) = pai_sync::backup::active_elsewhere(&store, device.id, id)? {
                return Err(Error::InvalidInput(format!(
                        "app {id} is active on {other} — run it there, or migrate it back with `pai apps migrate {id} --to <me>`"
                    )));
            }
            let out = pai_apps::run_logged(&cfg.data_dir, id, args)
                .await
                .map_err(|e| {
                    if e.to_string().contains("not installed") {
                        Error::NotFound(format!("app {id}"))
                    } else {
                        Error::Other(e.to_string())
                    }
                })?;
            print!("{}", String::from_utf8_lossy(&out.stdout));
            if !out.stderr.is_empty() {
                eprint!("{}", String::from_utf8_lossy(&out.stderr));
            }
            record_audit(
                &store,
                device.id,
                AuditKind::AppRun,
                serde_json::json!({
                    "app_id": id,
                    "exit_code": out.exit_code,
                    "fuel": out.fuel_consumed,
                }),
            )?;
            if let Some(code) = out.exit_code {
                println!("(exit {code}, fuel {})", out.fuel_consumed);
            }
        }
        AppsCmd::Remove { id } => {
            let reg = pai_apps::AppRegistry::new(&cfg.data_dir);
            if reg
                .remove(id)
                .map_err(|e| Error::InvalidInput(e.to_string()))?
            {
                // Tombstone the row — the next push ships `app/<id>`
                // as a deletion so peers remove it too.
                store.with_conn(|c| {
                    c.execute(
                        "UPDATE apps SET deleted=1, updated_at=?2 WHERE id=?1",
                        rusqlite::params![id, pai_storage::ts(&now())],
                    )?;
                    Ok(())
                })?;
                record_audit(
                    &store,
                    device.id,
                    AuditKind::AppRemoved,
                    serde_json::json!({"app_id": id}),
                )?;
                println!("removed {id}");
            } else {
                println!("no such app: {id}");
            }
        }
        AppsCmd::Backup { id } => {
            let path = pai_sync::backup::create(&store, &cfg.data_dir, device.id, id, None)?;
            record_audit(
                &store,
                device.id,
                AuditKind::AppBackedUp,
                serde_json::json!({"app_id": id}),
            )?;
            println!(
                "backup recorded at {} — `pai sync push` ships it to paired devices",
                path.display()
            );
        }
        AppsCmd::Backups => {
            let rows = pai_sync::backup::list(&store)?;
            if rows.is_empty() {
                println!("(no backups — `pai apps backup <id>`)");
            }
            let me = device.id.to_string();
            for r in rows {
                let who = if r.writer == me { "mine" } else { "peer" };
                let state = if r.deleted { " (deleted)" } else { "" };
                println!(
                    "  {:<28} {:.8}  {:<6} {}{}",
                    r.app_id, r.writer, who, r.created_at, state
                );
            }
        }
        AppsCmd::Restore { id, from } => {
            let p =
                pai_sync::backup::restore(&store, &cfg.data_dir, device.id, id, from.as_deref())?;
            record_audit(
                &store,
                device.id,
                AuditKind::AppRestored,
                serde_json::json!({
                    "app_id": id,
                    "backup_writer": p.writer,
                    "backup_created_at": p.created_at,
                }),
            )?;
            println!(
                "restored {id} from backup by {:.8} ({})",
                p.writer, p.created_at
            );
        }
        AppsCmd::BackupDelete { id } => {
            if pai_sync::backup::delete(&store, &cfg.data_dir, device.id, id)? {
                println!("backup deleted — tombstone ships on next push");
            } else {
                println!("no backup of mine for {id}");
            }
        }
        AppsCmd::Migrate { id, to } => {
            let target = resolve_peer(&store, to)?;
            // Order matters: snapshot while still active here
            // (create refuses on an inactive app), then hand over
            // placement, then quiesce local data. The app/ object
            // carries active_device; the bkp/ object carries
            // migrate_to — pull applies them in that rank order.
            let pak = pai_sync::backup::create(&store, &cfg.data_dir, device.id, id, Some(target))?;
            let now = pai_core::now().to_rfc3339();
            store.with_conn(|c| {
                c.execute(
                    "UPDATE apps SET active_device=?2, updated_at=?3 WHERE id=?1",
                    rusqlite::params![id, target.to_string(), now],
                )?;
                Ok(())
            })?;
            let rescue = pai_apps::AppRegistry::new(&cfg.data_dir)
                .deactivate_data(id)
                .map_err(|e| Error::Other(e.to_string()))?;
            record_audit(
                &store,
                device.id,
                AuditKind::AppMigrated,
                serde_json::json!({
                    "app_id": id,
                    "to": target.to_string(),
                }),
            )?;
            println!(
                "migrating {id} to {:.8} — snapshot {}, local data {}",
                target,
                pak.display(),
                rescue
                    .map(|p| format!("parked at {}", p.display()))
                    .unwrap_or_else(|| "was empty".into())
            );
            println!("`pai sync push` ships it; the target restores on pull");
        }
        AppsCmd::Rescue { id, all, from } => {
            let audit = |app_id: &str, outcome: &pai_sync::backup::RescueOutcome| {
                let _ = record_audit(
                    &store,
                    device.id,
                    AuditKind::AppRescued,
                    serde_json::json!({
                        "app_id": app_id,
                        "outcome": format!("{outcome:?}"),
                    }),
                );
            };
            if *all {
                let from_id = resolve_peer(
                    &store,
                    from.as_deref()
                        .ok_or_else(|| Error::InvalidInput("--all needs --from <device>".into()))?,
                )?;
                let results =
                    pai_sync::backup::rescue_from(&store, &cfg.data_dir, device.id, from_id)?;
                if results.is_empty() {
                    println!("(no apps were active on {from_id})");
                }
                for (app_id, outcome) in &results {
                    audit(app_id, outcome);
                    match outcome {
                        pai_sync::backup::RescueOutcome::Restored { writer, created_at } => {
                            println!(
                                "  {app_id}: claimed + restored from {:.8} ({created_at})",
                                writer
                            )
                        }
                        pai_sync::backup::RescueOutcome::ClaimedNoBackup => {
                            println!("  {app_id}: claimed — no backup; fresh install only")
                        }
                        pai_sync::backup::RescueOutcome::ClaimedRestoreFailed(e) => {
                            println!("  {app_id}: claimed — restore failed: {e}")
                        }
                    }
                }
            } else {
                let id = id.as_deref().ok_or_else(|| {
                    Error::InvalidInput("pass an app id, or --all --from <device>".into())
                })?;
                let outcome = pai_sync::backup::rescue(&store, &cfg.data_dir, device.id, id)?;
                audit(id, &outcome);
                match outcome {
                    pai_sync::backup::RescueOutcome::Restored { writer, created_at } => println!(
                        "rescued {id}: placement claimed, data restored from {:.8} ({created_at})",
                        writer
                    ),
                    pai_sync::backup::RescueOutcome::ClaimedNoBackup => println!(
                        "rescued {id}: placement claimed — no backup found; fresh install only"
                    ),
                    pai_sync::backup::RescueOutcome::ClaimedRestoreFailed(e) => {
                        println!("rescued {id}: placement claimed — restore failed: {e}")
                    }
                }
            }
            println!("claim ships on next `pai sync push`");
        }
        AppsCmd::Status { id } => {
            let ops = pai_agent::appops::StoreAppOperator::new(
                store.clone(),
                cfg.data_dir.clone(),
                device.id,
            );
            use pai_tools::AppOperator as _;
            let v = ops.status(id)?;
            if !v["installed"].as_bool().unwrap_or(false) {
                println!("{id}: not installed here");
            }
            let p = &v["placement"];
            let placed = p["device_id"].as_str();
            match placed {
                None => println!("placement: unplaced (local instance)"),
                Some(d) if p["this_device"].as_bool() == Some(true) => {
                    println!("placement: this device ({:.8})", d)
                }
                Some(d) => println!(
                    "placement: {} ({:.8}){}",
                    p["device_name"].as_str().unwrap_or("?"),
                    d,
                    if p["paired"].as_bool() == Some(true) {
                        ""
                    } else {
                        " (not a paired peer — stale claim?)"
                    }
                ),
            }
            let st = &v["storage"];
            println!(
                "storage: {} ({} bytes)",
                if st["data_present"].as_bool() == Some(true) {
                    "data present"
                } else {
                    "no data dir"
                },
                st["data_bytes"].as_u64().unwrap_or(0)
            );
            println!(
                "backups: {} (newest {})",
                v["backups"]["count"].as_u64().unwrap_or(0),
                v["backups"]["newest"].as_str().unwrap_or("none")
            );
            println!(
                "shares: {} active, {} expired, {} revoked",
                v["shares"]["active"].as_u64().unwrap_or(0),
                v["shares"]["expired"].as_u64().unwrap_or(0),
                v["shares"]["revoked"].as_u64().unwrap_or(0)
            );
            let lg = &v["logs"];
            print!("run logs: {}", lg["count"].as_u64().unwrap_or(0));
            match lg["last_at"].as_str() {
                None => println!(),
                Some(at) => {
                    let last = match lg["last_trap"].as_str() {
                        Some(t) => format!("last {at} — trap: {t}"),
                        None => format!(
                            "last {at} — exit {}",
                            lg["last_exit_code"]
                                .as_i64()
                                .map(|c| c.to_string())
                                .unwrap_or_else(|| "ok".into())
                        ),
                    };
                    println!(" ({last})");
                }
            }
            let events = v["recent_events"].as_array().cloned().unwrap_or_default();
            if events.is_empty() {
                println!("recent events: none");
            } else {
                println!("recent events:");
                for e in events {
                    println!(
                        "  {} {} — {}",
                        e["at"].as_str().unwrap_or("?"),
                        e["kind"].as_str().unwrap_or("?"),
                        e["outcome"].as_str().unwrap_or("?")
                    );
                }
            }
        }
        AppsCmd::Logs { id, limit } => {
            let entries = pai_apps::logs::tail(&cfg.data_dir, id, (*limit).min(20));
            if entries.is_empty() {
                println!("{id}: no run logs (apps/<id>/logs/ is empty — has it run?)");
            }
            for e in entries {
                let outcome = match (e.trap, e.exit_code) {
                    (Some(t), _) => format!("trap: {t}"),
                    (None, Some(c)) => format!("exit {c}"),
                    (None, None) => "ok".into(),
                };
                println!("── {} · {outcome} · fuel {}", e.at, e.fuel);
                if !e.stdout.is_empty() {
                    print!("{}", e.stdout);
                    if !e.stdout.ends_with('\n') {
                        println!();
                    }
                }
                if !e.stderr.is_empty() {
                    eprintln!("--- stderr ---\n{}", e.stderr);
                }
            }
        }
        AppsCmd::Auth {
            id,
            provider,
            client_id,
            scope,
            device_url,
            token_url,
            tenant,
            remove,
            status,
        } => {
            use pai_apps::auth::AppAuth;
            // Config changes require the package installed locally.
            if pai_apps::AppRegistry::new(&cfg.data_dir)
                .get(id)
                .map_err(|e| Error::Other(e.to_string()))?
                .is_none()
                && !*status
            {
                return Err(Error::InvalidInput(format!(
                    "app {id} not installed — `pai apps deploy` it first"
                )));
            }
            let mut auth =
                AppAuth::load(&cfg.data_dir, id).map_err(|e| Error::Other(e.to_string()))?;
            if *status {
                if auth.providers.is_empty() {
                    println!("{id}: no oauth providers configured");
                }
                for (name, c) in &auth.providers {
                    let token = if pai_apps::auth::has_token(&cfg.data_dir, id, name) {
                        "stored"
                    } else {
                        "missing — run `pai apps auth` on this device"
                    };
                    println!(
                        "{name}: provider={} client_id={} env={} token={token}",
                        c.provider,
                        c.client_id,
                        pai_apps::auth::env_name(name)
                    );
                }
            } else if *remove {
                let name = provider.as_deref().ok_or_else(|| {
                    Error::InvalidInput("apps auth --remove needs a provider".into())
                })?;
                if auth.providers.remove(name).is_none() {
                    return Err(Error::NotFound(format!(
                        "no oauth provider '{name}' configured for {id}"
                    )));
                }
                auth.save(&cfg.data_dir, id)
                    .map_err(|e| Error::Other(e.to_string()))?;
                println!("{id}: removed oauth provider '{name}' (syncs on next push)");
            } else {
                let name = provider.as_deref().ok_or_else(|| {
                    Error::InvalidInput(
                        "apps auth needs a provider — google|microsoft|custom".into(),
                    )
                })?;
                let oc = pai_oauth::OAuthConfig {
                    provider: name.into(),
                    client_id: client_id.clone().ok_or_else(|| {
                        Error::InvalidInput(
                            "--client-id required — register an oauth app first".into(),
                        )
                    })?,
                    tenant: tenant.clone(),
                    device_url: device_url.clone(),
                    token_url: token_url.clone(),
                    scopes: (!scope.is_empty()).then_some(scope.clone()),
                };
                auth.providers.insert(name.into(), oc.clone());
                auth.save(&cfg.data_dir, id)
                    .map_err(|e| Error::Other(e.to_string()))?;
                // Device-authorization flow: print the code, poll
                // until the user authorizes (or the grant expires).
                let grant = pai_oauth::device_flow(&oc).await?;
                let tokens = wait_device_grant(&oc, &grant).await?;
                let refresh = tokens.refresh_token.ok_or_else(|| {
                    Error::Provider(
                        "oauth grant returned no refresh_token — add `offline_access` scope".into(),
                    )
                })?;
                if !pai_apps::auth::store_refresh_token(&cfg.data_dir, id, name, &refresh) {
                    return Err(Error::Other(
                        "cannot persist the refresh token — keystore and file fallback both failed"
                            .into(),
                    ));
                }
                record_audit(
                    &store,
                    device.id,
                    AuditKind::AppAuthConfigured,
                    serde_json::json!({
                        "app_id": id,
                        "provider": name,
                        "env": pai_apps::auth::env_name(name),
                    }),
                )?;
                let env_var = pai_apps::auth::env_name(name);
                println!(
                    "{id}: {name} authorized — token stored; the app sees {env_var} at run time"
                );
            }
        }
        AppsCmd::Share {
            id,
            action,
            days,
            for_device,
            out,
        } => {
            let reg = pai_apps::AppRegistry::new(&cfg.data_dir);
            if reg
                .get(id)
                .map_err(|e| Error::InvalidInput(e.to_string()))?
                .is_none()
            {
                return Err(Error::NotFound(format!(
                    "app {id} — `pai apps deploy` it first"
                )));
            }
            let mut actions = Vec::new();
            for a in action {
                actions.push(match a.as_str() {
                    "exec" => pai_share::Action::Exec,
                    "read" => pai_share::Action::Read,
                    "write" => pai_share::Action::Write,
                    "share" => pai_share::Action::Share,
                    other => {
                        return Err(Error::InvalidInput(format!(
                            "unknown action '{other}' — exec|read|write|share"
                        )))
                    }
                });
            }
            // --for binds the grant to a peer's device key: guest
            // requests must then arrive signed by that key.
            let grantee_key = match for_device {
                Some(prefix) => {
                    let peers = pai_sync::pair::list_peers(&store)?;
                    let m: Vec<_> = peers
                        .iter()
                        .filter(|p| p.device_id.to_string().starts_with(prefix.as_str()))
                        .collect();
                    match m.len() {
                        0 => {
                            return Err(Error::NotFound(format!(
                                "no paired device matching '{prefix}'"
                            )))
                        }
                        1 => Some(m[0].ed_pubkey),
                        n => {
                            return Err(Error::InvalidInput(format!(
                                "'{prefix}' matches {n} devices — be more specific"
                            )))
                        }
                    }
                }
                None => None,
            };
            let shares = pai_share::ShareStore::new(&cfg.data_dir);
            let mut spec = pai_share::GrantSpec::for_app(id.clone(), actions.clone());
            spec.grantee_key = grantee_key;
            spec.expires = Some((pai_core::now() + chrono::Duration::days(*days)).timestamp());
            let cap = shares
                .grant(&ids, &key_dir, device.id, spec)
                .map_err(|e| Error::Other(e.to_string()))?;
            let json = cap.to_json().map_err(|e| Error::Other(e.to_string()))?;
            let path = match out {
                Some(p) => std::path::PathBuf::from(p),
                None => cfg
                    .data_dir
                    .join("share")
                    .join("tokens")
                    .join(format!("{}-{}.json", cap.app_id, cap.token_id)),
            };
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, &json).map_err(|e| Error::Storage(format!("{path:?}: {e}")))?;
            record_audit(
                &store,
                device.id,
                AuditKind::AppShared,
                serde_json::json!({
                    "app_id": id,
                    "token_id": cap.token_id,
                    "actions": actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    "bound": cap.grantee_key.is_some(),
                    "expires": cap.expires,
                }),
            )?;
            println!("wrote {}", path.display());
            println!(
                "token {} — {} on {}{} — expires {}",
                &cap.token_id[..8.min(cap.token_id.len())],
                actions
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                cap.app_id,
                if cap.grantee_key.is_some() {
                    " (bound to grantee)"
                } else {
                    " (bearer — anyone holding it may run the app)"
                },
                chrono::DateTime::from_timestamp(cap.expires.unwrap_or(0), 0)
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| "never".into())
            );
            println!(
                "guest runs it with: pai apps run {id} --on {} --cap <file>",
                device.id
            );
        }
        AppsCmd::Delegate {
            parent,
            action,
            days,
            for_device,
            out,
        } => {
            let raw = std::fs::read_to_string(parent)
                .map_err(|e| Error::InvalidInput(format!("{parent}: {e}")))?;
            let parent_cap = pai_share::Capability::from_json(&raw)
                .map_err(|e| Error::InvalidInput(format!("bad parent token: {e}")))?;
            let mut actions = Vec::new();
            for a in action {
                actions.push(match a.as_str() {
                    "exec" => pai_share::Action::Exec,
                    "read" => pai_share::Action::Read,
                    "write" => pai_share::Action::Write,
                    "share" => pai_share::Action::Share,
                    other => {
                        return Err(Error::InvalidInput(format!(
                            "unknown action '{other}' — exec|read|write|share"
                        )))
                    }
                });
            }
            // The chain only verifies when this device's key is the
            // key the parent was bound to — fail early otherwise.
            let my_key = hex::encode(&device.public_key);
            if parent_cap.grantee_key.as_deref() != Some(my_key.as_str()) {
                return Err(Error::InvalidInput(
                    "parent token isn't bound to this device's key".into(),
                ));
            }
            let grantee_key = match for_device {
                Some(target) => match hex::decode(target) {
                    Ok(v) if v.len() == 32 => Some(<[u8; 32]>::try_from(v.as_slice()).unwrap()),
                    Ok(_) => {
                        return Err(Error::InvalidInput(
                            "--for hex key must be 32 bytes (64 hex chars)".into(),
                        ))
                    }
                    Err(_) => Some(
                        pai_sync::pair::list_peers(&store)?
                            .iter()
                            .find(|p| p.device_id.to_string().starts_with(target.as_str()))
                            .map(|p| p.ed_pubkey)
                            .ok_or_else(|| {
                                Error::NotFound(format!("no paired device matching '{target}'"))
                            })?,
                    ),
                },
                None => None,
            };
            let shares = pai_share::ShareStore::new(&cfg.data_dir);
            let mut spec =
                pai_share::GrantSpec::for_app(parent_cap.app_id.clone(), actions.clone());
            spec.grantee_key = grantee_key;
            spec.device = parent_cap.device;
            spec.expires = days.map(|d| (pai_core::now() + chrono::Duration::days(d)).timestamp());
            let cap = shares
                .delegate(&ids, &key_dir, device.id, &parent_cap, spec)
                .map_err(|e| Error::Other(e.to_string()))?;
            let json = cap.to_json().map_err(|e| Error::Other(e.to_string()))?;
            let path = match out {
                Some(p) => std::path::PathBuf::from(p),
                None => cfg
                    .data_dir
                    .join("share")
                    .join("tokens")
                    .join(format!("{}-{}.json", cap.app_id, cap.token_id)),
            };
            if let Some(p) = path.parent() {
                std::fs::create_dir_all(p).ok();
            }
            std::fs::write(&path, &json).map_err(|e| Error::Storage(format!("{path:?}: {e}")))?;
            record_audit(
                &store,
                device.id,
                AuditKind::AppShared,
                serde_json::json!({
                    "app_id": cap.app_id,
                    "token_id": cap.token_id,
                    "parent": parent_cap.token_id,
                    "delegated": true,
                    "actions": actions.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    "bound": cap.grantee_key.is_some(),
                    "expires": cap.expires,
                }),
            )?;
            println!("wrote {}", path.display());
            println!(
                "sub-token {} — {} on {} — delegated from {}",
                &cap.token_id[..8.min(cap.token_id.len())],
                actions
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(","),
                cap.app_id,
                &parent_cap.token_id[..8.min(parent_cap.token_id.len())],
            );
        }
        AppsCmd::Grants => {
            let shares = pai_share::ShareStore::new(&cfg.data_dir);
            let list = shares.list().map_err(|e| Error::Other(e.to_string()))?;
            if list.is_empty() {
                println!("no capability grants issued");
            }
            for (cap, status) in list {
                let actions = cap
                    .actions
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(",");
                let exp = cap
                    .expires
                    .and_then(|e| chrono::DateTime::from_timestamp(e, 0))
                    .map(|d| d.to_rfc3339())
                    .unwrap_or_else(|| "never".into());
                let parent = cap
                    .parent
                    .as_ref()
                    .map(|p| format!("  ↳ {:.8}", p.token_id))
                    .unwrap_or_default();
                println!(
                    "{}  {:<24} {:<10} {:<8} exp {}  {}{}",
                    cap.token_id,
                    cap.app_id,
                    actions,
                    format!("{status:?}").to_lowercase(),
                    exp,
                    if cap.grantee_key.is_some() {
                        "bound"
                    } else {
                        "bearer"
                    },
                    parent,
                );
            }
        }
        AppsCmd::Revoke { id, token } => {
            let shares = pai_share::ShareStore::new(&cfg.data_dir);
            let list = shares.list().map_err(|e| Error::Other(e.to_string()))?;
            match list.iter().find(|(c, _)| c.token_id == *token) {
                None => {
                    return Err(Error::NotFound(format!(
                        "no grant {token} — `pai apps grants`"
                    )))
                }
                Some((c, _)) if c.app_id != *id => {
                    return Err(Error::InvalidInput(format!(
                        "token {token} grants app {} not {id}",
                        c.app_id
                    )))
                }
                _ => {}
            }
            shares
                .revoke(token)
                .map_err(|e| Error::Other(e.to_string()))?;
            record_audit(
                &store,
                device.id,
                AuditKind::AppShareRevoked,
                serde_json::json!({"app_id": id, "token_id": token}),
            )?;
            println!("revoked {token} — guests holding it are refused on next request");
        }
        AppsCmd::Read {
            id,
            path,
            cap,
            on,
            dir,
            relay,
            token,
            out,
        } => {
            use base64::Engine as _;
            let args = vec![path.clone()];
            let resp = match cap {
                Some(c) => {
                    let dev = on.as_deref().ok_or_else(|| {
                        Error::InvalidInput("--cap needs --on <device|any>".into())
                    })?;
                    let t = sync_transport(dir, relay, token)?;
                    let cx = GuestCtx {
                        store: &store,
                        ids: &ids,
                        key_dir: &key_dir,
                        device: &device,
                    };
                    guest_call(&*t, &cx, c, dev, id, "app-read", &args).await?.1
                }
                None => pai_apps::app_read_op(&cfg.data_dir, id, &args)
                    .map_err(|e| Error::InvalidInput(e.to_string()))?,
            };
            let v: serde_json::Value = serde_json::from_slice(&resp)
                .map_err(|e| Error::Other(format!("bad app-read response: {e}")))?;
            let bytes = v["data_b64"]
                .as_str()
                .and_then(|x| base64::engine::general_purpose::STANDARD.decode(x).ok())
                .ok_or_else(|| Error::Other("app-read returned no data_b64".into()))?;
            match out {
                Some(f) => {
                    std::fs::write(f, &bytes).map_err(|e| Error::Storage(format!("{f}: {e}")))?;
                    println!("wrote {} ({} bytes)", f, bytes.len());
                }
                None => {
                    use std::io::Write as _;
                    std::io::stdout().write_all(&bytes).ok();
                    println!();
                }
            }
        }
        AppsCmd::Write {
            id,
            path,
            file,
            text,
            cap,
            on,
            dir,
            relay,
            token,
        } => {
            use base64::Engine as _;
            let data = match (file, text) {
                (Some(f), None) => {
                    std::fs::read(f).map_err(|e| Error::InvalidInput(format!("{f}: {e}")))?
                }
                (None, Some(t)) => t.clone().into_bytes(),
                _ => {
                    return Err(Error::InvalidInput(
                        "pass --file <path> or --text <s>".into(),
                    ))
                }
            };
            let args = vec![
                path.clone(),
                base64::engine::general_purpose::STANDARD.encode(&data),
            ];
            match cap {
                Some(c) => {
                    let dev = on.as_deref().ok_or_else(|| {
                        Error::InvalidInput("--cap needs --on <device|any>".into())
                    })?;
                    let t = sync_transport(dir, relay, token)?;
                    let cx = GuestCtx {
                        store: &store,
                        ids: &ids,
                        key_dir: &key_dir,
                        device: &device,
                    };
                    guest_call(&*t, &cx, c, dev, id, "app-write", &args).await?;
                }
                None => {
                    pai_apps::app_write_op(&cfg.data_dir, id, &args)
                        .map_err(|e| Error::InvalidInput(e.to_string()))?;
                }
            }
            record_audit(
                &store,
                device.id,
                AuditKind::AppRun,
                serde_json::json!({
                    "app_id": id,
                    "guest_write": path,
                    "bytes": data.len(),
                }),
            )?;
            println!("wrote {} bytes to {id}:{path}", data.len());
        }
    }
    Ok(())
}
