//! Session credentials and the cache master key, kept in the OS keyring
//! (Secret Service) with a mode-0600 file fallback when no keyring is
//! available (DESIGN.md §12.1, §12.4).
//!
//! Adapted from pdcli `credentials.rs` (MIT, © Thribhu K,
//! github.com/tirbofish/proton-sdk-rs2): keyring service `pdmount` instead of
//! `pdcli`, one entry pair per profile, JSON instead of RON, and the file
//! fallback in the profile data directory.

use anyhow::Context;
use pdmount_core::ProfilePaths;
use proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials;
use rand::Rng;
use std::path::Path;

const KEYRING_SERVICE: &str = "pdmount";
const CACHE_KEY_LEN: usize = 32;

/// Where credentials and the cache key are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Storage {
    Keyring,
    File,
}

impl std::fmt::Display for Storage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keyring => "keyring",
            Self::File => "file (keyring unavailable)",
        })
    }
}

/// Keyring access, behind a trait so the file fallback can be tested.
pub trait SecretStore {
    fn get(&self, key: &str) -> Option<String>;
    /// Returns whether the value was stored.
    fn set(&self, key: &str, value: &str) -> bool;
    fn delete(&self, key: &str);
}

/// The desktop keyring (Secret Service on Linux).
pub struct OsKeyring;

/// A locked Secret Service collection waits for an unlock prompt, possibly
/// forever (no desktop, or nobody at the screen). Calls taking longer than
/// this are abandoned and the file fallback is used.
const KEYRING_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Runs a blocking keyring call on its own thread with [`KEYRING_TIMEOUT`].
/// Returns `None` on timeout; the abandoned thread ends with the process.
fn with_keyring_timeout<T: Send + 'static>(
    operation: &'static str,
    call: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    // After one timeout, skip the keyring for the rest of the process.
    static UNRESPONSIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if UNRESPONSIVE.load(std::sync::atomic::Ordering::Relaxed) {
        return None;
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    match receiver.recv_timeout(KEYRING_TIMEOUT) {
        Ok(value) => Some(value),
        Err(_) => {
            UNRESPONSIVE.store(true, std::sync::atomic::Ordering::Relaxed);
            tracing::warn!(
                operation,
                "the keyring did not respond within {} s (locked?); using file storage",
                KEYRING_TIMEOUT.as_secs()
            );
            None
        }
    }
}

impl SecretStore for OsKeyring {
    fn get(&self, key: &str) -> Option<String> {
        let key = key.to_owned();
        with_keyring_timeout("read", move || {
            keyring::Entry::new(KEYRING_SERVICE, &key)
                .ok()?
                .get_password()
                .ok()
        })
        .flatten()
    }

    fn set(&self, key: &str, value: &str) -> bool {
        let (key, value) = (key.to_owned(), value.to_owned());
        with_keyring_timeout("write", move || {
            keyring::Entry::new(KEYRING_SERVICE, &key)
                .and_then(|entry| entry.set_password(&value))
                .is_ok()
        })
        .unwrap_or(false)
    }

    fn delete(&self, key: &str) {
        let key = key.to_owned();
        with_keyring_timeout("delete", move || {
            if let Ok(entry) = keyring::Entry::new(KEYRING_SERVICE, &key) {
                let _ = entry.delete_credential();
            }
        });
    }
}

/// Probes whether the keyring can be reached at all.
pub fn keyring_available() -> Result<(), String> {
    let probe = with_keyring_timeout("probe", || {
        match keyring::Entry::new(KEYRING_SERVICE, "doctor-probe").and_then(|e| e.get_password()) {
            Ok(_) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(error) => Err(error.to_string()),
        }
    });
    probe
        .unwrap_or_else(|| Err("no response (locked keyring waiting for an unlock prompt?)".into()))
}

pub struct Credentials<'a, S: SecretStore> {
    paths: &'a ProfilePaths,
    store: S,
}

impl<'a> Credentials<'a, OsKeyring> {
    pub fn new(paths: &'a ProfilePaths) -> Self {
        Self {
            paths,
            store: OsKeyring,
        }
    }
}

impl<'a, S: SecretStore> Credentials<'a, S> {
    #[cfg(test)]
    pub fn with_store(paths: &'a ProfilePaths, store: S) -> Self {
        Self { paths, store }
    }

    fn session_key(&self) -> String {
        format!("{}:session", self.paths.profile)
    }

    fn cache_key_name(&self) -> String {
        format!("{}:cache-key", self.paths.profile)
    }

    /// Stored session, if any. A session found in the fallback file is moved
    /// into the keyring when the keyring has become available.
    pub fn load(&self) -> anyhow::Result<Option<(StoredCredentials, Storage)>> {
        if let Some(data) = self.store.get(&self.session_key()) {
            return Ok(Some((parse(&data)?, Storage::Keyring)));
        }
        let path = self.paths.credentials_file();
        let data = match std::fs::read_to_string(&path) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        };
        let credentials = parse(&data)?;
        if self.store.set(&self.session_key(), &data) {
            let _ = std::fs::remove_file(&path);
            tracing::info!("moved stored session from file to keyring");
            return Ok(Some((credentials, Storage::Keyring)));
        }
        Ok(Some((credentials, Storage::File)))
    }

    pub fn save(&self, credentials: &StoredCredentials) -> anyhow::Result<Storage> {
        let data = serde_json::to_string(credentials)?;
        let path = self.paths.credentials_file();
        if self.store.set(&self.session_key(), &data) {
            let _ = std::fs::remove_file(&path);
            return Ok(Storage::Keyring);
        }
        write_private_file(&path, data.as_bytes())?;
        Ok(Storage::File)
    }

    /// Removes the session and the cache key from both locations.
    pub fn remove(&self) {
        self.store.delete(&self.session_key());
        self.store.delete(&self.cache_key_name());
        let _ = std::fs::remove_file(self.paths.credentials_file());
        let _ = std::fs::remove_file(self.paths.cache_key_file());
    }

    /// 32-byte key encrypting the SDK caches; created on first use.
    pub fn cache_master_key(&self) -> anyhow::Result<Vec<u8>> {
        if let Some(encoded) = self.store.get(&self.cache_key_name()) {
            return decode_key(&encoded);
        }
        let path = self.paths.cache_key_file();
        match std::fs::read(&path) {
            Ok(key) if key.len() == CACHE_KEY_LEN => {
                if self.store.set(&self.cache_key_name(), &encode_key(&key)) {
                    let _ = std::fs::remove_file(&path);
                }
                return Ok(key);
            }
            Ok(_) => anyhow::bail!("invalid cache key in {}", path.display()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error).with_context(|| format!("cannot read {}", path.display()));
            }
        }
        let mut key = vec![0u8; CACHE_KEY_LEN];
        rand::rng().fill_bytes(&mut key);
        if !self.store.set(&self.cache_key_name(), &encode_key(&key)) {
            write_private_file(&path, &key)?;
        }
        Ok(key)
    }
}

fn parse(data: &str) -> anyhow::Result<StoredCredentials> {
    serde_json::from_str(data).context("stored session is corrupt; run `pdmount login` again")
}

fn encode_key(key: &[u8]) -> String {
    key.iter().map(|b| format!("{b:02x}")).collect()
}

fn decode_key(encoded: &str) -> anyhow::Result<Vec<u8>> {
    if encoded.len() != CACHE_KEY_LEN * 2 || !encoded.is_ascii() {
        anyhow::bail!("invalid cache key in keyring");
    }
    (0..CACHE_KEY_LEN)
        .map(|i| u8::from_str_radix(&encoded[i * 2..i * 2 + 2], 16))
        .collect::<Result<_, _>>()
        .context("invalid cache key in keyring")
}

/// Writes `data` to a new file readable only by the owner.
pub fn write_private_file(path: &Path, data: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    if let Some(parent) = path.parent() {
        create_private_dir(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("cannot write {}", tmp.display()))?;
    file.write_all(data)?;
    file.sync_all()?;
    std::fs::rename(&tmp, path).with_context(|| format!("cannot write {}", path.display()))?;
    Ok(())
}

/// Creates a directory (and parents) with mode 0700 for the last component.
pub fn create_private_dir(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path).with_context(|| format!("cannot create {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
        .with_context(|| format!("cannot restrict {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_core::paths::BaseDirs;
    use std::cell::RefCell;
    use std::collections::HashMap;

    /// In-memory keyring; `available = false` simulates a missing keyring.
    #[derive(Default)]
    struct MemoryStore {
        available: bool,
        entries: RefCell<HashMap<String, String>>,
    }

    impl SecretStore for &MemoryStore {
        fn get(&self, key: &str) -> Option<String> {
            self.entries.borrow().get(key).cloned()
        }
        fn set(&self, key: &str, value: &str) -> bool {
            if self.available {
                self.entries.borrow_mut().insert(key.into(), value.into());
            }
            self.available
        }
        fn delete(&self, key: &str) {
            self.entries.borrow_mut().remove(key);
        }
    }

    fn paths(dir: &Path) -> ProfilePaths {
        ProfilePaths::new(
            "test".parse().expect("profile"),
            &BaseDirs {
                config: dir.join("config"),
                data: dir.join("data"),
                cache: dir.join("cache"),
                state: dir.join("state"),
                runtime: None,
            },
        )
    }

    fn sample() -> StoredCredentials {
        StoredCredentials::new(
            "sid".into(),
            "user@example.test".into(),
            "uid".into(),
            "access".into(),
            "refresh".into(),
            vec!["full".into()],
            false,
            proton_drive_sdk::proton_sdk_rs2::PasswordMode::Single,
        )
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    #[test]
    fn keyring_is_preferred_when_available() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore {
            available: true,
            ..Default::default()
        };
        let credentials = Credentials::with_store(&paths, &store);
        assert_eq!(credentials.save(&sample()).expect("save"), Storage::Keyring);
        assert!(!paths.credentials_file().exists());
        let (loaded, storage) = credentials.load().expect("load").expect("present");
        assert_eq!(storage, Storage::Keyring);
        assert_eq!(loaded.username(), "user@example.test");
        assert!(store.entries.borrow().contains_key("test:session"));
    }

    #[test]
    fn file_fallback_is_private_and_migrates_to_the_keyring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let missing = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &missing);
        assert_eq!(credentials.save(&sample()).expect("save"), Storage::File);
        assert_eq!(mode(&paths.credentials_file()), 0o600);
        assert_eq!(mode(&paths.data_dir), 0o700);
        assert_eq!(
            credentials.load().expect("load").map(|(_, s)| s),
            Some(Storage::File)
        );

        let available = MemoryStore {
            available: true,
            ..Default::default()
        };
        let migrated = Credentials::with_store(&paths, &available);
        assert_eq!(
            migrated.load().expect("load").map(|(_, s)| s),
            Some(Storage::Keyring)
        );
        assert!(!paths.credentials_file().exists());
    }

    #[test]
    fn missing_and_removed_sessions_load_as_none() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &store);
        assert!(credentials.load().expect("load").is_none());
        credentials.save(&sample()).expect("save");
        credentials.remove();
        assert!(credentials.load().expect("load").is_none());
    }

    #[test]
    fn corrupt_session_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        write_private_file(&paths.credentials_file(), b"{not json").expect("write");
        let store = MemoryStore::default();
        assert!(Credentials::with_store(&paths, &store).load().is_err());
    }

    #[test]
    fn cache_key_is_stable_per_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &store);
        let first = credentials.cache_master_key().expect("create");
        assert_eq!(first.len(), 32);
        assert_eq!(mode(&paths.cache_key_file()), 0o600);
        assert_eq!(credentials.cache_master_key().expect("read"), first);
        assert_eq!(decode_key(&encode_key(&first)).expect("decode"), first);
        assert!(decode_key("zz").is_err());
    }
}
