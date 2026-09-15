//! Concrete [`pai_tools::AppOperator`] over the local store — the
//! `apps.*` agent tools mint capability tokens and snapshots through
//! the same `ShareStore`/`backup` machinery the `pai apps` commands
//! use. Lives here (not in pai-tools) because it needs share+sync+
//! apps+identity, which pai-tools deliberately doesn't depend on.

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
}
