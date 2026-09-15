//! Capability tokens for app sharing (ADR-0004 §sharing, arch §4.3).
//!
//! A [`Capability`] is a self-contained, Ed25519-signed grant issued by
//! the app owner's device key: `{token_id, app_id, actions, grantee,
//! device, expires, issued_by, issued_at, signature}`. The serialized
//! JSON is the wire form — a peer presents it to prove the grant.
//!
//! [`ShareStore`] persists issued tokens under `<data_dir>/share/`
//! (one JSON file per token plus a `revoked/` tombstone dir), so
//! `list`/`revoke`/`verify` work without schema changes. Verification
//! is pure token math — signature, expiry, revocation, action — and
//! needs only the issuer's public key, so it works for grantors whose
//! device isn't in the local `devices` table.
//!
//! Enforcement: [`guest`] turns a token into a request channel over
//! `SyncTransport` — `greq/` objects carry the capability, `gres/`
//! replies seal to the request's ephemeral X25519 key. `pai apps run
//! --cap <token>` presents it; `pai broker serve` verifies and runs it
//! through the same sandboxed `app_run_op` vault members use.

pub mod guest;

use pai_core::{DeviceId, Error as IdentityError};
use pai_identity::IdentityStore;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub type ShareResult<T> = Result<T, ShareError>;

#[derive(Debug, thiserror::Error)]
pub enum ShareError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("identity: {0}")]
    Identity(#[from] IdentityError),
    #[error("{0}")]
    InvalidInput(String),
    #[error("capability signature does not match issuer {0}")]
    BadSignature(String),
    #[error("capability {0} was revoked")]
    Revoked(String),
    #[error("capability {0} expired")]
    Expired(String),
    #[error("capability does not grant {0}")]
    ActionNotGranted(String),
}

/// What a grant allows on the app. `Share` lets the grantee re-issue
/// narrower grants (a subset of these actions) under its own key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Read,
    Write,
    Exec,
    Share,
}

impl std::fmt::Display for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Action::Read => "read",
            Action::Write => "write",
            Action::Exec => "exec",
            Action::Share => "share",
        })
    }
}

/// A signed capability token. `signature` is hex Ed25519 over
/// [`Capability::signing_payload`]; everything else is covered by it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capability {
    /// Unique id — the revocation handle and the store filename.
    pub token_id: String,
    pub app_id: String,
    pub actions: Vec<Action>,
    /// Hex Ed25519 public key of the grantee; `None` = bearer token —
    /// whoever presents a valid token holds the grant.
    pub grantee_key: Option<String>,
    /// Restrict use to this device id; `None` = any device.
    pub device: Option<DeviceId>,
    /// Unix seconds; `None` = no expiry.
    pub expires: Option<i64>,
    /// Device id of the issuing (signing) device.
    pub issued_by: DeviceId,
    /// Unix seconds.
    pub issued_at: i64,
    /// Hex Ed25519 signature over `signing_payload`.
    pub signature: String,
}

/// Outcome of a successful [`ShareStore::verify`].
#[derive(Debug)]
pub struct Grant {
    pub token_id: String,
    pub app_id: String,
    /// Whether the token may further delegate (holds `share`).
    pub can_share: bool,
}

/// What a [`ShareStore::grant`] mints — grouped to keep `grant` small.
#[derive(Debug, Clone)]
pub struct GrantSpec {
    pub app_id: String,
    pub actions: Vec<Action>,
    /// Grantee's Ed25519 public key; `None` = bearer token.
    pub grantee_key: Option<[u8; 32]>,
    /// Restrict to this device; `None` = any device.
    pub device: Option<DeviceId>,
    /// Unix-seconds expiry; `None` = never.
    pub expires: Option<i64>,
}

impl GrantSpec {
    /// Common case: grant `actions` on `app_id`, bearer token, no
    /// device restriction, no expiry.
    pub fn for_app(app_id: impl Into<String>, actions: Vec<Action>) -> Self {
        Self {
            app_id: app_id.into(),
            actions,
            grantee_key: None,
            device: None,
            expires: None,
        }
    }
}

/// Display-only status for [`ShareStore::list`] — no signature check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenStatus {
    Active,
    Revoked,
    Expired,
}

impl Capability {
    /// Canonical bytes the signature covers. Order is fixed; `Option`
    /// fields serialize as `-` when absent.
    fn signing_payload(&self) -> Vec<u8> {
        let actions = self
            .actions
            .iter()
            .map(|a| a.to_string())
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "{}\n{}\n{}\n{}\n{}\n{}\n{}\n{}",
            self.token_id,
            self.app_id,
            actions,
            self.grantee_key.as_deref().unwrap_or("-"),
            self.device
                .map(|d| d.to_string())
                .unwrap_or_else(|| "-".into()),
            self.expires
                .map(|e| e.to_string())
                .unwrap_or_else(|| "-".into()),
            self.issued_by,
            self.issued_at,
        )
        .into_bytes()
    }

    /// The token's wire form — what a grantee holds and presents.
    pub fn to_json(&self) -> ShareResult<String> {
        Ok(serde_json::to_string(self)?)
    }

    pub fn from_json(s: &str) -> ShareResult<Capability> {
        Ok(serde_json::from_str(s)?)
    }
}

/// Filesystem store for issued capabilities:
/// `<data_dir>/share/caps/<token_id>.json` + `share/revoked/<token_id>`.
pub struct ShareStore {
    dir: PathBuf,
}

impl ShareStore {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("share"),
        }
    }

    fn caps_dir(&self) -> PathBuf {
        self.dir.join("caps")
    }
    fn revoked_dir(&self) -> PathBuf {
        self.dir.join("revoked")
    }
    fn revoked_path(&self, token_id: &str) -> PathBuf {
        self.revoked_dir().join(token_id)
    }

    /// Mint, sign, and persist a capability. `issuer` is the device
    /// whose key signs (`key_dir` is the `<data_dir>/keys` fallback).
    pub fn grant(
        &self,
        ids: &IdentityStore,
        key_dir: &Path,
        issuer: DeviceId,
        spec: GrantSpec,
    ) -> ShareResult<Capability> {
        if spec.app_id.trim().is_empty() {
            return Err(ShareError::InvalidInput("app_id required".into()));
        }
        if spec.actions.is_empty() {
            return Err(ShareError::InvalidInput("at least one action".into()));
        }
        if let Some(exp) = spec.expires {
            if exp <= pai_core::now().timestamp() {
                return Err(ShareError::InvalidInput(
                    "expires must be in the future".into(),
                ));
            }
        }
        let mut cap = Capability {
            token_id: uuid::Uuid::new_v4().to_string(),
            app_id: spec.app_id,
            actions: spec.actions,
            grantee_key: spec.grantee_key.map(hex::encode),
            device: spec.device,
            expires: spec.expires,
            issued_by: issuer,
            issued_at: pai_core::now().timestamp(),
            signature: String::new(),
        };
        let sig = ids.sign(issuer, key_dir, &cap.signing_payload())?;
        cap.signature = hex::encode(sig);
        std::fs::create_dir_all(self.caps_dir())?;
        std::fs::write(
            self.caps_dir().join(format!("{}.json", cap.token_id)),
            cap.to_json()?,
        )?;
        Ok(cap)
    }

    /// Tombstone a token by id. Returns false when no such token exists.
    pub fn revoke(&self, token_id: &str) -> ShareResult<bool> {
        if !self.caps_dir().join(format!("{token_id}.json")).is_file() {
            return Ok(false);
        }
        std::fs::create_dir_all(self.revoked_dir())?;
        std::fs::write(self.revoked_path(token_id), b"revoked")?;
        Ok(true)
    }

    /// Full verification of a presented token: signature against the
    /// issuer's public key, revocation tombstone, expiry, and that
    /// `action` is covered. `issuer_pubkey` comes from the caller's
    /// device/peer tables — the token only names `issued_by`.
    pub fn verify(
        &self,
        ids: &IdentityStore,
        cap: &Capability,
        issuer_pubkey: &[u8; 32],
        action: Action,
    ) -> ShareResult<Grant> {
        if !cap.actions.contains(&action) {
            return Err(ShareError::ActionNotGranted(action.to_string()));
        }
        if self.revoked_path(&cap.token_id).is_file() {
            return Err(ShareError::Revoked(cap.token_id.clone()));
        }
        if let Some(exp) = cap.expires {
            if exp <= pai_core::now().timestamp() {
                return Err(ShareError::Expired(cap.token_id.clone()));
            }
        }
        let sig = hex::decode(&cap.signature)
            .map_err(|e| ShareError::InvalidInput(format!("signature hex: {e}")))?;
        match ids.verify_with_key(issuer_pubkey, &cap.signing_payload(), &sig)? {
            true => Ok(Grant {
                token_id: cap.token_id.clone(),
                app_id: cap.app_id.clone(),
                can_share: cap.actions.contains(&Action::Share),
            }),
            false => Err(ShareError::BadSignature(cap.issued_by.to_string())),
        }
    }

    /// Issued tokens, newest first, with tombstone/expiry status.
    /// Signature validity is NOT checked here — display aid only.
    pub fn list(&self) -> ShareResult<Vec<(Capability, TokenStatus)>> {
        let dir = self.caps_dir();
        let mut out = Vec::new();
        if dir.is_dir() {
            for e in std::fs::read_dir(&dir)? {
                let e = e?;
                if e.path().extension().and_then(|x| x.to_str()) != Some("json") {
                    continue;
                }
                let cap = Capability::from_json(&std::fs::read_to_string(e.path())?)?;
                let status = if self.revoked_path(&cap.token_id).is_file() {
                    TokenStatus::Revoked
                } else if cap
                    .expires
                    .is_some_and(|x| x <= pai_core::now().timestamp())
                {
                    TokenStatus::Expired
                } else {
                    TokenStatus::Active
                };
                out.push((cap, status));
            }
        }
        out.sort_by_key(|(c, _)| std::cmp::Reverse(c.issued_at));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pai_core::{Device, DeviceCapabilities, NetworkState, Platform};
    use pai_storage::Store;
    use std::sync::Arc;

    fn setup(root: &Path) -> (IdentityStore, Device, PathBuf) {
        let ids = IdentityStore::new(Arc::new(Store::in_memory().unwrap()));
        let user = ids.create_user("t").unwrap();
        let key_dir = root.join("keys");
        let dev = ids
            .register_device(
                user.id,
                "laptop",
                Platform::Linux,
                DeviceCapabilities {
                    cpu_cores: 4,
                    ram_bytes: 1 << 30,
                    gpu_vram_bytes: None,
                    gpu_name: None,
                    npu_available: false,
                    on_battery: None,
                    thermal_throttled: None,
                    network: NetworkState::Unknown,
                    available_models: vec![],
                    supported_capabilities: vec![],
                },
                &key_dir,
            )
            .unwrap();
        (ids, dev, key_dir)
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("pai-share-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn pubkey(dev: &Device) -> [u8; 32] {
        dev.public_key.as_slice().try_into().unwrap()
    }

    #[test]
    fn grant_verify_roundtrip() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        let cap = store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("my-app", vec![Action::Exec]),
            )
            .unwrap();
        let g = store
            .verify(&ids, &cap, &pubkey(&dev), Action::Exec)
            .unwrap();
        assert_eq!(g.app_id, "my-app");
        assert!(!g.can_share);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn json_wire_form_roundtrips() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        let cap = store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec {
                    expires: Some(pai_core::now().timestamp() + 3600),
                    ..GrantSpec::for_app("a", vec![Action::Read, Action::Exec, Action::Share])
                },
            )
            .unwrap();
        let wire = cap.to_json().unwrap();
        let back = Capability::from_json(&wire).unwrap();
        let g = store
            .verify(&ids, &back, &pubkey(&dev), Action::Read)
            .unwrap();
        assert!(g.can_share);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn action_not_granted_rejects() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        let cap = store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("a", vec![Action::Read]),
            )
            .unwrap();
        assert!(matches!(
            store.verify(&ids, &cap, &pubkey(&dev), Action::Exec),
            Err(ShareError::ActionNotGranted(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn revoked_rejects() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        let cap = store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("a", vec![Action::Exec]),
            )
            .unwrap();
        assert!(store.revoke(&cap.token_id).unwrap());
        assert!(matches!(
            store.verify(&ids, &cap, &pubkey(&dev), Action::Exec),
            Err(ShareError::Revoked(_))
        ));
        // Status shows up in list too.
        let listed = store.list().unwrap();
        assert_eq!(listed[0].1, TokenStatus::Revoked);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn expired_rejects() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        // Mint valid, then hand-craft an already-expired copy (grant
        // refuses past expiry, so resign the mutated token).
        let mut cap = store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("a", vec![Action::Exec]),
            )
            .unwrap();
        cap.expires = Some(pai_core::now().timestamp() - 1);
        let sig = ids.sign(dev.id, &key_dir, &cap.signing_payload()).unwrap();
        cap.signature = hex::encode(sig);
        assert!(matches!(
            store.verify(&ids, &cap, &pubkey(&dev), Action::Exec),
            Err(ShareError::Expired(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn tampered_token_bad_signature() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        let mut cap = store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec::for_app("a", vec![Action::Exec]),
            )
            .unwrap();
        cap.app_id = "other-app".into(); // unsigned field change
        assert!(matches!(
            store.verify(&ids, &cap, &pubkey(&dev), Action::Exec),
            Err(ShareError::BadSignature(_))
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn past_expiry_grant_refused_and_empty_actions_refused() {
        let root = tmp();
        let (ids, dev, key_dir) = setup(&root);
        let store = ShareStore::new(&root);
        assert!(store
            .grant(&ids, &key_dir, dev.id, GrantSpec::for_app("a", vec![]))
            .is_err());
        assert!(store
            .grant(
                &ids,
                &key_dir,
                dev.id,
                GrantSpec {
                    expires: Some(1),
                    ..GrantSpec::for_app("a", vec![Action::Exec])
                },
            )
            .is_err());
        let _ = std::fs::remove_dir_all(&root);
    }
}
