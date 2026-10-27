//! Proton session: web sign-in, resume from stored credentials, logout
//! (DESIGN.md §12).

use crate::credentials::{self, Credentials, Storage};
use anyhow::Context;
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
    let open = |path: &std::path::Path, capacity| -> anyhow::Result<Arc<dyn CacheRepository>> {
        if let Some(parent) = path.parent() {
            credentials::create_private_dir(parent)?;
        }
        let sqlite = SqliteCacheRepository::open_file(path, Some(capacity))
            .with_context(|| format!("cannot open cache {}", path.display()))?;
        restrict_to_owner(path);
        Ok(Arc::new(EncryptedCacheRepository::new(
            Arc::new(sqlite),
            key.clone(),
        )))
    };
    Ok(ProtonClientOptions {
        entity_cache_repository: Some(open(&paths.sdk_entity_cache(), 10_000)?),
        secret_cache_repository: Some(open(&paths.sdk_secret_cache(), 5_000)?),
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
        Ok(storage) => Ok(LoggedIn { session, storage }),
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
        let json = serde_json::to_string(&stored)?;
        if self.saved.as_ref() == Some(&json) {
            return Ok(());
        }
        let paths = paths.clone();
        tokio::task::spawn_blocking(move || Credentials::new(&paths).save(&stored))
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

/// The drive client of a session, with this device's client uid.
pub fn drive_client(
    session: &ProtonAPISession,
    device: &DeviceIdentity,
) -> anyhow::Result<ProtonDriveClient> {
    ProtonDriveClient::new(session, Some(device.client_uid()))
}

/// Drive client and backend of a signed-in session.
pub async fn proton_backend(
    logged_in: &LoggedIn,
    device: &DeviceIdentity,
) -> anyhow::Result<ProtonBackend> {
    let client = drive_client(&logged_in.session, device)?;
    Ok(ProtonBackend::new(
        client,
        logged_in.session.token_credential.clone(),
        logged_in.session.username.clone(),
    )
    .await?)
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

    pub fn load(paths: &ProfilePaths) -> Option<Self> {
        let text = std::fs::read_to_string(paths.proton_root_file()).ok()?;
        serde_json::from_str(&text).ok()
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
) -> anyhow::Result<ProtonBackend> {
    let client = drive_client(&logged_in.session, device)?;
    Ok(ProtonBackend::with_root(
        client,
        logged_in.session.token_credential.clone(),
        known.username.clone(),
        known.volume.clone(),
        pdmount_backend::LinkId::new(known.root.clone()),
    ))
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
/// latest tokens.
pub async fn resume(paths: &ProfilePaths) -> anyhow::Result<Resumed> {
    let credentials = Credentials::new(paths);
    let Some((stored, storage)) = credentials.load()? else {
        return Ok(Resumed::NotLoggedIn);
    };
    let options = client_options(paths, &credentials)?;
    let mut session = ProtonAPISession::from_stored_credentials(stored, app_version(), options);
    if let Err(error) = session.ensure_authenticated().await {
        if is_session_rejected(&error) {
            return Ok(Resumed::NeedsLogin {
                logged_in: Box::new(LoggedIn { session, storage }),
                error,
            });
        }
        if pdmount_backend::proton::is_connection_error(&error) {
            return Ok(Resumed::Offline {
                logged_in: Box::new(LoggedIn { session, storage }),
                error,
            });
        }
        return Err(error.context("cannot reach Proton to check the session"));
    }
    let storage = persist(&credentials, &session).await?;
    Ok(Resumed::Active(Box::new(LoggedIn { session, storage })))
}

/// Whether the server definitively rejected the stored session.
pub fn is_session_rejected(error: &anyhow::Error) -> bool {
    use proton_drive_sdk::proton_sdk_rs2::auth::TokenRefreshError;
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<TokenRefreshError>())
        .is_some_and(TokenRefreshError::is_refresh_token_expired)
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
    for cache in [paths.sdk_secret_cache(), paths.sdk_entity_cache()] {
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
