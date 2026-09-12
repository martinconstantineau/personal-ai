//! User identity + device registry.
//!
//! Each device holds an ed25519 keypair; the public key identifies the device
//! to peers during sync. V1 stores the private key under `data_dir` with
//! 0600 permissions; production targets the OS keystore (Keychain, Android
//! Keystore, TPM) — see docs/adr/0011-cryptography.md.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use pai_core::*;
use pai_storage::{parse_ts, store_err, ts, Store};
use rand_core::OsRng;
use rusqlite::params;
use std::path::Path;
use std::sync::Arc;

pub struct IdentityStore {
    store: Arc<Store>,
}

impl IdentityStore {
    pub fn new(store: Arc<Store>) -> Self {
        Self { store }
    }

    pub fn create_user(&self, display_name: &str) -> Result<User> {
        let user = User {
            id: UserId::new(),
            display_name: display_name.into(),
            created_at: now(),
        };
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO users(id, display_name, created_at) VALUES(?1,?2,?3)",
                params![user.id.to_string(), user.display_name, ts(&user.created_at)],
            )
        })?;
        Ok(user)
    }

    pub fn get_user(&self, id: UserId) -> Result<User> {
        self.store
            .with_conn(|c| {
                c.query_row(
                    "SELECT id, display_name, created_at FROM users WHERE id=?1",
                    params![id.to_string()],
                    |r| {
                        Ok(User {
                            id: UserId(parse_uuid(&r.get::<_, String>(0)?)),
                            display_name: r.get(1)?,
                            created_at: parse_ts(&r.get::<_, String>(2)?),
                        })
                    },
                )
            })
            .map_err(|e| match e {
                Error::Storage(_) => Error::NotFound(format!("user {id}")),
                other => other,
            })
    }

    /// Register this device. Generates a fresh keypair; writes the private
    /// key under `key_dir` (0600) and returns the public half.
    pub fn register_device(
        &self,
        owner: UserId,
        name: &str,
        platform: Platform,
        capabilities: DeviceCapabilities,
        key_dir: &Path,
    ) -> Result<Device> {
        std::fs::create_dir_all(key_dir).map_err(store_err)?;
        let signing = SigningKey::generate(&mut OsRng);
        let device = Device {
            id: DeviceId::new(),
            owner,
            name: name.into(),
            platform,
            public_key: signing.verifying_key().to_bytes().to_vec(),
            registered_at: now(),
            last_seen_at: now(),
            capabilities,
        };
        let key_path = key_dir.join(format!("{}.key", device.id));
        std::fs::write(&key_path, signing.to_bytes()).map_err(store_err)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                .map_err(store_err)?;
        }
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO devices(id, owner, name, platform, public_key,
                    registered_at, last_seen_at, capabilities_json)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    device.id.to_string(),
                    device.owner.to_string(),
                    device.name,
                    platform_name(device.platform),
                    device.public_key,
                    ts(&device.registered_at),
                    ts(&device.last_seen_at),
                    serde_json::to_string(&device.capabilities).unwrap_or_else(|_| "{}".into()),
                ],
            )
        })?;
        Ok(device)
    }

    pub fn list_devices(&self, owner: UserId) -> Result<Vec<Device>> {
        self.store.with_conn(|c| {
            let mut stmt = c.prepare(
                "SELECT id, name, platform, public_key, registered_at,
                        last_seen_at, capabilities_json
                 FROM devices WHERE owner=?1",
            )?;
            let rows = stmt.query_map(params![owner.to_string()], |r| {
                Ok(Device {
                    id: DeviceId(parse_uuid(&r.get::<_, String>(0)?)),
                    owner,
                    name: r.get(1)?,
                    platform: platform_from(&r.get::<_, String>(2)?),
                    public_key: r.get(3)?,
                    registered_at: parse_ts(&r.get::<_, String>(4)?),
                    last_seen_at: parse_ts(&r.get::<_, String>(5)?),
                    capabilities: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
                })
            })?;
            rows.collect()
        })
    }

    /// Sign `msg` with a device's private key loaded from `key_dir`.
    pub fn sign(&self, device: DeviceId, key_dir: &Path, msg: &[u8]) -> Result<Vec<u8>> {
        let key_path = key_dir.join(format!("{device}.key"));
        let raw = std::fs::read(&key_path)
            .map_err(|_| Error::NotFound(format!("device key {device}")))?;
        let signing = SigningKey::from_bytes(
            raw.as_slice()
                .try_into()
                .map_err(|_| Error::InvalidInput("corrupt device key".into()))?,
        );
        Ok(signing.sign(msg).to_bytes().to_vec())
    }

    /// Verify a signature from a known device.
    pub fn verify(&self, device: &Device, msg: &[u8], sig: &[u8]) -> Result<bool> {
        let key_bytes: [u8; 32] = device
            .public_key
            .as_slice()
            .try_into()
            .map_err(|_| Error::InvalidInput("bad public key length".into()))?;
        let key = VerifyingKey::from_bytes(&key_bytes)
            .map_err(|_| Error::InvalidInput("bad public key".into()))?;
        let sig =
            Signature::from_slice(sig).map_err(|_| Error::InvalidInput("bad signature".into()))?;
        Ok(key.verify(msg, &sig).is_ok())
    }
}

fn parse_uuid(s: &str) -> uuid::Uuid {
    uuid::Uuid::parse_str(s).unwrap_or_else(|_| uuid::Uuid::nil())
}

fn platform_name(p: Platform) -> &'static str {
    match p {
        Platform::Ios => "ios",
        Platform::Android => "android",
        Platform::Linux => "linux",
        Platform::MacOs => "macos",
        Platform::Windows => "windows",
        Platform::Server => "server",
    }
}

fn platform_from(s: &str) -> Platform {
    match s {
        "ios" => Platform::Ios,
        "android" => Platform::Android,
        "macos" => Platform::MacOs,
        "windows" => Platform::Windows,
        "server" => Platform::Server,
        _ => Platform::Linux,
    }
}

/// Best-effort local hardware probe for the compute broker.
pub fn probe_capabilities() -> DeviceCapabilities {
    let ram = std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|m| {
            m.lines()
                .find(|l| l.starts_with("MemTotal:"))
                .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                .map(|kb| kb * 1024)
        })
        .unwrap_or(0);
    DeviceCapabilities {
        cpu_cores: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        ram_bytes: ram,
        gpu_vram_bytes: None,
        gpu_name: None,
        npu_available: false,
        on_battery: None,
        thermal_throttled: None,
        network: NetworkState::Unknown,
        available_models: vec![],
        supported_capabilities: vec![ModelCapability::TextGeneration],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_sign_verify_roundtrip() {
        let store = Arc::new(Store::in_memory().unwrap());
        let ids = IdentityStore::new(store);
        let user = ids.create_user("Test").unwrap();
        let dir = std::env::temp_dir().join(format!("pai-keys-{}", uuid::Uuid::new_v4()));
        let dev = ids
            .register_device(
                user.id,
                "laptop",
                Platform::Linux,
                probe_capabilities(),
                &dir,
            )
            .unwrap();
        let sig = ids.sign(dev.id, &dir, b"hello").unwrap();
        assert!(ids.verify(&dev, b"hello", &sig).unwrap());
        assert!(!ids.verify(&dev, b"tampered", &sig).unwrap());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
