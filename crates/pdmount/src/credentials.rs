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
use std::time::{Duration, Instant};

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

/// Set once the keyring was unavailable, with the reason: the process stops
/// calling it. Also set by [`hold_keyring`].
static UNAVAILABLE: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// While set, no keyring call of this process waits past it (a service's
/// start, D106).
static DEADLINE: parking_lot::Mutex<Option<Instant>> = parking_lot::Mutex::new(None);

/// Lets the next keyring call through after an earlier failure (the
/// daemon's last token save at shutdown) or a [`hold_keyring`].
pub fn retry_keyring() {
    *UNAVAILABLE.lock() = None;
}

/// Makes every keyring call of this process fail at once as unavailable
/// with `reason`, until [`retry_keyring`]: a service's start that found the
/// collection locked reads nothing, so it opens no unlock dialog (D106).
pub fn hold_keyring(reason: String) {
    *UNAVAILABLE.lock() = Some(reason);
}

/// No keyring call waits past `deadline` (`None`: only the usual timeout).
/// A service's start sets it so that the supervisor's start timeout is never
/// reached through the keyring (D106).
pub fn limit_keyring_until(deadline: Option<Instant>) {
    *DEADLINE.lock() = deadline;
}

/// The timeout of a call starting now: the usual one, cut at the deadline.
fn call_timeout() -> Duration {
    let timeout = keyring_timeout();
    match *DEADLINE.lock() {
        Some(deadline) => timeout.min(deadline.saturating_duration_since(Instant::now())),
        None => timeout,
    }
}

/// `PDMOUNT_KEYRING_TIMEOUT_SECS` shortens the timeout for the keyring tests.
pub fn keyring_timeout() -> Duration {
    std::env::var("PDMOUNT_KEYRING_TIMEOUT_SECS")
        .ok()
        .and_then(|value| value.parse().ok())
        .map_or(KEYRING_TIMEOUT, Duration::from_secs)
}

/// The keyring's answer was "unavailable" somewhere in this error's chain
/// (D029): its reason. A daemon that has mounted before waits for the
/// keyring on such an error instead of exiting (D106).
pub fn unavailable_in(error: &anyhow::Error) -> Option<String> {
    error
        .chain()
        .find_map(|cause| cause.downcast_ref::<Unavailable>())
        .map(|unavailable| unavailable.0.clone())
}

/// What a keyring call most likely waits for when it is slow (D101).
#[derive(Debug, Clone, PartialEq, Eq)]
enum Wait {
    /// A Secret Service that runs: an unlock dialog.
    Prompt,
    /// D-Bus starts the Secret Service for this call (nothing owns the name
    /// yet): the activation `command`, when its file is found, on `address`.
    Starting {
        address: String,
        command: Option<String>,
    },
}

impl Wait {
    /// The note shown after [`KEYRING_NOTICE_AFTER`].
    fn note(&self) -> &'static str {
        match self {
            Self::Prompt => {
                "Waiting for the keyring: an unlock dialog may be open, possibly behind other windows…"
            }
            Self::Starting { .. } => {
                "Waiting for the keyring: D-Bus is starting the Secret Service; once it runs, an \
                 unlock dialog may open, possibly behind other windows…"
            }
        }
    }

    /// A failed call's `error`, said as what it was: a Secret Service that
    /// did not start, with the bus and the command D-Bus ran.
    fn explain(&self, error: String) -> String {
        match self {
            Self::Prompt => error,
            Self::Starting { address, command } => format!(
                "the Secret Service did not start: nothing owned org.freedesktop.secrets on \
                 {address}, D-Bus tried to start it{} and the call failed ({error})",
                command
                    .as_ref()
                    .map_or_else(String::new, |command| format!(" with `{command}`"))
            ),
        }
    }
}

/// The keyring is unavailable with `error`: latches for `operation`s that
/// latch and logs it, as a failed call does.
fn fail(operation: &'static str, error: String) -> Unavailable {
    if latches(operation) {
        *UNAVAILABLE.lock() = Some(format!(
            "it did not answer earlier in this process ({error})"
        ));
    }
    // A probe reports its answer to its caller (`doctor`, `status`, the
    // waiting daemon's timer, D106), which must not log it every minute; a
    // write's caller reports it once per failure streak (D106 amended).
    if matches!(operation, "probe" | "write") {
        tracing::debug!(operation, error, "keyring unavailable");
    } else {
        tracing::warn!(operation, error, "keyring unavailable");
    }
    Unavailable(error)
}

/// Runs a blocking keyring call on its own thread with a timeout; the
/// abandoned thread ends with the process.
fn with_keyring_timeout<T: Send + 'static, E: std::fmt::Display + Send + 'static>(
    operation: &'static str,
    wait: &Wait,
    call: impl FnOnce() -> Result<T, E> + Send + 'static,
) -> Result<T, Unavailable> {
    if let Some(reason) = UNAVAILABLE.lock().clone() {
        return Err(Unavailable(reason));
    }
    let timeout = call_timeout();
    if timeout.is_zero() {
        return Err(Unavailable(
            "no time left for the keyring in this start".into(),
        ));
    }
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(call());
    });
    let answer = match receiver.recv_timeout(KEYRING_NOTICE_AFTER.min(timeout)) {
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            let note = wait.note();
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
    Err(fail(operation, wait.explain(error)))
}

/// Whether a failed call of `operation` makes this process stop calling the
/// keyring. A write does not (D106 amended): it fails after a read worked,
/// so the keyring is there, only slow (gnome-keyring rewrites its whole
/// file), and the daemon's tokens are written again later.
fn latches(operation: &str) -> bool {
    operation != "write"
}

/// The pause before a timed-out write is read back (D106 amended, rule 3).
const WRITE_CHECK_PAUSE: Duration = Duration::from_millis(500);

/// D-Bus's name for a call whose reply did not come in time: the client gave
/// up (`dbus-secret-service` waits 2 s per call), but the service most likely
/// went on and finished.
const NO_REPLY: &str = "org.freedesktop.DBus.Error.NoReply";

/// How a write that timed out ended (D106 amended, rule 3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Checked {
    /// The first call answered.
    Written,
    /// It timed out, but the value read back is the one written.
    ReadBack,
    /// It timed out and the value was not there; the second write worked.
    Rewritten,
}

/// A write whose call timed out is not taken as failed (D106 amended, rule
/// 3): after `pause` the value is read back and compared (equality only,
/// never logged). If it differs or cannot be read, the write is made once
/// more, and a second time-out is read back too. Other errors fail at once.
/// All of it runs inside the caller's timeout.
fn checked_write<E>(
    value: &str,
    pause: Duration,
    mut set: impl FnMut() -> Result<(), E>,
    mut get: impl FnMut() -> Result<Option<String>, E>,
    timed_out: impl Fn(&E) -> bool,
) -> Result<Checked, E> {
    let mut stored = |pause| {
        std::thread::sleep(pause);
        matches!(get(), Ok(Some(found)) if found == value)
    };
    match set() {
        Ok(()) => return Ok(Checked::Written),
        Err(error) if !timed_out(&error) => return Err(error),
        Err(_) if stored(pause) => return Ok(Checked::ReadBack),
        Err(_) => {}
    }
    match set() {
        Ok(()) => Ok(Checked::Rewritten),
        Err(error) if !timed_out(&error) => Err(error),
        Err(_) if stored(pause) => Ok(Checked::Rewritten),
        Err(error) => Err(error),
    }
}

/// Whether a keyring error is D-Bus's `NoReply`: anywhere in its chain, as a
/// `dbus-secret-service` error (the `keyring` crate wraps it).
fn is_no_reply(error: &keyring::Error) -> bool {
    let mut cause: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = cause {
        if let Some(dbus_secret_service::Error::Dbus(dbus)) =
            current.downcast_ref::<dbus_secret_service::Error>()
            && dbus.name() == Some(NO_REPLY)
        {
            return true;
        }
        cause = current.source();
    }
    false
}

/// [`raw_set`] with rule 3's read-back after a time-out, on this process's
/// bus (in the keyring helper too).
fn raw_set_checked(key: &str, value: &str) -> Result<(), keyring::Error> {
    let checked = checked_write(
        value,
        WRITE_CHECK_PAUSE,
        || raw_set(key, value),
        || raw_get(key),
        is_no_reply,
    )?;
    match checked {
        Checked::Written => {}
        Checked::ReadBack => {
            tracing::info!(
                key,
                "a keyring write timed out, but the keyring holds the value"
            );
        }
        Checked::Rewritten => {
            tracing::info!(key, "a keyring write timed out; written again");
        }
    }
    Ok(())
}

// The keyring calls themselves, on the bus of this process's environment.

fn raw_get(key: &str) -> Result<Option<String>, keyring::Error> {
    match keyring::Entry::new(KEYRING_SERVICE, key)?.get_password() {
        Ok(value) => Ok(Some(value)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(error) => Err(error),
    }
}

fn raw_set(key: &str, value: &str) -> Result<(), keyring::Error> {
    keyring::Entry::new(KEYRING_SERVICE, key)?.set_password(value)
}

fn raw_delete(key: &str) -> Result<(), keyring::Error> {
    match keyring::Entry::new(KEYRING_SERVICE, key)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(error) => Err(error),
    }
}

/// Whether the default collection is locked, without prompting.
fn raw_is_locked() -> Result<bool, keyring::Error> {
    use dbus_secret_service::{EncryptionType, SecretService};
    let platform =
        |error: dbus_secret_service::Error| keyring::Error::PlatformFailure(error.into());
    let service = SecretService::connect(EncryptionType::Plain).map_err(platform)?;
    service
        .get_default_collection()
        .and_then(|collection| collection.is_locked())
        .map_err(platform)
}

/// The reason when neither bus has a Secret Service (D102 amended).
const NO_SECRET_SERVICE: &str = "no Secret Service owns or can start org.freedesktop.secrets on \
     the desktop's session bus or on this process's own";

/// How long the ownership check of a call on one bus may take at most (it
/// also stays within the call's own timeout).
const OWN_CHECK_LIMIT: Duration = Duration::from_secs(6);

/// Where a keyring call goes (D101, D102 and its amendments).
enum Target {
    /// The process's own bus, in this process: what a slow call waits for.
    Own(Wait),
    /// Another bus, through the keyring helper with its address in the
    /// child's environment; checked just now (`GetId`, the peer's uid).
    Helper {
        bus: String,
        wait: Wait,
        /// The service's bus (else the attached desktop's): a failed call
        /// says so, and it is not dropped like an attached one.
        service: bool,
    },
}

/// The bus of the next keyring call. With the desktop's bus attached (D102):
/// `bus::route` (owned beats activatable, the attached bus beats the own
/// one). Without one (D102 amended 2026-11-14): the own bus or the
/// service's (`bus::keyring_buses`). On the own bus, D101's check says what
/// the call waits for or fails it at once. An error is the call's answer.
fn target(operation: &'static str) -> Result<Target, Unavailable> {
    // A held or failed keyring is not asked, nor its bus checked.
    if let Some(reason) = UNAVAILABLE.lock().clone() {
        return Err(Unavailable(reason));
    }
    let limit = call_timeout();
    if limit.is_zero() {
        // `with_keyring_timeout` says there is no time left.
        return Ok(Target::Own(Wait::Prompt));
    }
    let limit = limit.min(OWN_CHECK_LIMIT);
    let refused = |reason: String| {
        crate::bus::invalidate();
        fail(operation, reason)
    };
    if crate::bus::attached().is_some() {
        // The chosen attached bus was checked just now: it still leads to a
        // bus of this user (D102); one that does not is dropped.
        return match crate::bus::route_blocking(crate::bus::Service::Keyring) {
            crate::bus::Route::Attached(bus) => Ok(Target::Helper {
                bus,
                wait: Wait::Prompt,
                service: false,
            }),
            crate::bus::Route::Default | crate::bus::Route::Own => {
                own_gate(limit).map(Target::Own).map_err(refused)
            }
            crate::bus::Route::None => Err(Unavailable(NO_SECRET_SERVICE.to_owned())),
        };
    }
    let buses = crate::bus::keyring_buses_blocking(limit);
    match buses.choice {
        crate::bus::Choice::Service => {
            let Some(crate::bus::SecondBus {
                address,
                state: Ok(state),
            }) = buses.service
            else {
                return Err(refused("the service's session bus was not asked".into()));
            };
            // The check before the child runs, as for an attached bus.
            if let Err(error) = crate::bus::verify_blocking(&address) {
                return Err(refused(format!(
                    "the service's session bus does not answer ({error})"
                )));
            }
            let wait = gate(
                crate::bus::OwnState::State {
                    address: format!("the service's session bus {address}"),
                    state,
                },
                activation_command,
            )
            .map_err(refused)?;
            Ok(Target::Helper {
                bus: address,
                wait,
                service: true,
            })
        }
        crate::bus::Choice::None => {
            let own = gate(buses.own, activation_command)
                .err()
                .unwrap_or_default();
            Err(refused(none_found(own, buses.service.as_ref())))
        }
        _ => gate(buses.own, activation_command)
            .map(Target::Own)
            .map_err(refused),
    }
}

/// Why neither bus serves the keyring: the own bus's reason (D101), and what
/// the service's bus said when it was asked.
fn none_found(own: String, service: Option<&crate::bus::SecondBus>) -> String {
    match service {
        None => own,
        Some(crate::bus::SecondBus {
            address,
            state: Err(error),
        }) => format!("{own}; the service's session bus {address} does not answer ({error})"),
        Some(crate::bus::SecondBus { address, .. }) => {
            format!("{own}; nor on the service's session bus {address}")
        }
    }
}

fn activation_command() -> Option<String> {
    crate::bus::activation_command(crate::bus::Service::Keyring.bus_name())
}

/// What a session bus says about the Secret Service, as the call must treat
/// it (D101): it runs, D-Bus starts it, or the call fails at once with the
/// reason.
fn gate(
    own: crate::bus::OwnState,
    command: impl FnOnce() -> Option<String>,
) -> Result<Wait, String> {
    use crate::bus::{NameState, OwnState};
    match own {
        OwnState::NoBus => Err(
            "no session bus: DBUS_SESSION_BUS_ADDRESS is not set and there \
             is no $XDG_RUNTIME_DIR/bus"
                .to_owned(),
        ),
        OwnState::Unreachable(error) => Err(format!("the session bus does not answer ({error})")),
        OwnState::State {
            address,
            state: NameState::Absent,
        } => Err(format!(
            "no Secret Service on the session bus: nothing owns or can start \
             org.freedesktop.secrets on {address}"
        )),
        OwnState::State {
            state: NameState::Owned,
            ..
        } => Ok(Wait::Prompt),
        OwnState::State {
            address,
            state: NameState::Activatable,
        } => Ok(Wait::Starting {
            address,
            command: command(),
        }),
    }
}

/// [`gate`] over the own bus now, within `limit`.
fn own_gate(limit: Duration) -> Result<Wait, String> {
    let own = crate::bus::own_state_blocking(crate::bus::Service::Keyring, limit);
    gate(own, activation_command)
}

/// The bus a keyring call would use now, for `doctor`'s line: its name as
/// the line says it (`None`: the process's own, with no other candidate) and
/// the Secret Service's state there before the probe (`None`: not known).
pub fn keyring_bus() -> (Option<String>, Option<crate::bus::NameState>) {
    use crate::bus::{Choice, OwnState};
    if crate::bus::attached().is_some() {
        return (Some("the desktop's session bus".into()), None);
    }
    let buses = crate::bus::keyring_buses_blocking(OWN_CHECK_LIMIT);
    let own_state = match &buses.own {
        OwnState::State { state, .. } => Some(*state),
        _ => None,
    };
    match (buses.choice, &buses.service) {
        (Choice::Service, Some(second)) => (
            Some(format!("the service's session bus ({})", second.address)),
            second.state.clone().ok(),
        ),
        (Choice::Own, Some(_)) => (Some("this terminal's session bus".into()), own_state),
        _ => (None, own_state),
    }
}

/// A keyring call: `own` in this process on its own bus, or `request` in
/// the keyring helper on the bus [`target`] chose, its reply turned into
/// the answer by `reply`. A failure forgets the cached bus states, so the
/// next call chooses afresh (D102 amended), and drops an attached bus that
/// is gone (logout).
fn call<T: Send + 'static, E: std::fmt::Display + Send + 'static>(
    operation: &'static str,
    request: HelperRequest,
    own: impl FnOnce() -> Result<T, E> + Send + 'static,
    reply: impl FnOnce(HelperReply) -> Result<T, Unavailable>,
) -> Result<T, Unavailable> {
    match target(operation)? {
        Target::Own(wait) => {
            let result = with_keyring_timeout(operation, &wait, own);
            if result.is_err() {
                crate::bus::invalidate();
            }
            result
        }
        Target::Helper { bus, wait, service } => {
            let program = helper_program();
            let timeout = call_timeout();
            let target = bus.clone();
            // An owned service's error names the bus it came from; an
            // activatable one's says so through `wait`.
            let named = service && wait == Wait::Prompt;
            let answer = with_keyring_timeout(operation, &wait, move || {
                let answer = match run_helper(&program, &target, &request, timeout) {
                    Ok(HelperReply::Failed { error }) | Err(error) => Err(error),
                    Ok(reply) => Ok(reply),
                };
                answer.map_err(|error| {
                    if named {
                        format!("on the service's session bus {target}: {error}")
                    } else {
                        error
                    }
                })
            });
            if answer.is_err() {
                crate::bus::invalidate();
                if !service {
                    crate::bus::drop_if_gone(&bus);
                }
            }
            reply(answer?)
        }
    }
}

fn unexpected(reply: &HelperReply) -> Unavailable {
    Unavailable(format!("the keyring helper answered {reply:?}"))
}

impl SecretStore for OsKeyring {
    fn get(&self, key: &str) -> Result<Option<String>, Unavailable> {
        let request = HelperRequest::Get {
            key: key.to_owned(),
        };
        let key = key.to_owned();
        call(
            "read",
            request,
            move || raw_get(&key),
            |reply| match reply {
                HelperReply::Found { value } => Ok(Some(value)),
                HelperReply::Missing => Ok(None),
                other => Err(unexpected(&other)),
            },
        )
    }

    fn set(&self, key: &str, value: &str) -> Result<(), Unavailable> {
        let request = HelperRequest::Set {
            key: key.to_owned(),
            value: value.to_owned(),
        };
        let (key, value) = (key.to_owned(), value.to_owned());
        call(
            "write",
            request,
            move || raw_set_checked(&key, &value),
            |reply| match reply {
                HelperReply::Done => Ok(()),
                other => Err(unexpected(&other)),
            },
        )
    }

    fn delete(&self, key: &str) -> Result<(), Unavailable> {
        let request = HelperRequest::Delete {
            key: key.to_owned(),
        };
        let key = key.to_owned();
        call(
            "delete",
            request,
            move || raw_delete(&key),
            |reply| match reply {
                HelperReply::Done => Ok(()),
                other => Err(unexpected(&other)),
            },
        )
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

/// Probes the Secret Service's default collection (for `doctor`, D104's
/// `read_would_prompt` and the daemon's retries, D106), on the bus every
/// other keyring call of the process would use: through the helper on an
/// attached bus or the service's.
pub fn keyring_state() -> KeyringState {
    let probe = call(
        "probe",
        HelperRequest::State,
        raw_is_locked,
        |reply| match reply {
            HelperReply::State { locked } => Ok(locked),
            other => Err(unexpected(&other)),
        },
    );
    // The probe must not make later commands of this process skip the keyring.
    retry_keyring();
    match probe {
        Ok(false) => KeyringState::Unlocked,
        Ok(true) => KeyringState::Locked,
        Err(Unavailable(reason)) => KeyringState::Unavailable(reason),
    }
}

/// Whether a command that only reads secrets for display must skip them:
/// the keyring's collection is locked, so the read would open an unlock
/// dialog (D104). Force mode never calls the keyring, so `probe` is not run.
/// An unavailable keyring is not locked: it fails at once, without a dialog.
pub fn read_would_prompt(mode: SecretsMode, probe: impl FnOnce() -> KeyringState) -> bool {
    mode != SecretsMode::ForceFile && probe() == KeyringState::Locked
}

// ---- The keyring helper (D102) ---------------------------------------------
//
// `keyring` and `dbus-secret-service` find the bus in the environment only, and
// the daemon must not change its own environment. So with the desktop's bus
// attached, each keyring call runs in a child, `pdmount keyring-helper`, with
// that bus in the child's environment. One JSON request goes to the child's
// stdin, one JSON reply comes from its stdout; key and value never go into
// argv, where other users could read them (D071). The child writes nothing
// else: no log, no stderr.

/// One keyring call for the helper.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HelperRequest {
    Get {
        key: String,
    },
    Set {
        key: String,
        value: String,
    },
    Delete {
        key: String,
    },
    /// Whether the default collection is locked (no prompt).
    State,
}

/// Never prints a secret value.
impl std::fmt::Debug for HelperRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Get { key } => write!(f, "Get {{ key: {key:?} }}"),
            Self::Set { key, .. } => write!(f, "Set {{ key: {key:?}, value: <hidden> }}"),
            Self::Delete { key } => write!(f, "Delete {{ key: {key:?} }}"),
            Self::State => f.write_str("State"),
        }
    }
}

/// The helper's answer.
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "reply", rename_all = "snake_case")]
pub enum HelperReply {
    Found {
        value: String,
    },
    Missing,
    Done,
    State {
        locked: bool,
    },
    /// The keyring could not answer: unavailable (D029).
    Failed {
        error: String,
    },
}

/// Never prints a secret value.
impl std::fmt::Debug for HelperReply {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Found { .. } => f.write_str("Found { value: <hidden> }"),
            Self::Missing => f.write_str("Missing"),
            Self::Done => f.write_str("Done"),
            Self::State { locked } => write!(f, "State {{ locked: {locked} }}"),
            Self::Failed { error } => write!(f, "Failed {{ error: {error:?} }}"),
        }
    }
}

/// A request or reply is a few kilobytes (the session JSON); anything larger
/// is refused.
const HELPER_MAX_BYTES: u64 = 1 << 20;

/// The program the helper runs as: this very binary, also when it was
/// replaced on disk since the start (then the old one, which speaks the same
/// protocol).
fn helper_program() -> PathBuf {
    #[cfg(test)]
    if let Some(program) = TEST_HELPER.get() {
        return program.clone();
    }
    PathBuf::from("/proc/self/exe")
}

/// The keyring tests set the `pdmount` binary here: the test binary is not one.
#[cfg(test)]
pub static TEST_HELPER: OnceLock<PathBuf> = OnceLock::new();

/// Runs `program keyring-helper` on `bus` for one request, and kills it at
/// `timeout`. Errors are the helper's failure to answer, never the keyring's
/// (those come back as [`HelperReply::Failed`]).
pub fn run_helper(
    program: &Path,
    bus: &str,
    request: &HelperRequest,
    timeout: Duration,
) -> Result<HelperReply, String> {
    use std::io::{Read, Write};
    use std::process::{Command, Stdio};
    let deadline = std::time::Instant::now() + timeout;
    let mut child = Command::new(program)
        .arg("keyring-helper")
        .env("DBUS_SESSION_BUS_ADDRESS", bus)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("cannot start the keyring helper: {error}"))?;
    let input = serde_json::to_vec(request).map_err(|error| error.to_string())?;
    // Written on a thread of its own, so that a child that does not read
    // its stdin is still stopped at the timeout below; the pipe closes when
    // the thread ends, or when the child is killed.
    let mut stdin = child.stdin.take().expect("piped");
    let writer = std::thread::spawn(move || stdin.write_all(&input));
    let mut stdout = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        (&mut stdout)
            .take(HELPER_MAX_BYTES)
            .read_to_end(&mut output)
            .map(|_| output)
    });
    // The child is waited on through its handle only, so it is never killed
    // after another process got its pid.
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "no answer within {} s (the keyring helper was stopped)",
                    timeout.as_secs()
                ));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("cannot wait for the keyring helper: {error}"));
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| "the keyring helper's reader panicked".to_owned())?
        .map_err(|error| format!("cannot read the keyring helper's answer: {error}"))?;
    // The child has exited, so the write has ended too. A child that exited
    // before it read the request breaks the pipe; its answer, or the lack of
    // one, says more than the broken pipe does.
    if let Ok(Err(error)) = writer.join()
        && error.kind() != std::io::ErrorKind::BrokenPipe
    {
        return Err(format!(
            "cannot send the request to the keyring helper: {error}"
        ));
    }
    serde_json::from_slice(&output)
        .map_err(|_| format!("the keyring helper gave no answer it could read (exit {status})"))
}

/// Answers one request with the keyring of this process's bus.
fn serve(request: HelperRequest) -> HelperReply {
    let failed = |error: keyring::Error| HelperReply::Failed {
        error: error.to_string(),
    };
    match request {
        HelperRequest::Get { key } => match raw_get(&key) {
            Ok(Some(value)) => HelperReply::Found { value },
            Ok(None) => HelperReply::Missing,
            Err(error) => failed(error),
        },
        HelperRequest::Set { key, value } => raw_set_checked(&key, &value)
            .map(|()| HelperReply::Done)
            .unwrap_or_else(failed),
        HelperRequest::Delete { key } => raw_delete(&key)
            .map(|()| HelperReply::Done)
            .unwrap_or_else(failed),
        HelperRequest::State => raw_is_locked()
            .map(|locked| HelperReply::State { locked })
            .unwrap_or_else(failed),
    }
}

/// `pdmount keyring-helper` (hidden): one request on stdin, one reply on
/// stdout. The parent sets the bus and kills it at its timeout.
pub fn keyring_helper(input: impl std::io::Read, mut output: impl std::io::Write) -> bool {
    use std::io::Read;
    let mut text = Vec::new();
    let reply = match input.take(HELPER_MAX_BYTES).read_to_end(&mut text) {
        Ok(_) => match serde_json::from_slice::<HelperRequest>(&text) {
            Ok(request) => serve(request),
            Err(error) => HelperReply::Failed {
                error: format!("unreadable request: {error}"),
            },
        },
        Err(error) => HelperReply::Failed {
            error: format!("cannot read the request: {error}"),
        },
    };
    let ok = !matches!(reply, HelperReply::Failed { .. });
    let written = serde_json::to_writer(&mut output, &reply).is_ok() && output.flush().is_ok();
    ok && written
}

/// The two secrets of a profile.
#[derive(Debug, Clone, Copy)]
enum Secret {
    Session,
    CacheKey,
}

/// A secret read: found, missing, or unknown because the keyring did not
/// answer and no file holds it (allow mode only; D029).
enum Lookup {
    Found(String, Storage),
    Missing,
    Unavailable(String),
}

pub struct Credentials<'a, S: SecretStore> {
    paths: &'a ProfilePaths,
    store: S,
    mode: SecretsMode,
    /// Another daemon of this profile runs: files are neither moved into the
    /// keyring nor caches deleted under it.
    other_daemon: bool,
    /// A missing cache key may be replaced by a new one (which deletes the
    /// caches); not in a process whose caches are open (D106).
    new_cache_key: bool,
}

/// Set by a daemon once it opens its caches: from then on no cache key is
/// made in this process, since a new one would delete the caches under the
/// running mount (D106).
static CACHES_OPEN: AtomicBool = AtomicBool::new(false);

/// The caches of this process's mount are open: a missing cache key is an
/// error ([`NoCacheKey`]) from now on, never a new key.
pub fn caches_open() {
    CACHES_OPEN.store(true, Ordering::Relaxed);
}

/// The keyring answered but holds no cache key, while this process has the
/// caches open: making one would delete them under the running mount (D106).
#[derive(Debug)]
pub struct NoCacheKey;

impl std::fmt::Display for NoCacheKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the keyring has no cache key for this profile")
    }
}

impl std::error::Error for NoCacheKey {}

impl<'a> Credentials<'a, OsKeyring> {
    pub fn new(paths: &'a ProfilePaths) -> Self {
        Self {
            paths,
            store: OsKeyring,
            mode: mode(),
            other_daemon: crate::daemon::daemon_pid(paths).is_some(),
            new_cache_key: !CACHES_OPEN.load(Ordering::Relaxed),
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
            new_cache_key: true,
        }
    }

    /// Reads without moving files into the keyring (for `doctor`).
    pub fn read_only(mut self) -> Self {
        self.other_daemon = true;
        self
    }

    /// Never makes a new cache key, as after [`caches_open`].
    #[cfg(test)]
    pub fn existing_cache_key_only(mut self) -> Self {
        self.new_cache_key = false;
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
    fn read(&self, secret: Secret) -> anyhow::Result<Lookup> {
        let path = self.file(secret);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let value = Self::from_file(secret, &path, bytes)?;
                if self.mode == SecretsMode::ForceFile || self.other_daemon {
                    return Ok(Lookup::Found(value, Storage::File));
                }
                match self.store.set(&self.key_name(secret), &value) {
                    Ok(()) => {
                        let _ = std::fs::remove_file(&path);
                        tracing::info!(file = %path.display(), "moved secret from file to keyring");
                        Ok(Lookup::Found(value, Storage::Keyring))
                    }
                    Err(error) if self.mode == SecretsMode::Keyring => Err(error.into()),
                    Err(Unavailable(reason)) => {
                        tracing::warn!(error = reason, file = %path.display(), "the secret stays in its file");
                        Ok(Lookup::Found(value, Storage::File))
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if self.mode == SecretsMode::ForceFile {
                    return Ok(Lookup::Missing);
                }
                match self.store.get(&self.key_name(secret)) {
                    Ok(Some(value)) => Ok(Lookup::Found(value, Storage::Keyring)),
                    Ok(None) => Ok(Lookup::Missing),
                    Err(error) if self.mode == SecretsMode::Keyring => Err(error.into()),
                    Err(Unavailable(reason)) => {
                        tracing::warn!(error = reason, "reading secrets from files only");
                        Ok(Lookup::Unavailable(reason))
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
        match self.read(Secret::Session)? {
            Lookup::Found(data, storage) => Ok(Some((parse(&data)?, storage))),
            Lookup::Missing | Lookup::Unavailable(_) => Ok(None),
        }
    }

    /// [`Credentials::load`] for the mount daemon: in allow mode too, a
    /// keyring that did not answer while no file holds the session is an
    /// [`Unavailable`] error, not "not logged in", so that a daemon that has
    /// mounted before waits for the keyring (D106).
    pub fn load_strict(&self) -> anyhow::Result<Option<(StoredCredentials, Storage)>> {
        match self.read(Secret::Session)? {
            Lookup::Found(data, storage) => Ok(Some((parse(&data)?, storage))),
            Lookup::Missing => Ok(None),
            Lookup::Unavailable(reason) => Err(Unavailable(reason).into()),
        }
    }

    pub fn save(&self, credentials: &StoredCredentials) -> anyhow::Result<Storage> {
        self.write(Secret::Session, &serde_json::to_string(credentials)?)
    }

    /// What a resume stores once the server accepted the session (D106
    /// amended): nothing when its tokens are those it loaded (`loaded`, the
    /// session as read and serialized again), else the session; a write
    /// that fails is reported, not an error, since the session works.
    pub fn save_changed(&self, loaded: &str, current: &StoredCredentials) -> SessionWrite {
        let json = match serde_json::to_string(current) {
            Ok(json) if json == loaded => return SessionWrite::Unchanged,
            Ok(json) => json,
            Err(error) => return SessionWrite::Failed(error.to_string()),
        };
        match self.write(Secret::Session, &json) {
            Ok(storage) => SessionWrite::Saved(storage),
            Err(error) => SessionWrite::Failed(write_failure(&error)),
        }
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
    /// deleted. An unavailable keyring is not a missing key (D029): while
    /// caches exist, the key that encrypted them may be in the keyring. Once
    /// this process has its caches open, no key is made ([`caches_open`]): a
    /// missing one is [`NoCacheKey`] (D106).
    pub fn cache_master_key(&self) -> anyhow::Result<Vec<u8>> {
        match self.read(Secret::CacheKey)? {
            Lookup::Found(encoded, _) => return decode_key(&encoded),
            Lookup::Missing if !self.new_cache_key => return Err(NoCacheKey.into()),
            // The Unavailable error stays in the chain (D106).
            Lookup::Unavailable(reason) if self.caches_exist() || !self.new_cache_key => {
                return Err(
                    anyhow::Error::new(Unavailable(reason.clone())).context(format!(
                        "the cache key is not in a file and the keyring is unavailable ({reason}); \
                     unlock the keyring and retry (a new key would delete the existing caches)"
                    )),
                );
            }
            Lookup::Missing | Lookup::Unavailable(_) => {}
        }
        let mut key = vec![0u8; CACHE_KEY_LEN];
        rand::rng().fill_bytes(&mut key);
        self.delete_caches();
        self.write(Secret::CacheKey, &encode_key(&key))?;
        Ok(key)
    }

    /// The SDK cache files and the block cache directory (encrypted with the
    /// cache key).
    fn cache_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = [
            self.paths.sdk_secret_cache(),
            self.paths.sdk_key_store(),
            self.paths.sdk_entity_cache(),
        ]
        .iter()
        .flat_map(|cache| pdmount_core::paths::sqlite_files(cache))
        .collect();
        paths.push(self.paths.blocks_dir());
        paths
    }

    fn caches_exist(&self) -> bool {
        self.cache_paths().iter().any(|path| path.exists())
    }

    fn delete_caches(&self) {
        if self.other_daemon {
            tracing::warn!("new cache key while another daemon runs; its caches are kept");
            return;
        }
        let mut deleted = false;
        for path in self.cache_paths() {
            deleted |= if path.is_dir() {
                std::fs::remove_dir_all(&path).is_ok()
            } else {
                std::fs::remove_file(&path).is_ok()
            };
        }
        if deleted {
            tracing::warn!("new cache key: deleted the caches encrypted with the old one");
        }
    }
}

/// The outcome of [`Credentials::save_changed`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionWrite {
    /// The tokens are those loaded: nothing was written.
    Unchanged,
    Saved(Storage),
    /// Why the new tokens are not stored; they live in memory only.
    Failed(String),
}

/// Why a write of the tokens failed, as `status` shows it: the keyring's own
/// answer, without [`Unavailable`]'s advice about the flags.
pub fn write_failure(error: &anyhow::Error) -> String {
    unavailable_in(error).unwrap_or_else(|| format!("{error:#}"))
}

fn parse(data: &str) -> anyhow::Result<StoredCredentials> {
    serde_json::from_str(data).context("stored session is corrupt; run `pdmount login` again")
}

fn encode_key(key: &[u8]) -> String {
    hex::encode(key)
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
        /// The next this many writes fail (a slow keyring, D106 amended).
        failing_sets: Cell<usize>,
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
            if self.failing_sets.get() > 0 {
                self.failing_sets.set(self.failing_sets.get() - 1);
                return Err(Unavailable("no reply within 2 s".into()));
            }
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

    /// D101: what the own bus's answer means for a call.
    #[test]
    fn the_own_bus_gate_names_what_the_call_waits_for() {
        use crate::bus::{NameState, OwnState};
        let state = |state| OwnState::State {
            address: "unix:path=/run/user/1/bus".into(),
            state,
        };
        let refused = |own| gate(own, || None).expect_err("refused");
        assert!(refused(OwnState::NoBus).contains("no session bus"));
        assert!(refused(OwnState::Unreachable("no answer".into())).contains("does not answer"));
        let none = refused(state(NameState::Absent));
        assert!(
            none.starts_with("no Secret Service on the session bus"),
            "{none}"
        );
        assert!(none.contains("unix:path=/run/user/1/bus"), "{none}");

        assert_eq!(gate(state(NameState::Owned), || None), Ok(Wait::Prompt));
        let starting = gate(state(NameState::Activatable), || {
            Some("/usr/bin/gnome-keyring-daemon --start".into())
        })
        .expect("allowed");
        assert!(
            starting
                .note()
                .contains("D-Bus is starting the Secret Service")
        );
        assert!(Wait::Prompt.note().contains("unlock dialog"));
        // A call that fails after it was only activatable says so, with the
        // bus and the command; one on a running service keeps its error.
        let error = starting.explain("Did not receive a reply".into());
        assert!(
            error.starts_with("the Secret Service did not start"),
            "{error}"
        );
        assert!(error.contains("unix:path=/run/user/1/bus"), "{error}");
        assert!(
            error.contains("`/usr/bin/gnome-keyring-daemon --start`"),
            "{error}"
        );
        assert!(error.contains("Did not receive a reply"), "{error}");
        assert_eq!(Wait::Prompt.explain("boom".into()), "boom");

        // D102 amended 2026-11-14: "none" names the service's bus too.
        use crate::bus::SecondBus;
        assert_eq!(none_found("own".into(), None), "own");
        let absent = SecondBus {
            address: "unix:path=/run/user/1/bus".into(),
            state: Ok(NameState::Absent),
        };
        assert_eq!(
            none_found("own".into(), Some(&absent)),
            "own; nor on the service's session bus unix:path=/run/user/1/bus"
        );
        let silent = SecondBus {
            state: Err("no answer".into()),
            ..absent
        };
        assert!(none_found("own".into(), Some(&silent)).contains("does not answer (no answer)"));
    }

    #[test]
    fn a_locked_keyring_blocks_display_reads_but_force_mode_never_probes() {
        let probed = Cell::new(0);
        let probe = |state: KeyringState| {
            let probed = &probed;
            move || {
                probed.set(probed.get() + 1);
                state
            }
        };
        assert!(read_would_prompt(
            SecretsMode::Keyring,
            probe(KeyringState::Locked)
        ));
        assert!(read_would_prompt(
            SecretsMode::AllowFile,
            probe(KeyringState::Locked)
        ));
        assert!(!read_would_prompt(
            SecretsMode::Keyring,
            probe(KeyringState::Unlocked)
        ));
        assert!(!read_would_prompt(
            SecretsMode::Keyring,
            probe(KeyringState::Unavailable("no session bus".into()))
        ));
        assert_eq!(probed.get(), 4);
        assert!(!read_would_prompt(
            SecretsMode::ForceFile,
            probe(KeyringState::Locked)
        ));
        assert_eq!(probed.get(), 4, "force mode must not call the keyring");
    }

    fn script(dir: &Path, name: &str, text: &str) -> PathBuf {
        let path = dir.join(name);
        crate::test_support::write_executable(&path, text);
        path
    }

    #[test]
    fn helper_messages_round_trip_and_never_show_the_secret() {
        let request = HelperRequest::Set {
            key: "p:session".into(),
            value: "s3cret".into(),
        };
        let text = serde_json::to_string(&request).expect("encode");
        assert_eq!(text, r#"{"op":"set","key":"p:session","value":"s3cret"}"#);
        assert_eq!(
            serde_json::from_str::<HelperRequest>(&text).expect("decode"),
            request
        );
        assert!(!format!("{request:?}").contains("s3cret"));
        let reply = HelperReply::Found {
            value: "s3cret".into(),
        };
        let text = serde_json::to_string(&reply).expect("encode");
        assert_eq!(text, r#"{"reply":"found","value":"s3cret"}"#);
        assert!(!format!("{reply:?}").contains("s3cret"));
        assert_eq!(
            serde_json::to_string(&HelperReply::State { locked: true }).expect("encode"),
            r#"{"reply":"state","locked":true}"#
        );
    }

    #[test]
    fn the_helper_answers_an_unreadable_request_with_a_failure() {
        let mut out = Vec::new();
        assert!(!keyring_helper(&b"{\"op\":\"steal\"}"[..], &mut out));
        match serde_json::from_slice::<HelperReply>(&out).expect("a reply") {
            HelperReply::Failed { error } => assert!(error.contains("unreadable"), "{error}"),
            other => panic!("{other:?}"),
        }
    }

    /// The parent's side of D102's helper: the request goes over stdin, the
    /// bus into the environment, nothing but the subcommand into argv.
    #[test]
    fn the_helper_gets_the_secret_on_stdin_and_the_bus_in_its_environment() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = script(
            dir.path(),
            "helper",
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >\"$0.argv\"\n\
             printf '%s\\n' \"$DBUS_SESSION_BUS_ADDRESS\" >\"$0.bus\"\n\
             cat >\"$0.in\"\nprintf '{\"reply\":\"done\"}'\n",
        );
        let request = HelperRequest::Set {
            key: "p:session".into(),
            value: "s3cret".into(),
        };
        let bus = "unix:path=/run/user/1000/desktop-bus";
        let reply = run_helper(&stub, bus, &request, Duration::from_secs(10)).expect("reply");
        assert_eq!(reply, HelperReply::Done);
        let read = |suffix: &str| {
            std::fs::read_to_string(format!("{}.{suffix}", stub.display())).expect("read")
        };
        assert_eq!(read("argv"), "keyring-helper\n");
        assert_eq!(read("bus"), format!("{bus}\n"));
        assert_eq!(
            serde_json::from_str::<HelperRequest>(&read("in")).expect("request"),
            request
        );
    }

    #[test]
    fn a_helper_that_answers_nonsense_or_nothing_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let nonsense = script(dir.path(), "nonsense", "#!/bin/sh\necho hello\n");
        let silent = script(dir.path(), "silent", "#!/bin/sh\nexit 3\n");
        for stub in [nonsense, silent] {
            let error = run_helper(
                &stub,
                "unix:path=/x",
                &HelperRequest::State,
                Duration::from_secs(10),
            )
            .expect_err("no reply");
            assert!(error.contains("no answer it could read"), "{error}");
        }
        let error = run_helper(
            &dir.path().join("missing"),
            "unix:path=/x",
            &HelperRequest::State,
            Duration::from_secs(10),
        )
        .expect_err("no program");
        assert!(error.contains("cannot start"), "{error}");
    }

    #[test]
    fn a_hung_helper_is_killed_at_the_timeout() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hang = script(
            dir.path(),
            "hang",
            "#!/bin/sh\necho $$ >\"$0.pid\"\nexec sleep 60\n",
        );
        let start = std::time::Instant::now();
        let error = run_helper(
            &hang,
            "unix:path=/x",
            &HelperRequest::State,
            Duration::from_secs(1),
        )
        .expect_err("timeout");
        assert!(error.contains("no answer within 1 s"), "{error}");
        assert!(start.elapsed() < Duration::from_secs(10));
        let pid = std::fs::read_to_string(format!("{}.pid", hang.display())).expect("pid");
        assert!(
            !Path::new(&format!("/proc/{}", pid.trim())).exists(),
            "the helper was not killed and reaped"
        );
    }

    #[test]
    fn an_unavailable_keyring_is_found_in_the_error_chain() {
        let error = anyhow::Error::new(Unavailable("no Secret Service".into())).context("start");
        assert_eq!(unavailable_in(&error).as_deref(), Some("no Secret Service"));
        assert_eq!(unavailable_in(&anyhow::anyhow!("other")), None);
    }

    #[test]
    fn a_strict_load_does_not_take_an_unavailable_keyring_for_no_session() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let unavailable = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &unavailable, SecretsMode::AllowFile);
        assert!(credentials.load().expect("load").is_none());
        let error = credentials.load_strict().expect_err("unavailable");
        assert!(unavailable_in(&error).is_some());
        // The cache key with caches present keeps the cause too.
        std::fs::create_dir_all(&paths.data_dir).expect("mkdir");
        std::fs::write(paths.sdk_secret_cache(), b"sealed").expect("write");
        let error = credentials.cache_master_key().expect_err("unavailable");
        assert!(unavailable_in(&error).is_some(), "{error:#}");
    }

    fn json(credentials: &StoredCredentials) -> String {
        serde_json::to_string(credentials).expect("json")
    }

    fn sample_with_tokens(access: &str) -> StoredCredentials {
        StoredCredentials::new(
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

    /// D106 amended, rule 1: a resume whose tokens did not change writes
    /// nothing, so a slow keyring cannot fail it.
    #[test]
    fn an_unchanged_session_is_not_written() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::Keyring);
        credentials.save(&sample()).expect("save");
        let calls = store.calls.get();
        store.failing_sets.set(1);
        assert_eq!(
            credentials.save_changed(&json(&sample()), &sample()),
            SessionWrite::Unchanged
        );
        assert_eq!(store.calls.get(), calls, "the keyring was not called");
        assert_eq!(store.failing_sets.get(), 1);
        // Refreshed tokens are written.
        store.failing_sets.set(0);
        let refreshed = sample_with_tokens("access-2");
        assert_eq!(
            credentials.save_changed(&json(&sample()), &refreshed),
            SessionWrite::Saved(Storage::Keyring)
        );
        assert_eq!(store.entries.borrow()["test:session"], json(&refreshed));
    }

    /// D106 amended, rule 2: a failed write after the server accepted the
    /// session is reported with the keyring's reason, not an error; the
    /// keyring keeps what it had.
    #[test]
    fn a_failed_write_of_new_tokens_is_reported_not_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        let credentials = Credentials::with_store(&paths, &store, SecretsMode::Keyring);
        credentials.save(&sample()).expect("save");
        store.failing_sets.set(1);
        let refreshed = sample_with_tokens("access-2");
        match credentials.save_changed(&json(&sample()), &refreshed) {
            SessionWrite::Failed(reason) => {
                assert_eq!(reason, "no reply within 2 s");
                assert!(!reason.contains("access-2"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(store.entries.borrow()["test:session"], json(&sample()));
        // The next write goes through: a failed write blocks nothing.
        assert_eq!(
            credentials.save_changed(&json(&sample()), &refreshed),
            SessionWrite::Saved(Storage::Keyring)
        );
    }

    /// D106 amended, rule 2: only reads (and probes) stop later keyring
    /// calls of the process; a write does not.
    #[test]
    fn a_failed_write_does_not_latch_the_keyring() {
        assert!(!latches("write"));
        for operation in ["read", "probe", "delete"] {
            assert!(latches(operation), "{operation}");
        }
    }

    /// D106 amended, rule 3: a write that timed out is read back.
    #[test]
    fn a_timed_out_write_is_read_back_and_retried_once() {
        #[derive(Debug, PartialEq)]
        enum Fault {
            NoReply,
            Denied,
        }
        let pause = Duration::ZERO;
        let run = |sets: Vec<Result<(), Fault>>, reads: Vec<Option<&str>>| {
            let (sets, reads) = (RefCell::new(sets), RefCell::new(reads));
            let (set_calls, get_calls) = (Cell::new(0), Cell::new(0));
            let result = checked_write(
                "new",
                pause,
                || {
                    set_calls.set(set_calls.get() + 1);
                    sets.borrow_mut().remove(0)
                },
                || {
                    get_calls.set(get_calls.get() + 1);
                    Ok(reads.borrow_mut().remove(0).map(str::to_owned))
                },
                |fault| *fault == Fault::NoReply,
            );
            (result, set_calls.get(), get_calls.get())
        };
        assert_eq!(run(vec![Ok(())], vec![]), (Ok(Checked::Written), 1, 0));
        // The keyring finished the write after the client gave up.
        assert_eq!(
            run(vec![Err(Fault::NoReply)], vec![Some("new")]),
            (Ok(Checked::ReadBack), 1, 1)
        );
        // It did not: one more write.
        assert_eq!(
            run(vec![Err(Fault::NoReply), Ok(())], vec![Some("old")]),
            (Ok(Checked::Rewritten), 2, 1)
        );
        assert_eq!(
            run(
                vec![Err(Fault::NoReply), Err(Fault::NoReply)],
                vec![None, Some("new")]
            ),
            (Ok(Checked::Rewritten), 2, 2)
        );
        assert_eq!(
            run(
                vec![Err(Fault::NoReply), Err(Fault::NoReply)],
                vec![None, Some("old")]
            ),
            (Err(Fault::NoReply), 2, 2)
        );
        // Any other error fails at once, unread.
        assert_eq!(
            run(vec![Err(Fault::Denied)], vec![]),
            (Err(Fault::Denied), 1, 0)
        );
        assert_eq!(
            run(vec![Err(Fault::NoReply), Err(Fault::Denied)], vec![None]),
            (Err(Fault::Denied), 2, 1)
        );
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

    /// Review finding 2 (D106): a daemon whose caches are open (the one that
    /// waited for the keyring, or a sign-in through it) never makes a key, so
    /// it never deletes the caches under its own mount.
    #[test]
    fn a_daemon_with_open_caches_never_makes_a_cache_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths(dir.path());
        let block = paths.blocks_dir().join("00").join("block");
        std::fs::create_dir_all(block.parent().expect("parent")).expect("mkdir");
        std::fs::write(&block, b"sealed").expect("write");
        std::fs::create_dir_all(&paths.data_dir).expect("mkdir");
        std::fs::write(paths.sdk_key_store(), b"sealed").expect("write");
        let store = MemoryStore::available();
        for mode in [SecretsMode::Keyring, SecretsMode::AllowFile] {
            let credentials =
                Credentials::with_store(&paths, &store, mode).existing_cache_key_only();
            let error = credentials.cache_master_key().expect_err("no key");
            assert!(
                error.chain().any(|cause| cause.is::<NoCacheKey>()),
                "{error:#}"
            );
        }
        // Without caches either: the running mount has its index open.
        let fresh = tempfile::tempdir().expect("tempdir");
        let fresh_paths = super::tests::paths(fresh.path());
        let unavailable = MemoryStore::default();
        let error = Credentials::with_store(&fresh_paths, &unavailable, SecretsMode::AllowFile)
            .existing_cache_key_only()
            .cache_master_key()
            .expect_err("no key");
        assert!(unavailable_in(&error).is_some(), "{error:#}");
        assert!(!fresh_paths.cache_key_file().exists());
        assert!(block.exists(), "the blocks stay");
        assert!(paths.sdk_key_store().exists(), "sdk-keys.db stays");
        assert!(store.entries.borrow().is_empty(), "no key was stored");
        // Once the key is back it is read as usual.
        let key = Credentials::with_store(&paths, &store, SecretsMode::Keyring)
            .cache_master_key()
            .expect("create");
        let credentials =
            Credentials::with_store(&paths, &store, SecretsMode::Keyring).existing_cache_key_only();
        assert_eq!(credentials.cache_master_key().expect("read"), key);
    }

    #[test]
    fn an_unavailable_keyring_is_not_a_missing_cache_key() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fresh = tempfile::tempdir().expect("tempdir");
        let fresh_paths = paths(fresh.path());
        let paths = paths(dir.path());
        let store = MemoryStore::available();
        let key = Credentials::with_store(&paths, &store, SecretsMode::Keyring)
            .cache_master_key()
            .expect("create");
        std::fs::create_dir_all(&paths.data_dir).expect("mkdir");
        std::fs::write(paths.sdk_secret_cache(), b"sealed").expect("write");

        // Allow mode with the keyring down: the key may be there, so neither a
        // new key nor deleted caches.
        let unavailable = MemoryStore::default();
        let credentials = Credentials::with_store(&paths, &unavailable, SecretsMode::AllowFile);
        assert!(credentials.cache_master_key().is_err());
        assert!(paths.sdk_secret_cache().exists());
        assert!(!paths.cache_key_file().exists());
        let back = Credentials::with_store(&paths, &store, SecretsMode::Keyring);
        assert_eq!(back.cache_master_key().expect("read"), key);

        // Without caches there is nothing to lose: the key goes to a file.
        let credentials =
            Credentials::with_store(&fresh_paths, &unavailable, SecretsMode::AllowFile);
        credentials.cache_master_key().expect("create");
        assert!(fresh_paths.cache_key_file().exists());
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
    /// `PDMOUNT_REPRO_MODE` (`keyring`, `allow` or `force`), optionally
    /// `PDMOUNT_REPRO_PROFILE` (default `test`) and, for `save`,
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
            let mut paths = paths(Path::new(&dir));
            // The keyring entries are named after the profile (`<profile>:session`).
            if let Ok(profile) = std::env::var("PDMOUNT_REPRO_PROFILE") {
                paths.profile = profile.parse().expect("profile");
            }
            paths
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

        /// D102: this process runs on a bus without a Secret Service, the
        /// keyring is on `PDMOUNT_REPRO_ATTACH`'s bus, reached through the
        /// helper (`PDMOUNT_BIN`). Saves, reads, probes and removes there.
        #[test]
        #[ignore = "needs the D024 harness"]
        fn attached() {
            let paths = repro_paths();
            let bus = std::env::var("PDMOUNT_REPRO_ATTACH").expect("PDMOUNT_REPRO_ATTACH");
            let binary = std::env::var_os("PDMOUNT_BIN").expect("PDMOUNT_BIN");
            let _ = TEST_HELPER.set(binary.into());
            let start = Instant::now();
            let direct = keyring_state();
            println!("REPRO direct_state={direct:?}");
            crate::bus::set_attached(Some(bus));
            let user =
                std::env::var("PDMOUNT_REPRO_USER").unwrap_or_else(|_| "h1@example.test".into());
            let credentials = Credentials::new(&paths);
            let result = (|| {
                let state = keyring_state();
                let storage = credentials.save(&sample_for(&user))?;
                let loaded = credentials
                    .load()?
                    .map(|(stored, storage)| format!("{}@{storage}", stored.username()));
                credentials.remove()?;
                let after = credentials.load()?.is_none();
                anyhow::Ok(format!(
                    "state:{state:?},saved:{storage},loaded:{},removed:{after}",
                    loaded.unwrap_or_else(|| "none".into())
                ))
            })();
            report("attached", start, result, &paths);
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
