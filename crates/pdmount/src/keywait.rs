//! A daemon that starts before the keyring answers (DESIGN.md §12.4, §14.4,
//! D102, D106), and `pdmount session attach`.
//!
//! The session and the cache key both live in the keyring. When it is
//! unavailable at the start (no Secret Service on the service's bus, a
//! dismissed unlock dialog) and the profile has mounted before, the daemon
//! does not exit, which a supervisor would answer with a respawn loop: it
//! mounts from the stored tree in `needs_login`, with a [`Deferred`] backend
//! that answers as the session would when rejected, and the disk block cache
//! closed (it is encrypted with the cache key). Staged files, writes and the
//! listings in the store work; the queue waits. A task retries the keyring at
//! `session attach`, at `pdmount login` and every 60 s (only when the
//! collection is unlocked then, so the timer never opens an unlock dialog).
//! Once it answers, the cache key opens the disk cache, the real backend takes
//! over and the mount goes on as if it had started that way.

use crate::auth::Auth;
use crate::control::{self, Request};
use crate::credentials::{self, Credentials, KeyringState};
use crate::daemon::{self, BackendChoice, Opened};
use crate::session;
use pdmount_backend::{
    AccountInfo, Backend, BackendError, BlockInfo, EventId, EventPage, FileMeta, LinkId, NameHash,
    RemoteNode, Result, RevisionId, RevisionMeta, StorageInfo, UploadSource,
};
use pdmount_core::ProfilePaths;
use pdmount_core::device::DeviceIdentity;
use pdmount_core::engine::Engine;
use pdmount_core::health::Connectivity;
use serde_json::Value;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime};

/// How often a waiting daemon looks at the keyring by itself.
const RETRY_INTERVAL: Duration = Duration::from_secs(60);
/// `session attach`: the daemon checks two buses, 2 s each, plus a margin.
const ATTACH_TIMEOUT: Duration = Duration::from_secs(15);

/// `PDMOUNT_KEYRING_RETRY_SECS` shortens the interval for the scenarios.
fn retry_interval() -> Duration {
    std::env::var("PDMOUNT_KEYRING_RETRY_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(RETRY_INTERVAL, Duration::from_secs)
}

// ---- The backend until the keyring answers ---------------------------------

/// Answers `account` from the stored root and every other call as a rejected
/// session would, until [`Deferred::set`] hands it the real backend.
pub struct Deferred {
    account: AccountInfo,
    inner: parking_lot::RwLock<Option<Arc<dyn Backend>>>,
}

impl Deferred {
    pub fn new(account: AccountInfo) -> Self {
        Self {
            account,
            inner: parking_lot::RwLock::new(None),
        }
    }

    pub fn set(&self, backend: Arc<dyn Backend>) {
        *self.inner.write() = Some(backend);
    }

    /// The backend, cloned out of the lock: never held across an `.await`.
    fn inner(&self) -> Result<Arc<dyn Backend>> {
        self.inner.read().clone().ok_or(BackendError::Unauthorized)
    }
}

#[async_trait::async_trait]
impl Backend for Deferred {
    async fn account(&self) -> Result<AccountInfo> {
        match self.inner() {
            Ok(inner) => inner.account().await,
            Err(_) => Ok(self.account.clone()),
        }
    }

    fn upload_block_size(&self) -> u64 {
        self.inner
            .read()
            .as_ref()
            .map_or(pdmount_backend::MAX_BLOCK_BYTES as u64, |inner| {
                inner.upload_block_size()
            })
    }

    async fn root(&self) -> Result<RemoteNode> {
        self.inner()?.root().await
    }

    async fn list_children(&self, folder: &LinkId) -> Result<Vec<RemoteNode>> {
        self.inner()?.list_children(folder).await
    }

    async fn get_nodes(&self, ids: &[LinkId]) -> Result<Vec<Option<RemoteNode>>> {
        self.inner()?.get_nodes(ids).await
    }

    async fn get_nodes_checked(&self, ids: &[LinkId]) -> Result<Vec<pdmount_backend::NodeLookup>> {
        self.inner()?.get_nodes_checked(ids).await
    }

    async fn blocks(&self, file: &LinkId, revision: &RevisionId) -> Result<Vec<BlockInfo>> {
        self.inner()?.blocks(file, revision).await
    }

    async fn fetch_block_into(
        &self,
        file: &LinkId,
        revision: &RevisionId,
        index: u32,
        buf: &mut [u8],
    ) -> Result<usize> {
        self.inner()?
            .fetch_block_into(file, revision, index, buf)
            .await
    }

    async fn upload_new(
        &self,
        parent: &LinkId,
        name: &str,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<(LinkId, RevisionId)> {
        self.inner()?.upload_new(parent, name, source, meta).await
    }

    async fn upload_revision(
        &self,
        file: &LinkId,
        base: &RevisionId,
        source: UploadSource,
        meta: FileMeta,
    ) -> Result<RevisionId> {
        self.inner()?
            .upload_revision(file, base, source, meta)
            .await
    }

    async fn create_folder(
        &self,
        parent: &LinkId,
        name: &str,
        mtime: SystemTime,
    ) -> Result<LinkId> {
        self.inner()?.create_folder(parent, name, mtime).await
    }

    async fn rename(
        &self,
        node: &LinkId,
        new_parent: Option<&LinkId>,
        new_name: &str,
        expected: Option<&NameHash>,
    ) -> Result<()> {
        self.inner()?
            .rename(node, new_parent, new_name, expected)
            .await
    }

    async fn trash(&self, nodes: &[LinkId]) -> Result<Vec<Result<()>>> {
        self.inner()?.trash(nodes).await
    }

    async fn revisions(&self, file: &LinkId) -> Result<Vec<RevisionMeta>> {
        self.inner()?.revisions(file).await
    }

    async fn latest_event_id(&self) -> Result<EventId> {
        self.inner()?.latest_event_id().await
    }

    async fn poll_events(&self, since: &EventId) -> Result<EventPage> {
        self.inner()?.poll_events(since).await
    }

    async fn storage_info(&self) -> Result<StorageInfo> {
        self.inner()?.storage_info().await
    }
}

// ---- What the daemon knows about its session --------------------------------

/// Why a daemon still waits to finish its start (D106), which also says when
/// it tries again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wait {
    /// The keyring did not answer, or its collection is locked: the timer
    /// tries again (probing first, so it never prompts), and so do an attach
    /// and a login.
    Keyring(String),
    /// Something only the user can change (no session, no cache key, another
    /// account's session): tried again at an attach or a login only.
    User(String),
    /// The keyring answered, but the backend did not open (Proton, the
    /// network): tried again at `until`, after an exponential back-off that
    /// also honours Proton's `Retry-After`. An attach or a login before then
    /// does not call.
    Backend { reason: String, until: Instant },
}

impl Wait {
    /// The text `status` shows.
    pub fn describe(&self, interval: Duration) -> String {
        match self {
            Self::Keyring(reason) => format!(
                "the daemon waits for the keyring: {reason}; it tries again at a desktop login, at \
                 `pdmount login` and every {} s",
                interval.as_secs()
            ),
            Self::User(reason) => format!(
                "the daemon waits for the keyring: {reason}; it tries again at a desktop login \
                 and at `pdmount login`"
            ),
            Self::Backend { reason, until } => format!(
                "the daemon read the keyring but could not finish its start: {reason}; it tries \
                 again in {} s",
                remaining(*until)
            ),
        }
    }

    /// The answer of a `pdmount login` that found the daemon still waiting.
    pub fn login_advice(&self) -> String {
        match self {
            Self::Keyring(reason) => format!(
                "the daemon still cannot read the keyring ({reason}); unlock it, or run `pdmount \
                 session attach` in the desktop session, and run `pdmount login` again"
            ),
            Self::User(reason) => format!("the daemon cannot finish its start: {reason}"),
            Self::Backend { reason, until } => format!(
                "the daemon read the keyring but could not finish its start ({reason}); it tries \
                 again by itself in {} s",
                remaining(*until)
            ),
        }
    }
}

/// Whole seconds until `until`, at least 1.
fn remaining(until: Instant) -> u64 {
    until
        .saturating_duration_since(Instant::now())
        .as_secs()
        .max(1)
}

/// How a `login`'s attempt ended.
#[derive(Debug, PartialEq, Eq)]
pub enum Attempt {
    /// It ended: what the daemon still waits for, if anything.
    Done(Option<Wait>),
    /// It is still running (a slow keyring or network).
    Running,
}

/// The session parts the control handler, the stop and the wait share. Set
/// at the start, or by the wait once the keyring answers.
pub struct Late {
    auth: OnceLock<Arc<Auth>>,
    /// Where the session is stored, as `status` shows it.
    credentials: parking_lot::Mutex<String>,
    /// Why the daemon waits to finish its start; `None` once it does not.
    waiting: parking_lot::Mutex<Option<Wait>>,
    retry: tokio::sync::Notify,
    /// Attempts made, so `login` can wait for the one it asked for.
    attempts: tokio::sync::watch::Sender<u64>,
    /// The timer's interval while the keyring does not answer.
    interval: Duration,
}

impl Late {
    pub fn new(credentials: String, waiting: Option<Wait>) -> Arc<Self> {
        Arc::new(Self {
            auth: OnceLock::new(),
            credentials: parking_lot::Mutex::new(credentials),
            waiting: parking_lot::Mutex::new(waiting),
            retry: tokio::sync::Notify::new(),
            attempts: tokio::sync::watch::channel(0).0,
            interval: retry_interval(),
        })
    }

    pub fn auth(&self) -> Option<&Arc<Auth>> {
        self.auth.get()
    }

    pub fn set_auth(&self, auth: Arc<Auth>) {
        let _ = self.auth.set(auth);
    }

    pub fn credentials(&self) -> String {
        self.credentials.lock().clone()
    }

    /// What `status` shows while the daemon waits.
    pub fn waiting(&self) -> Option<String> {
        self.waiting
            .lock()
            .as_ref()
            .map(|wait| wait.describe(self.interval))
    }

    pub fn wait(&self) -> Option<Wait> {
        self.waiting.lock().clone()
    }

    fn set_wait(&self, wait: Option<Wait>) {
        *self.waiting.lock() = wait;
    }

    /// Asks the wait for an attempt now (attach, login).
    pub fn retry(&self) {
        self.retry.notify_one();
    }

    /// Asks for an attempt and waits up to `timeout` for it to end.
    pub async fn retry_and_wait(&self, timeout: Duration) -> Attempt {
        let mut attempts = self.attempts.subscribe();
        let before = *attempts.borrow_and_update();
        self.retry();
        match tokio::time::timeout(timeout, attempts.wait_for(|&now| now > before)).await {
            Ok(_) => Attempt::Done(self.wait()),
            Err(_) => Attempt::Running,
        }
    }

    fn attempted(&self) {
        self.attempts.send_modify(|count| *count += 1);
    }
}

/// The stored root of a profile that has mounted before: one that may wait.
fn stored_root(paths: &ProfilePaths) -> Option<session::KnownRoot> {
    session::KnownRoot::mounted_before(paths)
}

/// Whether the start's error is a keyring that did not answer, for a profile
/// that has mounted before: the root to mount from and the reason.
pub fn waiting_start(
    paths: &ProfilePaths,
    error: &anyhow::Error,
) -> Option<(session::KnownRoot, String)> {
    let reason = credentials::unavailable_in(error)?;
    Some((stored_root(paths)?, reason))
}

/// How long a service's start may spend on the keyring in all, well below
/// systemd's default start timeout of 90 s (D106).
pub const START_KEYRING_TIME: Duration = Duration::from_secs(30);

/// The start of a daemon under a service manager (D106): no keyring call
/// waits past [`START_KEYRING_TIME`], and a profile that has mounted before
/// first probes the collection without a prompt. Unless it is unlocked, every
/// keyring call of the start fails at once as unavailable, so the daemon
/// mounts and waits (the attach or the login may prompt later). Whether the
/// keyring is held: the caller lets it go after the start.
pub async fn guard_service_start(paths: &ProfilePaths) -> bool {
    guard_start(paths, || async {
        off_thread(credentials::keyring_state)
            .await
            .unwrap_or_else(|error| KeyringState::Unavailable(format!("{error:#}")))
    })
    .await
}

async fn guard_start<F: std::future::Future<Output = KeyringState>>(
    paths: &ProfilePaths,
    probe: impl FnOnce() -> F,
) -> bool {
    if credentials::mode() == credentials::SecretsMode::ForceFile {
        return false;
    }
    credentials::limit_keyring_until(Some(Instant::now() + START_KEYRING_TIME));
    // A profile that has not mounted before cannot wait: it reads (and may
    // prompt) as before, within the time limit.
    if stored_root(paths).is_none() {
        return false;
    }
    let reason = match probe().await {
        KeyringState::Unlocked => return false,
        KeyringState::Locked => "the default collection is locked, and a service's start does \
                                 not open its unlock dialog"
            .to_owned(),
        KeyringState::Unavailable(reason) => reason,
    };
    tracing::info!(reason, "the keyring is not read at this start");
    credentials::hold_keyring(reason);
    true
}

/// Runs a blocking keyring call on a thread of its own and waits for its
/// answer: an aborted wait (the daemon's stop) leaves it behind at once, and
/// the runtime's shutdown does not wait for it.
async fn off_thread<T: Send + 'static>(
    call: impl FnOnce() -> T + Send + 'static,
) -> anyhow::Result<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name("pdmount-keyring".into())
        .spawn(move || {
            let _ = sender.send(call());
        })?;
    receiver
        .await
        .map_err(|_| anyhow::anyhow!("the keyring call ended without an answer"))
}

/// The longest back-off between attempts that failed on the backend.
const MAX_BACKOFF: Duration = Duration::from_secs(30 * 60);

/// What a failed attempt waits for (see [`Wait`]). `backoff`: the wait after
/// a backend failure, unless Proton asked for a longer one.
fn classify(error: &anyhow::Error, profile: &str, backoff: Duration, now: Instant) -> Wait {
    if let Some(reason) = credentials::unavailable_in(error) {
        return Wait::Keyring(reason);
    }
    if error
        .chain()
        .any(|cause| cause.is::<credentials::NoCacheKey>())
    {
        return Wait::User(
            "the keyring has no cache key for this profile, so the disk cache stays closed; \
             attach the desktop whose keyring holds it (`pdmount session attach` there), or \
             restart the mount to make a new key, which deletes the encrypted caches (a new \
             `pdmount login` may follow)"
                .into(),
        );
    }
    match error
        .chain()
        .find_map(|cause| cause.downcast_ref::<daemon::StartRefused>())
    {
        Some(daemon::StartRefused::NotLoggedIn(_)) => {
            return Wait::User(format!(
                "the keyring holds no session for profile {profile}; stop the mount (its service, \
                 or `pdmount unmount`), run `pdmount login`, then start it again"
            ));
        }
        Some(refused @ daemon::StartRefused::OtherRoot { .. }) => {
            return Wait::User(format!(
                "{refused}; attach the desktop whose keyring holds this profile's session \
                 (`pdmount session attach` there), or stop the mount and run `pdmount login` \
                 with the mount's account"
            ));
        }
        None => {}
    }
    let wait = backoff.max(retry_after_in(error).unwrap_or_default());
    Wait::Backend {
        reason: format!("{error:#}"),
        until: now + wait,
    }
}

/// When the timer tries again after `wait`; `None`: only when asked.
fn next_attempt(wait: &Wait, interval: Duration, now: Instant) -> Option<Instant> {
    match wait {
        Wait::Keyring(_) => Some(now + interval),
        Wait::User(_) => None,
        Wait::Backend { until, .. } => Some(*until),
    }
}

/// The back-off after the next backend failure: doubled after one, back to
/// `interval` after any other outcome.
fn next_backoff(wait: &Wait, backoff: Duration, interval: Duration) -> Duration {
    match wait {
        Wait::Backend { .. } => (backoff * 2).min(MAX_BACKOFF),
        Wait::Keyring(_) | Wait::User(_) => interval,
    }
}

/// How long Proton asked to wait, if the error is a rate limit.
fn retry_after_in(error: &anyhow::Error) -> Option<Duration> {
    let classified = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<BackendError>().cloned())
        .unwrap_or_else(|| pdmount_backend::proton::classify(error));
    match classified {
        BackendError::RateLimited { retry_after } => retry_after,
        _ => None,
    }
}

/// What the wait needs to finish the start.
pub struct Waiter {
    pub paths: ProfilePaths,
    pub choice: BackendChoice,
    pub device: DeviceIdentity,
    pub limits: session::TransferLimits,
    pub engine: Arc<Engine>,
    pub deferred: Arc<Deferred>,
    pub late: Arc<Late>,
    /// The root the engine was opened with.
    pub root: String,
    /// The further roots, which mount once the wait ends (D110 amended).
    pub roots: Option<Arc<crate::roots::Roots>>,
}

impl Waiter {
    /// Retries until the keyring answers and the backend is in place, then
    /// ends. One desktop notification, when one can be delivered.
    pub async fn run(self) {
        let interval = self.late.interval;
        let mut notified = notify_once(&self.paths, false).await;
        // When the timer tries next; `None`: only when asked.
        let mut next = Some(Instant::now() + interval);
        let mut backoff = interval;
        loop {
            let asked = tokio::select! {
                () = sleep_until(next) => false,
                () = self.late.retry.notified() => true,
            };
            // Proton's back-off holds for an attach or a login too.
            if let Some(Wait::Backend { until, .. }) = self.late.wait()
                && Instant::now() < until
            {
                next = Some(until);
                self.late.attempted();
                continue;
            }
            credentials::retry_keyring();
            if !asked {
                // The timer never opens an unlock dialog: only an unlocked
                // collection is read.
                let state = off_thread(credentials::keyring_state).await;
                match state {
                    Ok(KeyringState::Unlocked) => {}
                    Ok(KeyringState::Locked) => {
                        tracing::debug!("the keyring is still locked");
                        self.late.set_wait(Some(Wait::Keyring(
                            "the default collection is locked".into(),
                        )));
                        next = Some(Instant::now() + interval);
                        backoff = interval;
                        continue;
                    }
                    Ok(KeyringState::Unavailable(reason)) => {
                        tracing::debug!(reason, "the keyring is still not readable");
                        self.late.set_wait(Some(Wait::Keyring(reason)));
                        next = Some(Instant::now() + interval);
                        backoff = interval;
                        continue;
                    }
                    Err(error) => {
                        tracing::debug!(%error, "the keyring probe failed");
                        next = Some(Instant::now() + interval);
                        backoff = interval;
                        continue;
                    }
                }
            }
            match self.try_finish().await {
                Ok(state) => {
                    tracing::info!(
                        state = state.as_str(),
                        "the keyring answered; the mount goes on with its session and disk cache"
                    );
                    self.late.set_wait(None);
                    self.late.attempted();
                    return;
                }
                Err(error) => {
                    let profile = self.paths.profile.to_string();
                    let now = Instant::now();
                    let wait = classify(&error, &profile, backoff, now);
                    match &wait {
                        Wait::Keyring(reason) => {
                            tracing::debug!(reason, "the keyring is still unavailable");
                        }
                        Wait::User(reason) => tracing::warn!(
                            reason,
                            "the start cannot finish; waiting for an attach or a login"
                        ),
                        Wait::Backend { reason, until } => tracing::warn!(
                            reason,
                            retry_in_secs = remaining(*until),
                            "the keyring answered, but the backend did not open"
                        ),
                    }
                    next = next_attempt(&wait, interval, now);
                    backoff = next_backoff(&wait, backoff, interval);
                    self.late.set_wait(Some(wait));
                }
            }
            self.late.attempted();
            notified = notify_once(&self.paths, notified).await;
        }
    }

    /// Reads the cache key and the session, and puts them to work. The cache
    /// key is never made here ([`credentials::caches_open`]).
    async fn try_finish(&self) -> anyhow::Result<Connectivity> {
        // The read that may prompt runs off the async threads; the reads
        // in `open_backend` then find the collection unlocked.
        let paths = self.paths.clone();
        let key = off_thread(move || Credentials::new(&paths).cache_master_key()).await??;
        let key: [u8; 32] = key
            .try_into()
            .map_err(|_| anyhow::anyhow!("the cache key has the wrong length"))?;
        let opened: Opened = daemon::open_backend(
            &self.paths,
            &self.choice,
            &self.device,
            self.limits,
            Some(&self.root),
        )
        .await?;
        if opened.root != self.root {
            return Err(daemon::StartRefused::OtherRoot {
                expected: self.root.clone(),
                found: opened.root,
            }
            .into());
        }
        self.engine.unlock_cache(&key);
        *self.late.credentials.lock() = opened.credential_storage;
        let source = daemon::tree_source(
            opened.proton.as_ref().map(|(proton, _)| proton),
            opened.fake.as_ref(),
        );
        let health = Arc::clone(self.engine.health());
        let state = opened.start_state;
        match opened.proton {
            Some((proton, logged_in)) => {
                let auth = Auth::new(
                    self.paths.clone(),
                    self.device.clone(),
                    self.limits,
                    proton,
                    Arc::clone(&health),
                    *logged_in,
                )
                .await;
                // The token watch exists before the backend takes calls, so
                // no rotation goes unsaved; the login that follows a
                // rejected session starts only once the state is set.
                auth.watch().await;
                self.deferred.set(opened.backend);
                health.set(state);
                auth.supervise();
                self.late.set_auth(auth);
            }
            None => {
                self.deferred.set(opened.backend);
                health.set(state);
                daemon::watch_fake_session(Arc::clone(&health));
            }
        }
        health.kick();
        // The further roots mount now, in the background, as after the
        // readiness report of a start that had the keyring (D110 amended).
        if let (Some(roots), Some((source, _))) = (&self.roots, source) {
            tracing::info!("the further roots mount now");
            roots.set_session(crate::roots::Session {
                source,
                master_key: Some(key),
            });
        }
        Ok(state)
    }
}

/// Sleeps until `at`; for ever when `None`.
async fn sleep_until(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// The one notification of a wait, once it has reached a desktop. Only a
/// `notify-send` that succeeded counts: a name that is only activatable on
/// the daemon's own bus (a desktop not yet attached) can fail to start
/// there, and the attach that follows must still get its notification.
async fn notify_once(paths: &ProfilePaths, notified: bool) -> bool {
    if notified || !crate::notify::deliverable().await {
        return notified;
    }
    let sent = crate::notify::desktop_sent(
        "PdMount: keyring needed",
        &format!(
            "Profile {} is mounted but waits for the keyring: unlock it, or run `pdmount login`",
            paths.profile
        ),
    )
    .await;
    if !sent {
        tracing::debug!("the keyring notification did not go out; tried again at the next attempt");
    }
    sent
}

// ---- session attach --------------------------------------------------------------

/// The daemon's side of `AttachBus` (D102): keeps the desktop's bus unless it
/// is the daemon's own, saves it for the next start, passes the desktop's
/// display to the own bus when a service uses that bus (D102 amended), and
/// retries the keyring.
pub async fn handle_attach(address: String, display: crate::bus::Display, late: &Late) -> Value {
    if let Err(error) = crate::bus::validate(&address) {
        return control::failed(error);
    }
    let same = match same_as_own(&address).await {
        Ok(same) => same,
        Err(error) => return control::failed(format!("cannot use that bus: {error}")),
    };
    let display = checked_display(display, "session attach sent");
    if same {
        crate::bus::set_attached(None);
        tracing::debug!("the desktop uses the daemon's own session bus");
    } else {
        crate::bus::keep(address.clone(), &display);
        tracing::info!(
            address,
            "attached the desktop's session bus; the keyring and notifications each use the bus \
             their service runs on"
        );
    }
    let (keyring, notifications, display) = route_and_display(same, display).await;
    // A login may have unlocked the keyring: try it again.
    credentials::retry_keyring();
    late.retry();
    control::ok(control::AttachReply {
        same,
        keyring: Some(keyring),
        notifications: Some(notifications),
        display,
    })
}

/// At the daemon's start, before the keyring is first tried (D102 amended):
/// the attach saved by the last daemon, taken as an attach without a retry
/// of the keyring (nothing has tried it yet). The address passes the same
/// checks; a file that fails one is removed.
pub async fn restore_saved_attach(paths: &ProfilePaths) {
    crate::bus::save_attaches_in(paths.attach_file());
    let Some(saved) = crate::bus::take_saved() else {
        return;
    };
    if let Err(error) = crate::bus::validate(&saved.address) {
        crate::bus::discard_saved(&error);
        return;
    }
    match same_as_own(&saved.address).await {
        Ok(false) => {}
        Ok(true) => {
            crate::bus::discard_saved("it is the daemon's own session bus");
            return;
        }
        Err(error) => {
            crate::bus::discard_saved(&error);
            return;
        }
    }
    let display = checked_display(saved.display, "the saved attach holds");
    crate::bus::set_attached(Some(saved.address.clone()));
    tracing::info!(
        address = saved.address,
        "using the desktop's session bus saved by the last attach; the keyring and notifications \
         each use the bus their service runs on"
    );
    route_and_display(false, display).await;
}

/// Once the control socket listens (D102 amended 2026-11-14): an attach
/// that `session attach` saved while this daemon started, after
/// [`restore_saved_attach`] read the file and before an attach could reach
/// the socket. Taken like an attach: the keyring is tried again. A file
/// whose address is already kept, or none, changes nothing.
pub async fn restore_late_attach(late: &Late) {
    let Some(saved) = crate::bus::take_saved() else {
        return;
    };
    if crate::bus::attached().as_deref() == Some(saved.address.as_str()) {
        return;
    }
    if let Err(error) = crate::bus::validate(&saved.address) {
        crate::bus::discard_saved(&error);
        return;
    }
    match same_as_own(&saved.address).await {
        Ok(false) => {}
        Ok(true) => {
            crate::bus::discard_saved("it is the daemon's own session bus");
            return;
        }
        Err(error) => {
            crate::bus::discard_saved(&error);
            return;
        }
    }
    let display = checked_display(saved.display, "the saved attach holds");
    crate::bus::set_attached(Some(saved.address.clone()));
    tracing::info!(
        address = saved.address,
        "using the desktop's session bus saved by an attach during the start; the keyring and \
         notifications each use the bus their service runs on"
    );
    route_and_display(false, display).await;
    credentials::retry_keyring();
    late.retry();
}

/// Whether the bus at `address` is the daemon's own (equal bus IDs); an
/// error when it cannot be asked or is not this user's.
async fn same_as_own(address: &str) -> std::result::Result<bool, String> {
    let theirs = crate::bus::id_of(address).await?;
    let ours = match crate::bus::own_address() {
        Some(own) => crate::bus::id_of(&own).await.ok(),
        None => None,
    };
    Ok(ours.as_deref() == Some(theirs.as_str()))
}

/// The display variables that pass the check; the refused ones are logged.
fn checked_display(given: crate::bus::Display, from: &str) -> crate::bus::Display {
    let (display, refused) = crate::bus::checked_display(given);
    if !refused.is_empty() {
        tracing::warn!(
            "{from} display variables that were left out: {}",
            refused.join("; ")
        );
    }
    display
}

/// After an attach: each service's bus, chosen now so the log and the reply
/// say where it goes (D102 amended; the calls choose again, from the
/// cache), and the display passed to the own bus when a service uses it
/// (the names set). Before the keyring's retries: the unlock dialog and a
/// notification server that the own bus starts for them open on the
/// desktop.
async fn route_and_display(
    same: bool,
    display: crate::bus::Display,
) -> (crate::bus::Choice, crate::bus::Choice, Vec<String>) {
    let keyring = crate::bus::route(crate::bus::Service::Keyring).await;
    let notifications = crate::bus::route(crate::bus::Service::Notifications).await;
    let display = if crate::bus::display_for_own_bus(same, keyring.choice(), notifications.choice())
    {
        display_to_own_bus(display).await
    } else {
        Vec::new()
    };
    (keyring.choice(), notifications.choice(), display)
}

/// Sets the desktop's display variables (checked) in the own bus's
/// activation environment (D102 amended); the names it set. A failure is
/// logged and does not fail the attach.
async fn display_to_own_bus(display: crate::bus::Display) -> Vec<String> {
    if display.is_empty() {
        return Vec::new();
    }
    let Some(own) = crate::bus::own_address() else {
        tracing::debug!("no own session bus to pass the desktop's display to");
        return Vec::new();
    };
    let shown = display
        .iter()
        .map(|(name, value)| format!("{name}={value}"))
        .collect::<Vec<_>>()
        .join(" ");
    match crate::bus::update_activation_environment(&own, &display).await {
        Ok(()) => {
            tracing::info!(
                "the daemon's own session bus starts programs on the desktop's display from now on: \
                 {shown}"
            );
            display.into_keys().collect()
        }
        Err(error) => {
            tracing::warn!(
                error,
                "cannot pass the desktop's display ({shown}) to the daemon's own bus"
            );
            Vec::new()
        }
    }
}

/// `pdmount session attach`: hands this session's bus to the daemon. Without
/// a daemon, the attach is saved for its next start (D102 amended
/// 2026-11-14): at a login the service and the window manager's attach start
/// together, and the attach may come first.
pub async fn attach(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let Some(address) = crate::bus::own_address() else {
        anyhow::bail!("DBUS_SESSION_BUS_ADDRESS is not set: run this in the desktop session");
    };
    crate::bus::validate(&address).map_err(anyhow::Error::msg)?;
    let display = crate::bus::display_from_env();
    let request = || Request::AttachBus {
        address: address.clone(),
        display: display.clone(),
    };
    let socket = paths.control_socket();
    let mut reply =
        control::call_ok::<control::AttachReply>(&socket, request(), ATTACH_TIMEOUT).await?;
    let mut saved = false;
    if reply.is_none() {
        saved = save_for_next_start(
            &address,
            crate::bus::service_address().as_deref(),
            paths.attach_file().as_deref(),
            &display,
        )
        .await?;
        if saved {
            // A daemon that started meanwhile may have passed its start's
            // read of the file: it gets the attach now. One still starting
            // reads the file again once its control socket listens.
            reply = control::call_ok::<control::AttachReply>(&socket, request(), ATTACH_TIMEOUT)
                .await?;
        }
    }
    match reply {
        None if saved => println!(
            "No daemon of profile {} runs; its next start takes this session's bus.",
            paths.profile
        ),
        None => println!(
            "No daemon of profile {} is running; nothing to attach.",
            paths.profile
        ),
        Some(reply) if reply.same => println!(
            "The daemon of profile {} already uses this session's bus.",
            paths.profile
        ),
        Some(reply) => {
            // Each service on the bus it runs on (D102 amended).
            let word = |choice: Option<crate::bus::Choice>| match choice {
                Some(crate::bus::Choice::Attached) => "this session's bus",
                Some(choice) => choice.describe(),
                None => "unknown",
            };
            println!(
                "The daemon of profile {} now has this session's bus (keyring: {}; notifications: {}).",
                paths.profile,
                word(reply.keyring),
                word(reply.notifications)
            );
            if !reply.display.is_empty() {
                println!(
                    "Its own bus now starts programs, such as an unlock dialog, on this display ({}).",
                    reply.display.join(", ")
                );
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// The client's side of an attach without a daemon (D102 amended
/// 2026-11-14): saves the desktop's bus at `address` with `display` in
/// `file` (the daemon's saved attach, same format and writer) for the
/// daemon's next start, unless there is no runtime directory (`file`
/// `None`) or the desktop's bus is the service's (`service`, by bus ID; a
/// service's bus that does not answer counts as another, since the daemon
/// checks at its start). Whether it saved. A desktop's bus that does not
/// answer, or is not this user's, is an error, as at a daemon's attach.
pub async fn save_for_next_start(
    address: &str,
    service: Option<&str>,
    file: Option<&std::path::Path>,
    display: &crate::bus::Display,
) -> anyhow::Result<bool> {
    let Some(file) = file else {
        return Ok(false);
    };
    let ours = crate::bus::id_of(address)
        .await
        .map_err(|error| anyhow::anyhow!("cannot use that bus: {error}"))?;
    if let Some(service) = service
        && crate::bus::id_of(service).await.ok().as_deref() == Some(ours.as_str())
    {
        return Ok(false);
    }
    let (display, _) = crate::bus::checked_display(display.clone());
    crate::bus::write_saved(
        file,
        &crate::bus::Saved {
            address: address.to_owned(),
            display,
        },
    )?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_backend::fake::FakeBackend;

    #[tokio::test]
    async fn the_deferred_backend_answers_as_a_rejected_session_until_set() {
        let account = AccountInfo {
            username: "user@example.test".into(),
            root: LinkId::new("root"),
        };
        let deferred = Deferred::new(account.clone());
        assert_eq!(deferred.account().await, Ok(account));
        assert_eq!(
            deferred.storage_info().await,
            Err(BackendError::Unauthorized)
        );
        assert_eq!(
            deferred.list_children(&LinkId::new("root")).await,
            Err(BackendError::Unauthorized)
        );
        let fake = FakeBackend::new();
        let root = fake.root().await.expect("root").link_id;
        deferred.set(Arc::new(fake));
        assert_eq!(deferred.account().await.expect("account").root, root);
        assert!(deferred.list_children(&root).await.is_ok());
    }

    #[tokio::test]
    async fn login_waits_for_the_attempt_it_asked_for() {
        let locked = || Some(Wait::Keyring("locked".into()));
        let late = Late::new("keyring unavailable".into(), locked());
        let worker = {
            let late = Arc::clone(&late);
            tokio::spawn(async move {
                late.retry.notified().await;
                late.set_wait(None);
                late.attempted();
            })
        };
        assert_eq!(
            late.retry_and_wait(Duration::from_secs(10)).await,
            Attempt::Done(None)
        );
        worker.await.expect("worker");
        // Nobody answers: the attempt counts as still running, not failed.
        let late = Late::new(String::new(), locked());
        assert_eq!(
            late.retry_and_wait(Duration::from_millis(50)).await,
            Attempt::Running
        );
    }

    /// Review finding 4 (D106): only a keyring that does not answer keeps the
    /// timer; what the user must change waits for an attach or a login, and a
    /// backend failure backs off exponentially, at least as long as Proton's
    /// `Retry-After`.
    #[test]
    fn failures_are_classified_and_only_the_keyring_keeps_the_timer() {
        let interval = Duration::from_secs(60);
        let now = Instant::now();
        let classify_at = |error: anyhow::Error, backoff| classify(&error, "p", backoff, now);

        let keyring = anyhow::Error::new(credentials::Unavailable("locked".into()))
            .context("cannot read the session");
        let wait = classify_at(keyring, interval);
        assert_eq!(wait, Wait::Keyring("locked".into()));
        assert_eq!(next_attempt(&wait, interval, now), Some(now + interval));
        assert!(wait.describe(interval).contains("every 60 s"));

        let users = [
            anyhow::Error::new(credentials::NoCacheKey).context("cannot open the disk cache"),
            daemon::StartRefused::NotLoggedIn("p".into()).into(),
            daemon::StartRefused::OtherRoot {
                expected: "a".into(),
                found: "b".into(),
            }
            .into(),
        ];
        let mut texts = Vec::new();
        for error in users {
            let wait = classify_at(error, interval);
            assert!(matches!(wait, Wait::User(_)), "{wait:?}");
            assert_eq!(next_attempt(&wait, interval, now), None, "{wait:?}");
            assert_eq!(next_backoff(&wait, interval * 4, interval), interval);
            texts.push(wait.login_advice());
        }
        assert!(texts[0].contains("no cache key"), "{}", texts[0]);
        assert!(
            texts[1].contains("holds no session for profile p"),
            "{}",
            texts[1]
        );
        assert!(texts[1].contains("then start it again"), "{}", texts[1]);
        assert!(
            texts[2].contains("another account or drive"),
            "{}",
            texts[2]
        );

        // Backend failures: the back-off doubles up to its limit.
        let transient = anyhow::Error::new(BackendError::Transient("HTTP 503".into()));
        let wait = classify_at(transient, interval * 2);
        assert_eq!(next_attempt(&wait, interval, now), Some(now + interval * 2));
        assert_eq!(next_backoff(&wait, interval * 2, interval), interval * 4);
        assert_eq!(next_backoff(&wait, MAX_BACKOFF, interval), MAX_BACKOFF);
        // Proton's Retry-After wins over a shorter back-off.
        let limited = anyhow::Error::new(BackendError::RateLimited {
            retry_after: Some(Duration::from_secs(900)),
        })
        .context("cannot reach Proton to check the session");
        let wait = classify_at(limited, interval);
        assert_eq!(
            next_attempt(&wait, interval, now),
            Some(now + Duration::from_secs(900))
        );
        assert!(wait.login_advice().contains("tries again by itself"));
    }

    fn mounted_paths(dir: &std::path::Path) -> ProfilePaths {
        let paths = ProfilePaths::new(
            "keywait-test".parse().expect("profile"),
            &pdmount_core::paths::BaseDirs {
                config: dir.join("config"),
                data: dir.join("data"),
                cache: dir.join("cache"),
                state: dir.join("state"),
                runtime: None,
            },
        );
        std::fs::create_dir_all(paths.proton_root_file().parent().expect("parent")).expect("mkdir");
        session::KnownRoot {
            username: "user@example.test".into(),
            volume: "fake".into(),
            root: "root-1".into(),
        }
        .save(&paths)
        .expect("save");
        paths
    }

    /// Review findings 1 and 7 (D106): a service's start with the collection
    /// locked reads nothing, so it opens no dialog and the start goes on at
    /// once into the wait; a `state.db` without a root is not "mounted
    /// before".
    #[tokio::test]
    async fn a_locked_collection_at_a_service_start_waits_at_once() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = mounted_paths(dir.path());
        // The first mount did not finish: no root in the store, no wait.
        let store = pdmount_core::store::Store::open(&paths.state_db()).expect("store");
        assert!(stored_root(&paths).is_none());
        let probed = std::sync::atomic::AtomicBool::new(false);
        let held = guard_start(&paths, || async {
            probed.store(true, std::sync::atomic::Ordering::Relaxed);
            KeyringState::Locked
        })
        .await;
        assert!(!held && !probed.load(std::sync::atomic::Ordering::Relaxed));
        store
            .write_durable(|tx| pdmount_core::store::meta_set(tx, "root_link", "root-1"))
            .await
            .expect("root");
        drop(store);
        assert!(stored_root(&paths).is_some());

        let start = Instant::now();
        let held = guard_start(&paths, || async { KeyringState::Locked }).await;
        assert!(held);
        // Every read of the start now fails at once as unavailable, which is
        // the waiting start.
        let error = Credentials::new(&paths)
            .load_strict()
            .expect_err("the keyring is held");
        credentials::retry_keyring();
        credentials::limit_keyring_until(None);
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        let (known, reason) = waiting_start(&paths, &error).expect("a waiting start");
        assert_eq!(known.root, "root-1");
        assert!(reason.contains("locked"), "{reason}");
        // An unlocked collection is read as usual.
        assert!(!guard_start(&paths, || async { KeyringState::Unlocked }).await);
        credentials::limit_keyring_until(None);
    }
}
