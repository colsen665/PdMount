//! Proton session: web sign-in, resume from stored credentials, logout
//! (DESIGN.md §12).

use crate::credentials::{self, Credentials, Storage};
use crate::secret_cache::SecretCache;
use anyhow::Context;
use pdmount_backend::BackendError;
use pdmount_backend::proton::ProtonBackend;
use pdmount_core::config::BrowserMode;
use pdmount_core::device::{self, DeviceIdentity};
use pdmount_core::{Config, ProfilePaths};
use proton_drive_sdk::cache::encrypted::EncryptedCacheRepository;
use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
use proton_drive_sdk::client::ProtonDriveClient;
use proton_drive_sdk::proton_sdk_rs2::cache::CacheRepository;
use proton_drive_sdk::proton_sdk_rs2::client::ProtonClientOptions;
use proton_drive_sdk::proton_sdk_rs2::session::ProtonAPISession;
use proton_drive_sdk::proton_sdk_rs2::{AppVersionConfiguration, ReleaseChannel};
use std::io::{IsTerminal, Write};
use std::process::{Command, Stdio};
use std::sync::Arc;

pub const THIRD_PARTY_NOTICE: &str =
    "PdMount is a third-party application, not officially supported by Proton.";

/// `x-pm-appversion` identity: `external-drive-pdmount@<version>-alpha`.
pub fn app_version() -> AppVersionConfiguration {
    let mut parts = env!("CARGO_PKG_VERSION")
        .split('.')
        .map(|part| part.parse().unwrap_or(0));
    let mut next = || parts.next().unwrap_or(0);
    AppVersionConfiguration::new("pdmount", next(), next(), next())
        .with_channel(ReleaseChannel::Alpha)
}

/// Opens the encrypted SDK entity and secret caches of a profile.
fn client_options(
    paths: &ProfilePaths,
    credentials: &Credentials<'_, credentials::OsKeyring>,
) -> anyhow::Result<ProtonClientOptions> {
    let key = credentials.cache_master_key()?;
    let open = |path: &std::path::Path,
                capacity: Option<usize>|
     -> anyhow::Result<Arc<dyn CacheRepository>> {
        if let Some(parent) = path.parent() {
            credentials::create_private_dir(parent)?;
        }
        let sqlite = SqliteCacheRepository::open_file(path, capacity)
            .with_context(|| format!("cannot open cache {}", path.display()))?;
        restrict_to_owner(path);
        Ok(Arc::new(EncryptedCacheRepository::new(
            Arc::new(sqlite),
            key.clone(),
        )))
    };
    let secrets = open(&paths.sdk_secret_cache(), Some(5_000))?;
    let keys = open(&paths.sdk_key_store(), None)?;
    Ok(ProtonClientOptions {
        entity_cache_repository: Some(open(&paths.sdk_entity_cache(), Some(10_000))?),
        secret_cache_repository: Some(Arc::new(SecretCache::new(secrets, keys))),
        ..Default::default()
    })
}

/// Sets mode 0600 on a cache database and its SQLite side files.
fn restrict_to_owner(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    for file in pdmount_core::paths::sqlite_files(path) {
        let _ = std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600));
    }
}

/// Whether to open the sign-in link in a browser (DESIGN.md §12.2). The
/// link is printed in every case.
pub fn should_open_browser(mode: BrowserMode, interactive: bool, graphical: bool) -> bool {
    match mode {
        BrowserMode::Always => true,
        BrowserMode::Never => false,
        BrowserMode::Auto => interactive && graphical,
    }
}

fn interactive() -> bool {
    std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

fn graphical() -> bool {
    ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|value| !value.is_empty()))
}

fn open_in_browser(url: &str) -> std::io::Result<()> {
    Command::new("xdg-open")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(drop)
}

/// Text shown for a sign-in link.
pub fn sign_in_prompt(url: &str, code: &str) -> String {
    let mut text = format!("Open this link to sign in:  {url}\n");
    if !code.is_empty() {
        text.push_str(&format!("Code: {code}\n"));
    }
    text
}

pub fn present_link(url: &str, code: &str, browser: BrowserMode) {
    let mut out = std::io::stdout().lock();
    let _ = write!(out, "{}", sign_in_prompt(url, code));
    if should_open_browser(browser, interactive(), graphical()) {
        match open_in_browser(url) {
            Ok(()) => {
                let _ = writeln!(out, "Opened the link in your default browser.");
            }
            Err(error) => {
                let _ = writeln!(
                    out,
                    "warning: could not open a browser ({error}); open the link above manually."
                );
            }
        }
    }
    let _ = writeln!(out, "Waiting for sign-in (up to 10 minutes)…");
    let _ = out.flush();
}

/// Signs in through the browser session fork and stores the session.
pub async fn login(paths: &ProfilePaths, browser: BrowserMode) -> anyhow::Result<LoggedIn> {
    println!("{THIRD_PARTY_NOTICE}");
    let mut shown = false;
    let session = begin_web_login(paths, |url, code| {
        if !shown {
            shown = true;
            present_link(url, code, browser);
        }
    })
    .await?;
    store_login(paths, session).await
}

/// Waits for the sign-in in the browser; `on_sign_in(url, code)` says where
/// to go. Nothing is stored: the caller may still refuse the account.
pub async fn begin_web_login(
    paths: &ProfilePaths,
    on_sign_in: impl FnMut(&str, &str),
) -> anyhow::Result<ProtonAPISession> {
    let options = client_options(paths, &Credentials::new(paths))?;
    ProtonAPISession::begin_via_web(app_version(), options, on_sign_in)
        .await
        .context("sign-in did not complete")
}

/// Stores a new session (D029 modes).
pub async fn store_login(
    paths: &ProfilePaths,
    session: ProtonAPISession,
) -> anyhow::Result<LoggedIn> {
    let credentials = Credentials::new(paths);
    match persist(&credentials, &session).await {
        Ok(storage) => Ok(LoggedIn {
            session,
            storage,
            unsaved: None,
        }),
        Err(error) => {
            // A session that cannot be stored would stay open on the server.
            end_unstored(session).await;
            Err(error.context("cannot store the session; it was ended again"))
        }
    }
}

/// Ends a session on the server that will not be kept (best effort).
pub async fn end_unstored(mut session: ProtonAPISession) {
    if let Err(end) = session.end_from_session().await {
        tracing::warn!(error = %format!("{end:#}"), "could not end the unstored session on the server");
    }
}

pub struct LoggedIn {
    pub session: ProtonAPISession,
    pub storage: Storage,
    /// Why the session's current tokens are not stored (D106 amended): the
    /// server accepted them, but the write failed. The daemon's token saver
    /// writes them later.
    pub unsaved: Option<String>,
}

/// The `status` line of tokens that are not stored yet (D106 amended).
pub fn unsaved_text(reason: &str) -> String {
    format!("not yet saved to the keyring ({reason})")
}

async fn persist(
    credentials: &Credentials<'_, credentials::OsKeyring>,
    session: &ProtonAPISession,
) -> anyhow::Result<Storage> {
    credentials.save(&current_credentials(session).await?)
}

async fn current_credentials(
    session: &ProtonAPISession,
) -> anyhow::Result<proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials> {
    session
        .to_stored_credentials_with_latest_tokens()
        .await
        .context("cannot read session tokens")
}

/// Stores the session's tokens when they have changed (they rotate when
/// refreshed), so an unchanged session never opens a keyring dialog.
pub struct TokenSaver {
    saved: Option<String>,
}

impl TokenSaver {
    /// For a session whose current tokens were just stored.
    pub async fn new(session: &ProtonAPISession) -> Self {
        let saved = match current_credentials(session).await {
            Ok(stored) => serde_json::to_string(&stored).ok(),
            Err(_) => None,
        };
        Self { saved }
    }

    /// For a session that resumed or signed in: its tokens count as stored
    /// unless the write failed ([`LoggedIn::unsaved`], D106 amended), in
    /// which case the next save writes them.
    pub async fn of(logged_in: &LoggedIn) -> Self {
        match logged_in.unsaved {
            Some(_) => Self { saved: None },
            None => Self::new(&logged_in.session).await,
        }
    }

    /// The tokens as they are now, to hand to [`TokenSaver::save`].
    pub async fn snapshot(
        session: &ProtonAPISession,
    ) -> anyhow::Result<proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials> {
        current_credentials(session).await
    }

    /// Stores `stored` unless it is what was stored last. The keyring may
    /// make this wait for a prompt, so it runs off the async threads.
    pub async fn save(
        &mut self,
        paths: &ProfilePaths,
        stored: proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials,
    ) -> anyhow::Result<()> {
        let paths = paths.clone();
        self.save_with(stored, move |stored| {
            Credentials::new(&paths).save(stored).map(drop)
        })
        .await
    }

    /// [`TokenSaver::save`] with `write` storing the session.
    async fn save_with(
        &mut self,
        stored: proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials,
        write: impl FnOnce(
            &proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials,
        ) -> anyhow::Result<()>
        + Send
        + 'static,
    ) -> anyhow::Result<()> {
        let json = serde_json::to_string(&stored)?;
        if self.saved.as_ref() == Some(&json) {
            return Ok(());
        }
        tokio::task::spawn_blocking(move || write(&stored))
            .await
            .context("the token store task failed")??;
        self.saved = Some(json);
        Ok(())
    }
}

/// This device's identity (§10.2).
pub fn device_identity(paths: &ProfilePaths, config: &Config) -> anyhow::Result<DeviceIdentity> {
    Ok(DeviceIdentity {
        id: device::load_or_create_device_id(&paths.device_id_file())?,
        name: device::device_name(config.device.name.as_deref()),
    })
}

/// Transfers a drive client runs at once, per direction (D033, D058).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TransferLimits {
    /// Whole files: `upload.concurrency` (1-4).
    pub files: usize,
    /// Blocks over all files: one below the official C# SDK's 6.
    pub blocks: usize,
}

impl TransferLimits {
    pub const BLOCKS: usize = 5;

    pub fn of(config: &Config) -> Self {
        Self {
            files: config.upload.concurrency as usize,
            blocks: Self::BLOCKS,
        }
    }

    /// Sets the limits on a client being built. The SDK has no getters for
    /// them; the fork's `block_transfer_parallelism_applies_to_uploads_and_downloads`
    /// tests these two builders (P7, P18).
    pub fn apply(self, client: ProtonDriveClient) -> ProtonDriveClient {
        client
            .with_max_concurrent_file_transfers(self.files)
            .with_block_transfer_parallelism(self.blocks)
    }
}

/// The drive client of a session, with this device's client uid.
pub fn drive_client(
    session: &ProtonAPISession,
    device: &DeviceIdentity,
    limits: TransferLimits,
) -> anyhow::Result<ProtonDriveClient> {
    Ok(limits.apply(ProtonDriveClient::new(session, Some(device.client_uid()))?))
}

/// The Photos clients of a session (M5.13, D113), with this device's
/// client uid and the transfer limits; `None` (logged) when they cannot be
/// made, which only a Photos mount notices.
pub fn photos_clients(
    session: &ProtonAPISession,
    device: &DeviceIdentity,
    limits: TransferLimits,
) -> Option<pdmount_backend::proton::PhotosClients> {
    match pdmount_backend::proton::PhotosClients::new(
        session,
        Some(device.client_uid()),
        |client| limits.apply(client),
    ) {
        Ok(photos) => Some(photos),
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "cannot make the Photos clients");
            None
        }
    }
}

/// Drive client and backend of a signed-in session.
pub async fn proton_backend(
    logged_in: &LoggedIn,
    device: &DeviceIdentity,
    limits: TransferLimits,
) -> anyhow::Result<ProtonBackend> {
    let client = drive_client(&logged_in.session, device, limits)?;
    let backend = ProtonBackend::new(
        client,
        logged_in.session.token_credential.clone(),
        logged_in.session.username.clone(),
    )
    .await?;
    backend.set_photos(photos_clients(&logged_in.session, device, limits));
    Ok(backend)
}

/// Where the drive's volume and root were last seen (`proton-root.json`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct KnownRoot {
    pub username: String,
    pub volume: String,
    pub root: String,
}

impl KnownRoot {
    pub fn of(backend: &ProtonBackend, username: &str, root: &pdmount_backend::LinkId) -> Self {
        Self {
            username: username.to_owned(),
            volume: backend.volume_id().to_owned(),
            root: root.to_string(),
        }
    }

    /// The account and root, as the backend's `account` reports them.
    pub fn account(&self) -> pdmount_backend::AccountInfo {
        pdmount_backend::AccountInfo {
            username: self.username.clone(),
            root: pdmount_backend::LinkId::new(self.root.clone()),
        }
    }

    pub fn load(paths: &ProfilePaths) -> Option<Self> {
        let text = std::fs::read_to_string(paths.proton_root_file()).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// The stored root of a profile that has mounted before: its local state
    /// was made for this root (`root_link` in `state.db`), so the engine can
    /// open it without the drive (D042, D106). A first mount that did not
    /// finish leaves `state.db` without a root, which does not count.
    pub fn mounted_before(paths: &ProfilePaths) -> Option<Self> {
        let known = Self::load(paths)?;
        let stored = pdmount_core::store::stored_root(&paths.state_db())?;
        (stored == known.root).then_some(known)
    }

    /// Writes the file when it is missing or differs.
    pub fn save(&self, paths: &ProfilePaths) -> anyhow::Result<()> {
        if Self::load(paths).as_ref() == Some(self) {
            return Ok(());
        }
        let file = paths.proton_root_file();
        let temporary = file.with_extension("json.tmp");
        std::fs::write(&temporary, serde_json::to_string(self)?)?;
        std::fs::rename(&temporary, &file)?;
        Ok(())
    }
}

/// Drive client and backend for a mount without a connection: the volume and
/// root come from `known`, so nothing is requested.
pub fn proton_backend_offline(
    logged_in: &LoggedIn,
    device: &DeviceIdentity,
    known: &KnownRoot,
    limits: TransferLimits,
) -> anyhow::Result<ProtonBackend> {
    let client = drive_client(&logged_in.session, device, limits)?;
    let backend = ProtonBackend::with_root(
        client,
        logged_in.session.token_credential.clone(),
        known.username.clone(),
        known.volume.clone(),
        pdmount_backend::LinkId::new(known.root.clone()),
    );
    backend.set_photos(photos_clients(&logged_in.session, device, limits));
    Ok(backend)
}

/// Outcome of resuming the stored session.
pub enum Resumed {
    NotLoggedIn,
    /// Proton cannot be reached, so the session was not checked. The
    /// session is built from the stored credentials.
    Offline {
        logged_in: Box<LoggedIn>,
        error: anyhow::Error,
    },
    /// The server rejected the session; a new login is required. The session
    /// is built from the stored credentials, for a mount that waits for it
    /// (D042).
    NeedsLogin {
        logged_in: Box<LoggedIn>,
        error: anyhow::Error,
    },
    Active(Box<LoggedIn>),
}

/// Resumes the stored session, refreshing tokens if needed, and stores the
/// latest tokens when they changed. A failed write leaves the session active
/// with [`LoggedIn::unsaved`] set (D106 amended): `status` shows it, the
/// daemon writes the tokens later, and a one-off command (`status`, `login`,
/// `trash empty`) goes on, since the session works.
pub async fn resume(paths: &ProfilePaths) -> anyhow::Result<Resumed> {
    resume_with(paths, false).await
}

/// [`resume`] for the mount daemon: a keyring that does not answer is an
/// error in allow mode too, never "not logged in" (D106).
pub async fn resume_strict(paths: &ProfilePaths) -> anyhow::Result<Resumed> {
    resume_with(paths, true).await
}

async fn resume_with(paths: &ProfilePaths, strict: bool) -> anyhow::Result<Resumed> {
    let credentials = Credentials::new(paths);
    let loaded = if strict {
        credentials.load_strict()?
    } else {
        credentials.load()?
    };
    let Some((stored, storage)) = loaded else {
        return Ok(Resumed::NotLoggedIn);
    };
    // The session as loaded, to tell whether the check changed its tokens.
    let loaded = serde_json::to_string(&stored)?;
    let options = client_options(paths, &credentials)?;
    let mut session = ProtonAPISession::from_stored_credentials(stored, app_version(), options);
    let unchecked = |session| LoggedIn {
        session,
        storage,
        unsaved: None,
    };
    if let Err(error) = session.ensure_authenticated().await {
        if is_session_rejected(&error) {
            return Ok(Resumed::NeedsLogin {
                logged_in: Box::new(unchecked(session)),
                error,
            });
        }
        if pdmount_backend::proton::is_connection_error(&error) {
            return Ok(Resumed::Offline {
                logged_in: Box::new(unchecked(session)),
                error,
            });
        }
        return Err(error.context("cannot reach Proton to check the session"));
    }
    // The server accepted the session: the tokens are written only when the
    // check changed them, and a failed write does not fail the resume (D106
    // amended, rules 1 and 2).
    let current = current_credentials(&session).await?;
    let (storage, unsaved) = match credentials.save_changed(&loaded, &current) {
        credentials::SessionWrite::Unchanged => (storage, None),
        credentials::SessionWrite::Saved(storage) => (storage, None),
        credentials::SessionWrite::Failed(reason) => {
            tracing::warn!(
                reason,
                "the server accepted the session, but its new tokens could not be stored; \
                 they are kept in memory"
            );
            (storage, Some(reason))
        }
    };
    Ok(Resumed::Active(Box::new(LoggedIn {
        session,
        storage,
        unsaved,
    })))
}

/// Whether the server definitively rejected the stored session.
pub fn is_session_rejected(error: &anyhow::Error) -> bool {
    use proton_drive_sdk::proton_sdk_rs2::auth::TokenRefreshError;
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TokenRefreshError>())
        .is_some_and(TokenRefreshError::is_refresh_token_expired)
}

/// Whether a call on a session that resumed failed because only a new login
/// can help: the server rejected it, or its keys cannot be unlocked (D085).
pub fn needs_login(error: &anyhow::Error) -> bool {
    is_session_rejected(error)
        || error.chain().any(|cause| {
            matches!(
                cause.downcast_ref::<BackendError>(),
                Some(BackendError::Unauthorized)
            )
        })
}

/// Ends the session on the server (best effort) and removes local secrets.
pub async fn logout(paths: &ProfilePaths) -> anyhow::Result<LogoutOutcome> {
    let credentials = Credentials::new(paths);
    let Some((stored, _)) = credentials.load()? else {
        remove_local_state(paths, &credentials)?;
        return Ok(LogoutOutcome::WasNotLoggedIn);
    };
    let options = client_options(paths, &credentials)?;
    let mut session = ProtonAPISession::from_stored_credentials(stored, app_version(), options);
    let revoked = match session.end_from_session().await {
        Ok(_) => true,
        Err(error) => {
            tracing::warn!(error = %format!("{error:#}"), "could not end the session on the server");
            false
        }
    };
    remove_local_state(paths, &credentials)
        .context("logged out, but secrets may remain in the keyring")?;
    Ok(LogoutOutcome::LoggedOut { revoked })
}

pub enum LogoutOutcome {
    WasNotLoggedIn,
    LoggedOut { revoked: bool },
}

/// Fails if a keyring entry may remain; the files are removed regardless.
fn remove_local_state(
    paths: &ProfilePaths,
    credentials: &Credentials<'_, credentials::OsKeyring>,
) -> anyhow::Result<()> {
    let removed = credentials.remove();
    for cache in [
        paths.sdk_secret_cache(),
        paths.sdk_key_store(),
        paths.sdk_entity_cache(),
    ] {
        for path in pdmount_core::paths::sqlite_files(&cache) {
            let _ = std::fs::remove_file(path);
        }
    }
    Ok(removed?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_unauthorized_backend_means_a_new_login() {
        let locked = anyhow::Error::from(BackendError::Unauthorized).context("status");
        assert!(needs_login(&locked));
        assert!(!needs_login(&anyhow::Error::from(BackendError::NotFound)));
        assert!(!needs_login(&anyhow::anyhow!("other")));
    }

    fn stored(access: &str) -> proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials {
        proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials::new(
            "sid".into(),
            "user@example.test".into(),
            "uid".into(),
            access.into(),
            "refresh".into(),
            vec!["full".into()],
            false,
            proton_drive_sdk::proton_sdk_rs2::PasswordMode::Single,
        )
    }

    /// D106 amended, rule 2: tokens a resume could not store are written by
    /// the saver's next save; a failed one stays pending, a stored one is
    /// not written again.
    #[tokio::test]
    async fn the_saver_writes_tokens_a_resume_could_not_store() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let writes = Arc::new(AtomicUsize::new(0));
        let write = |fail: bool| {
            let writes = Arc::clone(&writes);
            move |_: &proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials| {
                writes.fetch_add(1, Ordering::SeqCst);
                if fail {
                    anyhow::bail!("no reply")
                }
                Ok(())
            }
        };
        let mut saver = TokenSaver { saved: None };
        saver
            .save_with(stored("a"), write(true))
            .await
            .expect_err("the keyring failed");
        saver
            .save_with(stored("a"), write(false))
            .await
            .expect("written");
        saver
            .save_with(stored("a"), write(false))
            .await
            .expect("unchanged");
        assert_eq!(writes.load(Ordering::SeqCst), 2);
        saver
            .save_with(stored("b"), write(false))
            .await
            .expect("rotated");
        assert_eq!(writes.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn transfer_limits_stay_below_the_official_sdks() {
        let mut config = Config::default();
        assert_eq!(
            TransferLimits::of(&config),
            TransferLimits {
                files: 4,
                blocks: 5
            }
        );
        config.upload.concurrency = 2;
        assert_eq!(TransferLimits::of(&config).files, 2);
    }

    #[test]
    fn browser_opens_only_when_asked_or_in_an_interactive_desktop() {
        use BrowserMode::*;
        assert!(should_open_browser(Always, false, false));
        assert!(!should_open_browser(Never, true, true));
        assert!(should_open_browser(Auto, true, true));
        assert!(
            !should_open_browser(Auto, false, true),
            "not a terminal (service)"
        );
        assert!(!should_open_browser(Auto, true, false), "no display (SSH)");
    }

    #[test]
    fn prompt_always_contains_the_link() {
        let prompt = sign_in_prompt("https://account.proton.me/x", "ABCD-EFGH");
        assert!(prompt.contains("https://account.proton.me/x"));
        assert!(prompt.contains("Code: ABCD-EFGH"));
        assert!(!sign_in_prompt("https://a", "").contains("Code"));
    }

    #[test]
    fn app_version_is_an_honest_alpha_identity() {
        let version = app_version();
        assert_eq!(version.app_name, "pdmount");
        assert_eq!(version.channel, Some(ReleaseChannel::Alpha));
        let expected: Vec<u64> = env!("CARGO_PKG_VERSION")
            .split('.')
            .map(|p| p.parse().expect("numeric version"))
            .collect();
        assert_eq!(vec![version.major, version.minor, version.patch], expected);
    }
}
