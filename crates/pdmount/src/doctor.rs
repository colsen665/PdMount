//! `pdmount doctor` (DESIGN.md §14.2): checks the local prerequisites for
//! mounting and signing in. Any failed check makes the command exit 1;
//! warnings do not.

use crate::bus::NameState;
use crate::credentials::{self, Credentials, KeyringState, SecretsMode};
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use std::fmt;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    pub name: &'static str,
    pub level: Level,
    pub detail: String,
}

impl Check {
    pub fn new(name: &'static str, level: Level, detail: impl Into<String>) -> Self {
        Self {
            name,
            level,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let tag = match self.level {
            Level::Ok => "ok  ",
            Level::Warn => "WARN",
            Level::Fail => "FAIL",
        };
        write!(f, "[{tag}] {:<12} {}", self.name, self.detail)
    }
}

/// Free space below which a warning is shown.
const MIN_FREE_SPACE: u64 = 1 << 30;
/// Clock difference to the server above which sign-in and tokens can fail.
const MAX_CLOCK_SKEW: Duration = Duration::from_secs(60);

fn check_fuse_device(path: &Path) -> Check {
    use nix::unistd::{AccessFlags, access};
    match std::fs::metadata(path) {
        Ok(meta) if !meta.file_type().is_char_device() => Check::new(
            "fuse device",
            Level::Fail,
            format!("{} is not a character device", path.display()),
        ),
        Ok(_) => match access(path, AccessFlags::R_OK | AccessFlags::W_OK) {
            Ok(()) => Check::new(
                "fuse device",
                Level::Ok,
                format!("{} is accessible", path.display()),
            ),
            Err(error) => Check::new(
                "fuse device",
                Level::Fail,
                format!("{} is not readable and writable: {error}", path.display()),
            ),
        },
        Err(error) => Check::new(
            "fuse device",
            Level::Fail,
            format!("{}: {error} (load the fuse kernel module)", path.display()),
        ),
    }
}

pub(crate) fn find_in_path(program: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(program))
            .find(|candidate| candidate.is_file())
    })
}

/// fusermount3 must be setuid root for unprivileged mounts.
fn classify_fusermount(path: Option<&Path>, mode: u32, uid: u32) -> Check {
    match path {
        None => Check::new(
            "fusermount3",
            Level::Fail,
            "not found in PATH (install fuse 3)",
        ),
        Some(path) if mode & 0o4000 != 0 && uid == 0 => Check::new(
            "fusermount3",
            Level::Ok,
            format!("{} (setuid root)", path.display()),
        ),
        Some(path) => Check::new(
            "fusermount3",
            Level::Fail,
            format!(
                "{} is not setuid root; unprivileged mounts will fail",
                path.display()
            ),
        ),
    }
}

fn check_fusermount() -> Check {
    let path = find_in_path("fusermount3");
    let (mode, uid) = path
        .as_deref()
        .and_then(|p| std::fs::metadata(p).ok())
        .map_or((0, u32::MAX), |meta| (meta.mode(), meta.uid()));
    classify_fusermount(path.as_deref(), mode, uid)
}

/// Whether `/etc/fuse.conf` enables `user_allow_other`.
pub fn fuse_conf_allows_other(text: &str) -> bool {
    text.lines()
        .map(|line| line.split('#').next().unwrap_or("").trim())
        .any(|line| line == "user_allow_other")
}

fn check_fuse_conf(path: &Path) -> Check {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    if fuse_conf_allows_other(&text) {
        Check::new(
            "fuse.conf",
            Level::Ok,
            "user_allow_other is set; AutoUnmount can be used",
        )
    } else {
        Check::new(
            "fuse.conf",
            Level::Warn,
            format!(
                "user_allow_other is not set in {}: no AutoUnmount, so a crashed daemon leaves a \
                 stale mount until `pdmount unmount --force` (decision D020)",
                path.display()
            ),
        )
    }
}

fn check_config(paths: &ProfilePaths) -> Check {
    let path = paths.config_file();
    match Config::load(&path) {
        Ok(_) if path.exists() => Check::new("config", Level::Ok, path.display().to_string()),
        Ok(_) => Check::new(
            "config",
            Level::Ok,
            format!("{} not present, using defaults", path.display()),
        ),
        Err(error) => Check::new("config", Level::Fail, error.to_string()),
    }
}

/// Reads the keyring's lock state without prompting; `None` in force mode.
fn check_keyring(mode: SecretsMode) -> (Check, Option<KeyringState>) {
    if mode == SecretsMode::ForceFile {
        return (
            Check::new("keyring", Level::Ok, format!("not used: {mode}")),
            None,
        );
    }
    // Asked first: the probe may start an activatable Secret Service (D101).
    // The probe then uses the same bus, from the cache (D102 amended).
    let (bus, name) = credentials::keyring_bus();
    let state = credentials::keyring_state();
    (
        classify_keyring(mode, &state, name, bus.as_deref()),
        Some(state),
    )
}

/// `name`: the Secret Service's name on the chosen bus before the probe
/// (D101); `None` when no bus answered. `bus`: that bus, as the line names
/// it, when there was a choice (`None`: the process's own, the only one).
fn classify_keyring(
    mode: SecretsMode,
    state: &KeyringState,
    name: Option<NameState>,
    bus: Option<&str>,
) -> Check {
    let started = name == Some(NameState::Activatable);
    let on = bus.map_or_else(String::new, |bus| format!(" on {bus}"));
    match state {
        KeyringState::Unlocked if started => Check::new(
            "keyring",
            Level::Ok,
            format!(
                "Secret Service not running before this check, started by D-Bus activation{on}; \
                 default collection unlocked; secrets: {mode}"
            ),
        ),
        KeyringState::Unlocked => Check::new(
            "keyring",
            Level::Ok,
            format!("Secret Service running{on}, default collection unlocked; secrets: {mode}"),
        ),
        KeyringState::Locked => Check::new(
            "keyring",
            Level::Warn,
            format!(
                "Secret Service {}{on}; the default collection is locked; the next command that \
                 reads secrets opens an unlock dialog",
                if started {
                    "started by D-Bus activation"
                } else {
                    "running"
                }
            ),
        ),
        KeyringState::Unavailable(reason) if mode == SecretsMode::Keyring => Check::new(
            "keyring",
            Level::Fail,
            format!(
                "unavailable ({reason}); pass --allow-file-secrets (files when the keyring is \
                 unavailable) or --force-file-secrets (files only)"
            ),
        ),
        KeyringState::Unavailable(reason) => Check::new(
            "keyring",
            Level::Warn,
            format!("unavailable ({reason}); secrets go to mode-0600 files"),
        ),
    }
}

fn check_login(paths: &ProfilePaths, keyring: Option<&KeyringState>) -> Check {
    // Reading a locked keyring would open an unlock dialog.
    if keyring == Some(&KeyringState::Locked) && !paths.credentials_file().exists() {
        return Check::new(
            "login",
            Level::Warn,
            "unknown: the keyring is locked (not read, to avoid an unlock dialog)",
        );
    }
    match Credentials::new(paths).read_only().load() {
        Ok(Some((_, storage))) => Check::new(
            "login",
            Level::Ok,
            format!("a session is stored ({storage})"),
        ),
        Ok(None) => Check::new("login", Level::Warn, "not logged in; run `pdmount login`"),
        Err(error) => Check::new("login", Level::Fail, format!("{error:#}")),
    }
}

/// Nearest existing ancestor, for checking space before directories exist.
fn existing_ancestor(path: &Path) -> &Path {
    path.ancestors()
        .find(|candidate| candidate.exists())
        .unwrap_or(Path::new("/"))
}

pub fn classify_free_space(name: &'static str, dir: &Path, free: u64) -> Check {
    let detail = format!("{} free at {}", ByteSize(free).human(), dir.display());
    if free < MIN_FREE_SPACE {
        Check::new(name, Level::Warn, detail)
    } else {
        Check::new(name, Level::Ok, detail)
    }
}

fn check_free_space(name: &'static str, dir: &Path) -> Check {
    let dir = existing_ancestor(dir);
    match nix::sys::statvfs::statvfs(dir) {
        Ok(stat) => classify_free_space(name, dir, stat.blocks_available() * stat.fragment_size()),
        Err(error) => Check::new(
            name,
            Level::Warn,
            format!("cannot check {}: {error}", dir.display()),
        ),
    }
}

pub fn classify_clock_skew(skew: chrono::TimeDelta) -> Check {
    let seconds = skew.num_seconds().unsigned_abs();
    let detail = format!("{seconds} s difference to the Proton API");
    if Duration::from_secs(seconds) > MAX_CLOCK_SKEW {
        Check::new(
            "clock",
            Level::Warn,
            format!("{detail}; sync the system clock"),
        )
    } else {
        Check::new("clock", Level::Ok, detail)
    }
}

/// An error with its sources, `outer: inner: …`.
fn error_chain(error: &dyn std::error::Error) -> String {
    let mut text = error.to_string();
    let mut source = error.source();
    while let Some(cause) = source {
        text.push_str(": ");
        text.push_str(&cause.to_string());
        source = cause.source();
    }
    text
}

/// Unauthenticated endpoint that answers with a `Date` header.
const PING_URL: &str = "https://drive-api.proton.me/core/v4/tests/ping";

async fn check_clock() -> Check {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
    {
        Ok(client) => client,
        Err(error) => return Check::new("clock", Level::Warn, format!("cannot check: {error}")),
    };
    let response = match client.get(PING_URL).send().await {
        Ok(response) => response,
        Err(error) => {
            return Check::new(
                "clock",
                Level::Warn,
                format!("cannot reach the Proton API: {}", error_chain(&error)),
            );
        }
    };
    let server_time = response
        .headers()
        .get(reqwest::header::DATE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| chrono::DateTime::parse_from_rfc2822(value).ok());
    match server_time {
        Some(server) => {
            classify_clock_skew(chrono::Utc::now() - server.with_timezone(&chrono::Utc))
        }
        None => Check::new("clock", Level::Warn, "the Proton API sent no Date header"),
    }
}

fn version_check() -> Check {
    Check::new(
        "version",
        Level::Ok,
        format!(
            "pdmount {}, x-pm-appversion {}",
            env!("CARGO_PKG_VERSION"),
            crate::session::app_version()
        ),
    )
}

/// The autostart entry; a warning when a user service starts the daemon too (D093).
fn autostart_check(paths: &ProfilePaths) -> Check {
    match crate::supervise::double_start(paths) {
        Some(conflict) => Check::new("autostart", Level::Warn, conflict),
        None => Check::new("autostart", Level::Ok, crate::autostart::state_text(paths)),
    }
}

/// The user service; a warning when its daemon runs outside OpenRC's state (D109).
fn service_check(paths: &ProfilePaths) -> Check {
    match crate::supervise::openrc_stray(paths) {
        Some(stray) => Check::new("service", Level::Warn, stray),
        None => Check::new(
            "service",
            Level::Ok,
            crate::supervise::service_state_text(paths),
        ),
    }
}

/// The mountpoints a profile's config names: My Files' and the further
/// roots', `~/` expanded.
fn configured_mountpoints(config: &Config, home: &Path) -> Vec<PathBuf> {
    std::iter::once(config.mountpoint(home))
        .chain(config.root_mountpoints(home))
        .collect()
}

/// The other profiles whose config puts a mount at one of this profile's
/// mountpoints (D127), each with the first such mountpoint. A profile is
/// one with a config file or a data directory; one whose config does not
/// load is left out.
pub fn profiles_sharing_a_mountpoint(
    paths: &ProfilePaths,
    config: &Config,
    home: &Path,
) -> Vec<(String, PathBuf)> {
    let mut names = std::collections::BTreeSet::new();
    if let Ok(dir) = std::fs::read_dir(&paths.config_dir) {
        names.extend(dir.flatten().filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            Some(name.strip_suffix(".toml")?.to_owned())
        }));
    }
    if let Some(Ok(dir)) = paths.data_dir.parent().map(std::fs::read_dir) {
        names.extend(
            dir.flatten()
                .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
                .filter_map(|entry| entry.file_name().into_string().ok()),
        );
    }
    let mine = configured_mountpoints(config, home);
    names
        .into_iter()
        .filter(|name| {
            name.parse::<pdmount_core::Profile>().is_ok() && name != paths.profile.as_str()
        })
        .filter_map(|name| {
            let other = Config::load(&paths.config_dir.join(format!("{name}.toml"))).ok()?;
            let shared = configured_mountpoints(&other, home)
                .into_iter()
                .find(|mountpoint| mine.contains(mountpoint))?;
            Some((name, shared))
        })
        .collect()
}

/// The text for profiles that share a mountpoint with this one (D127).
pub fn sharing_text(sharing: &[(String, PathBuf)]) -> String {
    let list: Vec<String> = sharing
        .iter()
        .map(|(profile, mountpoint)| {
            format!("profile {profile} also uses {}", mountpoint.display())
        })
        .collect();
    format!(
        "{}; only one profile can mount there at a time, and a second start fails: give each \
         profile its own `mountpoint` (D127)",
        list.join(", ")
    )
}

/// My Files' mountpoint; a warning when another profile's config uses one
/// of this profile's mountpoints (D127).
fn mountpoint_check(paths: &ProfilePaths, config: &Config, home: &Path) -> Check {
    let sharing = profiles_sharing_a_mountpoint(paths, config, home);
    if sharing.is_empty() {
        Check::new(
            "mountpoint",
            Level::Ok,
            format!(
                "{}; no other profile's config uses it or a root's mountpoint",
                config.mountpoint(home).display()
            ),
        )
    } else {
        Check::new("mountpoint", Level::Warn, sharing_text(&sharing))
    }
}

/// One line per further root (`[[roots]]`): its mountpoint, and for a
/// Photos root with `photos.thumbnail_cache` on, the thumbnail folder.
fn root_checks(paths: &ProfilePaths, config: &Config, home: &Path) -> Vec<Check> {
    let my_files = config.mountpoint(home);
    let mountpoints = config.root_mountpoints(home);
    config
        .roots
        .iter()
        .zip(&mountpoints)
        .enumerate()
        .map(|(index, (root, mountpoint))| {
            let others: Vec<&Path> = std::iter::once(my_files.as_path())
                .chain(
                    mountpoints
                        .iter()
                        .enumerate()
                        .filter(|(other, _)| *other != index)
                        .map(|(_, path)| path.as_path()),
                )
                .collect();
            let thumbnails = (root.kind == pdmount_core::config::RootKind::Photos
                && config.photos.thumbnail_cache)
                .then_some(paths.thumbnails_dir.as_path());
            root_check(
                paths.profile.as_str(),
                root.kind.as_str(),
                mountpoint,
                &others,
                thumbnails,
            )
        })
        .collect()
}

/// A root's line: `others` are My Files' and the other roots' mountpoints,
/// `thumbnails` the thumbnail folder when this root writes to it.
fn root_check(
    profile: &str,
    kind: &str,
    mountpoint: &Path,
    others: &[&Path],
    thumbnails: Option<&Path>,
) -> Check {
    let mut parts = Vec::new();
    if let Some(other) = others
        .iter()
        .find(|other| mountpoint.starts_with(other) || other.starts_with(mountpoint))
    {
        parts.push((
            Level::Fail,
            format!(
                "overlaps {}; give each root a mountpoint of its own",
                other.display()
            ),
        ));
    }
    parts.push(root_mountpoint_state(profile, mountpoint));
    if let Some(dir) = thumbnails {
        parts.push(thumbnail_dir_state(dir));
    }
    let level = parts
        .iter()
        .map(|(level, _)| *level)
        .max()
        .unwrap_or(Level::Ok);
    let detail: Vec<String> = parts.into_iter().map(|(_, text)| text).collect();
    Check::new(
        "root",
        level,
        format!("{kind} at {}: {}", mountpoint.display(), detail.join("; ")),
    )
}

/// Whether a root can mount at `mountpoint`: a directory that is empty or
/// PdMount's live mount of this profile, or one that can be created.
fn root_mountpoint_state(profile: &str, mountpoint: &Path) -> (Level, String) {
    use nix::unistd::{AccessFlags, access};
    // The mount table first: `stat` on a dead mount fails or hangs.
    match crate::mounts::find(mountpoint) {
        Ok(Some(entry)) if entry.is_pdmount() => {
            if crate::mounts::is_dead(mountpoint) {
                return (
                    Level::Warn,
                    "a dead PdMount mount, cleared at the next start".into(),
                );
            }
            return match entry.profile() {
                Some(owner) if owner != profile => (
                    Level::Fail,
                    format!("another PdMount mount (profile {owner}) is there"),
                ),
                _ => (Level::Ok, "mounted by PdMount".into()),
            };
        }
        Ok(Some(entry)) => {
            return (
                Level::Fail,
                format!("already a mount point ({})", entry.fs_type),
            );
        }
        Ok(None) => {}
        Err(error) => return (Level::Warn, format!("cannot read the mount table: {error}")),
    }
    match std::fs::metadata(mountpoint) {
        Ok(meta) if !meta.is_dir() => (Level::Fail, "not a directory".into()),
        Ok(_) => match std::fs::read_dir(mountpoint).map(|mut entries| entries.next().is_none()) {
            Ok(true) => (Level::Ok, "empty directory".into()),
            Ok(false) => (
                Level::Warn,
                "not empty: its files are hidden while the root is mounted".into(),
            ),
            Err(error) => (Level::Fail, format!("cannot be read: {error}")),
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = existing_ancestor(mountpoint);
            if parent.is_dir() && access(parent, AccessFlags::W_OK | AccessFlags::X_OK).is_ok() {
                (Level::Ok, "created at the mount".into())
            } else {
                (
                    Level::Fail,
                    format!("missing, and {} is not writable", parent.display()),
                )
            }
        }
        Err(error) => (Level::Fail, error.to_string()),
    }
}

/// The freedesktop thumbnail folder of a Photos root (D115): writable by the
/// user, and neither it nor its size folders open to group or others, since
/// the thumbnails are decrypted pictures. A warning at most: the mount works
/// without thumbnails.
fn thumbnail_dir_state(dir: &Path) -> (Level, String) {
    use nix::unistd::{AccessFlags, access};
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = existing_ancestor(dir);
            if parent.is_dir() && access(parent, AccessFlags::W_OK | AccessFlags::X_OK).is_ok() {
                (
                    Level::Ok,
                    format!("thumbnails: {} (made 0700 at the first)", dir.display()),
                )
            } else {
                (
                    Level::Warn,
                    format!(
                        "thumbnails: {} is missing and {} is not writable; none are written",
                        dir.display(),
                        parent.display()
                    ),
                )
            }
        }
        Err(error) => (
            Level::Warn,
            format!("thumbnails: cannot check {}: {error}", dir.display()),
        ),
        Ok(meta) if !meta.is_dir() => (
            Level::Warn,
            format!(
                "thumbnails: {} is not a directory; none are written",
                dir.display()
            ),
        ),
        Ok(_) if access(dir, AccessFlags::W_OK | AccessFlags::X_OK).is_err() => (
            Level::Warn,
            format!(
                "thumbnails: {} is not writable; none are written",
                dir.display()
            ),
        ),
        Ok(_) => {
            let open: Vec<String> = std::iter::once(dir.to_owned())
                .chain(
                    pdmount_core::photos::thumbnails::SIZES
                        .iter()
                        .map(|(size, _)| dir.join(size)),
                )
                .filter_map(|path| {
                    let mode = std::fs::metadata(&path).ok()?.permissions().mode() & 0o777;
                    (mode & 0o077 != 0).then(|| format!("{} ({mode:o})", path.display()))
                })
                .collect();
            if open.is_empty() {
                (
                    Level::Ok,
                    format!("thumbnails: {}, writable and private", dir.display()),
                )
            } else {
                (
                    Level::Warn,
                    format!(
                        "thumbnails: group or others can open {}; thumbnails are decrypted \
                         pictures: `chmod 700` these folders",
                        open.join(", ")
                    ),
                )
            }
        }
    }
}

/// What `doctor` learned about the desktop's bus and the service's daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Attach {
    /// No daemon of the profile answers.
    NoDaemon,
    /// A daemon runs with no desktop bus attached.
    NotAttached,
    /// A daemon has a desktop bus attached: its bus ID, or why it has none.
    Attached(Result<String, String>),
}

/// D100 (with D102): this terminal's session bus against the service's, by
/// bus ID. `terminal` and `service` are the IDs (or why there is none);
/// `service_names` says what the service's bus has (shown while a daemon
/// runs).
pub fn classify_desktop_bus(
    terminal: Result<String, String>,
    service: Result<String, String>,
    attach: &Attach,
    service_names: Option<String>,
) -> Check {
    const NAME: &str = "desktop bus";
    let names = service_names.map_or_else(String::new, |names| {
        format!("; on the service's bus: {names}")
    });
    let terminal = match terminal {
        Ok(id) => id,
        Err(error) => {
            return Check::new(
                NAME,
                Level::Warn,
                format!("cannot reach this terminal's session bus: {error}"),
            );
        }
    };
    let service = match service {
        Ok(id) => id,
        Err(error) => {
            return Check::new(
                NAME,
                Level::Warn,
                format!(
                    "cannot reach the service's session bus, so it cannot be compared with this \
                     terminal's: {error}"
                ),
            );
        }
    };
    if terminal == service {
        return Check::new(
            NAME,
            Level::Ok,
            format!("same as the service's; keyring prompts and notifications reach it{names}"),
        );
    }
    let hint = "log in again or run `pdmount session attach` in this desktop session";
    match attach {
        Attach::Attached(Ok(id)) if *id == terminal => Check::new(
            NAME,
            Level::Ok,
            format!(
                "attached: this terminal's bus is not the service's, but the daemon has it \
                 (`session attach`, D102){names}"
            ),
        ),
        Attach::Attached(Ok(_)) => Check::new(
            NAME,
            Level::Warn,
            format!(
                "not attached: the daemon has another desktop bus attached, not this terminal's \
                 (an earlier login?); {hint}{names}"
            ),
        ),
        Attach::Attached(Err(error)) => Check::new(
            NAME,
            Level::Warn,
            format!(
                "not attached: the daemon's attached bus does not answer ({error}); {hint}{names}"
            ),
        ),
        Attach::NotAttached => Check::new(
            NAME,
            Level::Warn,
            format!(
                "not attached: this terminal's bus is not the service's, so keyring prompts and \
                 notifications of the service do not reach it; {hint}{names}"
            ),
        ),
        Attach::NoDaemon => Check::new(
            NAME,
            Level::Warn,
            "not attached: this terminal's bus is not the service's and the daemon is not \
             running; it gets the desktop's bus at the next login or at `pdmount session attach` \
             once it runs",
        ),
    }
}

/// The desktop bus line (D100, D102), for an installed service when this
/// terminal has a session bus; `None` otherwise.
async fn desktop_bus_check(paths: &ProfilePaths) -> Option<Check> {
    crate::supervise::installed_service(paths)?;
    let terminal_address = crate::bus::own_address()?;
    let service_address = crate::bus::service_address()?;
    let terminal = crate::bus::id_of(&terminal_address).await;
    let service = crate::bus::id_of(&service_address).await;
    if terminal.is_ok() && terminal == service {
        return Some(classify_desktop_bus(
            terminal,
            service,
            &Attach::NoDaemon,
            None,
        ));
    }
    let reply = crate::control::call_ok::<crate::control::StatusReply>(
        &paths.control_socket(),
        crate::control::Request::Status,
        Duration::from_secs(5),
    )
    .await;
    let (attach, names) = match reply {
        Ok(Some(reply)) => {
            let attach = match reply.attached_bus {
                Some(address) => Attach::Attached(crate::bus::id_of(&address).await),
                None => Attach::NotAttached,
            };
            let names = if service.is_ok() {
                Some(crate::bus::describe_names(&service_address).await)
            } else {
                None
            };
            (attach, names)
        }
        _ => (Attach::NoDaemon, None),
    };
    Some(classify_desktop_bus(terminal, service, &attach, names))
}

pub async fn run(paths: &ProfilePaths) -> Vec<Check> {
    let (keyring, state) = check_keyring(credentials::mode());
    let mut checks = vec![
        version_check(),
        check_fuse_device(Path::new("/dev/fuse")),
        check_fusermount(),
        check_fuse_conf(Path::new("/etc/fuse.conf")),
        check_config(paths),
        keyring,
        check_login(paths, state.as_ref()),
        check_free_space("data space", &paths.data_dir),
        check_free_space("cache space", &paths.cache_dir),
        check_clock().await,
        autostart_check(paths),
        service_check(paths),
    ];
    checks.extend(desktop_bus_check(paths).await);
    if let Ok(config) = Config::load(&paths.config_file()) {
        if let Some(home) = std::env::var_os("HOME") {
            let home = Path::new(&home);
            checks.push(mountpoint_check(paths, &config, home));
            checks.extend(root_checks(paths, &config, home));
        }
        checks.extend(crate::service::doctor_checks(paths, &config));
    }
    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_keyring_fails_only_without_a_flag() {
        let unavailable = KeyringState::Unavailable("no session bus".into());
        let level = |mode, state: &KeyringState| classify_keyring(mode, state, None, None).level;
        assert_eq!(level(SecretsMode::Keyring, &unavailable), Level::Fail);
        assert_eq!(level(SecretsMode::AllowFile, &unavailable), Level::Warn);
        assert_eq!(
            level(SecretsMode::Keyring, &KeyringState::Locked),
            Level::Warn
        );
        assert_eq!(
            level(SecretsMode::Keyring, &KeyringState::Unlocked),
            Level::Ok
        );
        assert_eq!(check_keyring(SecretsMode::ForceFile).0.level, Level::Ok);
    }

    #[test]
    fn fuse_conf_option_must_be_uncommented() {
        assert!(fuse_conf_allows_other("user_allow_other\n"));
        assert!(fuse_conf_allows_other(
            "# comment\n  user_allow_other  # why\n"
        ));
        assert!(!fuse_conf_allows_other("#user_allow_other\n"));
        assert!(!fuse_conf_allows_other(
            "# user_allow_other - explanation\n"
        ));
        assert!(!fuse_conf_allows_other(""));
    }

    #[test]
    fn fusermount_must_be_setuid_root() {
        let path = Path::new("/usr/bin/fusermount3");
        assert_eq!(
            classify_fusermount(Some(path), 0o104755, 0).level,
            Level::Ok
        );
        assert_eq!(
            classify_fusermount(Some(path), 0o100755, 0).level,
            Level::Fail
        );
        assert_eq!(
            classify_fusermount(Some(path), 0o104755, 1000).level,
            Level::Fail
        );
        assert_eq!(classify_fusermount(None, 0, 0).level, Level::Fail);
    }

    #[test]
    fn missing_fuse_device_fails() {
        assert_eq!(
            check_fuse_device(Path::new("/nonexistent/fuse")).level,
            Level::Fail
        );
        assert_eq!(
            check_fuse_device(Path::new("/etc/hostname")).level,
            Level::Fail
        );
    }

    #[test]
    fn low_space_and_clock_skew_warn() {
        let dir = Path::new("/x");
        assert_eq!(
            classify_free_space("data space", dir, 10 << 30).level,
            Level::Ok
        );
        assert_eq!(
            classify_free_space("data space", dir, 100 << 20).level,
            Level::Warn
        );
        assert_eq!(
            classify_clock_skew(chrono::TimeDelta::seconds(5)).level,
            Level::Ok
        );
        assert_eq!(
            classify_clock_skew(chrono::TimeDelta::seconds(-300)).level,
            Level::Warn
        );
    }

    #[test]
    fn existing_ancestor_walks_up() {
        assert_eq!(
            existing_ancestor(Path::new("/nonexistent/a/b")),
            Path::new("/")
        );
        assert_eq!(existing_ancestor(Path::new("/etc")), Path::new("/etc"));
    }

    /// Profile paths and a home under `root`.
    fn profile_in(root: &Path, profile: &str) -> ProfilePaths {
        ProfilePaths::new(
            profile.parse().expect("profile"),
            &pdmount_core::paths::BaseDirs {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
                state: root.join("state"),
                runtime: Some(root.join("run")),
            },
        )
    }

    fn write_config(paths: &ProfilePaths, profile: &str, text: &str) {
        std::fs::create_dir_all(&paths.config_dir).expect("mkdir");
        std::fs::write(paths.config_dir.join(format!("{profile}.toml")), text).expect("write");
    }

    fn load(paths: &ProfilePaths) -> Config {
        Config::load(&paths.config_file()).expect("config")
    }

    /// V-6 (D127): a profile with a data directory but no config uses the
    /// default mountpoint, as does one whose config sets none; a root's
    /// mountpoint counts too. Unreadable configs and other names are skipped.
    #[test]
    fn profiles_on_one_mountpoint_are_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let work = profile_in(dir.path(), "work");
        let default = profile_in(dir.path(), "default");
        std::fs::create_dir_all(&default.data_dir).expect("data dir");
        std::fs::create_dir_all(dir.path().join("data/pdmount/Not A Profile")).expect("mkdir");
        write_config(&work, "work", "[cache]\nmax_bytes = \"1GiB\"\n");
        write_config(&work, "other", "mountpoint = \"~/Other\"\n");
        write_config(&work, "broken", "mountpoint = [\n");
        let sharing = profiles_sharing_a_mountpoint(&work, &load(&work), &home);
        assert_eq!(sharing, [("default".into(), home.join("ProtonDrive"))]);
        let text = sharing_text(&sharing);
        assert!(text.starts_with("profile default also uses "), "{text}");
        assert!(
            text.contains("give each profile its own `mountpoint`"),
            "{text}"
        );

        let other = profile_in(dir.path(), "other");
        assert!(profiles_sharing_a_mountpoint(&other, &load(&other), &home).is_empty());
        // A root of `other` at work's mountpoint.
        write_config(
            &work,
            "other",
            "mountpoint = \"~/Other\"\n[[roots]]\nkind = \"shared\"\nmountpoint = \"~/ProtonDrive\"\n",
        );
        let sharing = profiles_sharing_a_mountpoint(&work, &load(&work), &home);
        assert_eq!(sharing.len(), 2, "{sharing:?}");
        assert_eq!(sharing[1], ("other".into(), home.join("ProtonDrive")));
        let check = mountpoint_check(&work, &load(&work), &home);
        assert_eq!(check.level, Level::Warn, "{check}");
        let alone = profile_in(dir.path(), "alone");
        write_config(&alone, "alone", "mountpoint = \"~/Alone\"\n");
        assert_eq!(
            mountpoint_check(&alone, &load(&alone), &home).level,
            Level::Ok
        );
    }

    #[test]
    fn a_root_mountpoint_must_be_a_usable_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let check =
            |path: &Path, others: &[&Path]| root_check("work", "shared", path, others, None);
        let empty = dir.path().join("empty");
        std::fs::create_dir(&empty).expect("mkdir");
        let line = check(&empty, &[]);
        assert_eq!(line.level, Level::Ok, "{line}");
        assert!(line.to_string().starts_with(&format!(
            "[ok  ] root         shared at {}: empty directory",
            empty.display()
        )));
        let missing = dir.path().join("a/b");
        assert_eq!(check(&missing, &[]).level, Level::Ok);
        assert!(check(&missing, &[]).detail.contains("created at the mount"));
        let file = dir.path().join("file");
        std::fs::write(&file, "x").expect("write");
        assert_eq!(check(&file, &[]).level, Level::Fail);
        assert_eq!(check(&file.join("below"), &[]).level, Level::Fail);
        let full = dir.path().join("full");
        std::fs::create_dir(&full).expect("mkdir");
        std::fs::write(full.join("x"), "x").expect("write");
        assert_eq!(check(&full, &[]).level, Level::Warn);
        // Overlapping My Files' mountpoint or another root's, either way.
        let line = check(&empty, &[dir.path()]);
        assert_eq!(line.level, Level::Fail, "{line}");
        assert!(line.detail.contains("overlaps"), "{line}");
        assert_eq!(check(dir.path(), &[&empty]).level, Level::Fail);
        assert_eq!(check(&empty, &[&full]).level, Level::Ok);
    }

    #[test]
    fn the_thumbnail_folder_must_be_writable_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let mountpoint = dir.path().join("photos");
        let thumbnails = dir.path().join("thumbnails");
        let check = || root_check("work", "photos", &mountpoint, &[], Some(&thumbnails));
        let chmod = |path: &Path, mode: u32| {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
        };
        // Missing: made at the first thumbnail.
        assert_eq!(check().level, Level::Ok, "{}", check());
        std::fs::create_dir(&thumbnails).expect("mkdir");
        chmod(&thumbnails, 0o700);
        let line = check();
        assert_eq!(line.level, Level::Ok, "{line}");
        assert!(line.detail.contains("writable and private"), "{line}");
        chmod(&thumbnails, 0o755);
        let line = check();
        assert_eq!(line.level, Level::Warn, "{line}");
        assert!(line.detail.contains("(755)"), "{line}");
        chmod(&thumbnails, 0o700);
        let large = thumbnails.join("large");
        std::fs::create_dir(&large).expect("mkdir");
        chmod(&large, 0o750);
        let line = check();
        assert_eq!(line.level, Level::Warn, "{line}");
        assert!(
            line.detail.contains(&format!("{} (750)", large.display())),
            "{line}"
        );
        chmod(&large, 0o700);
        chmod(&thumbnails, 0o500);
        let line = check();
        assert_eq!(line.level, Level::Warn, "{line}");
        assert!(line.detail.contains("not writable"), "{line}");
        chmod(&thumbnails, 0o700);
    }

    /// One line per root, none without roots; the thumbnail folder only for
    /// a Photos root with `photos.thumbnail_cache` on.
    #[test]
    fn doctor_has_a_line_per_root() {
        let dir = tempfile::tempdir().expect("tempdir");
        let home = dir.path().join("home");
        let paths = profile_in(dir.path(), "work");
        write_config(&paths, "work", "mountpoint = \"~/Drive\"\n");
        assert!(root_checks(&paths, &load(&paths), &home).is_empty());
        write_config(
            &paths,
            "work",
            "mountpoint = \"~/Drive\"\n\
             [[roots]]\nkind = \"shared\"\nmountpoint = \"~/Shared\"\n\
             [[roots]]\nkind = \"photos\"\nmountpoint = \"~/Drive/Photos\"\n",
        );
        let lines = root_checks(&paths, &load(&paths), &home);
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0].level, Level::Ok, "{}", lines[0]);
        assert!(!lines[0].detail.contains("thumbnails"), "{}", lines[0]);
        assert_eq!(lines[1].level, Level::Fail, "{}", lines[1]);
        assert!(lines[1].detail.contains("overlaps"), "{}", lines[1]);
        assert!(lines[1].detail.contains("thumbnails: "), "{}", lines[1]);
        write_config(
            &paths,
            "work",
            "[photos]\nthumbnail_cache = false\n\
             [[roots]]\nkind = \"photos\"\nmountpoint = \"~/Photos\"\n",
        );
        let lines = root_checks(&paths, &load(&paths), &home);
        assert!(!lines[0].detail.contains("thumbnails"), "{}", lines[0]);
    }

    #[test]
    fn checks_render_with_their_level() {
        let check = Check::new("login", Level::Warn, "not logged in");
        assert_eq!(check.to_string(), "[WARN] login        not logged in");
    }
}
