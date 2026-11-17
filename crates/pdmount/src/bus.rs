//! The desktop's session bus, handed to the daemon by `pdmount session
//! attach` at each desktop login (DESIGN.md §12.3, §14.4, D102).
//!
//! A service started by OpenRC runs on the user bus at
//! `$XDG_RUNTIME_DIR/bus`, while a desktop started by a display manager may
//! run a bus of its own (`dbus-launch`) with the keyring and the notification
//! server on it. The daemon keeps the address it was given and runs
//! `notify-send` and the keyring helper with it in their environment; its own
//! environment never changes (setting a variable in a threaded process is
//! unsound). A call that fails on the kept bus checks whether the bus is still
//! there and, if not, drops the address: until the next attach, the daemon's
//! own bus is used again.
//!
//! Every connection the daemon makes checks that the bus is run by this user
//! (the socket peer's uid), and the kept address is checked so again just
//! before each child runs with it (`attached_verified`): once the desktop's
//! bus has exited, another local user could listen at its address.
//!
//! With an address kept, the bus is chosen per service ([`route`], D102
//! amended): the keyring and the notification server may run on different
//! buses (a keyring that PAM started on the service's bus, a notification
//! server that the window manager started on the desktop's).
//!
//! Without an attached address, a process chooses the keyring's bus between
//! its own and the service's, `$XDG_RUNTIME_DIR/bus`, when that one is
//! another bus ([`keyring_buses`], D102 amended 2026-11-14): a command run
//! in a desktop terminal whose bus only has a gnome-keyring that never
//! answers still reaches the one PAM started on the service's bus.

use std::time::{Duration, Instant};
use zbus::fdo::DBusProxy;

/// Connecting to a bus and each call on it (D102: 2 s).
pub const BUS_TIMEOUT: Duration = Duration::from_secs(2);
/// Longest address `session attach` sends.
const MAX_ADDRESS: usize = 1024;

/// The attached address, set by the control handler; `None`: the daemon's own
/// bus.
static ATTACHED: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// The kept address of the desktop's bus, if one is attached.
pub fn attached() -> Option<String> {
    ATTACHED.lock().clone()
}

/// Keeps `address` (`None`: back to the daemon's own bus, and the saved
/// attach is removed). The services are looked up again. The saved attach
/// is not written: [`keep`] does that for an attach.
pub fn set_attached(address: Option<String>) {
    let mut attached = ATTACHED.lock();
    if address.is_none() {
        unsave();
    }
    *attached = address;
    drop(attached);
    invalidate();
}

/// An attach that keeps `address`: also saved, with the checked `display`,
/// for the next daemon's start (D102 amended). Written under the lock, so a
/// drop of the same address that races with it removes the file after it.
pub fn keep(address: String, display: &Display) {
    let mut attached = ATTACHED.lock();
    if let Some(path) = SAVED_FILE.lock().clone() {
        let saved = Saved {
            address: address.clone(),
            display: display.clone(),
        };
        if let Err(error) = write_saved(&path, &saved) {
            tracing::warn!(
                error = format!("{error:#}"),
                "cannot save the attach for the next start"
            );
        }
    }
    *attached = Some(address);
    drop(attached);
    invalidate();
}

/// Drops `address` if it is still the attached one; whether it was. The
/// saved attach goes with it.
fn detach(address: &str) -> bool {
    let mut attached = ATTACHED.lock();
    if attached.as_deref() == Some(address) {
        *attached = None;
        unsave();
        drop(attached);
        invalidate();
        true
    } else {
        false
    }
}

/// The process's own session bus, from its environment.
pub fn own_address() -> Option<String> {
    std::env::var("DBUS_SESSION_BUS_ADDRESS")
        .ok()
        .filter(|address| !address.is_empty())
}

/// Accepts only a single `unix:path=` or `unix:abstract=` address (with an
/// optional `guid=`): a bus on this machine. Other transports are refused,
/// `unixexec:` above all, which would make the daemon run a program.
pub fn validate(address: &str) -> Result<(), String> {
    if address.is_empty() {
        return Err("the bus address is empty".into());
    }
    if address.len() > MAX_ADDRESS {
        return Err(format!(
            "the bus address is longer than {MAX_ADDRESS} bytes"
        ));
    }
    if address
        .chars()
        .any(|c| c.is_control() || c.is_whitespace() || c == ';')
    {
        return Err(
            "the bus address holds a control character, a space or more than one address".into(),
        );
    }
    let Some(pairs) = address.strip_prefix("unix:") else {
        return Err(format!(
            "only unix:path= and unix:abstract= addresses are accepted, not {address}"
        ));
    };
    let mut socket = None;
    for pair in pairs.split(',') {
        let Some((key, value)) = pair.split_once('=') else {
            return Err(format!("malformed bus address {address}"));
        };
        match key {
            "path" | "abstract" if socket.is_some() => {
                return Err(format!("the bus address {address} names two sockets"));
            }
            "path" if value.starts_with('/') => socket = Some(key),
            "path" => return Err(format!("the socket path in {address} is not absolute")),
            "abstract" if !value.is_empty() => socket = Some(key),
            "guid" => {}
            _ => {
                return Err(format!(
                    "only unix:path= and unix:abstract= addresses are accepted, not {address}"
                ));
            }
        }
    }
    if socket.is_none() {
        return Err(format!(
            "only unix:path= and unix:abstract= addresses are accepted, not {address}"
        ));
    }
    address
        .parse::<zbus::Address>()
        .map(drop)
        .map_err(|error| format!("malformed bus address {address}: {error}"))
}

/// Connects to the bus at `address`, and only to one this user runs: the
/// socket's peer (`SO_PEERCRED`, the process that listens on it) must have
/// this process's uid. A kept address can outlive its bus: after a logout
/// another local user may listen on the same abstract name or `/tmp` path,
/// and would then get the session JSON of a token save (D102).
async fn connect(address: &str) -> Result<zbus::Connection, String> {
    let builder = zbus::connection::Builder::address(address)
        .map_err(|error| format!("malformed bus address {address}: {error}"))?
        .method_timeout(BUS_TIMEOUT);
    let connection = match tokio::time::timeout(BUS_TIMEOUT, builder.build()).await {
        Ok(Ok(connection)) => connection,
        Ok(Err(error)) => return Err(format!("cannot connect to {address}: {error}")),
        Err(_) => {
            return Err(format!(
                "no answer from {address} within {} s",
                BUS_TIMEOUT.as_secs()
            ));
        }
    };
    let peer = match tokio::time::timeout(BUS_TIMEOUT, connection.peer_creds()).await {
        Ok(Ok(credentials)) => credentials.unix_user_id(),
        _ => None,
    };
    owned_by_us(address, peer, nix::unistd::geteuid().as_raw())?;
    Ok(connection)
}

/// Whether a bus whose socket peer has uid `peer` is this user's (`ours`).
fn owned_by_us(address: &str, peer: Option<u32>, ours: u32) -> Result<(), String> {
    match peer {
        Some(uid) if uid == ours => Ok(()),
        Some(uid) => Err(format!(
            "the bus at {address} is run by another user (uid {uid})"
        )),
        None => Err(format!("cannot tell which user runs the bus at {address}")),
    }
}

/// The bus's ID (`org.freedesktop.DBus.GetId`): two addresses name one bus
/// when their IDs are equal.
pub async fn id_of(address: &str) -> Result<String, String> {
    let connection = connect(address).await?;
    let id = async {
        DBusProxy::new(&connection)
            .await?
            .get_id()
            .await
            .map(|id| id.to_string())
    };
    match tokio::time::timeout(BUS_TIMEOUT, id).await {
        Ok(Ok(id)) => Ok(id),
        Ok(Err(error)) => Err(format!("the bus at {address} did not answer: {error}")),
        Err(_) => Err(format!(
            "no answer from {address} within {} s",
            BUS_TIMEOUT.as_secs()
        )),
    }
}

/// Runs `future` from code that is not async: on a thread of its own with
/// its own runtime, so it can be called from a runtime's thread as well.
fn block_on<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Result<T, String> {
    std::thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map(|runtime| runtime.block_on(future))
            .map_err(|error| error.to_string())
    })
    .join()
    .map_err(|_| "the bus check panicked".to_owned())?
}

/// [`id_of`] from code that is not async.
fn id_of_blocking(address: &str) -> Result<String, String> {
    let address = address.to_owned();
    block_on(async move { id_of(&address).await })?
}

// ---- The bus of each service (D102 amended) ------------------------------------

/// A desktop service the daemon reaches over a session bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Service {
    /// The Secret Service.
    Keyring,
    /// The notification server (`notify-send`).
    Notifications,
}

impl Service {
    /// The well-known name the service owns on its bus.
    pub fn bus_name(self) -> &'static str {
        match self {
            Self::Keyring => "org.freedesktop.secrets",
            Self::Notifications => "org.freedesktop.Notifications",
        }
    }

    fn title(self) -> &'static str {
        match self {
            Self::Keyring => "the keyring",
            Self::Notifications => "notifications",
        }
    }

    fn index(self) -> usize {
        match self {
            Self::Keyring => 0,
            Self::Notifications => 1,
        }
    }
}

/// A service's name on one bus.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NameState {
    /// A process owns it and answers the calls.
    Owned,
    /// No owner, but D-Bus can start one (an activation file). On the wrong
    /// bus that start may never answer (D101), so an owner elsewhere wins.
    Activatable,
    Absent,
}

/// Which bus a service's calls go to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    /// The desktop's bus that `session attach` handed over.
    Attached,
    /// The process's own bus (its environment).
    Own,
    /// The service's bus (`$XDG_RUNTIME_DIR/bus`), for a process without an
    /// attached bus whose own bus is another one ([`keyring_buses`]).
    Service,
    /// Neither: the keyring is unavailable, no notification is sent.
    None,
}

impl Choice {
    /// How `status` and `session attach` name it.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Attached => "the desktop's bus",
            Self::Own => "the daemon's own bus",
            Self::Service => "the service's bus",
            Self::None => "none found",
        }
    }
}

/// The rule (D102 amended): the attached bus if the name is owned there,
/// else the own bus if owned there, else the attached bus if the name is
/// activatable there, else the own bus if activatable, else none. Owned
/// beats activatable, since an activatable service on the wrong bus may
/// never answer (D101). `None`: no such bus, or it could not be asked.
pub fn choose(attached: Option<NameState>, own: Option<NameState>) -> Choice {
    match prefer(attached, own) {
        Pick::First => Choice::Attached,
        Pick::Second => Choice::Own,
        Pick::Neither => Choice::None,
    }
}

/// The keyring's bus of a process without an attached bus (D102 amended
/// 2026-11-14): its own bus if the name is owned there, else the service's
/// bus if owned there, else the own bus if activatable, else the service's
/// bus if activatable, else none. `None`: no such bus (no second bus, or the
/// same bus as the own), or it could not be asked.
pub fn choose_without_attach(own: Option<NameState>, service: Option<NameState>) -> Choice {
    match prefer(own, service) {
        Pick::First => Choice::Own,
        Pick::Second => Choice::Service,
        Pick::Neither => Choice::None,
    }
}

/// Which of two buses: [`choose`] and [`choose_without_attach`] differ only
/// in which bus comes first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pick {
    First,
    Second,
    Neither,
}

/// Owned beats activatable; between equals, the first bus wins.
fn prefer(first: Option<NameState>, second: Option<NameState>) -> Pick {
    use NameState::{Activatable, Owned};
    match (first, second) {
        (Some(Owned), _) => Pick::First,
        (_, Some(Owned)) => Pick::Second,
        (Some(Activatable), _) => Pick::First,
        (_, Some(Activatable)) => Pick::Second,
        _ => Pick::Neither,
    }
}

/// Where a service's next call goes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Route {
    /// No address attached: the process's own bus. The keyring's calls
    /// there are gated by [`state_on`] (D101, M5.16).
    Default,
    /// The attached bus, checked just now (`attached_verified`).
    Attached(String),
    /// The own bus, chosen over the attached one.
    Own,
    /// Neither bus has the service.
    None,
}

impl Route {
    /// The choice it stands for ([`Route::Default`] is the own bus).
    pub fn choice(&self) -> Choice {
        match self {
            Self::Default | Self::Own => Choice::Own,
            Self::Attached(_) => Choice::Attached,
            Self::None => Choice::None,
        }
    }
}

/// How long a name's state on a bus is kept: it saves the round trips of
/// each call, and a service that starts or stops is seen within this time.
const NAME_CACHE: Duration = Duration::from_secs(30);

/// `(address, name, state, when asked)`: a handful of entries at most.
type NameEntry = (String, &'static str, NameState, Instant);
static NAMES: parking_lot::Mutex<Vec<NameEntry>> = parking_lot::Mutex::new(Vec::new());

/// `(address, bus ID, when asked)`: the IDs that tell whether the service's
/// bus is another bus than the own ([`keyring_buses`]).
static IDS: parking_lot::Mutex<Vec<(String, String, Instant)>> =
    parking_lot::Mutex::new(Vec::new());

/// Forgets every cached name state and bus ID: after an attach, a drop, or
/// a call that failed on the bus it was routed to.
pub fn invalidate() {
    NAMES.lock().clear();
    IDS.lock().clear();
}

/// [`id_of`], cached for [`NAME_CACHE`] (only to compare buses: a check
/// before a child runs asks [`id_of`] itself).
async fn cached_id(address: &str) -> Result<String, String> {
    let cached = IDS
        .lock()
        .iter()
        .find(|(a, _, at)| a == address && at.elapsed() < NAME_CACHE)
        .map(|(_, id, _)| id.clone());
    if let Some(id) = cached {
        return Ok(id);
    }
    let id = id_of(address).await?;
    let mut ids = IDS.lock();
    ids.retain(|(a, _, _)| a != address);
    ids.push((address.to_owned(), id.clone(), Instant::now()));
    Ok(id)
}

/// Whether `name` is owned or activatable on the bus at `address`
/// (`NameHasOwner`, then `ListActivatableNames`), asked on a connection
/// that passed the user check ([`connect`]); cached for [`NAME_CACHE`]. An
/// error is a bus that is gone, silent or not this user's.
async fn name_state(address: &str, name: &'static str) -> Result<NameState, String> {
    let cached = NAMES
        .lock()
        .iter()
        .find(|(a, n, _, at)| a == address && *n == name && at.elapsed() < NAME_CACHE)
        .map(|(_, _, state, _)| *state);
    if let Some(state) = cached {
        return Ok(state);
    }
    let connection = connect(address).await?;
    let check = async {
        let proxy = DBusProxy::new(&connection).await?;
        let bus_name = zbus::names::BusName::try_from(name)?;
        if proxy.name_has_owner(bus_name).await? {
            return Ok::<_, zbus::Error>(NameState::Owned);
        }
        let activatable = proxy
            .list_activatable_names()
            .await?
            .iter()
            .any(|activatable| activatable.as_str() == name);
        Ok(if activatable {
            NameState::Activatable
        } else {
            NameState::Absent
        })
    };
    let state = match tokio::time::timeout(BUS_TIMEOUT, check).await {
        Ok(Ok(state)) => state,
        Ok(Err(error)) => return Err(format!("the bus at {address} did not answer: {error}")),
        Err(_) => {
            return Err(format!(
                "no answer from {address} within {} s",
                BUS_TIMEOUT.as_secs()
            ));
        }
    };
    let mut names = NAMES.lock();
    names.retain(|(a, n, _, _)| !(a == address && *n == name));
    names.push((address.to_owned(), name, state, Instant::now()));
    Ok(state)
}

/// The bus a process without an attached address reaches: its environment's
/// `DBUS_SESSION_BUS_ADDRESS`, else `$XDG_RUNTIME_DIR/bus` when that socket
/// exists (libdbus's own fallback on systems built for user sessions).
pub fn session_address() -> Option<String> {
    own_address().or_else(|| {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
        let socket = std::path::Path::new(&runtime).join("bus");
        let address = format!("unix:path={}", socket.display());
        (socket.exists() && validate(&address).is_ok()).then_some(address)
    })
}

/// The service's bus on OpenRC and systemd (D100, D102):
/// `unix:path=$XDG_RUNTIME_DIR/bus`, whether or not it exists; `None`
/// without a runtime directory or with a path the address check refuses.
pub fn service_address() -> Option<String> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|dir| !dir.is_empty())?;
    service_address_in(std::path::Path::new(&runtime))
}

/// [`service_address`] for the runtime directory `runtime`.
pub fn service_address_in(runtime: &std::path::Path) -> Option<String> {
    let address = format!("unix:path={}", runtime.join("bus").display());
    validate(&address).is_ok().then_some(address)
}

/// What a process's own session bus says about a service's name (D101).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OwnState {
    /// No address in the environment and no `$XDG_RUNTIME_DIR/bus`.
    NoBus,
    /// The bus did not answer within the limit, or is not this user's.
    Unreachable(String),
    State {
        address: String,
        state: NameState,
    },
}

/// [`OwnState`] of `service` on `address` (`None`: [`session_address`]),
/// within `limit` in all.
pub async fn state_on(address: Option<String>, service: Service, limit: Duration) -> OwnState {
    let Some(address) = address.or_else(session_address) else {
        return OwnState::NoBus;
    };
    match tokio::time::timeout(limit, name_state(&address, service.bus_name())).await {
        Ok(Ok(state)) => OwnState::State { address, state },
        Ok(Err(error)) => OwnState::Unreachable(error),
        Err(_) => OwnState::Unreachable(format!(
            "no answer from {address} within {} s",
            limit.as_secs()
        )),
    }
}

/// [`state_on`] the process's own bus, from code that is not async.
pub fn own_state_blocking(service: Service, limit: Duration) -> OwnState {
    block_on(state_on(None, service, limit)).unwrap_or_else(OwnState::Unreachable)
}

/// The service's bus as the second candidate for the keyring.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SecondBus {
    pub address: String,
    /// The Secret Service's name there, or why it could not be asked.
    pub state: Result<NameState, String>,
}

/// The keyring's buses of a process without an attached bus (D102 amended
/// 2026-11-14).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyringBuses {
    /// The process's own bus.
    pub own: OwnState,
    /// The service's bus, asked only when the own bus does not own the name
    /// and it is another bus (by bus ID, or the own bus cannot be asked).
    pub service: Option<SecondBus>,
    /// [`choose_without_attach`] over the two: `Own`, `Service` or `None`.
    pub choice: Choice,
}

/// The Secret Service's name on the own bus (`own`, `None`:
/// [`session_address`]) and, unless the own bus owns it, on the service's
/// bus at `service` when that is another bus; each bus within `limit`. The
/// answers are cached like [`name_state`]'s, so the next call chooses the
/// same bus without a round trip.
pub async fn keyring_buses(
    own: Option<String>,
    service: Option<String>,
    limit: Duration,
) -> KeyringBuses {
    let own_address = own.or_else(session_address);
    let own = state_on(own_address.clone(), Service::Keyring, limit).await;
    let own_name = match &own {
        OwnState::State { state, .. } => Some(*state),
        _ => None,
    };
    let second = match service {
        Some(service) if own_name != Some(NameState::Owned) => {
            let asked = second_bus(own_address.as_deref(), &service);
            match tokio::time::timeout(limit, asked).await {
                Ok(second) => second,
                Err(_) => Some(SecondBus {
                    state: Err(format!(
                        "no answer from {service} within {} s",
                        limit.as_secs()
                    )),
                    address: service,
                }),
            }
        }
        _ => None,
    };
    let choice = choose_without_attach(
        own_name,
        second.as_ref().and_then(|second| second.state.clone().ok()),
    );
    KeyringBuses {
        own,
        service: second,
        choice,
    }
}

/// The service's bus at `service` with its Secret Service's state, unless it
/// is the own bus at `own` (the same address, or the same bus ID) or its
/// socket file does not exist (no user bus on this system).
async fn second_bus(own: Option<&str>, service: &str) -> Option<SecondBus> {
    if own == Some(service) {
        return None;
    }
    if let Some(path) = service.strip_prefix("unix:path=")
        && !std::path::Path::new(path.split(',').next().unwrap_or(path)).exists()
    {
        return None;
    }
    let theirs = match cached_id(service).await {
        Ok(id) => id,
        Err(error) => {
            return Some(SecondBus {
                address: service.to_owned(),
                state: Err(error),
            });
        }
    };
    if let Some(own) = own
        && cached_id(own).await.ok().as_deref() == Some(theirs.as_str())
    {
        return None;
    }
    Some(SecondBus {
        address: service.to_owned(),
        state: name_state(service, Service::Keyring.bus_name()).await,
    })
}

/// [`keyring_buses`] of this process (its own bus, [`service_address`]),
/// from code that is not async. The choice is logged like [`route`]'s.
pub fn keyring_buses_blocking(limit: Duration) -> KeyringBuses {
    let buses = block_on(keyring_buses(None, service_address(), limit)).unwrap_or_else(|error| {
        KeyringBuses {
            own: OwnState::Unreachable(error),
            service: None,
            choice: Choice::None,
        }
    });
    let address = match buses.choice {
        Choice::Service => buses.service.as_ref().map(|second| second.address.clone()),
        _ => None,
    };
    // "None" is said by the caller's error (D101), not logged here.
    if buses.choice != Choice::None {
        record_choice(Service::Keyring, buses.choice, address);
    }
    buses
}

/// Whether the bus at `address` still answers as a bus of this user, just
/// before a keyring helper runs with it.
pub fn verify_blocking(address: &str) -> Result<(), String> {
    id_of_blocking(address).map(drop)
}

/// Where each service's name stands on the bus at `address`, as `doctor`
/// says it: "the Secret Service owned, notifications activatable".
pub async fn describe_names(address: &str) -> String {
    let mut parts = Vec::new();
    for service in [Service::Keyring, Service::Notifications] {
        let word = match state_on(Some(address.to_owned()), service, BUS_TIMEOUT * 2).await {
            OwnState::State { state, .. } => match state {
                NameState::Owned => "owned",
                NameState::Activatable => "activatable only",
                NameState::Absent => "absent",
            },
            _ => "not answered",
        };
        let title = match service {
            Service::Keyring => "the Secret Service",
            Service::Notifications => "notifications",
        };
        parts.push(format!("{title} {word}"));
    }
    parts.join(", ")
}

/// The `Exec` line of the D-Bus activation file for `name` in the first of
/// `dirs` that has one (`*.service` files with `Name=<name>`).
pub fn activation_command_in(name: &str, dirs: &[std::path::PathBuf]) -> Option<String> {
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut files: Vec<_> = entries
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "service"))
            .collect();
        files.sort();
        for file in files {
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let value = |key: &str| {
                text.lines()
                    .find_map(|line| {
                        line.trim()
                            .strip_prefix(key)?
                            .trim_start()
                            .strip_prefix('=')
                    })
                    .map(|value| value.trim().to_owned())
            };
            if value("Name").as_deref() == Some(name) {
                return value("Exec");
            }
        }
    }
    None
}

/// [`activation_command_in`] the standard service directories of the
/// session bus (`$XDG_DATA_HOME`, `$XDG_DATA_DIRS`, with `dbus-1/services`).
/// A bus with other directories (a private test bus) is not known here.
pub fn activation_command(name: &str) -> Option<String> {
    let home = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .filter(|path| path.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME").map(|home| std::path::Path::new(&home).join(".local/share"))
        });
    let system = std::env::var_os("XDG_DATA_DIRS")
        .filter(|dirs| !dirs.is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let dirs: Vec<_> = home
        .into_iter()
        .chain(std::env::split_paths(&system))
        .map(|dir| dir.join("dbus-1/services"))
        .collect();
    activation_command_in(name, &dirs)
}

/// Whether a notification server owns its name on the bus, or can be
/// started there.
pub async fn has_notification_server(address: &str) -> bool {
    matches!(
        name_state(address, Service::Notifications.bus_name()).await,
        Ok(NameState::Owned | NameState::Activatable)
    )
}

/// The bus for `service`'s next call. Without an attached address, the
/// process's own bus ([`Route::Default`]). With one, [`choose`] over the
/// name's state on the attached bus and on the own bus (asked only when the
/// attached bus does not own it); an attached bus that cannot be asked is
/// dropped, and one that is chosen is checked again just before it is
/// returned. A change of the choice is logged at INFO.
pub async fn route(service: Service) -> Route {
    let route = choose_route(service).await;
    record(service, &route);
    route
}

async fn choose_route(service: Service) -> Route {
    let name = service.bus_name();
    let Some(address) = attached() else {
        return Route::Default;
    };
    let theirs = match name_state(&address, name).await {
        Ok(state) => state,
        Err(error) => {
            forget(&address, &error);
            return Route::Default;
        }
    };
    let own = if theirs == NameState::Owned {
        None
    } else {
        match own_address() {
            Some(own) => name_state(&own, name).await.ok(),
            None => None,
        }
    };
    match choose(Some(theirs), own) {
        // `choose` never names the service's bus.
        Choice::Own | Choice::Service => Route::Own,
        Choice::None => Route::None,
        Choice::Attached => match attached_verified().await {
            Some(address) => Route::Attached(address),
            // Dropped just now: the own bus, as without an attach.
            None => Route::Default,
        },
    }
}

/// [`route`] from code that is not async (on a thread of its own; a few
/// seconds at most).
pub fn route_blocking(service: Service) -> Route {
    block_on(route(service)).unwrap_or(Route::Default)
}

/// The last choice per service, as logged; the own bus at the start.
static CHOSEN: parking_lot::Mutex<[(Choice, Option<String>); 2]> =
    parking_lot::Mutex::new([(Choice::Own, None), (Choice::Own, None)]);

/// The last choice for `service`, without a D-Bus call (for `status`).
pub fn chosen(service: Service) -> Choice {
    CHOSEN.lock()[service.index()].0
}

/// `status`'s line while a desktop's bus is attached: where the keyring and
/// notifications went last. No D-Bus call.
pub fn status_text() -> Option<String> {
    attached()?;
    Some(format!(
        "desktop's bus attached; keyring: {}, notifications: {}",
        chosen(Service::Keyring).describe(),
        chosen(Service::Notifications).describe()
    ))
}

/// Logs a change of `service`'s bus at INFO.
fn record(service: Service, route: &Route) {
    let address = match route {
        Route::Attached(address) => Some(address.clone()),
        _ => None,
    };
    record_choice(service, route.choice(), address);
}

/// [`record`] of a choice, with the address of a bus other than the own.
fn record_choice(service: Service, choice: Choice, address: Option<String>) {
    let now = (choice, address);
    {
        let mut chosen = CHOSEN.lock();
        if chosen[service.index()] == now {
            return;
        }
        chosen[service.index()] = now.clone();
    }
    let title = service.title();
    match (choice, now.1.as_deref()) {
        (Choice::Attached, address) => {
            tracing::info!(address, "{title}: using the desktop's session bus");
        }
        (Choice::Service, address) => {
            tracing::info!(address, "{title}: using the service's session bus");
        }
        (Choice::Own, _) => {
            tracing::info!("{title}: using the daemon's own session bus");
        }
        (Choice::None, _) => tracing::info!(
            "{title}: {} is neither owned nor activatable on the desktop's session bus or the \
             daemon's own; {}",
            service.bus_name(),
            match service {
                Service::Keyring => "the keyring is unavailable",
                Service::Notifications => "no notification is sent",
            }
        ),
    }
}

/// After a call on the attached bus failed: drops the address when the bus
/// is gone or does not answer (the desktop session ended). A bus that answers
/// stays: the call failed for another reason.
pub fn drop_if_gone(address: &str) {
    if let Err(error) = id_of_blocking(address) {
        forget(address, &error);
    }
}

/// [`drop_if_gone`] for async callers.
pub async fn drop_if_gone_async(address: &str) {
    if let Err(error) = id_of(address).await {
        forget(address, &error);
    }
}

/// The attached address, checked just before a child (the keyring helper,
/// `notify-send`) is run with it: it must still lead to a bus of this user
/// that answers ([`connect`]). One that does not is dropped, and `None`
/// sends the caller to the process's own bus. [`route`] makes this check
/// whenever it chooses the attached bus.
async fn attached_verified() -> Option<String> {
    let address = attached()?;
    match id_of(&address).await {
        Ok(_) => Some(address),
        Err(error) => {
            forget(&address, &error);
            None
        }
    }
}

fn forget(address: &str, error: &str) {
    if detach(address) {
        tracing::info!(
            address,
            error,
            "the desktop's session bus is gone or not this user's; using the daemon's own bus until \
             the next attach"
        );
    }
}

// ---- The desktop's display for the own bus (D102 amended) -----------------------

/// The variables `session attach` passes, so that the daemon's own bus
/// starts programs (a keyring's unlock dialog, a notification server) on the
/// desktop's display.
pub const DISPLAY_VARS: [&str; 3] = ["DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY"];
/// Longest value of one of [`DISPLAY_VARS`].
const MAX_DISPLAY_VALUE: usize = 4096;

/// The desktop's display variables: name to value.
pub type Display = std::collections::BTreeMap<String, String>;

/// Why a display variable's value is refused: empty, too long, or with a
/// control character (NUL and newline among them).
fn check_display_value(value: &str) -> Result<(), &'static str> {
    if value.is_empty() {
        Err("empty")
    } else if value.len() > MAX_DISPLAY_VALUE {
        Err("longer than 4096 bytes")
    } else if value.chars().any(char::is_control) {
        Err("holds a control character")
    } else {
        Ok(())
    }
}

/// Those of [`DISPLAY_VARS`] that `lookup` has with a value that passes the
/// check (the client's side of `session attach`).
pub fn display_from(lookup: impl Fn(&str) -> Option<String>) -> Display {
    DISPLAY_VARS
        .iter()
        .filter_map(|name| {
            let value = lookup(name)?;
            check_display_value(&value).ok()?;
            Some(((*name).to_owned(), value))
        })
        .collect()
}

/// [`display_from`] this process's environment (values that are not UTF-8
/// are left out).
pub fn display_from_env() -> Display {
    display_from(|name| std::env::var(name).ok())
}

/// The daemon's side: keeps the entries of `given` with a known name and a
/// value that passes the check, and names each refused one with the reason.
/// A refused entry is left out, not the attach.
pub fn checked_display(given: Display) -> (Display, Vec<String>) {
    let mut kept = Display::new();
    let mut refused = Vec::new();
    for (name, value) in given {
        if !DISPLAY_VARS.contains(&name.as_str()) {
            refused.push(format!("{name:?}: not a display variable"));
        } else if let Err(reason) = check_display_value(&value) {
            refused.push(format!("{name}: {reason}"));
        } else {
            kept.insert(name, value);
        }
    }
    (kept, refused)
}

/// Whether an attach passes the display to the daemon's own bus: only when
/// it kept the desktop's bus (`same` false) and the keyring or the
/// notifications go to the own bus. [`Route::Default`] counts as the own bus
/// (its [`Route::choice`]): after an attach it means that the attached bus
/// was dropped while the route was chosen, so the calls go to the own bus.
pub fn display_for_own_bus(same: bool, keyring: Choice, notifications: Choice) -> bool {
    !same && (keyring == Choice::Own || notifications == Choice::Own)
}

/// Sets `display` in the activation environment of the bus at `address`
/// (`org.freedesktop.DBus.UpdateActivationEnvironment`): the programs that
/// bus starts from then on get it. Variables not in `display` stay as they
/// are, and this process's own environment does not change.
pub async fn update_activation_environment(address: &str, display: &Display) -> Result<(), String> {
    let connection = connect(address).await?;
    let update = async {
        let environment = display
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect();
        DBusProxy::new(&connection)
            .await?
            .update_activation_environment(environment)
            .await
    };
    match tokio::time::timeout(BUS_TIMEOUT, update).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(error)) => Err(format!(
            "the bus at {address} did not take the display: {error}"
        )),
        Err(_) => Err(format!(
            "no answer from {address} within {} s",
            BUS_TIMEOUT.as_secs()
        )),
    }
}

// ---- The attach saved across restarts (D102 amended) ----------------------------

/// Largest saved attach that is read: an address and three display values.
const MAX_SAVED: u64 = 16 * 1024;

/// Where the daemon saves an attach (`ProfilePaths::attach_file`); `None`
/// (other processes, a session without a runtime directory): nothing is
/// saved or removed.
static SAVED_FILE: parking_lot::Mutex<Option<std::path::PathBuf>> = parking_lot::Mutex::new(None);

/// Sets where attaches are saved, at the daemon's start.
pub fn save_attaches_in(path: Option<std::path::PathBuf>) {
    *SAVED_FILE.lock() = path;
}

/// The saved attach: the kept address and the checked display variables.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Saved {
    pub address: String,
    #[serde(default)]
    pub display: Display,
}

/// Writes `saved` to `path`, mode 0600, through a temporary file and a
/// rename.
pub fn write_saved(path: &std::path::Path, saved: &Saved) -> anyhow::Result<()> {
    let json = serde_json::to_vec(saved)?;
    crate::credentials::write_private_file(path, &json)
}

/// Reads a saved attach: `Ok(None)` when there is none. Only a regular file
/// of this user, not a symlink (`O_NOFOLLOW`, checked on the open file), of
/// at most [`MAX_SAVED`] bytes and holding a [`Saved`] is accepted; the
/// error says why one is not. The address and display are not checked here.
pub fn read_saved(path: &std::path::Path) -> Result<Option<Saved>, String> {
    use std::io::Read;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = match std::fs::OpenOptions::new()
        .read(true)
        // No symlink is followed, and a FIFO does not block the open.
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.raw_os_error() == Some(nix::libc::ELOOP) => {
            return Err("it is a symlink".into());
        }
        Err(error) => return Err(format!("cannot open it: {error}")),
    };
    let meta = file
        .metadata()
        .map_err(|error| format!("cannot stat it: {error}"))?;
    if !meta.file_type().is_file() {
        return Err("it is not a regular file".into());
    }
    let ours = nix::unistd::geteuid().as_raw();
    if meta.uid() != ours {
        return Err(format!("it belongs to uid {}, not this user", meta.uid()));
    }
    if meta.len() > MAX_SAVED {
        return Err(format!("it is larger than {MAX_SAVED} bytes"));
    }
    let mut data = Vec::new();
    file.take(MAX_SAVED + 1)
        .read_to_end(&mut data)
        .map_err(|error| format!("cannot read it: {error}"))?;
    if data.len() as u64 > MAX_SAVED {
        return Err(format!("it is larger than {MAX_SAVED} bytes"));
    }
    serde_json::from_slice(&data)
        .map(Some)
        .map_err(|error| format!("it is not a saved attach: {error}"))
}

/// Removes the saved attach at `path`; a symlink itself, never its target.
pub fn remove_saved(path: &std::path::Path) {
    match std::fs::remove_file(path) {
        Ok(()) => tracing::debug!(path = %path.display(), "removed the saved attach"),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "cannot remove the saved attach");
        }
    }
}

/// Removes the daemon's saved attach, if it saves one.
fn unsave() {
    if let Some(path) = SAVED_FILE.lock().clone() {
        remove_saved(&path);
    }
}

/// The daemon's saved attach, if it saves one and the file can be used
/// (see [`read_saved`]). One that cannot is removed and logged at INFO.
pub fn take_saved() -> Option<Saved> {
    let path = SAVED_FILE.lock().clone()?;
    match read_saved(&path) {
        Ok(saved) => saved,
        Err(error) => {
            discard_saved(&error);
            None
        }
    }
}

/// Removes the daemon's saved attach after it failed a check, logged at
/// INFO with the reason.
pub fn discard_saved(error: &str) {
    if let Some(path) = SAVED_FILE.lock().clone() {
        tracing::info!(
            path = %path.display(),
            error,
            "the desktop's session bus saved by the last attach is not used; removed the file"
        );
        remove_saved(&path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The tests that change the process's attach state ([`ATTACHED`],
    /// [`SAVED_FILE`]) take this.
    static GLOBAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

    #[test]
    fn only_local_unix_sockets_are_accepted() {
        for good in [
            "unix:path=/run/user/1000/bus",
            "unix:abstract=/tmp/dbus-AbCdEf,guid=0123456789abcdef0123456789abcdef",
            "unix:path=/tmp/dbus-x,guid=0123456789abcdef0123456789abcdef",
        ] {
            assert_eq!(validate(good), Ok(()), "{good}");
        }
        for bad in [
            "",
            "tcp:host=localhost,port=1234",
            "unixexec:path=/bin/sh,argv1=-c",
            "autolaunch:",
            "unix:tmpdir=/tmp",
            "unix:runtime=yes",
            "unix:path=relative/bus",
            "unix:path=/a,abstract=/b",
            "unix:guid=0123456789abcdef0123456789abcdef",
            "unix:path=/run/user/1000/bus;tcp:host=x,port=1",
            "unix:path=/run/user/1000/bus\n",
            "unix:path=/a b",
            "unix:abstract=",
            "unix:path",
        ] {
            assert!(validate(bad).is_err(), "{bad:?} was accepted");
        }
        assert!(validate(&format!("unix:path=/{}", "a".repeat(MAX_ADDRESS))).is_err());
    }

    #[test]
    fn a_gone_bus_is_dropped_only_if_still_attached() {
        let _global = GLOBAL.lock();
        let gone = "unix:path=/nonexistent/pdmount-test-bus";
        set_attached(Some(gone.to_owned()));
        drop_if_gone(gone);
        assert_eq!(attached(), None);
        // A later attach of another bus is not undone by a late check.
        set_attached(Some("unix:path=/other".to_owned()));
        assert!(!detach(gone));
        assert_eq!(attached().as_deref(), Some("unix:path=/other"));
        // The check before a child runs drops an address that fails it.
        set_attached(Some(gone.to_owned()));
        assert_eq!(block_on(attached_verified()), Ok(None));
        assert_eq!(attached(), None);
        assert_eq!(block_on(attached_verified()), Ok(None), "nothing attached");
        // So does the choice of a service's bus: back to the own bus.
        set_attached(Some(gone.to_owned()));
        assert_eq!(route_blocking(Service::Keyring), Route::Default);
        assert_eq!(attached(), None);
        assert_eq!(route_blocking(Service::Notifications), Route::Default);
        assert_eq!(chosen(Service::Keyring), Choice::Own);
        set_attached(None);
    }

    /// D102: only a bus whose socket peer is this user is used. Running a bus
    /// as another user needs a second account, so the decision is tested here
    /// and the uid check runs on every connection of X13 and the keyring tests.
    #[test]
    fn only_a_bus_of_this_user_is_used() {
        let address = "unix:abstract=/tmp/dbus-AbCdEf";
        assert_eq!(owned_by_us(address, Some(1000), 1000), Ok(()));
        let error = owned_by_us(address, Some(1001), 1000).expect_err("another user");
        assert!(error.contains("another user (uid 1001)"), "{error}");
        assert!(
            owned_by_us(address, Some(0), 1000).is_err(),
            "not even root's"
        );
        assert!(owned_by_us(address, None, 1000).is_err(), "unknown peer");
    }

    /// D102 amended: every combination of a name's state on the attached
    /// bus and on the own bus, `None` for a bus that is missing (no attach,
    /// no own address) or could not be asked.
    #[test]
    fn owned_beats_activatable_and_attached_beats_own() {
        use NameState::{Absent, Activatable, Owned};
        let states = [None, Some(Owned), Some(Activatable), Some(Absent)];
        for attached in states {
            for own in states {
                let expected = match (attached, own) {
                    (Some(Owned), _) => Choice::Attached,
                    (_, Some(Owned)) => Choice::Own,
                    (Some(Activatable), _) => Choice::Attached,
                    (_, Some(Activatable)) => Choice::Own,
                    _ => Choice::None,
                };
                assert_eq!(choose(attached, own), expected, "{attached:?} {own:?}");
            }
        }
        // The cases of the two Gentoo layouts, spelled out.
        assert_eq!(
            choose(Some(Activatable), Some(Owned)),
            Choice::Own,
            "a keyring PAM started on the service's bus beats an activation on the desktop's"
        );
        assert_eq!(
            choose(Some(Owned), Some(Activatable)),
            Choice::Attached,
            "a keyring on the desktop's bus beats an activation on the service's"
        );
        assert_eq!(choose(Some(Owned), Some(Owned)), Choice::Attached);
        assert_eq!(
            choose(Some(Activatable), Some(Activatable)),
            Choice::Attached
        );
        assert_eq!(choose(Some(Absent), Some(Absent)), Choice::None);
        assert_eq!(choose(None, Some(Owned)), Choice::Own, "no attached bus");
        assert_eq!(choose(None, Some(Activatable)), Choice::Own);
        assert_eq!(choose(None, Some(Absent)), Choice::None);
        assert_eq!(choose(None, None), Choice::None);
        assert_eq!(choose(Some(Activatable), None), Choice::Attached);
    }

    /// D102 amended 2026-11-14: without an attached bus, every combination
    /// of the name's state on the own bus and on the service's (`None`: no
    /// second bus, the same bus, or one that could not be asked).
    #[test]
    fn without_an_attach_owned_beats_activatable_and_own_beats_service() {
        use NameState::{Absent, Activatable, Owned};
        let states = [None, Some(Owned), Some(Activatable), Some(Absent)];
        for own in states {
            for service in states {
                let expected = match (own, service) {
                    (Some(Owned), _) => Choice::Own,
                    (_, Some(Owned)) => Choice::Service,
                    (Some(Activatable), _) => Choice::Own,
                    (_, Some(Activatable)) => Choice::Service,
                    _ => Choice::None,
                };
                assert_eq!(
                    choose_without_attach(own, service),
                    expected,
                    "{own:?} {service:?}"
                );
            }
        }
        assert_eq!(
            choose_without_attach(Some(Activatable), Some(Owned)),
            Choice::Service,
            "the Gentoo/i3 case: PAM's keyring on the service's bus beats the desktop's activation"
        );
        assert_eq!(choose_without_attach(Some(Owned), Some(Owned)), Choice::Own);
        assert_eq!(choose_without_attach(None, None), Choice::None);
        assert_eq!(choose_without_attach(Some(Absent), None), Choice::None);
        assert_eq!(
            choose_without_attach(None, Some(Activatable)),
            Choice::Service
        );
    }

    /// D102 amended: `session attach` sends only the display variables that
    /// are set with a usable value.
    #[test]
    fn the_client_sends_only_set_display_variables() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == name)
                    .map(|(_, v)| (*v).to_owned())
            }
        };
        let display = display_from(env(&[
            ("DISPLAY", ":0"),
            ("XAUTHORITY", "/home/u/.Xauthority"),
            ("HOME", "/home/u"),
        ]));
        assert_eq!(
            display.into_iter().collect::<Vec<_>>(),
            [
                ("DISPLAY".to_owned(), ":0".to_owned()),
                ("XAUTHORITY".to_owned(), "/home/u/.Xauthority".to_owned()),
            ]
        );
        let display = display_from(env(&[
            ("DISPLAY", ""),
            ("WAYLAND_DISPLAY", "wayland-1"),
            ("XAUTHORITY", "/a\nb"),
        ]));
        assert_eq!(
            display.into_iter().collect::<Vec<_>>(),
            [("WAYLAND_DISPLAY".to_owned(), "wayland-1".to_owned())]
        );
        assert!(display_from(env(&[])).is_empty());
    }

    /// D102 amended: the daemon keeps only the three names with a checked
    /// value, and names what it refused.
    #[test]
    fn the_daemon_checks_the_display_it_is_given() {
        let given: Display = [
            ("DISPLAY", ":1"),
            ("WAYLAND_DISPLAY", "/run/user/1000/wayland-0"),
            ("XAUTHORITY", "/tmp/x\0y"),
            ("LD_PRELOAD", "/tmp/evil.so"),
            ("PATH", "/tmp"),
        ]
        .into_iter()
        .map(|(n, v)| (n.to_owned(), v.to_owned()))
        .collect();
        let (kept, refused) = checked_display(given);
        assert_eq!(
            kept.keys().map(String::as_str).collect::<Vec<_>>(),
            ["DISPLAY", "WAYLAND_DISPLAY"]
        );
        assert_eq!(refused.len(), 3, "{refused:?}");
        assert!(refused.iter().any(|r| r.contains("LD_PRELOAD")));
        assert!(
            refused
                .iter()
                .any(|r| r.contains("XAUTHORITY: holds a control"))
        );
        for (value, ok) in [
            ("x".repeat(MAX_DISPLAY_VALUE), true),
            ("x".repeat(MAX_DISPLAY_VALUE + 1), false),
            (String::new(), false),
            ("a\rb".to_owned(), false),
            ("localhost:10.0".to_owned(), true),
        ] {
            let (kept, _) = checked_display(Display::from([("DISPLAY".to_owned(), value)]));
            assert_eq!(kept.len() == 1, ok);
        }
    }

    #[test]
    fn the_display_goes_to_the_own_bus_only_when_a_service_uses_it() {
        use Choice::{Attached, None as Neither, Own};
        assert!(display_for_own_bus(false, Own, Attached));
        assert!(display_for_own_bus(false, Attached, Own));
        assert!(display_for_own_bus(false, Neither, Own));
        assert!(!display_for_own_bus(false, Attached, Attached));
        assert!(!display_for_own_bus(false, Neither, Attached));
        assert!(!display_for_own_bus(false, Neither, Neither));
        assert!(
            !display_for_own_bus(true, Own, Own),
            "the same bus: a no-op"
        );
        // An attached bus dropped while the route was chosen: the own bus.
        assert!(display_for_own_bus(
            false,
            Route::Default.choice(),
            Attached
        ));
    }

    /// A private `dbus-daemon`, stopped when dropped.
    struct PrivateBus {
        child: std::process::Child,
        address: String,
    }

    impl Drop for PrivateBus {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// Starts a session bus listening in `dir`, with `dir/services` as its
    /// service directory and its messages in `dir/bus.log`.
    fn private_bus(dbus_daemon: &std::path::Path, dir: &std::path::Path) -> PrivateBus {
        let socket = dir.join("bus");
        let config = dir.join("bus.conf");
        std::fs::write(
            &config,
            format!(
                r#"<!DOCTYPE busconfig PUBLIC "-//freedesktop//DTD D-Bus Bus Configuration 1.0//EN"
 "http://www.freedesktop.org/standards/dbus/1.0/busconfig.dtd">
<busconfig>
  <type>session</type>
  <listen>unix:path={}</listen>
  <auth>EXTERNAL</auth>
  <servicedir>{}</servicedir>
  <policy context="default">
    <allow send_destination="*" eavesdrop="true"/>
    <allow eavesdrop="true"/>
    <allow own="*"/>
  </policy>
</busconfig>
"#,
                socket.display(),
                dir.join("services").display()
            ),
        )
        .expect("bus config");
        let log = dir.join("bus.log");
        let child = std::process::Command::new(dbus_daemon)
            .arg(format!("--config-file={}", config.display()))
            .args(["--nofork", "--nopidfile"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).expect("bus log"))
            .spawn()
            .expect("start dbus-daemon");
        let bus = PrivateBus {
            child,
            address: format!("unix:path={}", socket.display()),
        };
        let start = Instant::now();
        while !socket.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "dbus-daemon did not listen: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        bus
    }

    /// D102 amended: a program the bus starts after the update gets the
    /// display; a variable missing from a later update keeps its value.
    #[tokio::test]
    async fn the_own_bus_starts_programs_on_the_given_display() {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let services = dir.path().join("services");
        std::fs::create_dir(&services).expect("mkdir");
        let probe = dir.path().join("probe.sh");
        let out = dir.path().join("display.out");
        crate::test_support::write_executable(
            &probe,
            &format!(
                "#!/bin/sh\nprintf '%s %s' \"$DISPLAY\" \"$XAUTHORITY\" >'{}.tmp' && mv '{0}.tmp' '{0}'\n",
                out.display()
            ),
        );
        let name = "org.pdmount.Test.DisplayProbe";
        std::fs::write(
            services.join(format!("{name}.service")),
            // Through sh: the temporary directory may be mounted noexec.
            format!(
                "[D-BUS Service]\nName={name}\nExec=/bin/sh {}\n",
                probe.display()
            ),
        )
        .expect("service file");
        let bus = private_bus(&dbus_daemon, dir.path());

        let display = |pairs: &[(&str, &str)]| -> Display {
            pairs
                .iter()
                .map(|(n, v)| ((*n).to_owned(), (*v).to_owned()))
                .collect()
        };
        update_activation_environment(
            &bus.address,
            &display(&[("DISPLAY", ":41"), ("XAUTHORITY", "/x/.Xauthority")]),
        )
        .await
        .expect("first update");
        update_activation_environment(&bus.address, &display(&[("DISPLAY", ":42")]))
            .await
            .expect("second update");

        // The probe exits without taking its name, so the start reports an
        // error; what counts is what the probe wrote.
        let connection = connect(&bus.address).await.expect("connect");
        let proxy = DBusProxy::new(&connection).await.expect("proxy");
        let _ = proxy
            .start_service_by_name(zbus::names::WellKnownName::try_from(name).expect("name"), 0)
            .await;
        let start = Instant::now();
        while !out.exists() {
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "the probe did not run"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert_eq!(
            std::fs::read_to_string(&out).expect("read"),
            ":42 /x/.Xauthority"
        );

        let error = update_activation_environment(
            "unix:path=/nonexistent/pdmount-test-bus",
            &display(&[("DISPLAY", ":1")]),
        )
        .await
        .expect_err("no bus there");
        assert!(error.contains("cannot connect"), "{error}");
    }

    /// A private bus whose `services` directory holds the activation file
    /// of `org.freedesktop.secrets` with `exec` (no file: no provider).
    fn secrets_bus(
        dbus_daemon: &std::path::Path,
        exec: Option<&str>,
    ) -> (tempfile::TempDir, PrivateBus) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir(dir.path().join("services")).expect("mkdir");
        if let Some(exec) = exec {
            std::fs::write(
                dir.path().join("services/org.freedesktop.secrets.service"),
                format!("[D-BUS Service]\nName=org.freedesktop.secrets\nExec={exec}\n"),
            )
            .expect("service file");
        }
        let bus = private_bus(dbus_daemon, dir.path());
        (dir, bus)
    }

    /// D101: the ownership check on a private bus with no provider, with an
    /// activation file whose command never claims the name, and (when
    /// gnome-keyring is installed) with a Secret Service that runs.
    #[tokio::test]
    async fn the_secret_service_is_absent_activatable_or_owned() {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let limit = Duration::from_secs(5);
        let state = |address: &str| {
            let address = address.to_owned();
            async move { state_on(Some(address), Service::Keyring, limit).await }
        };
        let is = |state: &OwnState, wanted: NameState| matches!(state, OwnState::State { state, .. } if *state == wanted);

        // No provider at all.
        let (_none_dir, none) = secrets_bus(&dbus_daemon, None);
        let answer = state(&none.address).await;
        assert!(is(&answer, NameState::Absent), "{answer:?}");

        // Activatable, and the command (through sh: the directory may be
        // noexec) never claims the name.
        let (dir, never) = secrets_bus(&dbus_daemon, Some("/bin/sh -c 'sleep 30'"));
        let started = Instant::now();
        let answer = state(&never.address).await;
        assert!(is(&answer, NameState::Activatable), "{answer:?}");
        // The check starts nothing and waits for nothing.
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(
            activation_command_in("org.freedesktop.secrets", &[dir.path().join("services")])
                .as_deref(),
            Some("/bin/sh -c 'sleep 30'")
        );

        // A bus that does not exist.
        let answer = state("unix:path=/nonexistent/pdmount-test-bus").await;
        assert!(matches!(answer, OwnState::Unreachable(_)), "{answer:?}");

        // gnome-keyring on a bus of its own.
        let Some(gnome_keyring) = crate::doctor::find_in_path("gnome-keyring-daemon") else {
            eprintln!("gnome-keyring-daemon not installed; the owned case is skipped");
            return;
        };
        let (home, bus) = secrets_bus(&dbus_daemon, None);
        let mut keyring = KillOnDrop(
            std::process::Command::new(gnome_keyring)
                .args(["--foreground", "--components=secrets"])
                .env("DBUS_SESSION_BUS_ADDRESS", &bus.address)
                .env("HOME", home.path())
                .env("XDG_RUNTIME_DIR", home.path())
                .env("XDG_DATA_HOME", home.path().join("data"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("start gnome-keyring-daemon"),
        );
        let start = Instant::now();
        loop {
            invalidate();
            let answer = state(&bus.address).await;
            if is(&answer, NameState::Owned) {
                break;
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "gnome-keyring never owned its name: {answer:?}"
            );
            assert!(keyring.0.try_wait().expect("wait").is_none(), "it exited");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let names = describe_names(&bus.address).await;
        assert!(names.contains("the Secret Service owned"), "{names}");
    }

    /// gnome-keyring on the bus at `address`, with its files in `home`;
    /// waits until it owns its name. `None` when it is not installed.
    async fn gnome_keyring_on(address: &str, home: &std::path::Path) -> Option<KillOnDrop> {
        let gnome_keyring = crate::doctor::find_in_path("gnome-keyring-daemon")?;
        let mut keyring = KillOnDrop(
            std::process::Command::new(gnome_keyring)
                .args(["--foreground", "--components=secrets"])
                .env("DBUS_SESSION_BUS_ADDRESS", address)
                .env("HOME", home)
                .env("XDG_RUNTIME_DIR", home)
                .env("XDG_DATA_HOME", home.join("data"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .expect("start gnome-keyring-daemon"),
        );
        let start = Instant::now();
        loop {
            invalidate();
            let answer = state_on(
                Some(address.to_owned()),
                Service::Keyring,
                Duration::from_secs(5),
            )
            .await;
            if matches!(
                answer,
                OwnState::State {
                    state: NameState::Owned,
                    ..
                }
            ) {
                return Some(keyring);
            }
            assert!(
                start.elapsed() < Duration::from_secs(15),
                "gnome-keyring never owned its name: {answer:?}"
            );
            assert!(keyring.0.try_wait().expect("wait").is_none(), "it exited");
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    /// D102 amended 2026-11-14: a process without an attached bus whose own
    /// bus A only has an activatable Secret Service that never answers
    /// takes the service's bus B where gnome-keyring runs; with B the same
    /// bus as A, or without a Secret Service, it stays on A.
    #[tokio::test]
    async fn without_an_attach_the_keyring_on_the_services_bus_wins() {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let limit = Duration::from_secs(5);
        let (a_dir, a) = secrets_bus(&dbus_daemon, Some("/bin/sh -c 'sleep 30'"));
        let (b_dir, b) = secrets_bus(&dbus_daemon, None);
        let activatable_a = |buses: &KeyringBuses| {
            matches!(
                buses.own,
                OwnState::State {
                    state: NameState::Activatable,
                    ..
                }
            )
        };

        // B without a Secret Service: A's activation, which never answers,
        // is all there is.
        let buses = keyring_buses(Some(a.address.clone()), Some(b.address.clone()), limit).await;
        assert!(activatable_a(&buses), "{buses:?}");
        assert_eq!(
            buses.service,
            Some(SecondBus {
                address: b.address.clone(),
                state: Ok(NameState::Absent)
            })
        );
        assert_eq!(buses.choice, Choice::Own);
        // The service's bus under the own bus's address is not a second bus.
        let buses = keyring_buses(Some(a.address.clone()), Some(a.address.clone()), limit).await;
        assert_eq!((buses.service, buses.choice), (None, Choice::Own));
        // A service's bus without a socket is no candidate; one that does
        // not answer is not chosen.
        let gone = "unix:path=/nonexistent/pdmount-test-bus".to_owned();
        let buses = keyring_buses(Some(a.address.clone()), Some(gone), limit).await;
        assert_eq!((buses.service, buses.choice), (None, Choice::Own));
        let silent = a_dir.path().join("silent");
        let _listener = std::os::unix::net::UnixListener::bind(&silent).expect("bind");
        let silent = format!("unix:path={}", silent.display());
        let buses = keyring_buses(Some(a.address.clone()), Some(silent), limit).await;
        assert!(
            matches!(&buses.service, Some(SecondBus { state: Err(_), .. })),
            "{buses:?}"
        );
        assert_eq!(buses.choice, Choice::Own);

        let Some(_keyring) = gnome_keyring_on(&b.address, b_dir.path()).await else {
            eprintln!("gnome-keyring-daemon not installed; the owned case is skipped");
            return;
        };
        invalidate();
        let buses = keyring_buses(Some(a.address.clone()), Some(b.address.clone()), limit).await;
        assert!(activatable_a(&buses), "{buses:?}");
        assert_eq!(
            buses.service,
            Some(SecondBus {
                address: b.address.clone(),
                state: Ok(NameState::Owned)
            })
        );
        assert_eq!(buses.choice, Choice::Service);
        // From the cache: the same choice.
        let again = keyring_buses(Some(a.address.clone()), Some(b.address.clone()), limit).await;
        assert_eq!(again, buses);
        // The other way round, the own bus owns it: B is not asked.
        let buses = keyring_buses(Some(b.address.clone()), Some(a.address.clone()), limit).await;
        assert_eq!((buses.service, buses.choice), (None, Choice::Own));
    }

    /// Stops the child when dropped.
    struct KillOnDrop(std::process::Child);

    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn the_activation_command_comes_from_the_file_that_names_the_service() {
        let dir = tempfile::tempdir().expect("tempdir");
        let write = |name: &str, text: &str| std::fs::write(dir.path().join(name), text).unwrap();
        write(
            "a.service",
            "[D-BUS Service]\nName=org.example.Other\nExec=/bin/other\n",
        );
        write(
            "b.service",
            "[D-BUS Service]\nName = org.freedesktop.secrets\nExec = /usr/bin/gnome-keyring-daemon --start\n",
        );
        write(
            "c.txt",
            "Name=org.freedesktop.secrets\nExec=/bin/not-a-service-file\n",
        );
        let dirs = [
            std::path::PathBuf::from("/nonexistent"),
            dir.path().to_owned(),
        ];
        assert_eq!(
            activation_command_in("org.freedesktop.secrets", &dirs).as_deref(),
            Some("/usr/bin/gnome-keyring-daemon --start")
        );
        assert_eq!(activation_command_in("org.example.Missing", &dirs), None);
    }

    /// D100: the line for the same bus, another bus that is attached, and
    /// one that is not, from the IDs of two private buses.
    #[tokio::test]
    async fn the_desktop_bus_line_follows_the_bus_ids() {
        use crate::doctor::{Attach, Level, classify_desktop_bus};
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let (_a_dir, a) = secrets_bus(&dbus_daemon, None);
        let (_b_dir, b) = secrets_bus(&dbus_daemon, None);
        let (_c_dir, c) = secrets_bus(&dbus_daemon, None);
        let id = |address: &str| {
            let address = address.to_owned();
            async move { id_of(&address).await }
        };
        let (service, desktop, other) = (
            id(&a.address).await,
            id(&b.address).await,
            id(&c.address).await,
        );
        assert!(service.is_ok() && desktop.is_ok() && other.is_ok());
        assert_ne!(service, desktop);
        let line = |terminal: &Result<String, String>, attach: Attach| {
            classify_desktop_bus(terminal.clone(), service.clone(), &attach, None)
        };

        // One bus under two names (the same address twice) is the same bus.
        let same = line(&id(&a.address).await, Attach::NoDaemon);
        assert_eq!(
            (same.level, same.detail.starts_with("same as the service's")),
            (Level::Ok, true),
            "{same}"
        );
        // Another bus, handed over, is attached; the daemon's other desktop
        // bus, an unreachable one, none, and no daemon are warnings.
        let attached = line(&desktop, Attach::Attached(desktop.clone()));
        assert_eq!(attached.level, Level::Ok, "{attached}");
        assert!(attached.detail.starts_with("attached"), "{attached}");
        for attach in [
            Attach::Attached(other.clone()),
            Attach::Attached(Err("gone".into())),
            Attach::NotAttached,
            Attach::NoDaemon,
        ] {
            let check = line(&desktop, attach);
            assert_eq!(check.level, Level::Warn, "{check}");
            assert!(check.detail.starts_with("not attached"), "{check}");
        }
        let not_attached = line(&desktop, Attach::NotAttached);
        assert!(
            not_attached.detail.contains("pdmount session attach"),
            "{not_attached}"
        );
        // A bus that cannot be asked is a warning that says so.
        let gone = id("unix:path=/nonexistent/pdmount-test-bus").await;
        let check = classify_desktop_bus(gone, service.clone(), &Attach::NotAttached, None);
        assert_eq!(check.level, Level::Warn, "{check}");
        assert!(
            check.detail.contains("this terminal's session bus"),
            "{check}"
        );
        let check = classify_desktop_bus(
            desktop.clone(),
            Err("no answer".into()),
            &Attach::NotAttached,
            None,
        );
        assert!(
            check.detail.contains("the service's session bus"),
            "{check}"
        );
    }

    #[tokio::test]
    async fn an_unreachable_bus_has_no_id() {
        let error = id_of("unix:path=/nonexistent/pdmount-test-bus")
            .await
            .expect_err("no bus there");
        assert!(error.contains("cannot connect"), "{error}");
        assert!(!has_notification_server("unix:path=/nonexistent/pdmount-test-bus").await);
    }

    fn saved(address: &str) -> Saved {
        Saved {
            address: address.to_owned(),
            display: Display::from([("DISPLAY".to_owned(), ":7".to_owned())]),
        }
    }

    /// D102 amended: the saved attach is written privately and read back.
    #[test]
    fn a_saved_attach_round_trips_privately() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("pdmount").join("p.attach");
        assert_eq!(read_saved(&path), Ok(None), "none saved");
        let one = saved("unix:path=/run/user/1000/bus-a");
        write_saved(&path, &one).expect("write");
        let mode = std::fs::metadata(&path).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(read_saved(&path), Ok(Some(one)));
        // A second save replaces the first; no temporary file is left.
        let two = Saved {
            address: "unix:abstract=/tmp/dbus-x".to_owned(),
            display: Display::new(),
        };
        write_saved(&path, &two).expect("write");
        assert_eq!(read_saved(&path), Ok(Some(two)));
        let names: Vec<_> = std::fs::read_dir(path.parent().expect("dir"))
            .expect("list")
            .map(|entry| entry.expect("entry").file_name())
            .collect();
        assert_eq!(names, ["p.attach"]);
        // The display may be missing from a file written by hand.
        std::fs::write(&path, r#"{"address":"unix:path=/b"}"#).expect("write");
        assert_eq!(
            read_saved(&path),
            Ok(Some(Saved {
                address: "unix:path=/b".to_owned(),
                display: Display::new()
            }))
        );
    }

    /// D102 amended: only a regular file of this user, not a symlink, of at
    /// most 16 KiB and holding a saved attach is read; a symlink is removed
    /// itself, its target stays.
    #[test]
    fn a_saved_attach_that_is_not_a_small_regular_json_file_is_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.attach");
        let target = dir.path().join("target");
        write_saved(&target, &saved("unix:path=/b")).expect("write");
        std::os::unix::fs::symlink(&target, &path).expect("symlink");
        let error = read_saved(&path).expect_err("a symlink");
        assert!(error.contains("symlink"), "{error}");
        remove_saved(&path);
        assert!(
            std::fs::symlink_metadata(&path).is_err(),
            "the link is gone"
        );
        assert!(target.exists(), "its target stays");

        std::fs::write(&path, vec![b' '; MAX_SAVED as usize + 1]).expect("write");
        let error = read_saved(&path).expect_err("too large");
        assert!(error.contains("larger than 16384"), "{error}");
        // At the limit it is read (and here refused as no JSON object).
        std::fs::write(&path, vec![b' '; MAX_SAVED as usize]).expect("write");
        let error = read_saved(&path).expect_err("blank");
        assert!(error.contains("not a saved attach"), "{error}");
        std::fs::write(&path, "unix:path=/b\n").expect("write");
        let error = read_saved(&path).expect_err("not JSON");
        assert!(error.contains("not a saved attach"), "{error}");
        std::fs::write(&path, r#"{"display":{}}"#).expect("write");
        assert!(read_saved(&path).is_err(), "no address");

        std::fs::remove_file(&path).expect("rm");
        nix::unistd::mkfifo(&path, nix::sys::stat::Mode::from_bits_truncate(0o600))
            .expect("mkfifo");
        let error = read_saved(&path).expect_err("a FIFO");
        assert!(error.contains("not a regular file"), "{error}");
        std::fs::remove_file(&path).expect("rm");
        std::fs::create_dir(&path).expect("mkdir");
        assert!(read_saved(&path).is_err(), "a directory");
    }

    /// D102 amended: an attach that keeps an address saves it; one of the own
    /// bus and a dropped address remove it; a late drop of an older address
    /// leaves a newer attach's file; a file that cannot be read is removed.
    #[test]
    fn the_saved_attach_follows_the_kept_address() {
        let _global = GLOBAL.lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.attach");
        save_attaches_in(Some(path.clone()));
        let first = "unix:path=/nonexistent/pdmount-test-bus-1";
        let display = Display::from([("DISPLAY".to_owned(), ":7".to_owned())]);
        keep(first.to_owned(), &display);
        assert_eq!(attached().as_deref(), Some(first));
        assert_eq!(
            read_saved(&path),
            Ok(Some(Saved {
                address: first.to_owned(),
                display: display.clone()
            }))
        );
        // An attach of the daemon's own bus.
        set_attached(None);
        assert!(!path.exists(), "removed with the kept address");
        // The kept bus is gone.
        keep(first.to_owned(), &display);
        drop_if_gone(first);
        assert_eq!(attached(), None);
        assert!(!path.exists(), "removed when the bus is gone");
        // A late check of an older address leaves the newer attach.
        let second = "unix:path=/nonexistent/pdmount-test-bus-2";
        keep(second.to_owned(), &Display::new());
        assert!(!detach(first));
        assert_eq!(
            read_saved(&path).map(|s| s.map(|s| s.address)),
            Ok(Some(second.to_owned()))
        );
        // The start: a usable file is taken and stays; a broken one is removed.
        assert_eq!(take_saved().map(|s| s.address), Some(second.to_owned()));
        assert!(path.exists());
        std::fs::write(&path, "garbage").expect("write");
        assert_eq!(take_saved(), None);
        assert!(!path.exists(), "a broken file is removed");
        assert_eq!(take_saved(), None, "none saved");
        // Without a path (another process), nothing is saved.
        save_attaches_in(None);
        keep(second.to_owned(), &Display::new());
        assert!(!path.exists());
        set_attached(None);
    }

    fn runtime_paths(dir: &std::path::Path) -> pdmount_core::ProfilePaths {
        let base = pdmount_core::paths::BaseDirs {
            config: dir.join("config"),
            data: dir.join("data"),
            cache: dir.join("cache"),
            state: dir.join("state"),
            runtime: Some(dir.join("run")),
        };
        pdmount_core::ProfilePaths::new("restore".parse().expect("profile"), &base)
    }

    /// D102 amended: at the start a saved attach of a live bus of this user
    /// is kept again; one of a bus that is gone, or with an address of
    /// another transport, is removed.
    #[test]
    fn a_start_takes_the_saved_attach_of_a_live_bus_only() {
        let _global = GLOBAL.lock();
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = runtime_paths(dir.path());
        // On a thread of its own: the lock above is not held across an await.
        let restore = || {
            let paths = paths.clone();
            block_on(async move { crate::keywait::restore_saved_attach(&paths).await })
                .expect("restore");
        };
        let file = paths.attach_file().expect("runtime dir");

        // A file pointing at a socket nobody listens on.
        write_saved(&file, &saved("unix:path=/nonexistent/pdmount-test-bus")).expect("write");
        restore();
        assert_eq!(attached(), None);
        assert!(!file.exists(), "a dead bus's file is removed");
        // A transport that is never used.
        write_saved(&file, &saved("tcp:host=localhost,port=1")).expect("write");
        restore();
        assert_eq!(attached(), None);
        assert!(!file.exists(), "a refused address's file is removed");
        // No file: nothing happens.
        restore();
        assert_eq!(attached(), None);

        if let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") {
            let bus_dir = tempfile::tempdir().expect("tempdir");
            std::fs::create_dir(bus_dir.path().join("services")).expect("mkdir");
            let bus = private_bus(&dbus_daemon, bus_dir.path());
            // No display: the test's own session bus, if any, is not changed.
            let live = Saved {
                address: bus.address.clone(),
                display: Display::new(),
            };
            write_saved(&file, &live).expect("write");
            restore();
            assert_eq!(attached().as_deref(), Some(bus.address.as_str()));
            assert_eq!(read_saved(&file), Ok(Some(live)), "a used file stays");
        } else {
            eprintln!("dbus-daemon not installed; the live part is skipped");
        }
        save_attaches_in(None);
        set_attached(None);
    }

    /// D102 amended 2026-11-14: `session attach` without a daemon saves the
    /// desktop's bus for the next start, and that start takes it; nothing is
    /// saved for the service's own bus or without a runtime directory.
    #[test]
    fn an_attach_without_a_daemon_is_taken_at_the_next_start() {
        let _global = GLOBAL.lock();
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = runtime_paths(dir.path());
        let file = paths.attach_file().expect("runtime dir");
        let (_desktop_dir, desktop) = secrets_bus(&dbus_daemon, None);
        let display = Display::from([("DISPLAY".to_owned(), ":5".to_owned())]);
        let save = |address: &str, service: Option<String>, file: Option<std::path::PathBuf>| {
            let (address, display) = (address.to_owned(), display.clone());
            block_on(async move {
                crate::keywait::save_for_next_start(
                    &address,
                    service.as_deref(),
                    file.as_deref(),
                    &display,
                )
                .await
                .map_err(|error| format!("{error:#}"))
            })
            .expect("thread")
        };

        // The desktop's bus is the service's: nothing to save.
        assert_eq!(
            save(
                &desktop.address,
                Some(desktop.address.clone()),
                Some(file.clone())
            ),
            Ok(false)
        );
        assert!(!file.exists());
        // No runtime directory: nothing to save.
        assert_eq!(save(&desktop.address, None, None), Ok(false));
        // A desktop's bus that does not answer is refused.
        let error = save(
            "unix:path=/nonexistent/pdmount-test-bus",
            None,
            Some(file.clone()),
        )
        .expect_err("a dead bus");
        assert!(error.contains("cannot use that bus"), "{error}");
        assert!(!file.exists());
        // Another bus, the service's not answering (not up yet): saved.
        let service = service_address_in(&dir.path().join("run"));
        assert_eq!(
            save(&desktop.address, service, Some(file.clone())),
            Ok(true)
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&file).expect("stat").permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(
            read_saved(&file),
            Ok(Some(Saved {
                address: desktop.address.clone(),
                display: display.clone()
            }))
        );
        // The daemon's start takes it. Saved again without a display: the
        // restore may pass one to this test's own session bus, if it has one.
        let service = service_address_in(&dir.path().join("run"));
        let no_display = Display::new();
        let saved_again = block_on({
            let (address, file) = (desktop.address.clone(), file.clone());
            async move {
                crate::keywait::save_for_next_start(
                    &address,
                    service.as_deref(),
                    Some(&file),
                    &no_display,
                )
                .await
                .map_err(|error| format!("{error:#}"))
            }
        })
        .expect("thread");
        assert_eq!(saved_again, Ok(true));
        let restore = {
            let paths = paths.clone();
            move || {
                let paths = paths.clone();
                block_on(async move { crate::keywait::restore_saved_attach(&paths).await })
                    .expect("restore");
            }
        };
        restore();
        assert_eq!(attached().as_deref(), Some(desktop.address.as_str()));
        assert!(file.exists(), "a used file stays");
        save_attaches_in(None);
        set_attached(None);
    }
}
