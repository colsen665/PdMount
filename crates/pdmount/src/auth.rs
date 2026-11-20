//! The session of a running mount (DESIGN.md §12.3, D042): token rotation
//! stored as it happens, a proactive refresh, and the web login the daemon
//! runs itself when the server rejects the session.

use crate::credentials::{self, KeyringState, SecretsMode};
use crate::notify;
use crate::session::{self, LoggedIn, TokenSaver};
use pdmount_backend::proton::ProtonBackend;
use pdmount_core::device::DeviceIdentity;
use pdmount_core::health::{Connectivity, Health};
use pdmount_core::{Profile, ProfilePaths};
use proton_drive_sdk::proton_sdk_rs2::auth::TokenCredential;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;

/// While online, the session is checked this often, so an expiry shows up
/// without a request having to fail first.
const REFRESH_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// How long a sign-in link is valid (the SDK gives up after the same time).
pub const SIGN_IN_TIME: Duration = Duration::from_secs(10 * 60);
/// How long `Request::Login` waits for the link.
const LINK_WAIT: Duration = Duration::from_secs(30);

/// Why a sign-in started; it decides how it is announced (D108).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignInReason {
    /// The user asked for it: `pdmount login` (`--force`).
    Requested,
    /// The server rejected the session (D042).
    Rejected,
}

/// A sign-in link waiting to be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignIn {
    pub url: String,
    pub code: String,
}

pub struct Auth {
    paths: ProfilePaths,
    device: DeviceIdentity,
    limits: session::TransferLimits,
    backend: Arc<ProtonBackend>,
    health: Arc<Health>,
    /// The account of the mount: a login as anyone else is refused.
    username: String,
    /// Shared by the refresh task, the login task and the token saver.
    logged_in: tokio::sync::Mutex<LoggedIn>,
    saver: tokio::sync::Mutex<TokenSaver>,
    /// Why the current tokens are not stored, while they are not (D106
    /// amended).
    unsaved: parking_lot::Mutex<Unsaved>,
    sign_in: parking_lot::Mutex<Option<SignIn>>,
    login_running: AtomicBool,
    /// Why the last login did not end in a new session.
    login_error: parking_lot::Mutex<Option<String>>,
    watch: parking_lot::Mutex<Option<tokio::task::JoinHandle<()>>>,
    tasks: parking_lot::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl Auth {
    pub async fn new(
        paths: ProfilePaths,
        device: DeviceIdentity,
        limits: session::TransferLimits,
        backend: Arc<ProtonBackend>,
        health: Arc<Health>,
        logged_in: LoggedIn,
    ) -> Arc<Self> {
        let saver = TokenSaver::of(&logged_in).await;
        let unsaved = Unsaved(logged_in.unsaved.clone());
        Arc::new(Self {
            paths,
            device,
            limits,
            backend,
            health,
            username: logged_in.session.username.clone(),
            logged_in: tokio::sync::Mutex::new(logged_in),
            saver: tokio::sync::Mutex::new(saver),
            unsaved: parking_lot::Mutex::new(unsaved),
            sign_in: parking_lot::Mutex::new(None),
            login_running: AtomicBool::new(false),
            login_error: parking_lot::Mutex::new(None),
            watch: parking_lot::Mutex::new(None),
            tasks: parking_lot::Mutex::new(Vec::new()),
        })
    }

    /// Starts the background work: token watching, the proactive refresh and
    /// the login that follows a rejected session.
    pub async fn start(self: &Arc<Self>) {
        self.watch().await;
        self.supervise();
    }

    /// The token watching: every rotation is stored.
    pub async fn watch(self: &Arc<Self>) {
        let credential = self.logged_in.lock().await.session.token_credential.clone();
        self.watch_tokens(credential);
    }

    /// The proactive refresh and the login that follows a rejected session;
    /// after [`Auth::watch`], once the connectivity is set (D106).
    pub fn supervise(self: &Arc<Self>) {
        let this = Arc::clone(self);
        let refresh = tokio::spawn(async move { this.keep_fresh().await });
        let this = Arc::clone(self);
        let supervisor = tokio::spawn(async move { this.login_when_rejected().await });
        self.tasks.lock().extend([refresh, supervisor]);
    }

    pub fn shutdown(&self) {
        for task in self.tasks.lock().drain(..) {
            task.abort();
        }
        if let Some(watch) = self.watch.lock().take() {
            watch.abort();
        }
    }

    /// The pending sign-in link, if a login is waiting.
    pub fn sign_in(&self) -> Option<SignIn> {
        self.sign_in.lock().clone()
    }

    /// Why the last login failed, until the next one starts.
    pub fn login_error(&self) -> Option<String> {
        self.login_error.lock().clone()
    }

    /// Why the session's current tokens are not stored yet, for `status`.
    pub fn unsaved(&self) -> Option<String> {
        self.unsaved.lock().0.clone()
    }

    /// Stores the tokens now (shutdown). The keyring may prompt.
    pub async fn save_tokens(&self) {
        self.save(false).await;
    }

    /// Stores the tokens when they changed. The snapshot is taken under the
    /// saver lock, so an older one is never written after a newer one (a
    /// rotated-out refresh token is dead). With `check_keyring`, tokens that
    /// are not stored yet are written only when the collection is unlocked,
    /// probed without a prompt: a retry never opens an unlock dialog (D106
    /// amended).
    async fn save(&self, check_keyring: bool) {
        // Probed before the saver lock is taken: a probe that hangs must not
        // hold up the shutdown's last save.
        let unsaved = self.unsaved.lock().0.is_some();
        if check_keyring && unsaved {
            let mode = credentials::mode();
            let state = match mode {
                SecretsMode::ForceFile => None,
                _ => Some(
                    tokio::task::spawn_blocking(credentials::keyring_state)
                        .await
                        .unwrap_or_else(|error| KeyringState::Unavailable(error.to_string())),
                ),
            };
            if !write_now(mode, state.as_ref()) {
                tracing::debug!(?state, "the session tokens wait for an unlocked keyring");
                return;
            }
        }
        let mut saver = self.saver.lock().await;
        let stored = {
            let logged_in = self.logged_in.lock().await;
            TokenSaver::snapshot(&logged_in.session).await
        };
        let result = match stored {
            Ok(stored) => saver.save(&self.paths, stored).await,
            Err(error) => Err(error),
        };
        match result {
            Ok(()) => {
                if self.unsaved.lock().saved() {
                    tracing::info!("the session tokens are stored now");
                }
            }
            Err(error) => {
                let reason = credentials::write_failure(&error);
                if self.unsaved.lock().failed(reason.clone()) {
                    tracing::warn!(
                        reason,
                        "cannot store the session tokens; they are kept in memory and written \
                         once the keyring is unlocked"
                    );
                } else {
                    tracing::debug!(reason, "the session tokens are still not stored");
                }
            }
        }
    }

    /// Stores changed tokens in a task of its own: a keyring wait of up to
    /// 120 s must not hold up whoever noticed the new tokens (the token
    /// timer, a rotation).
    pub fn spawn_save(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.save(true).await });
    }

    /// Follows one credential: every rotation is stored at once, and a
    /// refresh token the server no longer accepts ends in `needs_login`.
    fn watch_tokens(self: &Arc<Self>, credential: TokenCredential) {
        let this = Arc::clone(self);
        let task = tokio::spawn(async move {
            let mut refreshed = credential.subscribe_tokens_refreshed();
            let mut expired = credential.subscribe_refresh_token_expired();
            loop {
                tokio::select! {
                    rotated = refreshed.recv() => match rotated {
                        Ok(_) | Err(RecvError::Lagged(_)) => this.spawn_save(),
                        Err(RecvError::Closed) => return,
                    },
                    rejected = expired.recv() => match rejected {
                        Ok(()) | Err(RecvError::Lagged(_)) => {
                            this.health.set(Connectivity::NeedsLogin);
                        }
                        Err(RecvError::Closed) => return,
                    },
                }
            }
        });
        if let Some(old) = self.watch.lock().replace(task) {
            old.abort();
        }
    }

    /// Every [`REFRESH_INTERVAL`] while online, checks the session (which
    /// refreshes the tokens when they are about to expire).
    async fn keep_fresh(self: Arc<Self>) {
        loop {
            tokio::time::sleep(REFRESH_INTERVAL).await;
            if self.health.state() != Connectivity::Online {
                continue;
            }
            let result = self
                .logged_in
                .lock()
                .await
                .session
                .ensure_authenticated()
                .await;
            if let Err(error) = result {
                if session::is_session_rejected(&error) {
                    self.health.set(Connectivity::NeedsLogin);
                } else {
                    // Connection errors are Health's business; the rest is
                    // retried at the next turn.
                    tracing::debug!(error = %format!("{error:#}"), "session check failed");
                }
            }
        }
    }

    /// Starts a login each time the session becomes rejected.
    async fn login_when_rejected(self: Arc<Self>) {
        let mut state = self.health.subscribe();
        loop {
            if state
                .wait_for(|state| *state == Connectivity::NeedsLogin)
                .await
                .is_err()
            {
                return;
            }
            self.start_login(SignInReason::Rejected);
            if state
                .wait_for(|state| *state != Connectivity::NeedsLogin)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// Starts the web login unless one is running; a running one keeps its
    /// reason.
    pub fn start_login(self: &Arc<Self>, reason: SignInReason) {
        if self.login_running.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.login_error.lock() = None;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = this.run_login(reason).await {
                // `status` returns `login_error`, and `login` prints it (D071).
                let error = session::redact_fork_selector(&error);
                tracing::warn!(%error, "no new session");
                *this.login_error.lock() = Some(error);
            }
            *this.sign_in.lock() = None;
            this.login_running.store(false, Ordering::Release);
        });
    }

    /// `Request::Login`: starts a login if none runs and waits for its link.
    pub async fn begin_sign_in(self: &Arc<Self>) -> Result<SignIn, String> {
        self.start_login(SignInReason::Requested);
        let deadline = tokio::time::Instant::now() + LINK_WAIT;
        loop {
            if let Some(sign_in) = self.sign_in() {
                return Ok(sign_in);
            }
            if !self.login_running.load(Ordering::Acquire) {
                return Err("the sign-in could not be started; see the log".into());
            }
            if tokio::time::Instant::now() > deadline {
                return Err("no sign-in link within 30 s; see the log".into());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    async fn run_login(self: &Arc<Self>, reason: SignInReason) -> Result<(), String> {
        let mut announced = false;
        let slot = Arc::clone(self);
        let begin = session::begin_web_login(&self.paths, move |url, code| {
            link_received(
                &slot.sign_in,
                &slot.paths.profile,
                reason,
                url,
                code,
                &mut announced,
            );
        });
        let session = tokio::time::timeout(SIGN_IN_TIME + Duration::from_secs(30), begin)
            .await
            .map_err(|_| "the sign-in link expired; `pdmount login` starts a new one".to_owned())?
            // The text starts with "sign-in did not complete" and has no selector.
            .map_err(|error| session::redact_fork_selector(&format!("{error:#}")))?;
        self.adopt(session).await
    }

    /// Continues with a new login's session if it is the mount's account.
    async fn adopt(
        self: &Arc<Self>,
        session: proton_drive_sdk::proton_sdk_rs2::session::ProtonAPISession,
    ) -> Result<(), String> {
        if session.username != self.username {
            let message = format!(
                "signed in as {}, but this mount belongs to {}; sign in as that account",
                session.username, self.username
            );
            session::end_unstored(session).await;
            return Err(message);
        }
        // Lock order: saver, then logged_in (as in `save_tokens`). The saver
        // is held from the store on, so a timer save of the old session
        // cannot overwrite the new one.
        let mut saver = self.saver.lock().await;
        let logged_in = session::store_login(&self.paths, session)
            .await
            .map_err(|error| format!("cannot store the new session: {error:#}"))?;
        let client = session::drive_client(&logged_in.session, &self.device, self.limits)
            .map_err(|error| format!("cannot use the new session: {error:#}"))?;
        let credential = logged_in.session.token_credential.clone();
        // The Photos clients with the rest, in one write (D119).
        let photos = session::photos_clients(&logged_in.session, &self.device, self.limits);
        self.backend
            .replace_session(client, credential.clone(), photos);
        *saver = TokenSaver::of(&logged_in).await;
        self.unsaved.lock().saved();
        *self.logged_in.lock().await = logged_in;
        drop(saver);
        self.watch_tokens(credential);
        tracing::info!("signed in again");
        self.health.kick();
        Ok(())
    }
}

/// Tokens the keyring did not take (D106 amended): the reason while they
/// are not stored, so the log has one WARN per failure streak.
#[derive(Debug, Default)]
struct Unsaved(Option<String>);

impl Unsaved {
    /// A write failed; whether it starts a streak.
    fn failed(&mut self, reason: String) -> bool {
        self.0.replace(reason).is_none()
    }

    /// The tokens are stored; whether that ends a streak.
    fn saved(&mut self) -> bool {
        self.0.take().is_some()
    }
}

/// Whether tokens that are not stored yet are written now, given the
/// collection's state probed without a prompt (`None`: not probed, force
/// mode). Only an unlocked collection takes a write without a dialog; in
/// allow mode an unavailable keyring is no dialog either (the write goes to
/// the file).
fn write_now(mode: SecretsMode, state: Option<&KeyringState>) -> bool {
    match (mode, state) {
        (SecretsMode::ForceFile, _) | (_, None) => true,
        (_, Some(KeyringState::Unlocked)) => true,
        (SecretsMode::AllowFile, Some(KeyringState::Unavailable(_))) => true,
        (_, Some(_)) => false,
    }
}

/// A sign-in link from the SDK: kept for `status` and `login`, which read it
/// over the 0600 control socket. The log and the desktop notification name
/// the code only, never the link (D071): its fragment holds half of the
/// fork secret.
fn link_received(
    slot: &parking_lot::Mutex<Option<SignIn>>,
    profile: &Profile,
    reason: SignInReason,
    url: &str,
    code: &str,
    announced: &mut bool,
) {
    *slot.lock() = Some(SignIn {
        url: url.to_owned(),
        code: code.to_owned(),
    });
    if !std::mem::replace(announced, true) {
        match reason {
            SignInReason::Rejected => tracing::warn!(
                code,
                "sign-in needed: run `pdmount login` or `pdmount status` for the link"
            ),
            SignInReason::Requested => tracing::info!(
                code,
                "sign-in link ready: open it from `pdmount login` or `pdmount status`"
            ),
        }
        let (summary, body) = sign_in_notice(profile, reason);
        notify::desktop(summary, &body);
    }
}

/// The desktop notification for a sign-in: one the daemon found necessary
/// says so, one the user asked for only that its link is ready (D108).
fn sign_in_notice(profile: &Profile, reason: SignInReason) -> (&'static str, String) {
    match reason {
        SignInReason::Rejected => (
            "PdMount: sign in needed",
            format!("Sign-in needed for profile {profile}: run `pdmount login`"),
        ),
        SignInReason::Requested => (
            "PdMount: sign-in link ready",
            format!(
                "Sign-in link ready for profile {profile}: open it from `pdmount login` or \
                 `pdmount status`"
            ),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tracing_subscriber::layer::SubscriberExt;

    const URL: &str = "https://account.proton.me/sign-in#fork-key";

    /// D106 amended, rule 2: one WARN per failure streak, an INFO when a
    /// save ends it.
    #[test]
    fn unsaved_tokens_are_reported_once_per_streak() {
        let mut unsaved = Unsaved::default();
        assert!(!unsaved.saved(), "nothing to end");
        assert!(unsaved.failed("no reply".into()));
        assert!(!unsaved.failed("no reply again".into()));
        assert_eq!(unsaved.0.as_deref(), Some("no reply again"));
        assert!(unsaved.saved());
        assert!(unsaved.0.is_none());
        // A resume that could not store them starts the streak.
        let mut unsaved = Unsaved(Some("no reply".into()));
        assert!(!unsaved.failed("locked".into()));
    }

    /// D106 amended, rule 2: a retry writes only into an unlocked
    /// collection, so it never opens an unlock dialog.
    #[test]
    fn unsaved_tokens_wait_for_an_unlocked_keyring() {
        let unavailable = KeyringState::Unavailable("no Secret Service".into());
        for mode in [SecretsMode::Keyring, SecretsMode::AllowFile] {
            assert!(write_now(mode, Some(&KeyringState::Unlocked)));
            assert!(!write_now(mode, Some(&KeyringState::Locked)));
        }
        assert!(!write_now(SecretsMode::Keyring, Some(&unavailable)));
        // Allow mode writes the file instead, without a dialog.
        assert!(write_now(SecretsMode::AllowFile, Some(&unavailable)));
        assert!(write_now(SecretsMode::ForceFile, None));
    }

    #[test]
    fn the_sign_in_link_stays_out_of_the_log_and_the_notification() {
        let captured = crate::tests::Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false),
        );
        let slot = parking_lot::Mutex::new(None);
        let profile = Profile::default();
        let mut announced = false;
        let rejected = SignInReason::Rejected;
        tracing::subscriber::with_default(subscriber, || {
            link_received(&slot, &profile, rejected, URL, "ABCD-1234", &mut announced);
            link_received(&slot, &profile, rejected, URL, "ABCD-1234", &mut announced);
        });
        let text = captured.text();
        assert!(text.contains("ABCD-1234"), "{text}");
        assert!(!text.contains("account.proton.me"), "{text}");
        assert_eq!(text.matches("sign-in needed").count(), 1, "{text}");
        assert_eq!(
            slot.lock().as_ref().map(|sign_in| sign_in.url.as_str()),
            Some(URL),
            "status and login still get the link"
        );

        for reason in [SignInReason::Rejected, SignInReason::Requested] {
            let (summary, body) = sign_in_notice(&profile, reason);
            assert!(body.contains("pdmount login"), "{body}");
            assert!(!summary.contains("proton.me") && !body.contains("proton.me"));
        }
    }

    /// A sign-in the user asked for is not announced as needed (D108).
    #[test]
    fn the_notification_names_why_the_sign_in_started() {
        let profile = Profile::default();
        let (summary, body) = sign_in_notice(&profile, SignInReason::Rejected);
        assert_eq!(summary, "PdMount: sign in needed");
        assert_eq!(
            body,
            format!("Sign-in needed for profile {profile}: run `pdmount login`")
        );
        let (summary, body) = sign_in_notice(&profile, SignInReason::Requested);
        assert_eq!(summary, "PdMount: sign-in link ready");
        assert_eq!(
            body,
            format!(
                "Sign-in link ready for profile {profile}: open it from `pdmount login` or \
                 `pdmount status`"
            )
        );

        let captured = crate::tests::Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .with_writer(move || writer.clone())
                .with_ansi(false),
        );
        let slot = parking_lot::Mutex::new(None);
        let mut announced = false;
        tracing::subscriber::with_default(subscriber, || {
            let requested = SignInReason::Requested;
            link_received(&slot, &profile, requested, URL, "ABCD-1234", &mut announced);
        });
        let text = captured.text();
        assert!(text.contains("sign-in link ready"), "{text}");
        assert!(!text.contains("sign-in needed"), "{text}");
        assert!(!text.contains("account.proton.me"), "{text}");
    }
}
