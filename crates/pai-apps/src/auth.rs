//! Per-app OAuth — "add Google login to the invoice app" (PRD §6.8).
//!
//! Model: the **host** holds the credential; the sandboxed app gets a
//! fresh *access token* injected as `PAI_OAUTH_<NAME>` at run time.
//! The refresh token lives in the OS keystore at
//! `app-oauth:<app_id>:<name>`, falling back to a 0600 file under
//! `data_dir/.oauth/` when no keystore is reachable (same pattern as
//! `store.key`) — it never enters the sandbox, `data/`, backups, or
//! sync objects.
//!
//! Config lives at `apps/<id>/auth.json` (outside `data/` — never
//! CRDT-synced or backed up) and rides the `app/` sync object's `auth`
//! field so "add Google login" propagates to every device; each device
//! then authorizes locally (refresh tokens stay device-local, same as
//! the email connector).

use pai_oauth::OAuthConfig;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// `apps/<id>/auth.json` — provider-name → client config.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AppAuth {
    #[serde(default)]
    pub providers: BTreeMap<String, OAuthConfig>,
}

fn auth_path(data_dir: &Path, app_id: &str) -> PathBuf {
    data_dir.join("apps").join(app_id).join("auth.json")
}

impl AppAuth {
    /// Load the app's auth config — absent file means "no providers".
    pub fn load(data_dir: &Path, app_id: &str) -> crate::AppResult<Self> {
        let p = auth_path(data_dir, app_id);
        match std::fs::read(&p) {
            Ok(raw) => serde_json::from_slice(&raw)
                .map_err(|e| crate::AppError::Layout(format!("auth.json: {e}"))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(crate::AppError::Layout(format!("auth.json: {e}"))),
        }
    }

    /// Persist the config next to the package (host-side file).
    pub fn save(&self, data_dir: &Path, app_id: &str) -> crate::AppResult<()> {
        let dir = data_dir.join("apps").join(app_id);
        std::fs::create_dir_all(&dir)?;
        let raw = serde_json::to_vec_pretty(self)
            .map_err(|e| crate::AppError::Layout(format!("auth.json: {e}")))?;
        std::fs::write(auth_path(data_dir, app_id), raw)?;
        Ok(())
    }
}

/// Keystore key for a provider's refresh token.
pub fn keystore_key(app_id: &str, provider: &str) -> String {
    format!("app-oauth:{app_id}:{provider}")
}

/// File-fallback location — `data_dir/.oauth/<app>/<provider>.token`,
/// sibling of `apps/` so it can never ride a sync object or a backup.
fn token_file(data_dir: &Path, app_id: &str, provider: &str) -> PathBuf {
    data_dir
        .join(".oauth")
        .join(app_id)
        .join(format!("{provider}.token"))
}

/// `google-work` → `PAI_OAUTH_GOOGLE_WORK`. Only `[A-Za-z0-9]` survives
/// uppercasing; everything else becomes `_`.
pub fn env_name(provider: &str) -> String {
    let up: String = provider
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    format!("PAI_OAUTH_{up}")
}

/// Store a provider's refresh token — OS keystore first, 0600 file
/// fallback under `data_dir/.oauth/` (headless hosts, tests).
pub fn store_refresh_token(data_dir: &Path, app_id: &str, provider: &str, token: &str) -> bool {
    if pai_identity::keystore::store(&keystore_key(app_id, provider), token.as_bytes()) {
        return true;
    }
    let p = token_file(data_dir, app_id, provider);
    if let Some(parent) = p.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if std::fs::write(&p, token.as_bytes()).is_err() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
    }
    true
}

/// Load a provider's refresh token — keystore first, then the file.
pub fn load_refresh_token(data_dir: &Path, app_id: &str, provider: &str) -> Option<String> {
    if let Some(raw) = pai_identity::keystore::load(&keystore_key(app_id, provider)) {
        return String::from_utf8(raw).ok();
    }
    std::fs::read_to_string(token_file(data_dir, app_id, provider)).ok()
}

/// Whether a refresh token exists locally for this provider.
pub fn has_token(data_dir: &Path, app_id: &str, provider: &str) -> bool {
    load_refresh_token(data_dir, app_id, provider).is_some()
}

/// Resolve the env pairs to inject for one run: for each configured
/// provider with a locally-stored refresh token, mint a fresh access
/// token (`PAI_OAUTH_<NAME>=<token>`). A provider without a local token
/// is skipped with a warning — the app still runs; it just won't see
/// that env var. Rotation is re-stored transparently.
pub async fn resolve_envs(data_dir: &Path, app_id: &str) -> Vec<(String, String)> {
    let auth = match AppAuth::load(data_dir, app_id) {
        Ok(a) => a,
        Err(e) => {
            tracing::warn!(app = %app_id, "auth.json unreadable: {e}");
            return vec![];
        }
    };
    let mut envs = Vec::new();
    for (name, cfg) in &auth.providers {
        let Some(refresh) = load_refresh_token(data_dir, app_id, name) else {
            tracing::warn!(app = %app_id, provider = %name,
                "no local oauth token — run `pai apps auth` on this device");
            continue;
        };
        match pai_oauth::refresh_access_token(cfg, &refresh).await {
            Ok((access, rotated)) => {
                if let Some(r) = rotated {
                    if !r.is_empty() && r != refresh {
                        let _ = store_refresh_token(data_dir, app_id, name, &r);
                    }
                }
                envs.push((env_name(name), access));
            }
            Err(e) => {
                tracing::warn!(app = %app_id, provider = %name, "oauth refresh failed: {e}")
            }
        }
    }
    envs
}
