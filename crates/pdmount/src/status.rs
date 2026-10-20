//! `pdmount status` (DESIGN.md §14.2): profile, session, account, quota,
//! device and mount state. While a daemon runs, everything comes from it
//! over the control socket; a second session resume could rotate the
//! daemon's refresh token.

use crate::control::{self, Request};
use crate::mounts;
use crate::session::{self, Resumed};
use pdmount_backend::{Backend, StorageInfo};
use pdmount_core::device::DeviceIdentity;
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use std::fmt;
use std::time::Duration;

pub struct StatusReport {
    pub profile: String,
    pub session: SessionState,
    pub device: DeviceIdentity,
    pub mountpoint: std::path::PathBuf,
    pub mount: MountState,
}

pub enum SessionState {
    NotLoggedIn,
    NeedsLogin(String),
    Active {
        username: String,
        /// Where the credentials are stored.
        storage: String,
        /// `None` until the daemon fetched it.
        quota: Option<StorageInfo>,
    },
}

pub enum MountState {
    NotMounted,
    /// Mounted, but no daemon answers.
    Stale,
    Mounted {
        pid: u64,
        inodes: u64,
        cache_bytes: u64,
        cache_blocks: u64,
    },
}

impl SessionState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

fn percent(used: u64, max: u64) -> f64 {
    if max == 0 {
        0.0
    } else {
        used as f64 * 100.0 / max as f64
    }
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Profile:    {}", self.profile)?;
        match &self.session {
            SessionState::NotLoggedIn => {
                writeln!(f, "Session:    not logged in (run `pdmount login`)")?;
            }
            SessionState::NeedsLogin(reason) => {
                writeln!(f, "Session:    expired, run `pdmount login` ({reason})")?;
            }
            SessionState::Active {
                username,
                storage,
                quota,
            } => {
                writeln!(f, "Account:    {username}")?;
                writeln!(f, "Session:    active (stored in {storage})")?;
                match quota {
                    Some(quota) => writeln!(
                        f,
                        "Storage:    {} used of {} ({:.1} %)",
                        ByteSize(quota.used).human(),
                        ByteSize(quota.max).human(),
                        percent(quota.used, quota.max)
                    )?,
                    None => writeln!(f, "Storage:    not fetched yet")?,
                }
            }
        }
        writeln!(f, "Device:     {} ({})", self.device.name, self.device.id)?;
        writeln!(f, "Mountpoint: {}", self.mountpoint.display())?;
        match &self.mount {
            MountState::NotMounted => write!(f, "Mount:      not mounted"),
            MountState::Stale => write!(
                f,
                "Mount:      stale (no daemon answers); run `pdmount unmount --force`"
            ),
            MountState::Mounted {
                pid,
                inodes,
                cache_bytes,
                cache_blocks,
            } => {
                writeln!(f, "Mount:      mounted read-only (daemon pid {pid})")?;
                writeln!(f, "Inodes:     {inodes}")?;
                write!(
                    f,
                    "Cache:      {} in {cache_blocks} blocks",
                    ByteSize(*cache_bytes).human()
                )
            }
        }
    }
}

pub async fn status(
    paths: &ProfilePaths,
    config: &Config,
    home: &std::path::Path,
) -> anyhow::Result<StatusReport> {
    let device = session::device_identity(paths, config)?;
    let mountpoint = config.mountpoint(home);
    let daemon = control::call(
        &paths.control_socket(),
        Request::Status,
        Duration::from_secs(5),
    )
    .await?;
    if let Some(reply) = daemon {
        let number = |key: &str| reply[key].as_u64().unwrap_or(0);
        let quota = match (reply["quota_used"].as_u64(), reply["quota_max"].as_u64()) {
            (Some(used), Some(max)) => Some(StorageInfo { used, max }),
            _ => None,
        };
        return Ok(StatusReport {
            profile: paths.profile.to_string(),
            session: SessionState::Active {
                username: reply["username"].as_str().unwrap_or("?").to_owned(),
                storage: reply["credentials"].as_str().unwrap_or("?").to_owned(),
                quota,
            },
            device,
            mountpoint: reply["mountpoint"]
                .as_str()
                .map_or(mountpoint, std::path::PathBuf::from),
            mount: MountState::Mounted {
                pid: number("pid"),
                inodes: number("inodes"),
                cache_bytes: number("cache_bytes"),
                cache_blocks: number("cache_blocks"),
            },
        });
    }
    let session = match session::resume(paths).await? {
        Resumed::NotLoggedIn => SessionState::NotLoggedIn,
        Resumed::NeedsLogin(error) => SessionState::NeedsLogin(format!("{error:#}")),
        Resumed::Active(logged_in) => {
            let backend = session::proton_backend(&logged_in, &device).await?;
            SessionState::Active {
                username: backend.account().await?.username,
                storage: logged_in.storage.to_string(),
                quota: Some(backend.storage_info().await?),
            }
        }
    };
    let mount = match mounts::find(&mountpoint)? {
        Some(entry) if entry.is_pdmount() => MountState::Stale,
        _ => MountState::NotMounted,
    };
    Ok(StatusReport {
        profile: paths.profile.to_string(),
        session,
        device,
        mountpoint,
        mount,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(session: SessionState) -> String {
        report_with(session, MountState::NotMounted)
    }

    fn report_with(session: SessionState, mount: MountState) -> String {
        StatusReport {
            mount,
            profile: "default".into(),
            session,
            device: DeviceIdentity {
                id: pdmount_core::device::Uuid::nil(),
                name: "laptop".into(),
            },
            mountpoint: "/home/u/ProtonDrive".into(),
        }
        .to_string()
    }

    #[test]
    fn active_report_shows_account_and_quota() {
        let text = report(SessionState::Active {
            username: "user@proton.me".into(),
            storage: "keyring".into(),
            quota: Some(StorageInfo {
                used: 3 << 30,
                max: 500 << 30,
            }),
        });
        assert!(text.contains("Account:    user@proton.me"));
        assert!(text.contains("Storage:    3.0 GiB used of 500.0 GiB (0.6 %)"));
        assert!(text.contains("stored in keyring"));
        assert!(text.contains("Device:     laptop"));
    }

    #[test]
    fn mount_states_are_reported() {
        let active = || SessionState::Active {
            username: "u".into(),
            storage: "file".into(),
            quota: None,
        };
        assert!(report(active()).contains("Mount:      not mounted"));
        assert!(report(active()).contains("Storage:    not fetched yet"));
        assert!(report_with(active(), MountState::Stale).contains("unmount --force"));
        let mounted = report_with(
            active(),
            MountState::Mounted {
                pid: 42,
                inodes: 7,
                cache_bytes: 1 << 20,
                cache_blocks: 3,
            },
        );
        assert!(mounted.contains("mounted read-only (daemon pid 42)"));
        assert!(mounted.contains("Cache:      1.0 MiB in 3 blocks"));
    }

    #[test]
    fn logged_out_report_tells_how_to_log_in() {
        assert!(report(SessionState::NotLoggedIn).contains("run `pdmount login`"));
        assert!(report(SessionState::NeedsLogin("expired".into())).contains("expired"));
    }

    #[test]
    fn percent_handles_zero_quota() {
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(percent(1, 4), 25.0);
    }
}
