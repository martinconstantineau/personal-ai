//! OS-keystore access for secrets: device signing keys and the store
//! encryption key. Backends: Windows Credential Manager, macOS Keychain,
//! Secret Service (D-Bus) on Linux. Every operation degrades to `None`/`false`
//! when no keystore is reachable (headless Linux, old desktops) so callers can
//! fall back to a 0600 file — never to an error.

#[cfg(not(target_os = "android"))]
const SERVICE: &str = "personal-ai";

fn entry(name: &str) -> Option<keyring::Entry> {
    // Escape hatch for tests and hosts with a broken/full credential
    // vault: PAI_KEYSTORE_OFF=1 makes every op report "no keystore", so
    // callers use their 0600-file fallbacks under data_dir.
    if std::env::var_os("PAI_KEYSTORE_OFF").is_some() {
        return None;
    }
    // keyring's Android backend keeps secrets in process memory — they
    // do not survive a restart, so a store/report-ok/load-miss sequence
    // mints a fresh store.key that cannot decrypt the existing DB.
    // File fallbacks under the app-private data dir are the safe path.
    #[cfg(target_os = "android")]
    {
        let _ = name;
        return None;
    }
    #[cfg(not(target_os = "android"))]
    match keyring::Entry::new(SERVICE, name) {
        Ok(e) => Some(e),
        Err(e) => {
            tracing::debug!(%name, error = %e, "keystore entry unavailable");
            None
        }
    }
}

/// Store `secret` under `name` in the OS keystore. `false` → caller should
/// use the file fallback.
pub fn store(name: &str, secret: &[u8]) -> bool {
    match entry(name).map(|e| e.set_secret(secret)) {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            tracing::debug!(%name, error = %e, "keystore store failed");
            false
        }
        None => false,
    }
}

pub fn load(name: &str) -> Option<Vec<u8>> {
    match entry(name).map(|e| e.get_secret()) {
        Some(Ok(bytes)) => Some(bytes),
        Some(Err(keyring::Error::NoEntry)) => None,
        Some(Err(e)) => {
            tracing::debug!(%name, error = %e, "keystore load failed");
            None
        }
        None => None,
    }
}

pub fn delete(name: &str) {
    if let Some(e) = entry(name) {
        let _ = e.delete_credential();
    }
}

/// The store encryption key (`Some` = open the DB encrypted).
///
/// Resolution order: `PAI_PLAINTEXT_STORE=1` escape hatch → OS keystore →
/// `data_dir/store.key` file (0600). A file key is migrated into the
/// keystore opportunistically; a missing key is generated fresh.
pub fn store_key(data_dir: &std::path::Path) -> Option<[u8; 32]> {
    use rand_core::RngCore;
    if std::env::var_os("PAI_PLAINTEXT_STORE").is_some() {
        return None;
    }
    const NAME: &str = "store-key";
    let file = data_dir.join("store.key");
    if let Some(k) = load(NAME) {
        return k.as_slice().try_into().ok();
    }
    if let Ok(raw) = std::fs::read(&file) {
        if store(NAME, &raw) {
            let _ = std::fs::remove_file(&file);
        }
        return raw.as_slice().try_into().ok();
    }
    let mut k = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut k);
    if !store(NAME, &k) {
        if let Some(parent) = file.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Err(e) = std::fs::write(&file, k) {
            tracing::error!(error = %e, "could not persist store key; store will be unreadable");
            return None;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
        }
        tracing::warn!("OS keystore unavailable; store key written to {file:?}");
    }
    Some(k)
}
