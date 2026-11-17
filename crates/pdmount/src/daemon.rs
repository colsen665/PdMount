//! Mount daemon and mount management: `mount`, `unmount`, `reset`
//! (DESIGN.md §14).
//!
//! `pdmount mount` starts `pdmount mount --foreground` as a detached child in
//! its own process group, with output appended to the profile's log file,
//! and returns once the child's control socket answers. The foreground
//! process runs the engine, the FUSE session and the control socket until
//! SIGTERM/SIGINT/SIGHUP (after a flush, `flush.rs`), an `unmount` request,
//! or an external unmount.
//!
//! # Module map
//!
//! - `daemon.rs` (here): the timeouts and limits shared by the submodules,
//!   [`BackendChoice`] and the mountpoint helpers.
//! - `launch.rs`: `mount` in the background (log file, pre-start checks,
//!   stale mounts).
//! - `run.rs`: [`run_foreground`], the daemon's start-up and main loop.
//! - `backend.rs`: opening the backend ([`open_backend`], `open_proton`) and
//!   the start refusals ([`StartRefused`]).
//! - `handler.rs`: the control request handler (`request_handler`).
//! - `stop.rs`: stop signals, waiting for a stop and the shutdown sequence.
//! - `client.rs`: the CLI side of `unmount`, `drain` and `login` against a
//!   running daemon.
//! - `process.rs`: [`fusermount`], the pid file and process checks.
//! - `reset.rs`: `reset`.

mod backend;
mod client;
mod handler;
mod launch;
mod process;
mod reset;
mod run;
mod stop;

use backend::open_with_key;
pub(crate) use backend::{Opened, StartRefused, open_backend, watch_fake_session};
pub use client::{login_via_daemon, unmount};
use handler::request_handler;
pub(crate) use handler::tree_source;
pub(crate) use launch::foreign_mount_text;
use launch::{clear_stale_mount, foreign_mount, preflight};
pub use launch::{mount_background, unmount_if_stale};
pub(crate) use process::{daemon_pid, fusermount, refuse_while_running};
use process::{stop_limit, wait_for_daemon_exit};
pub use reset::reset;
pub use run::run_foreground;
use stop::{StopSignals, announce_conflicts, hurry_on_signal, shutdown, wait_for_stop};

use crate::auth::Auth;
use crate::control::{self, Request};
use crate::credentials::Credentials;
use crate::keywait::{self, Late};
use crate::mounts;
use crate::session::{self, LoggedIn, Resumed};
use crate::supervise::{self, Supervision};
use anyhow::{Context, bail};
use pdmount_backend::fake::{FakeBackend, FakeClient};
use pdmount_backend::proton::ProtonBackend;
use pdmount_backend::{Backend, BackendError};
use pdmount_core::cache::{CacheOptions, DiskBudget, SharedCap};
use pdmount_core::config::BrowserMode;
use pdmount_core::conflicts::system_clock;
use pdmount_core::device::DeviceIdentity;
use pdmount_core::engine::{Engine, EngineOptions};
use pdmount_core::health::Connectivity;
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// The daemon waits up to 30 s for the sign-in link.
const LOGIN_LINK_TIMEOUT: Duration = Duration::from_secs(40);
const LOGIN_POLL: Duration = Duration::from_secs(5);
/// A waiting daemon's attempt at `login` (D106): the cache key read, which
/// may wait for an unlock dialog, then the network of a start.
const LOGIN_ATTEMPT_NETWORK: Duration = Duration::from_secs(60);

/// How long `login` waits for a waiting daemon's attempt; the client waits
/// longer (`login_via_daemon`), so a slow attempt is reported as running.
fn login_attempt_wait() -> Duration {
    crate::credentials::keyring_timeout() + LOGIN_ATTEMPT_NETWORK
}
/// First mounts list the root and may refresh the session.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const TOKEN_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(10);
/// After a second stop signal the session gets this long at most: OpenRC's
/// shutdown sends KILL about a second after its second TERM (D092).
const HURRIED_SESSION_END: Duration = Duration::from_millis(300);
/// `unmount --force` sends SIGKILL this long after `timeouts.shutdown_grace`:
/// longer than the daemon's own wait for the session plus its store commit
/// and last token save.
const FORCE_KILL_MARGIN: Duration = Duration::from_secs(30);
/// A log above this is moved to `<profile>.log.1` at a background `mount`.
const LOG_ROTATE_BYTES: u64 = 16 << 20;
/// How long `unmount` waits for pending uploads.
const DRAIN_TIMEOUT: Duration = Duration::from_secs(600);
/// Largest descriptor table grown at startup (about 0.5 MiB of kernel memory).
const FD_TABLE_MAX: u64 = 65_536;

/// Where a mount's data lives (`mount --backend`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackendChoice {
    Proton,
    /// A persisted fake drive; test builds only (DESIGN.md §15.1).
    Fake(PathBuf),
}

impl BackendChoice {
    /// Parses `proton` or, with the `fake-backend` feature, `fake:<dir>`.
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.split_once(':') {
            None if value == "proton" => Ok(Self::Proton),
            Some(("fake", dir)) if cfg!(feature = "fake-backend") && !dir.is_empty() => {
                let dir = PathBuf::from(dir);
                let dir = if dir.is_absolute() {
                    dir
                } else {
                    std::env::current_dir()
                        .map_err(|e| e.to_string())?
                        .join(dir)
                };
                Ok(Self::Fake(dir))
            }
            Some(("fake", _)) if !cfg!(feature = "fake-backend") => {
                Err("the fake backend exists only in test builds (feature `fake-backend`)".into())
            }
            _ => Err(format!(
                "unknown backend {value:?}; expected `proton` or `fake:<dir>`"
            )),
        }
    }

    fn arg(&self) -> String {
        match self {
            Self::Proton => "proton".into(),
            Self::Fake(dir) => format!("fake:{}", dir.display()),
        }
    }
}

/// The mountpoint of the profile's running or crashed daemon, from its pid
/// file.
pub fn recorded_mountpoint(paths: &ProfilePaths) -> Option<PathBuf> {
    let text = std::fs::read_to_string(paths.pid_file()).ok()?;
    let line = text.lines().nth(1)?.trim();
    (!line.is_empty()).then(|| PathBuf::from(line))
}

/// For commands acting on an existing mount: the command-line mountpoint,
/// else the one the daemon recorded, else the configured one.
pub fn existing_mountpoint(
    paths: &ProfilePaths,
    config: &Config,
    home: &Path,
    arg: Option<PathBuf>,
) -> PathBuf {
    match arg {
        Some(arg) => mountpoint(config, home, Some(arg)),
        None => recorded_mountpoint(paths).unwrap_or_else(|| config.mountpoint(home)),
    }
}

/// The mountpoint: the command-line one, else the configured one.
pub fn mountpoint(config: &Config, home: &Path, arg: Option<PathBuf>) -> PathBuf {
    let path = arg.unwrap_or_else(|| config.mountpoint(home));
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir().map_or(path.clone(), |cwd| cwd.join(path))
    }
}

/// Profile paths under `root`, for the tests of the submodules.
#[cfg(test)]
pub(super) fn test_paths(root: &Path) -> ProfilePaths {
    ProfilePaths::new(
        pdmount_core::Profile::default(),
        &pdmount_core::paths::BaseDirs {
            config: root.join("config"),
            data: root.join("data"),
            cache: root.join("cache"),
            state: root.join("state"),
            runtime: Some(root.join("run")),
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_mountpoints_are_made_absolute() {
        let config = Config::default();
        let home = Path::new("/home/u");
        assert_eq!(
            mountpoint(&config, home, None),
            PathBuf::from("/home/u/ProtonDrive")
        );
        let relative = mountpoint(&config, home, Some(PathBuf::from("mnt")));
        assert!(relative.is_absolute());
        assert!(relative.ends_with("mnt"));
    }
}
