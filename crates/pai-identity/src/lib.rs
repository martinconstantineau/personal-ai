//! User identity + device registry.
//!
//! Each device holds an ed25519 keypair; the public key identifies the device
//! to peers during sync. Private keys live in the OS keystore when one is
//! reachable (Windows Credential Manager, macOS Keychain, Secret Service);
//! otherwise they fall back to a 0600 file under `data_dir`. See
//! docs/adr/0011-cryptography.md.

pub mod keystore;

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

    /// Register this device. Generates a fresh keypair; the private key goes
    /// to the OS keystore when available, else a 0600 file under `key_dir`.
    pub fn register_device(
        &self,
        owner: UserId,
        name: &str,
        platform: Platform,
        capabilities: DeviceCapabilities,
        key_dir: &Path,
    ) -> Result<Device> {
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
        let key_name = format!("device:{}", device.id);
        let key_storage = if keystore::store(&key_name, &signing.to_bytes()) {
            "os"
        } else {
            std::fs::create_dir_all(key_dir).map_err(store_err)?;
            let key_path = key_dir.join(format!("{}.key", device.id));
            std::fs::write(&key_path, signing.to_bytes()).map_err(store_err)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))
                    .map_err(store_err)?;
            }
            tracing::warn!("OS keystore unavailable; device key written to {key_path:?}");
            "file"
        };
        self.store.with_conn(|c| {
            c.execute(
                "INSERT INTO devices(id, owner, name, platform, public_key,
                    registered_at, last_seen_at, capabilities_json, key_storage)
                 VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                params![
                    device.id.to_string(),
                    device.owner.to_string(),
                    device.name,
                    platform_name(device.platform),
                    device.public_key,
                    ts(&device.registered_at),
                    ts(&device.last_seen_at),
                    serde_json::to_string(&device.capabilities).unwrap_or_else(|_| "{}".into()),
                    key_storage,
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

    /// Sign `msg` with a device's private key. Reads the OS keystore first,
    /// then the `key_dir` file fallback; a file key is migrated into the
    /// keystore opportunistically so it stops living on disk.
    pub fn sign(&self, device: DeviceId, key_dir: &Path, msg: &[u8]) -> Result<Vec<u8>> {
        let key_name = format!("device:{device}");
        let raw = if let Some(bytes) = keystore::load(&key_name) {
            bytes
        } else {
            let key_path = key_dir.join(format!("{device}.key"));
            let raw = std::fs::read(&key_path)
                .map_err(|_| Error::NotFound(format!("device key {device}")))?;
            if keystore::store(&key_name, &raw) {
                let _ = std::fs::remove_file(&key_path);
                tracing::info!(%device, "migrated device key into OS keystore");
            }
            raw
        };
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
        self.verify_with_key(&key_bytes, msg, sig)
    }

    /// Verify a signature against a raw Ed25519 public key — for signers
    /// known only by key (e.g. `sync_peers.ed_pubkey` when applying a
    /// synced app package from a paired device).
    pub fn verify_with_key(&self, key_bytes: &[u8; 32], msg: &[u8], sig: &[u8]) -> Result<bool> {
        let key = VerifyingKey::from_bytes(key_bytes)
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
    let (on_battery, thermal_throttled) = probe_power();
    DeviceCapabilities {
        cpu_cores: std::thread::available_parallelism()
            .map(|n| n.get() as u32)
            .unwrap_or(1),
        ram_bytes: probe_ram(),
        gpu_vram_bytes: None,
        gpu_name: None,
        npu_available: false,
        on_battery,
        thermal_throttled,
        network: NetworkState::Unknown,
        available_models: vec![],
        supported_capabilities: vec![ModelCapability::TextGeneration],
    }
}

/// Total physical RAM, 0 if undetectable. `/proc/meminfo` on Linux,
/// `GlobalMemoryStatusEx` on Windows.
fn probe_ram() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut st: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        st.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut st) } != 0 {
            st.ullTotalPhys
        } else {
            0
        }
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|m| {
                m.lines()
                    .find(|l| l.starts_with("MemTotal:"))
                    .and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok())
                    .map(|kb| kb * 1024)
            })
            .unwrap_or(0)
    }
}

/// Live power/thermal state — `(on_battery, thermal_throttled)`.
/// Called at device registration AND re-sampled by the broker's load
/// probe each announce, so `bcap` hints track the cord.
///
/// - **Windows**: `GetSystemPowerStatus` — `ACLineStatus` 0/1/255 maps
///   to on-battery/AC/unknown.
/// - **Linux**: `/sys/class/power_supply` — a `BAT*` reporting
///   `Discharging` → on battery; an `AC*`/`ADP*`/`Mains*` `online=0`
///   → on battery; supplies present but none discharging → AC.
///   Thermal: any `cpu*/cpufreq` running <80% of `cpuinfo_max_freq`
///   counts as throttled (best-effort).
/// - **Other platforms**: `(None, None)` — callers treat `None` as
///   neutral.
pub fn probe_power() -> (Option<bool>, Option<bool>) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::Power::{GetSystemPowerStatus, SYSTEM_POWER_STATUS};
        let mut st: SYSTEM_POWER_STATUS = unsafe { std::mem::zeroed() };
        let on_battery = if unsafe { GetSystemPowerStatus(&mut st) } != 0 {
            match st.ACLineStatus {
                0 => Some(true),
                1 => Some(false),
                _ => None,
            }
        } else {
            None
        };
        // Thermal throttling needs WMI/perf counters — not worth a
        // subprocess per announce; None = neutral.
        (on_battery, None)
    }
    #[cfg(target_os = "linux")]
    {
        let ps = std::path::Path::new("/sys/class/power_supply");
        let mut on_battery = None;
        if let Ok(entries) = std::fs::read_dir(ps) {
            let mut saw_supply = false;
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                let read = |f: &str| {
                    std::fs::read_to_string(e.path().join(f))
                        .ok()
                        .map(|s| s.trim().to_string())
                };
                if name.starts_with("BAT") {
                    saw_supply = true;
                    if read("status").as_deref() == Some("Discharging") {
                        on_battery = Some(true);
                        break;
                    }
                } else if name.starts_with("AC")
                    || name.starts_with("ADP")
                    || name.starts_with("Mains")
                {
                    saw_supply = true;
                    if read("online").as_deref() == Some("0") {
                        on_battery = Some(true);
                        break;
                    }
                }
            }
            if on_battery.is_none() && saw_supply {
                on_battery = Some(false);
            }
        }
        // Throttled if any core runs <80% of its max frequency.
        let mut throttled = None;
        let cpu = std::path::Path::new("/sys/devices/system/cpu");
        if let Ok(entries) = std::fs::read_dir(cpu) {
            for e in entries.flatten() {
                let freq = e.path().join("cpufreq");
                let (Ok(cur), Ok(max)) = (
                    std::fs::read_to_string(freq.join("scaling_cur_freq")),
                    std::fs::read_to_string(freq.join("cpuinfo_max_freq")),
                ) else {
                    continue;
                };
                if let (Ok(c), Ok(m)) = (cur.trim().parse::<u64>(), max.trim().parse::<u64>()) {
                    if m > 0 && c * 5 < m * 4 {
                        throttled = Some(true);
                        break;
                    }
                    throttled.get_or_insert(false);
                }
            }
        }
        (on_battery, throttled)
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        (None, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn power_probe_returns_sane_options() {
        // Exercises the real OS path (GetSystemPowerStatus here) —
        // can't assert a value, but it must not panic and must agree
        // with the capability probe.
        let (bat, th) = probe_power();
        let caps = probe_capabilities();
        assert_eq!(caps.on_battery, bat);
        assert_eq!(caps.thermal_throttled, th);
        // RAM probe should find real memory on any dev host.
        assert!(caps.ram_bytes > 0);
        eprintln!(
            "probe_power: on_battery={bat:?} thermal={th:?} ram={}",
            caps.ram_bytes
        );
    }

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
