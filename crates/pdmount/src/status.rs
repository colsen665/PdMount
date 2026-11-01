//! `pdmount status` (DESIGN.md §14.2): profile, session, account, quota,
//! device and mount state. While a daemon runs, everything comes from it
//! over the control socket; a second session resume could rotate the
//! daemon's refresh token.

use crate::control::{self, Request};
use crate::mounts;
use crate::session::{self, Resumed};
use pdmount_backend::{Backend, StorageInfo};
use pdmount_core::device::DeviceIdentity;
use pdmount_core::health::Connectivity;
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
    NeedsLogin {
        reason: String,
        /// A sign-in link and its code, while the daemon waits for one.
        sign_in: Option<(String, String)>,
    },
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
        queue_pending: u64,
        queue_parked: u64,
        queue_oldest_secs: Option<u64>,
        events_last_poll_secs: Option<u64>,
        events_refreshing: bool,
        read_only: bool,
        /// Conflicts waiting for the user.
        conflicts: u64,
        inodes: u64,
        cache_bytes: u64,
        cache_blocks: u64,
        pinned: Option<control::PinnedReply>,
        connectivity: Connectivity,
        /// Unix seconds when `connectivity` began.
        connectivity_since: u64,
    },
}

impl SessionState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

/// `HH:MM`, or the date as well when it is not today.
fn local_time(unix_secs: u64) -> String {
    use chrono::{Local, TimeZone};
    let Some(time) = i64::try_from(unix_secs)
        .ok()
        .and_then(|secs| Local.timestamp_opt(secs, 0).single())
    else {
        return "an unknown time".into();
    };
    if time.date_naive() == Local::now().date_naive() {
        time.format("%H:%M").to_string()
    } else {
        time.format("%Y-%m-%d %H:%M").to_string()
    }
}

fn percent(used: u64, max: u64) -> f64 {
    if max == 0 {
        0.0
    } else {
        used as f64 * 100.0 / max as f64
    }
}

/// `45s`, `3m`, `2h`.
fn short_age(secs: u64) -> String {
    match secs {
        0..60 => format!("{secs}s"),
        60..3600 => format!("{}m", secs / 60),
        _ => format!("{}h", secs / 3600),
    }
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Profile:    {}", self.profile)?;
        match &self.session {
            SessionState::NotLoggedIn => {
                writeln!(f, "Session:    not logged in (run `pdmount login`)")?;
            }
            SessionState::NeedsLogin { reason, sign_in } => {
                writeln!(f, "Session:    needs login, run `pdmount login` ({reason})")?;
                if let Some((url, code)) = sign_in {
                    writeln!(f, "Sign-in:    {url}")?;
                    if !code.is_empty() {
                        writeln!(f, "Code:       {code}")?;
                    }
                }
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
                queue_pending,
                queue_parked,
                queue_oldest_secs,
                events_last_poll_secs,
                events_refreshing,
                read_only,
                conflicts,
                inodes,
                cache_bytes,
                cache_blocks,
                pinned,
                connectivity,
                connectivity_since,
            } => {
                let ro = if *read_only { ", read-only" } else { "" };
                writeln!(f, "Mount:      mounted (daemon pid {pid}{ro})")?;
                match connectivity {
                    Connectivity::Online => writeln!(f, "Connection: online")?,
                    Connectivity::Offline => writeln!(
                        f,
                        "Connection: offline since {} ({queue_pending} pending)",
                        local_time(*connectivity_since)
                    )?,
                    Connectivity::NeedsLogin => writeln!(
                        f,
                        "Connection: needs login since {} ({queue_pending} pending)",
                        local_time(*connectivity_since)
                    )?,
                }
                let oldest = match queue_oldest_secs {
                    Some(secs) if *queue_pending > 0 => format!(" (oldest {})", short_age(*secs)),
                    _ => String::new(),
                };
                writeln!(
                    f,
                    "Queue:      {queue_pending} pending{oldest}, {queue_parked} parked"
                )?;
                let refreshing = if *events_refreshing {
                    ", refreshing"
                } else {
                    ""
                };
                match events_last_poll_secs {
                    Some(secs) => {
                        writeln!(f, "Events:     last poll {secs} s ago{refreshing}")?;
                    }
                    None => writeln!(f, "Events:     no poll finished yet{refreshing}")?,
                }
                if *conflicts > 0 {
                    writeln!(f, "Conflicts:  {conflicts} open (pdmount conflicts list)")?;
                }
                writeln!(f, "Inodes:     {inodes}")?;
                write!(
                    f,
                    "Cache:      {} in {cache_blocks} blocks",
                    ByteSize(*cache_bytes).human()
                )?;
                if let Some(pinned) = pinned {
                    write!(
                        f,
                        "\nPinned:     {} paths, {} of {} fetched",
                        pinned.paths,
                        ByteSize(pinned.fetched_bytes).human(),
                        ByteSize(pinned.total_bytes).human()
                    )?;
                }
                Ok(())
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
    let mountpoint = crate::daemon::existing_mountpoint(paths, config, home, None);
    let daemon = control::call_ok::<control::StatusReply>(
        &paths.control_socket(),
        Request::Status,
        Duration::from_secs(5),
    )
    .await?;
    if let Some(reply) = daemon {
        let quota = match (reply.quota_used, reply.quota_max) {
            (Some(used), Some(max)) => Some(StorageInfo { used, max }),
            _ => None,
        };
        let connectivity = reply.connectivity;
        let session = if connectivity == Connectivity::NeedsLogin {
            SessionState::NeedsLogin {
                reason: reply
                    .sign_in_error
                    .unwrap_or_else(|| "the server rejected the session".to_owned()),
                sign_in: reply
                    .sign_in_url
                    .map(|url| (url, reply.sign_in_code.unwrap_or_default())),
            }
        } else {
            SessionState::Active {
                username: reply.username,
                storage: reply.credentials,
                quota,
            }
        };
        return Ok(StatusReport {
            profile: paths.profile.to_string(),
            session,
            device,
            mountpoint: reply.mountpoint,
            mount: MountState::Mounted {
                pid: reply.pid.into(),
                queue_pending: reply.queue_pending,
                queue_parked: reply.queue_parked,
                queue_oldest_secs: reply.queue_oldest_secs,
                events_last_poll_secs: reply.events_last_poll_secs,
                events_refreshing: reply.events_refreshing,
                read_only: reply.read_only,
                conflicts: reply.conflicts,
                inodes: reply.inodes,
                cache_bytes: reply.cache_bytes,
                cache_blocks: reply.cache_blocks,
                pinned: reply.pinned,
                connectivity,
                connectivity_since: reply.connectivity_since,
            },
        });
    }
    // A daemon that no longer answers may still save its tokens: a resume
    // here could rotate them.
    if let Some(pid) = crate::daemon::daemon_pid(paths) {
        anyhow::bail!(
            "the daemon of profile {} (pid {pid}) is shutting down; run `pdmount status` again in a moment",
            paths.profile
        );
    }
    let session = match session::resume(paths).await? {
        Resumed::NotLoggedIn => SessionState::NotLoggedIn,
        Resumed::Offline { error, .. } => {
            return Err(error.context("cannot reach Proton to check the session"));
        }
        Resumed::NeedsLogin { error, .. } => SessionState::NeedsLogin {
            reason: format!("{error:#}"),
            sign_in: None,
        },
        Resumed::Active(logged_in) => {
            let active = async {
                let backend = session::proton_backend(
                    &logged_in,
                    &device,
                    session::TransferLimits::of(config),
                )
                .await?;
                anyhow::Ok(SessionState::Active {
                    username: backend.account().await?.username,
                    storage: logged_in.storage.to_string(),
                    quota: Some(backend.storage_info().await?),
                })
            }
            .await;
            match active {
                // The token is fine but the keys cannot be unlocked (D085).
                Err(error) if session::needs_login(&error) => SessionState::NeedsLogin {
                    reason: format!("{error:#}"),
                    sign_in: None,
                },
                other => other?,
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
    fn mounted_report_shows_the_oldest_change_and_the_event_stream() {
        let text = report_with(
            SessionState::NotLoggedIn,
            MountState::Mounted {
                pid: 7,
                queue_pending: 2,
                queue_parked: 1,
                queue_oldest_secs: Some(180),
                events_last_poll_secs: Some(12),
                events_refreshing: true,
                read_only: true,
                conflicts: 0,
                inodes: 1,
                cache_bytes: 0,
                cache_blocks: 0,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
            },
        );
        assert!(
            text.contains("Queue:      2 pending (oldest 3m), 1 parked"),
            "{text}"
        );
        assert!(
            text.contains("Events:     last poll 12 s ago, refreshing"),
            "{text}"
        );
        assert!(text.contains("(daemon pid 7, read-only)"), "{text}");
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
                queue_pending: 2,
                queue_parked: 0,
                queue_oldest_secs: None,
                events_last_poll_secs: None,
                events_refreshing: false,
                read_only: false,
                conflicts: 0,
                inodes: 7,
                cache_bytes: 1 << 20,
                cache_blocks: 3,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
            },
        );
        assert!(mounted.contains("mounted (daemon pid 42)"));
        assert!(mounted.contains("Queue:      2 pending, 0 parked"));
        assert!(!mounted.contains("Conflicts:"));
        let conflicted = report_with(
            active(),
            MountState::Mounted {
                pid: 42,
                queue_pending: 0,
                queue_parked: 0,
                queue_oldest_secs: None,
                events_last_poll_secs: None,
                events_refreshing: false,
                read_only: false,
                conflicts: 3,
                inodes: 7,
                cache_bytes: 0,
                cache_blocks: 0,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
            },
        );
        assert!(conflicted.contains("Conflicts:  3 open (pdmount conflicts list)"));
        assert!(mounted.contains("Cache:      1.0 MiB in 3 blocks"));
    }

    #[test]
    fn connection_line_follows_the_daemon() {
        let mounted = |connectivity| {
            report_with(
                SessionState::Active {
                    username: "u".into(),
                    storage: "file".into(),
                    quota: None,
                },
                MountState::Mounted {
                    pid: 1,
                    queue_pending: 4,
                    queue_parked: 0,
                    queue_oldest_secs: None,
                    events_last_poll_secs: None,
                    events_refreshing: false,
                    read_only: false,
                    conflicts: 0,
                    inodes: 1,
                    cache_bytes: 0,
                    cache_blocks: 0,
                    pinned: None,
                    connectivity,
                    connectivity_since: 1_700_000_000,
                },
            )
        };
        assert!(mounted(Connectivity::Online).contains("Connection: online\n"));
        let offline = mounted(Connectivity::Offline);
        assert!(offline.contains("Connection: offline since "));
        assert!(offline.contains("(4 pending)"));
        assert!(mounted(Connectivity::NeedsLogin).contains("Connection: needs login since "));
    }

    #[test]
    fn logged_out_report_tells_how_to_log_in() {
        assert!(report(SessionState::NotLoggedIn).contains("run `pdmount login`"));
        let waiting = report(SessionState::NeedsLogin {
            reason: "expired".into(),
            sign_in: Some(("https://account.proton.me/x".into(), "ABCD".into())),
        });
        assert!(waiting.contains("Session:    needs login"));
        assert!(waiting.contains("expired"));
        assert!(waiting.contains("Sign-in:    https://account.proton.me/x\nCode:       ABCD\n"));
        let idle = report(SessionState::NeedsLogin {
            reason: "expired".into(),
            sign_in: None,
        });
        assert!(!idle.contains("Sign-in:"));
    }

    #[test]
    fn percent_handles_zero_quota() {
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(percent(1, 4), 25.0);
    }
}
