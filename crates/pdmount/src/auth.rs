//! The session of a running mount (DESIGN.md §12.3, D042): token rotation
//! stored as it happens, a proactive refresh, and the web login the daemon
//! runs itself when the server rejects the session.

use crate::notify;
use crate::session::{self, LoggedIn, TokenSaver};
use pdmount_backend::proton::ProtonBackend;
use pdmount_core::ProfilePaths;
use pdmount_core::device::DeviceIdentity;
use pdmount_core::health::{Connectivity, Health};
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

/// A sign-in link waiting to be opened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignIn {
    pub url: String,
    pub code: String,
}

pub struct Auth {
    paths: ProfilePaths,
    device: DeviceIdentity,
    backend: Arc<ProtonBackend>,
    health: Arc<Health>,
    /// The account of the mount: a login as anyone else is refused.
    username: String,
    /// Shared by the refresh task, the login task and the token saver.
    logged_in: tokio::sync::Mutex<LoggedIn>,
    saver: tokio::sync::Mutex<TokenSaver>,
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
        backend: Arc<ProtonBackend>,
        health: Arc<Health>,
        logged_in: LoggedIn,
    ) -> Arc<Self> {
        let saver = TokenSaver::new(&logged_in.session).await;
        Arc::new(Self {
            paths,
            device,
            backend,
            health,
            username: logged_in.session.username.clone(),
            logged_in: tokio::sync::Mutex::new(logged_in),
            saver: tokio::sync::Mutex::new(saver),
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
        let credential = self.logged_in.lock().await.session.token_credential.clone();
        self.watch_tokens(credential);
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

    /// Stores the tokens now (shutdown). The keyring may prompt. The snapshot
    /// is taken under the saver lock, so an older one is never written after
    /// a newer one (a rotated-out refresh token is dead).
    pub async fn save_tokens(&self) {
        let mut saver = self.saver.lock().await;
        let stored = {
            let logged_in = self.logged_in.lock().await;
            TokenSaver::snapshot(&logged_in.session).await
        };
        let result = match stored {
            Ok(stored) => saver.save(&self.paths, stored).await,
            Err(error) => Err(error),
        };
        if let Err(error) = result {
            tracing::warn!(error = %format!("{error:#}"), "cannot store refreshed session tokens");
        }
    }

    /// [`Auth::save_tokens`] in a task of its own: a keyring wait of up to
    /// 120 s must not hold up whoever noticed the new tokens.
    pub fn spawn_save(self: &Arc<Self>) {
        let this = Arc::clone(self);
        tokio::spawn(async move { this.save_tokens().await });
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
            self.start_login();
            if state
                .wait_for(|state| *state != Connectivity::NeedsLogin)
                .await
                .is_err()
            {
                return;
            }
        }
    }

    /// Starts the web login unless one is running.
    pub fn start_login(self: &Arc<Self>) {
        if self.login_running.swap(true, Ordering::AcqRel) {
            return;
        }
        *self.login_error.lock() = None;
        let this = Arc::clone(self);
        tokio::spawn(async move {
            if let Err(error) = this.run_login().await {
                tracing::warn!(%error, "no new session");
                *this.login_error.lock() = Some(error);
            }
            *this.sign_in.lock() = None;
            this.login_running.store(false, Ordering::Release);
        });
    }

    /// `Request::Login`: starts a login if needed and waits for its link.
    pub async fn begin_sign_in(self: &Arc<Self>) -> Result<SignIn, String> {
        self.start_login();
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

    async fn run_login(self: &Arc<Self>) -> Result<(), String> {
        let mut announced = false;
        let slot = Arc::clone(self);
        let begin = session::begin_web_login(&self.paths, move |url, code| {
            *slot.sign_in.lock() = Some(SignIn {
                url: url.to_owned(),
                code: code.to_owned(),
            });
            if !announced {
                announced = true;
                tracing::warn!(
                    url,
                    code,
                    "sign-in needed: open the link (or run `pdmount login`)"
                );
                notify::desktop("PdMount: sign in needed", url);
            }
        });
        let session = tokio::time::timeout(SIGN_IN_TIME + Duration::from_secs(30), begin)
            .await
            .map_err(|_| "the sign-in link expired; `pdmount login` starts a new one".to_owned())?
            .map_err(|error| format!("sign-in did not complete: {error:#}"))?;
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
        let client = session::drive_client(&logged_in.session, &self.device)
            .map_err(|error| format!("cannot use the new session: {error:#}"))?;
        let credential = logged_in.session.token_credential.clone();
        self.backend.replace_session(client, credential.clone());
        *saver = TokenSaver::new(&logged_in.session).await;
        *self.logged_in.lock().await = logged_in;
        drop(saver);
        self.watch_tokens(credential);
        tracing::info!("signed in again");
        self.health.kick();
        Ok(())
    }
}
