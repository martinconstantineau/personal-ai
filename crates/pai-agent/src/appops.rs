//! Concrete [`pai_tools::AppOperator`] over the local store — the
//! `apps.*` agent tools mint capability tokens and snapshots through
//! the same `ShareStore`/`backup` machinery the `pai apps` commands
//! use. Lives here (not in pai-tools) because it needs share+sync+
//! apps+identity, which pai-tools deliberately doesn't depend on.

use async_trait::async_trait;
use pai_core::*;
use pai_identity::IdentityStore;
use pai_storage::Store;
use pai_tools::AppOperator;
use std::path::PathBuf;
use std::sync::Arc;

pub struct StoreAppOperator {
    store: Arc<Store>,
    data_dir: PathBuf,
    ids: IdentityStore,
    device: DeviceId,
}

impl StoreAppOperator {
    pub fn new(store: Arc<Store>, data_dir: PathBuf, device: DeviceId) -> Self {
        Self {
            ids: IdentityStore::new(store.clone()),
            store,
            data_dir,
            device,
        }
    }
}

#[async_trait]
impl AppOperator for StoreAppOperator {
    fn share_grant(
        &self,
        app_id: &str,
        actions: &[String],
        days: i64,
        for_device: Option<&str>,
    ) -> Result<serde_json::Value> {
        if pai_apps::AppRegistry::new(&self.data_dir)
            .get(app_id)
            .map_err(|e| Error::InvalidInput(e.to_string()))?
            .is_none()
        {
            return Err(Error::NotFound(format!(
                "app {app_id} — `pai apps deploy` it first"
            )));
        }
        let mut acts = Vec::new();
        for a in actions {
            acts.push(match a.as_str() {
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
        let grantee_key = match for_device {
            Some(prefix) => {
                let peers = pai_sync::pair::list_peers(&self.store)?;
                let m: Vec<_> = peers
                    .iter()
                    .filter(|p| p.device_id.to_string().starts_with(prefix))
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
        let mut spec = pai_share::GrantSpec::for_app(app_id.to_string(), acts);
        spec.grantee_key = grantee_key;
        spec.expires = Some((now() + chrono::Duration::days(days)).timestamp());
        let cap = pai_share::ShareStore::new(&self.data_dir)
            .grant(&self.ids, &self.data_dir.join("keys"), self.device, spec)
            .map_err(|e| Error::Other(e.to_string()))?;
        Ok(serde_json::json!({
            "token_id": cap.token_id,
            "app_id": cap.app_id,
            "actions": actions,
            "expires": cap.expires,
            "grantee_key": cap.grantee_key,
            "token": cap.to_json().map_err(|e| Error::Other(e.to_string()))?,
        }))
    }

    fn backup(&self, app_id: &str) -> Result<serde_json::Value> {
        let p = pai_sync::backup::create(&self.store, &self.data_dir, self.device, app_id, None)?;
        Ok(serde_json::json!({
            "app_id": app_id,
            "path": p.display().to_string(),
        }))
    }

    fn status(&self, app_id: &str) -> Result<serde_json::Value> {
        // apps-table row: the sync'd install record (deleted/active_device).
        let row = self.store.with_conn(|c| {
            Ok(c.query_row(
                "SELECT name, version, runtime, installed_at, deleted,
                        active_device FROM apps WHERE id=?1",
                rusqlite::params![app_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, i64>(4)? != 0,
                        r.get::<_, Option<String>>(5)?,
                    ))
                },
            )
            .ok())
        })?;
        let (name, version, runtime, installed_at, deleted, active) = match &row {
            Some((n, v, r, at, d, a)) => (
                Some(n.clone()),
                Some(v.clone()),
                Some(r.clone()),
                Some(at.clone()),
                *d,
                a.clone(),
            ),
            None => (None, None, None, None, false, None),
        };
        // On-disk package presence is the ground truth for "installed
        // here" — a synced apps row can exist without the package (or
        // vice versa mid-sync).
        let pkg = pai_apps::AppRegistry::new(&self.data_dir)
            .get(app_id)
            .map_err(|e| Error::InvalidInput(e.to_string()))?;
        let installed = pkg.is_some() && !deleted;

        // Resolve the placed device to a name: local `devices` first,
        // then paired peers.
        let placement = match active.as_deref() {
            None => serde_json::json!({"device_id": null, "device_name": null,
                                       "this_device": false, "paired": false}),
            Some(dev) => {
                let this = uuid::Uuid::parse_str(dev).ok() == Some(self.device.0);
                let name = self.store.with_conn(|c| {
                    Ok(c.query_row(
                        "SELECT name FROM devices WHERE id=?1",
                        rusqlite::params![dev],
                        |r| r.get::<_, String>(0),
                    )
                    .ok()
                    .or_else(|| {
                        c.query_row(
                            "SELECT name FROM sync_peers WHERE device_id=?1",
                            rusqlite::params![dev],
                            |r| r.get::<_, String>(0),
                        )
                        .ok()
                    }))
                })?;
                let paired = pai_sync::pair::list_peers(&self.store)?
                    .iter()
                    .any(|p| p.device_id.to_string() == dev);
                serde_json::json!({
                    "device_id": dev, "device_name": name,
                    "this_device": this, "paired": paired,
                })
            }
        };

        // Live data dir: presence + size answers "is there state here
        // worth backing up / did the last run even create it".
        let data_dir = self.data_dir.join("apps").join(app_id).join("data");
        let data_bytes = if data_dir.is_dir() {
            dir_size(&data_dir)
        } else {
            0
        };

        // Backups for this app (all writers).
        let bkps: Vec<_> = pai_sync::backup::list(&self.store)?
            .into_iter()
            .filter(|b| b.app_id == app_id)
            .collect();
        let newest = bkps
            .iter()
            .filter(|b| !b.deleted)
            .map(|b| b.created_at.clone())
            .max();
        let backups = serde_json::json!({
            "count": bkps.iter().filter(|b| !b.deleted).count(),
            "newest": newest,
        });

        // Share tokens by status.
        let mut shares = serde_json::json!({"active": 0, "expired": 0, "revoked": 0});
        for (cap, st) in pai_share::ShareStore::new(&self.data_dir)
            .list()
            .map_err(|e| Error::Other(e.to_string()))?
            .into_iter()
            .filter(|(c, _)| c.app_id == app_id)
        {
            let _ = cap;
            let k = match st {
                pai_share::TokenStatus::Active => "active",
                pai_share::TokenStatus::Expired => "expired",
                pai_share::TokenStatus::Revoked => "revoked",
            };
            shares[k] = serde_json::json!(shares[k].as_u64().unwrap_or(0) + 1);
        }

        // Run-log rollup: how many runs are on disk + the last one's
        // outcome and stderr tail — the actual failure text.
        let run_logs = pai_apps::logs::tail(&self.data_dir, app_id, 1);
        let last = run_logs.first();
        let logs_rollup = serde_json::json!({
            "count": pai_apps::logs::tail(&self.data_dir, app_id, pai_apps::logs::LOG_KEEP).len(),
            "last_at": last.map(|l| l.at.clone()),
            "last_exit_code": last.and_then(|l| l.exit_code),
            "last_trap": last.and_then(|l| l.trap.clone()),
            "last_stderr_tail": last.map(|l| pai_apps::logs::tail_str(&l.stderr, 5, 2048)),
        });

        // Recent audit events for this app — the "what went wrong" trail.
        let recent: Vec<_> = pai_audit::AuditLog::new(self.store.clone())
            .recent(200)
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.detail["app_id"].as_str() == Some(app_id))
            .take(10)
            .map(|e| {
                serde_json::json!({
                    "kind": pai_audit::kind_name(e.kind),
                    "outcome": pai_audit::outcome_name(e.outcome),
                    "at": e.at.to_rfc3339(),
                    "detail": e.detail,
                })
            })
            .collect();

        Ok(serde_json::json!({
            "app_id": app_id,
            "installed": installed,
            "name": name,
            "version": version,
            "runtime": runtime,
            "installed_at": installed_at,
            "deleted": deleted,
            "placement": placement,
            "storage": {
                "data_dir": data_dir.display().to_string(),
                "data_bytes": data_bytes,
                "data_present": data_dir.is_dir(),
            },
            "backups": backups,
            "shares": shares,
            "logs": logs_rollup,
            "recent_events": recent,
        }))
    }

    fn logs(&self, app_id: &str, limit: usize) -> Result<serde_json::Value> {
        let entries: Vec<_> = pai_apps::logs::tail(&self.data_dir, app_id, limit)
            .into_iter()
            .map(|l| serde_json::to_value(l).unwrap_or_default())
            .collect();
        Ok(serde_json::json!({"app_id": app_id, "entries": entries}))
    }

    async fn configure_auth(
        &self,
        app_id: &str,
        provider: &str,
        client_id: &str,
        scopes: Vec<String>,
        device_code: Option<&str>,
    ) -> Result<serde_json::Value> {
        use pai_apps::auth::AppAuth;
        if pai_apps::AppRegistry::new(&self.data_dir)
            .get(app_id)
            .map_err(|e| Error::InvalidInput(e.to_string()))?
            .is_none()
        {
            return Err(Error::NotFound(format!(
                "app {app_id} — `pai apps deploy` it first"
            )));
        }
        let mut auth = AppAuth::load(&self.data_dir, app_id)
            .map_err(|e| Error::InvalidInput(e.to_string()))?;
        match device_code {
            None => {
                let cfg = pai_oauth::OAuthConfig {
                    provider: provider.into(),
                    client_id: client_id.into(),
                    tenant: None,
                    device_url: None,
                    token_url: None,
                    scopes: (!scopes.is_empty()).then_some(scopes),
                };
                // Refuse to persist a provider whose endpoints don't
                // resolve — a typo'd provider would wedge every run.
                cfg.device_url()?;
                cfg.token_url()?;
                cfg.scope_string()?;
                auth.providers.insert(provider.into(), cfg.clone());
                auth.save(&self.data_dir, app_id)
                    .map_err(|e| Error::Other(e.to_string()))?;
                let grant = pai_oauth::device_flow(&cfg).await?;
                Ok(serde_json::json!({
                    "state": "pending",
                    "app_id": app_id,
                    "provider": provider,
                    "verification_uri": grant.verification_uri,
                    "verification_uri_complete": grant.verification_uri_complete,
                    "user_code": grant.user_code,
                    "device_code": grant.device_code,
                    "interval": grant.interval,
                    "expires_in": grant.expires_in,
                }))
            }
            Some(code) => {
                let cfg = auth.providers.get(provider).ok_or_else(|| {
                    Error::InvalidInput(format!(
                        "no oauth config for '{provider}' — run the begin phase first"
                    ))
                })?;
                match pai_oauth::poll_token_once(cfg, code).await? {
                    pai_oauth::Poll::Pending => Ok(serde_json::json!({
                        "state": "pending",
                        "app_id": app_id,
                        "provider": provider,
                    })),
                    pai_oauth::Poll::Granted(tokens) => {
                        let refresh = tokens.refresh_token.ok_or_else(|| {
                            Error::Provider(
                                "grant returned no refresh_token — add `offline_access` scope"
                                    .into(),
                            )
                        })?;
                        if !pai_apps::auth::store_refresh_token(
                            &self.data_dir,
                            app_id,
                            provider,
                            &refresh,
                        ) {
                            return Err(Error::Other(
                                "keystore unavailable — cannot persist the refresh token".into(),
                            ));
                        }
                        Ok(serde_json::json!({
                            "state": "authorized",
                            "app_id": app_id,
                            "provider": provider,
                            "env": pai_apps::auth::env_name(provider),
                        }))
                    }
                }
            }
        }
    }
}

/// Recursive size of `dir` in bytes (best-effort — unreadable entries
/// are skipped, matching "how much state is here" intent).
fn dir_size(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .map(|it| {
            it.flatten()
                .map(|e| {
                    let p = e.path();
                    if p.is_dir() {
                        dir_size(&p)
                    } else {
                        e.metadata().map(|m| m.len()).unwrap_or(0)
                    }
                })
                .sum()
        })
        .unwrap_or(0)
}
