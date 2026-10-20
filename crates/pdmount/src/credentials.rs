//! Session credentials and the cache master key, kept in the OS keyring
//! (Secret Service), or in mode-0600 files when a flag allows it
//! (DESIGN.md §12.1, §12.4; D029).
//!
//! Adapted from pdcli `credentials.rs` (MIT, © Thribhu K,
//! github.com/tirbofish/proton-sdk-rs2): keyring service `pdmount` instead of
//! `pdcli`, one entry pair per profile, JSON instead of RON, and the files
//! in the profile data directory.

use anyhow::Context;
use pdmount_core::ProfilePaths;
use proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials;
use rand::Rng;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const KEYRING_SERVICE: &str = "pdmount";
const CACHE_KEY_LEN: usize = 32;

/// Where secrets may be stored, from the global flags (D029).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SecretsMode {
    /// No flag: the keyring only.
    #[default]
    Keyring,
    /// `--allow-file-secrets`: files when the keyring is unavailable.
    AllowFile,
    /// `--force-file-secrets`: files only; the keyring is never called.
    ForceFile,
}

impl SecretsMode {
    /// The flag selecting this mode, for re-executing `pdmount`.
    pub fn flag(self) -> Option<&'static str> {
        match self {
            Self::Keyring => None,
            Self::AllowFile => Some("--allow-file-secrets"),
            Self::ForceFile => Some("--force-file-secrets"),
        }
    }
}

impl std::fmt::Display for SecretsMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keyring => "keyring only",
            Self::AllowFile => "keyring, files if unavailable (--allow-file-secrets)",
            Self::ForceFile => "files only (--force-file-secrets)",
        })
    }
}

static MODE: OnceLock<SecretsMode> = OnceLock::new();

/// Sets the process-wide mode; called once, from the command line.
pub fn set_mode(mode: SecretsMode) {
    let _ = MODE.set(mode);
}

pub fn mode() -> SecretsMode {
    MODE.get().copied().unwrap_or_default()
}

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
            Self::File => "file",
        })
    }
}

/// The keyring could not answer: no Secret Service, a dismissed unlock
/// prompt, a timeout, or another error. Never the same as "no entry".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unavailable(pub String);

impl std::fmt::Display for Unavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the keyring is unavailable ({}); unlock it and retry, or pass --allow-file-secrets \
             (files when the keyring is unavailable) or --force-file-secrets (files only)",
            self.0
        )
    }
}

impl std::error::Error for Unavailable {}

/// Keyring access, behind a trait so the modes can be tested.
pub trait SecretStore {
    /// `Ok(None)` only when the entry does not exist.
    fn get(&self, key: &str) -> Result<Option<String>, Unavailable>;
    fn set(&self, key: &str, value: &str) -> Result<(), Unavailable>;
    /// Deleting a missing entry succeeds.
    fn delete(&self, key: &str) -> Result<(), Unavailable>;
}

/// The desktop keyring (Secret Service on Linux).
pub struct OsKeyring;

/// A locked Secret Service collection waits for its unlock prompt, possibly
/// forever (`dbus-secret-service` waits up to a year). Calls taking longer
/// are abandoned.
const KEYRING_TIMEOUT: Duration = Duration::from_secs(120);
/// After this long, tell the user that a dialog may be waiting.
const KEYRING_NOTICE_AFTER: Duration = Duration::from_secs(2);

/// Set once the keyring was unavailable: the process stops calling it.
static UNAVAILABLE: AtomicBool = AtomicBool::new(false);

/// Lets the next keyring call through after an earlier failure (the
/// daemon's last token save at shutdown).
pub fn retry_keyring() {
    UNAVAILABLE.store(false, Ordering::Relaxed);
}

/// `PDMOUNT_KEYRING_TIMEOUT_SECS` shortens the timeout for the keyring tests.
fn keyring_timeout() -> Duration {
    std::env::var("PDMOUNT_KEYRING_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(KEYRING_TIMEOUT, Duration::from_secs)
}

/// Runs a blocking keyring call on its own thread with a timeout; the
/// abandoned thread ends with the process.
fn with_keyring_timeout<T: Send + 'static>(
    operation: &'static str,
    call: impl FnOnce() -> Result<T, keyring::Error> + Send + 'static,
) -> Result<T, Unavailable> {
    if UNAVAILABLE.load(Ordering::Relaxed) {
        return Err(Unavailable(
            "it did not answer earlier in this process".into(),
        ));
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    let timeout = keyring_timeout();
    let answer = match receiver.recv_timeout(KEYRING_NOTICE_AFTER.min(timeout)) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let note = "Waiting for the keyring: an unlock dialog may be open, possibly behind other windows…";
            if std::io::IsTerminal::is_terminal(&std::io::stderr()) {
                eprintln!("{note}");
            } else {
                tracing::warn!(operation, "{note}");
            }
            receiver.recv_timeout(timeout.saturating_sub(KEYRING_NOTICE_AFTER))
        }
        answer => answer,
    };
    let error = match answer {
        Ok(Ok(value)) => return Ok(value),
        Ok(Err(error)) => error.to_string(),
        Err(_) => format!("no answer within {} s", timeout.as_secs()),
    };
    UNAVAILABLE.store(true, Ordering::Relaxed);
    tracing::warn!(operation, error, "keyring unavailable");
    Err(Unavailable(error))
}

impl SecretStore for OsKeyring {
    fn get(&self, key: &str) -> Result<Option<String>, Unavailable> {
        let key = key.to_owned();
        with_keyring_timeout("read", move || {
            match keyring::Entry::new(KEYRING_SERVICE, &key)?.get_password() {
                Ok(value) => Ok(Some(value)),
                Err(keyring::Error::NoEntry) => Ok(None),
                Err(error) => Err(error),
            }
        })
    }

    fn set(&self, key: &str, value: &str) -> Result<(), Unavailable> {
        let (key, value) = (key.to_owned(), value.to_owned());
        with_keyring_timeout("write", move || {
            keyring::Entry::new(KEYRING_SERVICE, &key)?.set_password(&value)
        })
    }

    fn delete(&self, key: &str) -> Result<(), Unavailable> {
        let key = key.to_owned();
        with_keyring_timeout("delete", move || {
            match keyring::Entry::new(KEYRING_SERVICE, &key)?.delete_credential() {
                Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
                Err(error) => Err(error),
            }
        })
    }
}

/// State of the default keyring collection, read without prompting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyringState {
    Unlocked,
    /// The next secret read or write opens an unlock dialog.
    Locked,
    Unavailable(String),
}

/// Probes the Secret Service's default collection (for `doctor`).
pub fn keyring_state() -> KeyringState {
    let probe = with_keyring_timeout("probe", || {
        use dbus_secret_service::{EncryptionType, SecretService};
        let platform =
            |error: dbus_secret_service::Error| keyring::Error::PlatformFailure(error.into());
        let service = SecretService::connect(EncryptionType::Plain).map_err(platform)?;
        service
            .get_default_collection()
            .and_then(|collection| collection.is_locked())
            .map_err(platform)
    });
    // The probe must not make later commands of this process skip the keyring.
    retry_keyring();
    match probe {
        Ok(false) => KeyringState::Unlocked,
        Ok(true) => KeyringState::Locked,
        Err(Unavailable(reason)) => KeyringState::Unavailable(reason),
    }
}

/// The two secrets of a profile.
#[derive(Debug, Clone, Copy)]
enum Secret {
    Session,
    CacheKey,
}

pub struct Credentials<'a, S: SecretStore> {
    paths: &'a ProfilePaths,
    store: S,
    mode: SecretsMode,
    /// Another daemon of this profile runs: files are neither moved into the
    /// keyring nor caches deleted under it.
    other_daemon: bool,
}

impl<'a> Credentials<'a, OsKeyring> {
    pub fn new(paths: &'a ProfilePaths) -> Self {
        Self {
            paths,
            store: OsKeyring,
            mode: mode(),
            other_daemon: crate::daemon::daemon_pid(paths).is_some(),
        }
    }
}

impl<'a, S: SecretStore> Credentials<'a, S> {
    #[cfg(test)]
    pub fn with_store(paths: &'a ProfilePaths, store: S, mode: SecretsMode) -> Self {
        Self {
            paths,
            store,
            mode,
            other_daemon: false,
        }
    }

    /// Reads without moving files into the keyring (for `doctor`).
    pub fn read_only(mut self) -> Self {
        self.other_daemon = true;
        self
    }

    fn key_name(&self, secret: Secret) -> String {
        match secret {
            Secret::Session => format!("{}:session", self.paths.profile),
            Secret::CacheKey => format!("{}:cache-key", self.paths.profile),
        }
    }

    fn file(&self, secret: Secret) -> PathBuf {
        match secret {
            Secret::Session => self.paths.credentials_file(),
            Secret::CacheKey => self.paths.cache_key_file(),
        }
    }

    /// The keyring stores text: the session JSON, or the hex cache key.
    fn from_file(secret: Secret, path: &Path, bytes: Vec<u8>) -> anyhow::Result<String> {
        match secret {
            Secret::Session => String::from_utf8(bytes)
                .with_context(|| format!("{} is not valid UTF-8", path.display())),
            Secret::CacheKey if bytes.len() == CACHE_KEY_LEN => Ok(encode_key(&bytes)),
            Secret::CacheKey => anyhow::bail!("invalid cache key in {}", path.display()),
        }
    }

    fn to_file(secret: Secret, value: &str) -> anyhow::Result<Vec<u8>> {
        match secret {
            Secret::Session => Ok(value.as_bytes().to_vec()),
            Secret::CacheKey => decode_key(value),
        }
    }

    /// The file, if present, is the newest copy: every keyring write deletes
    /// it. Outside force mode it is moved into the keyring.
    fn read(&self, secret: Secret) -> anyhow::Result<Option<(String, Storage)>> {
        let path = self.file(secret);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let value = Self::from_file(secret, &path, bytes)?;
                if self.mode == SecretsMode::ForceFile || self.other_daemon {
                    return Ok(Some((value, Storage::File)));
                }
                match self.store.set(&self.key_name(secret), &value) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&path);
                        tracing::info!(file = %path.display(), "moved secret from file to keyring");
                        Ok(Some((value, Storage::Keyring)))
                    }
                    Err(error) if self.mode == SecretsMode::Keyring => Err(error.into()),
                    Err(_) => Ok(Some((value, Storage::File))),
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.mode == SecretsMode::ForceFile {
                    return Ok(None);
                }
                match self.store.get(&self.key_name(secret)) {
                    Ok(value) => Ok(value.map(|value| (value, Storage::Keyring))),
                    Err(error) if self.mode == SecretsMode::Keyring => Err(error.into()),
                    Err(error) => {
                        tracing::warn!(%error, "reading secrets from files only");
                        Ok(None)
                    }
                }
            }
            Err(error) => Err(error).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    fn write(&self, secret: Secret, value: &str) -> anyhow::Result<Storage> {
        let path = self.file(secret);
        if self.mode != SecretsMode::ForceFile {
            match self.store.set(&self.key_name(secret), value) {
                Ok(()) => {
                    let _ = std::fs::remove_file(&path);
                    return Ok(Storage::Keyring);
                }
                Err(error) if self.mode == SecretsMode::Keyring => return Err(error.into()),
                Err(error) => {
                    tracing::warn!(%error, file = %path.display(), "storing secret in a file")
                }
            }
        }
        write_private_file(&path, &Self::to_file(secret, value)?)?;
        Ok(Storage::File)
    }

    /// Stored session, if any.
    pub fn load(&self) -> anyhow::Result<Option<(StoredCredentials, Storage)>> {
        self.read(Secret::Session)?
            .map(|(data, storage)| Ok((parse(&data)?, storage)))
            .transpose()
    }

    pub fn save(&self, credentials: &StoredCredentials) -> anyhow::Result<Storage> {
        self.write(Secret::Session, &serde_json::to_string(credentials)?)
    }

    /// Removes the session and the cache key from the files and, outside
    /// force mode, from the keyring. Fails if a keyring entry may remain.
    pub fn remove(&self) -> Result<(), Unavailable> {
        let mut result = Ok(());
        for secret in [Secret::Session, Secret::CacheKey] {
            let _ = std::fs::remove_file(self.file(secret));
            if self.mode != SecretsMode::ForceFile {
                result = result.and(self.store.delete(&self.key_name(secret)));
            }
        }
        result
    }

    /// 32-byte key encrypting the SDK caches and the block cache; created on
    /// first use. A new key makes the existing caches unreadable, so they are
    /// deleted.
    pub fn cache_master_key(&self) -> anyhow::Result<Vec<u8>> {
        if let Some((encoded, _)) = self.read(Secret::CacheKey)? {
            return decode_key(&encoded);
        }
        let mut key = vec![0u8; CACHE_KEY_LEN];
        rand::rng().fill_bytes(&mut key);
        self.delete_caches();
        self.write(Secret::CacheKey, &encode_key(&key))?;
        Ok(key)
    }

    fn delete_caches(&self) {
        if self.other_daemon {
            tracing::warn!("new cache key while another daemon runs; its caches are kept");
            return;
        }
        let mut deleted = false;
        for cache in [self.paths.sdk_secret_cache(), self.paths.sdk_entity_cache()] {
            for suffix in ["", "-wal", "-shm"] {
                let mut path = cache.clone().into_os_string();
                path.push(suffix);
                deleted |= std::fs::remove_file(path).is_ok();
            }
        }
        deleted |= std::fs::remove_dir_all(self.paths.blocks_dir()).is_ok();
        if deleted {
            tracing::warn!("new cache key: deleted the caches encrypted with the old one");
        }
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
    use std::cell::{Cell, RefCell};
    use std::collections::HashMap;

    /// In-memory keyring; `available = false` simulates an unavailable one
    /// (no Secret Service, or a dismissed unlock prompt).
    #[derive(Default)]
    struct MemoryStore {
        available: bool,
        entries: RefCell<HashMap<String, String>>,
        calls: Cell<usize>,
    }

    impl MemoryStore {
        fn available() -> Self {
            Self {
                available: true,
                ..Default::default()
            }
        }

        fn check(&self) -> Result<(), Unavailable> {
            self.calls.set(self.calls.get() + 1);
            if self.available {
                Ok(())
            } else {
                Err(Unavailable("unlock prompt was dismissed".into()))
            }
        }
    }

    impl SecretStore for &MemoryStore {
        fn get(&self, key: &str) -> Result<Option<String>, Unavailable> {
            self.check()?;
            Ok(self.entries.borrow().get(key).cloned())
        }
        fn set(&self, key: &str, value: &str) -> Result<(), Unavailable> {
            self.check()?;
            self.entries.borrow_mut().insert(key.into(), value.into());
            Ok(())
        }
        fn delete(&self, key: &str) -> Result<(), Unavailable> {
            self.check()?;
            self.entries.borrow_mut().remove(key);
            Ok(())
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

    fn sample_for(username: &str) -> StoredCredentials {
        StoredCredentials::new(
            "sid".into(),
            username.into(),
            "uid".into(),
            "access".into(),
            "refresh".into(),
            vec!["full".into()],
            false,
            proton_drive_sdk::proton_sdk_rs2::PasswordMode::Single,
        )
    }

    fn sample() -> StoredCredentials {
        sample_for("user@example.test")
    }

    fn mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    }

    fn storage_of(credentials: &Credentials<'_, &MemoryStore>) -> Option<Storage> {
        credentials
            .load()
            .expect("load")
            .map(|(_, storage)| storage)
    }

    #[test]
    fn keyring_mode_uses_the_keyring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::Keyring);
        assert_eq!(credentials.save(&sample()).expect("save"), Storage::Keyring);
        assert!(!paths.credentials_file().exists());
        let (loaded, storage) = credentials.load().expect("load").expect("present");
        assert_eq!(storage, Storage::Keyring);
        assert_eq!(loaded.username(), "user@example.test");
        assert!(store.entries.borrow().contains_key("test:session"));
    }

    #[test]
    fn keyring_mode_fails_instead_of_using_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::Keyring);
        let error = credentials.save(&sample()).expect_err("save must fail");
        assert!(format!("{error:#}").contains("--allow-file-secrets"));
        assert!(!paths.credentials_file().exists());
        // A dismissed prompt is not "not logged in", and creates no cache key.
        assert!(credentials.load().is_err());
        assert!(credentials.cache_master_key().is_err());
        assert!(!paths.cache_key_file().exists());
    }

    #[test]
    fn allow_mode_falls_back_and_migrates_later() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let unavailable = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &unavailable, SecretsMode::AllowFile);
        assert_eq!(credentials.save(&sample()).expect("save"), Storage::File);
        assert_eq!(mode(&paths.credentials_file()), 0o600);
        assert_eq!(mode(&paths.data_dir), 0o700);
        assert_eq!(storage_of(&credentials), Some(Storage::File));

        // A later run without a flag moves the file into the keyring.
        let available = MemoryStore::available();
        let migrated = Credentials::with_store(&paths, &available, SecretsMode::Keyring);
        assert_eq!(storage_of(&migrated), Some(Storage::Keyring));
        assert!(!paths.credentials_file().exists());
    }

    #[test]
    fn keyring_mode_with_a_file_and_no_keyring_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let available = MemoryStore::available();
        let forced = Credentials::with_store(&paths, &available, SecretsMode::ForceFile);
        forced.save(&sample()).expect("save");
        let unavailable = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &unavailable, SecretsMode::Keyring);
        assert!(credentials.load().is_err());
        assert!(paths.credentials_file().exists());
    }

    #[test]
    fn force_mode_never_calls_the_keyring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::ForceFile);
        assert_eq!(credentials.save(&sample()).expect("save"), Storage::File);
        assert_eq!(storage_of(&credentials), Some(Storage::File));
        let key = credentials.cache_master_key().expect("cache key");
        assert_eq!(credentials.cache_master_key().expect("cache key"), key);
        credentials.remove().expect("remove");
        assert!(credentials.load().expect("load").is_none());
        assert_eq!(store.calls.get(), 0);
    }

    #[test]
    fn the_file_is_newer_than_the_keyring() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        Credentials::with_store(&paths, &store, SecretsMode::Keyring)
            .save(&sample_for("old@example.test"))
            .expect("save");
        Credentials::with_store(&paths, &store, SecretsMode::ForceFile)
            .save(&sample_for("new@example.test"))
            .expect("save");
        let (loaded, storage) = Credentials::with_store(&paths, &store, SecretsMode::Keyring)
            .load()
            .expect("load")
            .expect("present");
        assert_eq!(
            (loaded.username(), storage),
            ("new@example.test", Storage::Keyring)
        );
        assert!(!paths.credentials_file().exists());
        assert!(store.entries.borrow()["test:session"].contains("new@example.test"));
    }

    #[test]
    fn files_stay_while_another_daemon_runs() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        Credentials::with_store(&paths, &store, SecretsMode::ForceFile)
            .save(&sample())
            .expect("save");
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::Keyring).read_only();
        assert_eq!(storage_of(&credentials), Some(Storage::File));
        assert!(paths.credentials_file().exists());
        assert_eq!(store.calls.get(), 0);
    }

    #[test]
    fn a_new_cache_key_deletes_the_old_caches() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let block = paths.blocks_dir().join("00").join("block");
        std::fs::create_dir_all(block.parent().expect("parent")).expect("mkdir");
        std::fs::write(&block, b"sealed").expect("write");
        std::fs::create_dir_all(&paths.data_dir).expect("mkdir");
        std::fs::write(paths.sdk_secret_cache(), b"sealed").expect("write");
        let store = MemoryStore::available();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::Keyring);
        let key = credentials.cache_master_key().expect("create");
        assert!(!block.exists());
        assert!(!paths.sdk_secret_cache().exists());

        // An existing key keeps the caches.
        std::fs::write(paths.sdk_secret_cache(), b"sealed").expect("write");
        assert_eq!(credentials.cache_master_key().expect("read"), key);
        assert!(paths.sdk_secret_cache().exists());
    }

    #[test]
    fn remove_reports_keyring_entries_it_could_not_delete() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::default();
        Credentials::with_store(&paths, &store, SecretsMode::AllowFile)
            .save(&sample())
            .expect("save");
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::AllowFile);
        assert!(credentials.remove().is_err());
        assert!(!paths.credentials_file().exists());
        assert!(credentials.load().expect("load").is_none());
    }

    #[test]
    fn corrupt_session_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        write_private_file(&paths.credentials_file(), b"{not json").expect("write");
        let store = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::ForceFile);
        assert!(credentials.load().is_err());
    }

    #[test]
    fn cache_key_is_stable_per_profile() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::ForceFile);
        let first = credentials.cache_master_key().expect("create");
        assert_eq!(first.len(), 32);
        assert_eq!(mode(&paths.cache_key_file()), 0o600);
        assert_eq!(credentials.cache_master_key().expect("read"), first);
        assert_eq!(decode_key(&encode_key(&first)).expect("decode"), first);
        assert!(decode_key("zz").is_err());
    }

    /// Keyring tests against a real Secret Service (D024, D029), driven by
    /// `scripts/repro/keyring-d024.sh` inside a private D-Bus session.
    /// Environment: `PDMOUNT_REPRO_DIR` (profile directory),
    /// `PDMOUNT_REPRO_MODE` (`keyring`, `allow` or `force`) and, for `save`,
    /// `PDMOUNT_REPRO_USER`. Results are printed as `REPRO key=value` lines.
    mod repro_d024 {
        use super::*;
        use std::time::Instant;

        fn repro_paths() -> ProfilePaths {
            let dir = std::env::var_os("PDMOUNT_REPRO_DIR").expect("PDMOUNT_REPRO_DIR is not set");
            let _ = tracing_subscriber::fmt()
                .with_writer(std::io::stderr)
                .with_ansi(false)
                .try_init();
            set_mode(match std::env::var("PDMOUNT_REPRO_MODE").as_deref() {
                Ok("allow") => SecretsMode::AllowFile,
                Ok("force") => SecretsMode::ForceFile,
                _ => SecretsMode::Keyring,
            });
            paths(Path::new(&dir))
        }

        fn report<T: std::fmt::Display>(
            name: &str,
            start: Instant,
            result: anyhow::Result<T>,
            paths: &ProfilePaths,
        ) {
            let outcome = match result {
                Ok(value) => value.to_string(),
                Err(error) => format!("error({error:#})"),
            };
            println!(
                "REPRO {name}={outcome} elapsed_s={} session_file={} key_file={}",
                start.elapsed().as_secs(),
                paths.credentials_file().exists(),
                paths.cache_key_file().exists()
            );
        }

        /// The keyring crate alone, without PdMount's timeout.
        #[test]
        #[ignore = "needs the D024 harness"]
        fn raw_set_password() {
            let start = Instant::now();
            let result = keyring::Entry::new("pdmount-repro", "raw")
                .and_then(|entry| entry.set_password("throwaway"));
            println!(
                "REPRO raw_result={:?} elapsed_s={}",
                result.map_err(|e| e.to_string()),
                start.elapsed().as_secs()
            );
        }

        #[test]
        #[ignore = "needs the D024 harness"]
        fn save() {
            let paths = repro_paths();
            let user =
                std::env::var("PDMOUNT_REPRO_USER").unwrap_or_else(|_| "user@example.test".into());
            let start = Instant::now();
            let result = Credentials::new(&paths).save(&sample_for(&user));
            report("save", start, result, &paths);
        }

        #[test]
        #[ignore = "needs the D024 harness"]
        fn load() {
            let paths = repro_paths();
            let start = Instant::now();
            let result = Credentials::new(&paths).load().map(|loaded| {
                loaded.map_or_else(
                    || "none".to_owned(),
                    |(stored, storage)| format!("{}@{storage}", stored.username()),
                )
            });
            report("load", start, result, &paths);
        }

        /// Prints the first bytes of the (throwaway) cache key, to show
        /// whether repeated runs see the same key.
        #[test]
        #[ignore = "needs the D024 harness"]
        fn cache_key() {
            let paths = repro_paths();
            let start = Instant::now();
            let result = Credentials::new(&paths)
                .cache_master_key()
                .map(|key| encode_key(&key[..4]));
            report("cache_key", start, result, &paths);
        }

        #[test]
        #[ignore = "needs the D024 harness"]
        fn remove() {
            let paths = repro_paths();
            let start = Instant::now();
            let result = Credentials::new(&paths)
                .remove()
                .map(|()| "ok")
                .map_err(anyhow::Error::from);
            report("remove", start, result, &paths);
        }
    }
}
