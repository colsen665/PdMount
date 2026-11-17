//! `pdmount status` (DESIGN.md §14.2): profile, session, account, quota,
//! device and mount state. While a daemon runs, everything comes from it
//! over the control socket; a second session resume could rotate the
//! daemon's refresh token.

use crate::control::{self, Request};
use crate::credentials::{self, read_would_prompt};
use crate::mounts;
use crate::session::{self, Resumed};
use pdmount_backend::{Backend, StorageInfo};
use pdmount_core::device::DeviceIdentity;
use pdmount_core::engine::marker::IncomingMarker;
use pdmount_core::health::Connectivity;
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use std::fmt;
use std::time::{Duration, SystemTime};

pub struct StatusReport {
    pub profile: String,
    pub session: SessionState,
    pub device: DeviceIdentity,
    pub mountpoint: std::path::PathBuf,
    pub mount: MountState,
    /// Changes of shares that went away with them (D080, D114), one line
    /// per directory under `orphaned/`.
    pub orphaned: Vec<String>,
}

pub enum SessionState {
    NotLoggedIn,
    /// No daemon answers and the keyring is locked: reading the session
    /// would open an unlock dialog, which a query must not do (D104).
    KeyringLocked,
    NeedsLogin {
        reason: String,
        /// A sign-in link and its code, while the daemon waits for one.
        sign_in: Option<(String, String)>,
    },
    Active {
        username: String,
        /// Where the credentials are stored, as shown in brackets:
        /// "stored in keyring".
        storage: String,
        /// `None` until the daemon fetched it.
        quota: Option<StorageInfo>,
        /// Why the current tokens are not stored yet (D106 amended).
        unsaved: Option<String>,
    },
}

// Built once per `status`; boxing the large variant would buy nothing.
#[allow(clippy::large_enum_variant)]
pub enum MountState {
    NotMounted,
    /// Mounted, but no daemon answers.
    Stale,
    /// A live mount of another profile (named, when its source names one)
    /// is at the mountpoint (D127).
    Foreign(Option<String>),
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
        /// The profile's cap and total, with further trees on it (D110).
        cache_profile: Option<control::CacheProfileReply>,
        pinned: Option<control::PinnedReply>,
        connectivity: Connectivity,
        /// Unix seconds when `connectivity` began.
        connectivity_since: u64,
        /// Pending markers of other devices (D076).
        incoming: Vec<IncomingMarker>,
        /// Markers beyond the engine's cap, not read (D126).
        incoming_not_read: u64,
        /// With the desktop's bus attached: the bus of each service (D102).
        desktop_bus: Option<String>,
        /// The further roots (M5.10, D110).
        roots: Vec<control::RootReply>,
    },
}

/// The profile's disk cache with further trees on its cap (D110 amended).
pub(crate) fn cache_profile_line(profile: &control::CacheProfileReply) -> String {
    format!(
        "Cache cap:  {} of {} for {} trees together (My Files keeps at least {})",
        ByteSize(profile.bytes).human(),
        ByteSize(profile.max_bytes).human(),
        profile.trees,
        ByteSize(profile.max_bytes / 2).human()
    )
}

/// `text` with control characters replaced by `?`: names of shares, their
/// owners and the errors that quote them come from other users (D070), and
/// a terminal would act on escape sequences in them.
pub(crate) fn printable(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect()
}

/// The lines of the further roots: one per mount, one per tree below it.
/// Control characters in them are replaced ([`printable`]).
pub(crate) fn root_lines(roots: &[control::RootReply]) -> Vec<String> {
    let mut lines = Vec::new();
    for root in roots {
        let ro = if root.read_only { ", read-only" } else { "" };
        lines.push(format!(
            "Root:       {} ({}): {}{ro}",
            root.mountpoint.display(),
            root.kind,
            root.state
        ));
        for tree in &root.trees {
            let line = match &tree.error {
                Some(error) => format!("  {}: not opened: {error}", tree.name),
                None => {
                    let gate = if tree.writable {
                        "writable"
                    } else {
                        "read-only"
                    };
                    let poll = tree.events_last_poll_secs.map_or_else(
                        || "no poll yet".to_owned(),
                        |secs| format!("last poll {secs} s ago"),
                    );
                    let role = tree
                        .role
                        .as_ref()
                        .map_or_else(String::new, |role| format!(" ({role})"));
                    let every = tree
                        .poll_interval_secs
                        .map_or_else(String::new, |secs| format!(", every {secs} s"));
                    let detail = tree
                        .detail
                        .as_ref()
                        .map_or_else(String::new, |detail| format!(", {detail}"));
                    format!(
                        "  {}: {gate}{role}, {} pending, {} parked, {poll}{every}, cache {}{detail}",
                        tree.name,
                        tree.pending,
                        tree.parked,
                        ByteSize(tree.cache_bytes).human()
                    )
                }
            };
            lines.push(line);
            if let Some(note) = &tree.note {
                lines.push(format!("    {note}"));
            }
            for line in crate::incoming::status_lines(
                &tree.incoming,
                tree.incoming_not_read,
                SystemTime::now(),
            ) {
                lines.push(format!("    Incoming: {line}"));
            }
        }
    }
    lines.iter().map(|line| printable(line)).collect()
}

/// The `Orphaned:` lines (D114): one per directory under `orphaned/` with
/// its manifest, the newest first.
pub(crate) fn orphaned_lines(dir: &std::path::Path) -> Vec<String> {
    use pdmount_core::engine::orphan::OrphanManifest;
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut found: Vec<(u64, String)> = entries
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let manifest = OrphanManifest::read(&path)?;
            let count = manifest.entries.len();
            let noun = if count == 1 { "change" } else { "changes" };
            let incomplete = match manifest.incomplete() {
                0 => String::new(),
                n => format!(", {n} without their content (the raw state is in state/)"),
            };
            Some((
                manifest.time,
                format!(
                    "{count} {noun} of {} ({}){incomplete} in {}",
                    manifest.tree,
                    manifest.reason,
                    path.display()
                ),
            ))
        })
        .collect();
    found.sort_by(|a, b| b.cmp(a));
    // The tree's name, and so the directory's, is a share's.
    found
        .into_iter()
        .map(|(_, line)| printable(&line))
        .collect()
}

impl SessionState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

/// `HH:MM`, or the date as well when it is not today.
pub(crate) fn local_time(unix_secs: u64) -> String {
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
pub(crate) fn short_age(secs: u64) -> String {
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
            SessionState::KeyringLocked => writeln!(
                f,
                "Session:    unknown (keyring locked; unlock it or run `pdmount login`)"
            )?,
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
                unsaved,
            } => {
                writeln!(f, "Account:    {username}")?;
                writeln!(f, "Session:    active ({storage})")?;
                if let Some(reason) = unsaved {
                    writeln!(f, "Tokens:     {}", session::unsaved_text(reason))?;
                }
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
        self.fmt_mount(f)?;
        for (index, line) in self.orphaned.iter().enumerate() {
            let label = if index == 0 { "Orphaned:" } else { "" };
            write!(f, "\n{label:<11} {line}")?;
        }
        Ok(())
    }
}

impl StatusReport {
    fn fmt_mount(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.mount {
            MountState::NotMounted => write!(f, "Mount:      not mounted"),
            MountState::Stale => write!(
                f,
                "Mount:      stale (no daemon answers); run `pdmount unmount --force`"
            ),
            MountState::Foreign(owner) => {
                let whose = owner
                    .as_deref()
                    .map_or_else(String::new, |owner| format!(" (profile {owner})"));
                write!(
                    f,
                    "Mount:      not mounted; another PdMount mount{whose} is at the mountpoint \
                     (give this profile its own `mountpoint`, D127)"
                )
            }
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
                cache_profile,
                pinned,
                connectivity,
                connectivity_since,
                incoming,
                incoming_not_read,
                desktop_bus,
                roots,
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
                if let Some(desktop_bus) = desktop_bus {
                    writeln!(f, "Bus:        {desktop_bus}")?;
                }
                let lines =
                    crate::incoming::status_lines(incoming, *incoming_not_read, SystemTime::now());
                for (index, line) in lines.iter().enumerate() {
                    let label = if index == 0 { "Incoming:" } else { "" };
                    writeln!(f, "{label:<11} {line}")?;
                }
                writeln!(f, "Inodes:     {inodes}")?;
                write!(
                    f,
                    "Cache:      {} in {cache_blocks} blocks",
                    ByteSize(*cache_bytes).human()
                )?;
                if let Some(profile) = cache_profile.filter(|profile| profile.shared()) {
                    write!(f, "\n{}", cache_profile_line(&profile))?;
                }
                if let Some(pinned) = pinned {
                    write!(
                        f,
                        "\nPinned:     {} paths, {} of {} fetched",
                        pinned.paths,
                        ByteSize(pinned.fetched_bytes).human(),
                        ByteSize(pinned.total_bytes).human()
                    )?;
                }
                for line in root_lines(roots) {
                    write!(f, "\n{line}")?;
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
                // The daemon says what it waits for and when it tries again.
                reason: reply
                    .keyring_wait
                    .or(reply.sign_in_error)
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
                unsaved: reply.tokens_unsaved,
            }
        };
        return Ok(StatusReport {
            orphaned: orphaned_lines(&paths.orphaned_dir()),
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
                cache_profile: reply.cache_profile,
                pinned: reply.pinned,
                connectivity,
                connectivity_since: reply.connectivity_since,
                incoming: reply.incoming,
                incoming_not_read: reply.incoming_not_read,
                desktop_bus: reply.desktop_bus,
                roots: reply.roots,
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
    // A query must not open an unlock dialog (D104).
    let session = if read_would_prompt(credentials::mode(), credentials::keyring_state) {
        SessionState::KeyringLocked
    } else {
        resumed_session(paths, config, &device, true).await?
    };
    let mount = match mounts::find(&mountpoint)? {
        // No daemon of this profile: a live mount is another's (D127).
        Some(entry)
            if entry.is_pdmount()
                && entry.profile() != Some(paths.profile.as_str())
                && !mounts::is_dead(&mountpoint) =>
        {
            MountState::Foreign(entry.profile().map(str::to_owned))
        }
        Some(entry) if entry.is_pdmount() => MountState::Stale,
        _ => MountState::NotMounted,
    };
    Ok(StatusReport {
        orphaned: orphaned_lines(&paths.orphaned_dir()),
        profile: paths.profile.to_string(),
        session,
        device,
        mountpoint,
        mount,
    })
}

/// What `pdmount login` without a daemon does before a sign-in (D108).
#[derive(Debug, PartialEq, Eq)]
pub enum LoginCheck {
    /// The stored session works: report it and keep it. `unsaved`: why its
    /// new tokens could not be stored (D106 amended).
    SignedIn {
        account: String,
        unsaved: Option<String>,
    },
    /// No session, or the server rejected it: the web sign-in.
    SignIn,
    /// The session could not be checked (no network, keyring unavailable).
    Failed(String),
}

impl LoginCheck {
    fn of(session: anyhow::Result<SessionState>) -> Self {
        match session {
            Ok(SessionState::Active {
                username, unsaved, ..
            }) => Self::SignedIn {
                account: username,
                unsaved,
            },
            Ok(SessionState::NotLoggedIn | SessionState::NeedsLogin { .. }) => Self::SignIn,
            // Not made by `login_check`, which reads a locked keyring.
            Ok(SessionState::KeyringLocked) => Self::Failed(
                "the keyring is locked; unlock it and run `pdmount login` again".into(),
            ),
            Err(error) => Self::Failed(format!("{error:#}")),
        }
    }
}

/// `pdmount login` without a daemon (D108): resumes a stored session first,
/// as `status` does, but may prompt for a locked keyring (D104). `Some`: no
/// sign-in follows, and `login` exits with this code.
pub async fn login_check(
    paths: &ProfilePaths,
    config: &Config,
) -> anyhow::Result<Option<std::process::ExitCode>> {
    // A daemon that no longer answers may still save its tokens: a resume
    // here could rotate them.
    if let Some(pid) = crate::daemon::daemon_pid(paths) {
        anyhow::bail!(
            "the daemon of profile {} (pid {pid}) is shutting down; run `pdmount login` again in a moment",
            paths.profile
        );
    }
    let device = session::device_identity(paths, config)?;
    let checked = LoginCheck::of(resumed_session(paths, config, &device, false).await);
    Ok(match checked {
        LoginCheck::SignedIn { account, unsaved } => {
            println!(
                "Profile {} is already signed in as {account}; the session is kept \
                 (`pdmount login --force` signs in anew).",
                paths.profile
            );
            // The check refreshed the tokens, and the stored ones may no
            // longer work (D106 amended).
            if let Some(reason) = unsaved {
                eprintln!(
                    "warning: its new tokens are {}; if the next mount needs a sign-in, run \
                     `pdmount login` again",
                    session::unsaved_text(&reason)
                );
            }
            Some(std::process::ExitCode::SUCCESS)
        }
        LoginCheck::SignIn => None,
        LoginCheck::Failed(error) => {
            eprintln!("error: cannot check the stored session: {error}");
            eprintln!("`pdmount login --force` signs in anew without checking it.");
            Some(std::process::ExitCode::FAILURE)
        }
    })
}

/// The session as the server sees it, from the stored credentials, with the
/// quota if asked. `status` calls it only for a keyring that can be read
/// without a dialog (D104), `login` always.
async fn resumed_session(
    paths: &ProfilePaths,
    config: &Config,
    device: &DeviceIdentity,
    quota: bool,
) -> anyhow::Result<SessionState> {
    Ok(match session::resume(paths).await? {
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
                    device,
                    session::TransferLimits::of(config),
                )
                .await?;
                let username = backend.account().await?.username;
                let quota = if quota {
                    Some(backend.storage_info().await?)
                } else {
                    None
                };
                anyhow::Ok(SessionState::Active {
                    username,
                    storage: format!("stored in {}", logged_in.storage),
                    quota,
                    unsaved: logged_in.unsaved.clone(),
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
            orphaned: Vec::new(),
        }
        .to_string()
    }

    /// `login` without a daemon (D108): a working session is kept, a missing
    /// or rejected one leads to the sign-in, anything else is an error.
    #[test]
    fn login_check_keeps_a_working_session() {
        let active = SessionState::Active {
            username: "user@example.com".into(),
            storage: "stored in keyring".into(),
            quota: None,
            unsaved: None,
        };
        assert_eq!(
            LoginCheck::of(Ok(active)),
            LoginCheck::SignedIn {
                account: "user@example.com".into(),
                unsaved: None,
            }
        );
        assert_eq!(
            LoginCheck::of(Ok(SessionState::NotLoggedIn)),
            LoginCheck::SignIn
        );
        let rejected = SessionState::NeedsLogin {
            reason: "refresh token expired".into(),
            sign_in: None,
        };
        assert_eq!(LoginCheck::of(Ok(rejected)), LoginCheck::SignIn);
        let offline = anyhow::anyhow!("connection refused")
            .context("cannot reach Proton to check the session");
        assert_eq!(
            LoginCheck::of(Err(offline)),
            LoginCheck::Failed(
                "cannot reach Proton to check the session: connection refused".into()
            )
        );
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
                cache_profile: None,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
                incoming: Vec::new(),
                incoming_not_read: 0,
                desktop_bus: None,
                roots: Vec::new(),
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
        assert!(!text.contains("Incoming:"), "{text}");
    }

    #[test]
    fn mounted_report_shows_each_incoming_marker() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs();
        let marker = |device: &str, entries| IncomingMarker {
            device_id: "0b5f6c1e-6a7d-4c2b-9f1e-2a3b4c5d6e7f".into(),
            device: Some(device.into()),
            time: now - 120,
            entries,
            bytes: 2048,
        };
        let text = report_with(
            SessionState::NotLoggedIn,
            MountState::Mounted {
                pid: 7,
                queue_pending: 0,
                queue_parked: 0,
                queue_oldest_secs: None,
                events_last_poll_secs: None,
                events_refreshing: false,
                read_only: false,
                conflicts: 0,
                inodes: 1,
                cache_bytes: 0,
                cache_blocks: 0,
                cache_profile: None,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
                incoming: vec![marker("desk", 3), marker("tablet", 1)],
                incoming_not_read: 0,
                desktop_bus: None,
                roots: Vec::new(),
            },
        );
        let lines: Vec<&str> = text
            .lines()
            .skip_while(|line| !line.starts_with("Incoming:"))
            .take(2)
            .collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines[0].starts_with("Incoming:   desk: 3 changes (2.0 KiB), since "),
            "{text}"
        );
        assert!(lines[0].ends_with("(2m ago)"), "{text}");
        assert!(
            lines[1].starts_with("            tablet: 1 change (2.0 KiB), since "),
            "{text}"
        );
    }

    #[test]
    fn active_report_shows_account_and_quota() {
        let text = report(SessionState::Active {
            username: "user@proton.me".into(),
            storage: "stored in keyring".into(),
            quota: Some(StorageInfo {
                used: 3 << 30,
                max: 500 << 30,
            }),
            unsaved: None,
        });
        assert!(text.contains("Account:    user@proton.me"));
        assert!(text.contains("Storage:    3.0 GiB used of 500.0 GiB (0.6 %)"));
        assert!(text.contains("Session:    active (stored in keyring)"));
        assert!(text.contains("Device:     laptop"));
        assert!(!text.contains("Tokens:"));
    }

    /// D106 amended: tokens the keyring did not take are shown.
    #[test]
    fn unsaved_tokens_are_shown() {
        let text = report(SessionState::Active {
            username: "user@proton.me".into(),
            storage: "stored in keyring".into(),
            quota: None,
            unsaved: Some("no reply".into()),
        });
        assert!(
            text.contains("Session:    active (stored in keyring)\nTokens:     not yet saved to the keyring (no reply)\n"),
            "{text}"
        );
    }

    #[test]
    fn mount_states_are_reported() {
        let active = || SessionState::Active {
            username: "u".into(),
            storage: "stored in file".into(),
            quota: None,
            unsaved: None,
        };
        assert!(report(active()).contains("Mount:      not mounted"));
        assert!(report(active()).contains("Storage:    not fetched yet"));
        assert!(report_with(active(), MountState::Stale).contains("unmount --force"));
        let foreign = report_with(active(), MountState::Foreign(Some("work".into())));
        assert!(
            foreign.contains("Mount:      not mounted; another PdMount mount (profile work)"),
            "{foreign}"
        );
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
                cache_profile: None,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
                incoming: Vec::new(),
                incoming_not_read: 0,
                desktop_bus: None,
                roots: Vec::new(),
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
                cache_profile: None,
                pinned: None,
                connectivity: Connectivity::Online,
                connectivity_since: 0,
                incoming: Vec::new(),
                incoming_not_read: 0,
                desktop_bus: None,
                roots: Vec::new(),
            },
        );
        assert!(conflicted.contains("Conflicts:  3 open (pdmount conflicts list)"));
        assert!(mounted.contains("Cache:      1.0 MiB in 3 blocks"));
        assert!(!mounted.contains("Cache cap:"));
    }

    #[test]
    fn the_profile_cache_and_each_tree_are_shown_with_further_trees() {
        let profile = control::CacheProfileReply {
            bytes: 3 << 20,
            max_bytes: 10 << 30,
            trees: 2,
        };
        assert_eq!(
            cache_profile_line(&profile),
            "Cache cap:  3.0 MiB of 10.0 GiB for 2 trees together (My Files keeps at least 5.0 GiB)"
        );
        let lines = root_lines(&[control::RootReply {
            mountpoint: "/m".into(),
            kind: "test".into(),
            state: "mounted".into(),
            read_only: false,
            trees: vec![control::TreeReply {
                name: "work".into(),
                writable: true,
                pending: 0,
                parked: 0,
                events_last_poll_secs: Some(3),
                error: None,
                cache_bytes: 2 << 20,
                role: None,
                poll_interval_secs: None,
                detail: None,
                incoming: Vec::new(),
                incoming_not_read: 0,
                note: None,
            }],
        }]);
        assert_eq!(
            lines[1],
            "  work: writable, 0 pending, 0 parked, last poll 3 s ago, cache 2.0 MiB"
        );
        let lines = root_lines(&[control::RootReply {
            mountpoint: "/s".into(),
            kind: "shared".into(),
            state: "mounted".into(),
            read_only: false,
            trees: vec![control::TreeReply {
                name: "Team (alice)".into(),
                writable: false,
                pending: 0,
                parked: 0,
                events_last_poll_secs: Some(3),
                error: None,
                cache_bytes: 0,
                role: Some("viewer".into()),
                poll_interval_secs: Some(600),
                detail: None,
                incoming: Vec::new(),
                incoming_not_read: 0,
                note: None,
            }],
        }]);
        assert_eq!(
            lines[1],
            "  Team (alice): read-only (viewer), 0 pending, 0 parked, last poll 3 s ago, every 600 s, cache 0 B"
        );
        // A tree that lost write access, with another device's marker (D114).
        let lines = root_lines(&[control::RootReply {
            mountpoint: "/s".into(),
            kind: "shared".into(),
            state: "mounted".into(),
            read_only: false,
            trees: vec![control::TreeReply {
                name: "Team".into(),
                writable: false,
                pending: 0,
                parked: 2,
                events_last_poll_secs: Some(3),
                error: None,
                cache_bytes: 0,
                role: Some("editor".into()),
                poll_interval_secs: Some(30),
                incoming: vec![IncomingMarker {
                    device_id: "0b5f6c1e-6a7d-4c2b-9f1e-2a3b4c5d6e7f".into(),
                    device: None,
                    time: 1_800_000_000,
                    entries: 3,
                    bytes: 2048,
                }],
                incoming_not_read: 7,
                note: Some("the server refused a change".into()),
                detail: None,
            }],
        }]);
        assert_eq!(
            lines[1],
            "  Team: read-only (editor), 0 pending, 2 parked, last poll 3 s ago, every 30 s, cache 0 B"
        );
        assert_eq!(lines[2], "    the server refused a change");
        assert!(
            lines[3].starts_with("    Incoming: device 0b5f6c1e: 3 changes (2.0 KiB), since "),
            "{}",
            lines[3]
        );
        // The markers the engine's cap left out (D126).
        assert_eq!(lines[4], "    Incoming: +7 more");
        assert_eq!(lines.len(), 5, "{lines:?}");
    }

    /// Names from other users never put control characters (terminal
    /// escapes) into `status` (D070).
    #[test]
    fn share_names_and_errors_are_printed_without_control_characters() {
        let tree = |name: &str, error: Option<&str>| control::TreeReply {
            name: name.into(),
            writable: false,
            pending: 0,
            parked: 0,
            events_last_poll_secs: None,
            error: error.map(str::to_owned),
            cache_bytes: 0,
            role: Some("viewer".into()),
            poll_interval_secs: None,
            detail: None,
            incoming: Vec::new(),
            incoming_not_read: 0,
            note: None,
        };
        let lines = root_lines(&[control::RootReply {
            mountpoint: "/s".into(),
            kind: "shared".into(),
            state: "failed: share \u{1b}]0;x\u{7}".into(),
            read_only: false,
            trees: vec![
                tree("Team\u{1b}[2J", None),
                tree("Other", Some("cannot open \u{1b}[31mRed\u{1b}[0m\nline")),
            ],
        }]);
        assert!(
            lines.iter().all(|line| !line.chars().any(char::is_control)),
            "{lines:?}"
        );
        assert_eq!(lines[0], "Root:       /s (shared): failed: share ?]0;x?");
        assert!(
            lines[1].starts_with("  Team?[2J: read-only (viewer)"),
            "{}",
            lines[1]
        );
        assert_eq!(
            lines[2],
            "  Other: not opened: cannot open ?[31mRed?[0m?line"
        );

        use pdmount_core::engine::orphan::OrphanManifest;
        let dir = tempfile::tempdir().expect("tempdir");
        let export = dir.path().join("Team\u{1b}[2J (1)");
        std::fs::create_dir_all(&export).expect("dir");
        OrphanManifest::new("Team\u{1b}[2J", "the share was revoked", Vec::new())
            .write(&export)
            .expect("write");
        let lines = orphaned_lines(dir.path());
        assert!(
            lines[0].starts_with("0 changes of Team?[2J (the share"),
            "{}",
            lines[0]
        );
        assert!(!lines[0].chars().any(char::is_control), "{}", lines[0]);
    }

    #[test]
    fn orphaned_changes_are_listed_from_their_manifests() {
        use pdmount_core::engine::orphan::{OrphanEntry, OrphanManifest};
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(orphaned_lines(&dir.path().join("none")).is_empty());
        let entry = |path: &str, error: Option<&str>| OrphanEntry {
            path: path.into(),
            change: "upload".into(),
            file: None,
            error: error.map(str::to_owned),
        };
        let first = dir.path().join("Team (1)");
        std::fs::create_dir_all(&first).expect("dir");
        let mut manifest = OrphanManifest::new(
            "Team",
            "the share was revoked",
            vec![entry("/a", None), entry("/b", Some("gone"))],
        );
        manifest.time = 100;
        manifest.write(&first).expect("write");
        let second = dir.path().join("Docs (2)");
        std::fs::create_dir_all(&second).expect("dir");
        let mut manifest =
            OrphanManifest::new("Docs", "the share was revoked", vec![entry("/c", None)]);
        manifest.time = 200;
        manifest.write(&second).expect("write");
        // A directory without a manifest is not ours to report.
        std::fs::create_dir_all(dir.path().join("other")).expect("dir");
        let lines = orphaned_lines(dir.path());
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("1 change of Docs (the share was revoked) in "),
            "{}",
            lines[0]
        );
        assert!(
            lines[1].starts_with(
                "2 changes of Team (the share was revoked), 1 without their content (the raw state is in state/) in "
            ),
            "{}",
            lines[1]
        );
        let text = report_with_orphaned(lines);
        assert!(text.contains("\nOrphaned:   1 change of Docs"), "{text}");
    }

    fn report_with_orphaned(orphaned: Vec<String>) -> String {
        StatusReport {
            mount: MountState::NotMounted,
            profile: "default".into(),
            session: SessionState::NotLoggedIn,
            device: DeviceIdentity {
                id: pdmount_core::device::Uuid::nil(),
                name: "laptop".into(),
            },
            mountpoint: "/home/u/ProtonDrive".into(),
            orphaned,
        }
        .to_string()
    }

    #[test]
    fn connection_line_follows_the_daemon() {
        let mounted = |connectivity| {
            report_with(
                SessionState::Active {
                    username: "u".into(),
                    storage: "stored in file".into(),
                    quota: None,
                    unsaved: None,
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
                    cache_profile: None,
                    pinned: None,
                    connectivity,
                    connectivity_since: 1_700_000_000,
                    incoming: Vec::new(),
                    incoming_not_read: 0,
                    desktop_bus: None,
                    roots: Vec::new(),
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
    fn locked_keyring_report_says_the_session_is_unknown() {
        let text = report(SessionState::KeyringLocked);
        assert!(
            text.contains("Session:    unknown (keyring locked; unlock it or run `pdmount login`)"),
            "{text}"
        );
        assert!(!SessionState::KeyringLocked.is_active());
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
