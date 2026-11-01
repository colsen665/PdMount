//! `pdmount doctor` (DESIGN.md §14.2): checks the local prerequisites for
//! mounting and signing in. Any failed check makes the command exit 1;
//! warnings do not.

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
    fn new(name: &'static str, level: Level, detail: impl Into<String>) -> Self {
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
    let state = credentials::keyring_state();
    (classify_keyring(mode, &state), Some(state))
}

fn classify_keyring(mode: SecretsMode, state: &KeyringState) -> Check {
    match state {
        KeyringState::Unlocked => Check::new(
            "keyring",
            Level::Ok,
            format!("Secret Service reachable, default collection unlocked; secrets: {mode}"),
        ),
        KeyringState::Locked => Check::new(
            "keyring",
            Level::Warn,
            "the default collection is locked; the next command that reads secrets opens an \
             unlock dialog",
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

pub async fn run(paths: &ProfilePaths) -> Vec<Check> {
    let (keyring, state) = check_keyring(credentials::mode());
    vec![
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
        Check::new("autostart", Level::Ok, crate::autostart::state_text(paths)),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_unavailable_keyring_fails_only_without_a_flag() {
        let unavailable = KeyringState::Unavailable("no session bus".into());
        let level = |mode, state: &KeyringState| classify_keyring(mode, state).level;
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

    #[test]
    fn checks_render_with_their_level() {
        let check = Check::new("login", Level::Warn, "not logged in");
        assert_eq!(check.to_string(), "[WARN] login        not logged in");
    }
}
