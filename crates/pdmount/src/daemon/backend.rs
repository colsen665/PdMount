//! Opening the backend of a start: the fake drive, the real account (session, root check) and the refusals.

use super::*;

/// The backend and the cache key; both come from the keyring.
pub(super) async fn open_with_key(
    paths: &ProfilePaths,
    choice: &BackendChoice,
    device: &DeviceIdentity,
    limits: session::TransferLimits,
) -> anyhow::Result<(Opened, [u8; 32])> {
    let opened = open_backend(paths, choice, device, limits, None).await?;
    let key: [u8; 32] = Credentials::new(paths)
        .cache_master_key()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the cache key has the wrong length"))?;
    Ok((opened, key))
}

/// The backend a mount runs on.
pub(crate) struct Opened {
    pub backend: Arc<dyn Backend>,
    /// The Proton backend and its session; `None` on the fake.
    pub proton: Option<(Arc<ProtonBackend>, Box<LoggedIn>)>,
    /// The fake drive; `None` on Proton.
    pub fake: Option<FakeBackend>,
    pub username: String,
    /// The drive's root folder.
    pub root: String,
    /// Where the session is stored, as `status` shows it: "stored in keyring".
    pub credential_storage: String,
    /// The connectivity the mount starts with.
    pub start_state: Connectivity,
}

/// A start that only the user can unblock; the waiting daemon (D106) tries
/// again only when asked (`session attach`, `pdmount login`).
#[derive(Debug)]
pub(crate) enum StartRefused {
    /// No session is stored.
    NotLoggedIn(String),
    /// The session reaches another account or drive than the mount's root.
    OtherRoot { expected: String, found: String },
}

impl std::fmt::Display for StartRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotLoggedIn(profile) => {
                write!(f, "profile {profile} is not logged in; run `pdmount login`")
            }
            Self::OtherRoot { expected, found } => write!(
                f,
                "the stored session reaches another account or drive (root {found}) than the \
                 mount (root {expected})"
            ),
        }
    }
}

impl std::error::Error for StartRefused {}

/// The backend a mount runs on. `expect_root`: the root the mount already
/// runs on (a daemon that waited for the keyring, D106); a session that
/// reaches another root fails with [`StartRefused::OtherRoot`] before
/// anything is stored about it.
pub(crate) async fn open_backend(
    paths: &ProfilePaths,
    choice: &BackendChoice,
    device: &DeviceIdentity,
    limits: session::TransferLimits,
    expect_root: Option<&str>,
) -> anyhow::Result<Opened> {
    let dir = match choice {
        BackendChoice::Proton => return open_proton(paths, device, limits, expect_root).await,
        BackendChoice::Fake(dir) => dir,
    };
    let fake = FakeBackend::open(dir)
        .with_context(|| format!("cannot open the fake drive {}", dir.display()))?
        .with_client(FakeClient {
            uid: Some(device.client_uid()),
            ..FakeClient::default()
        });
    // Like the Proton path: an online start stores the account, and a start
    // that cannot reach the drive (or is refused) uses the stored one.
    let (account, start_state) = match fake.account().await {
        Ok(account) => {
            refuse_other_root(expect_root, &account.root.0)?;
            let known = session::KnownRoot {
                username: account.username.clone(),
                volume: "fake".into(),
                root: account.root.to_string(),
            };
            if let Err(error) = known.save(paths) {
                tracing::warn!(%error, "cannot store the drive's root for offline mounts");
            }
            (account, Connectivity::Online)
        }
        Err(BackendError::Offline(reason)) => {
            let error = anyhow::anyhow!("offline: {reason}");
            let known = session::KnownRoot::load(paths).with_context(|| {
                format!(
                    "cannot reach the fake drive, and this profile has not been mounted online yet ({error:#})"
                )
            })?;
            tracing::warn!(error = %format!("{error:#}"), "cannot reach the drive; mounting offline");
            (known.account(), Connectivity::Offline)
        }
        Err(BackendError::Unauthorized) => {
            let error = anyhow::anyhow!("not signed in or session expired");
            let known = known_root_for_login(paths, &error)?;
            tracing::warn!(error = %format!("{error:#}"), "the session was rejected; mounting until the sign-in");
            (known.account(), Connectivity::NeedsLogin)
        }
        Err(error) => return Err(error.into()),
    };
    let fake = fake.with_client(FakeClient {
        uid: Some(device.client_uid()),
        known_account: (start_state != Connectivity::Online).then(|| account.clone()),
        ..FakeClient::default()
    });
    Ok(Opened {
        username: account.username,
        root: account.root.to_string(),
        backend: Arc::new(fake.clone()),
        proton: None,
        fake: Some(fake),
        credential_storage: "fake backend, no stored secrets".into(),
        start_state,
    })
}

/// The stored root of a profile that has mounted before, so that it can come
/// up and wait for the sign-in (D042); for any other profile, an error that
/// asks for a login.
pub(super) fn known_root_for_login(
    paths: &ProfilePaths,
    error: &anyhow::Error,
) -> anyhow::Result<session::KnownRoot> {
    session::KnownRoot::mounted_before(paths)
        .with_context(|| format!("the session has expired; run `pdmount login` ({error:#})"))
}

/// [`StartRefused::OtherRoot`] when the drive's root is not the one expected.
pub(super) fn refuse_other_root(expect_root: Option<&str>, found: &str) -> anyhow::Result<()> {
    match expect_root {
        Some(expected) if expected != found => Err(StartRefused::OtherRoot {
            expected: expected.to_owned(),
            found: found.to_owned(),
        }
        .into()),
        _ => Ok(()),
    }
}

/// Resumes the session; without the network, or with a rejected session,
/// a profile mounted online before starts from its stored root.
pub(super) async fn open_proton(
    paths: &ProfilePaths,
    device: &DeviceIdentity,
    limits: session::TransferLimits,
    expect_root: Option<&str>,
) -> anyhow::Result<Opened> {
    let (logged_in, known, mut start_state) = match session::resume_strict(paths).await? {
        Resumed::Active(logged_in) => (logged_in, None, Connectivity::Online),
        Resumed::Offline { logged_in, error } => {
            let Some(known) = session::KnownRoot::load(paths) else {
                return Err(error.context(
                    "cannot reach Proton, and this profile has not been mounted online yet",
                ));
            };
            tracing::warn!(error = %format!("{error:#}"), "cannot reach Proton; mounting offline");
            (logged_in, Some(known), Connectivity::Offline)
        }
        Resumed::NotLoggedIn => {
            return Err(StartRefused::NotLoggedIn(paths.profile.to_string()).into());
        }
        Resumed::NeedsLogin { logged_in, error } => {
            // A mount that has run before comes up and waits for the
            // sign-in: local work goes on (D042).
            let known = known_root_for_login(paths, &error)?;
            tracing::warn!(error = %format!("{error:#}"), "the session was rejected; mounting until the sign-in");
            (logged_in, Some(known), Connectivity::NeedsLogin)
        }
    };
    let mut root = known.as_ref().map(|known| known.root.clone());
    let backend = match &known {
        Some(known) => session::proton_backend_offline(&logged_in, device, known, limits)?,
        None => {
            let online = async {
                let backend = session::proton_backend(&logged_in, device, limits).await?;
                let root = backend.account().await?.root;
                anyhow::Ok((backend, root))
            }
            .await;
            match online {
                Ok((backend, online_root)) => {
                    // Before the root is stored: another account's never
                    // replaces the mount's.
                    refuse_other_root(expect_root, &online_root.0)?;
                    let known =
                        session::KnownRoot::of(&backend, &logged_in.session.username, &online_root);
                    if let Err(error) = known.save(paths) {
                        tracing::warn!(%error, "cannot store the drive's root for offline mounts");
                    }
                    root = Some(online_root.to_string());
                    backend
                }
                // The session resumed, but its keys cannot be unlocked (D085).
                Err(error) if session::needs_login(&error) => {
                    let known = known_root_for_login(paths, &error)?;
                    tracing::warn!(error = %format!("{error:#}"), "a new login is needed; mounting until the sign-in");
                    start_state = Connectivity::NeedsLogin;
                    root = Some(known.root.clone());
                    session::proton_backend_offline(&logged_in, device, &known, limits)?
                }
                Err(error) => return Err(error),
            }
        }
    };
    let backend = Arc::new(backend);
    Ok(Opened {
        backend: Arc::clone(&backend) as Arc<dyn Backend>,
        root: root.unwrap_or_default(),
        username: logged_in.session.username.clone(),
        credential_storage: format!("stored in {}", logged_in.storage),
        proton: Some((backend, logged_in)),
        fake: None,
        start_state,
    })
}

/// The fake backend has no sign-in, so a `needs_login` that its fault rule
/// caused would never end. This stands in for the user signing in again: it
/// probes while the state lasts, and the first answer after `pdmount fake
/// fault clear` ends it (the engine does the same after a real login).
pub(crate) fn watch_fake_session(health: Arc<pdmount_core::health::Health>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if health.state() == pdmount_core::health::Connectivity::NeedsLogin {
                health.kick();
            }
        }
    });
}
