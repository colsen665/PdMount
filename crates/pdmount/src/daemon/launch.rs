//! Starting a background daemon from the CLI: the log file, the pre-start checks, stale mounts.

use super::*;

/// Opens the daemon log for appending: 0600 in a 0700 directory (DESIGN.md
/// §13.1), also when an older version created them with the umask's modes.
/// Keeps one old log: renames a log above `limit` to `<log>.1`, replacing
/// it. The daemon's output goes to a new file then.
pub(super) fn rotate_log(path: &Path, limit: u64) -> std::io::Result<()> {
    let size = match std::fs::metadata(path) {
        Ok(metadata) => metadata.len(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if size <= limit {
        return Ok(());
    }
    let mut old = path.as_os_str().to_owned();
    old.push(".1");
    std::fs::rename(path, old)
}

pub(super) fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)?;
    log.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    Ok(log)
}

/// `pdmount mount` without `--foreground`.
pub async fn mount_background(
    paths: &ProfilePaths,
    mountpoint: &Path,
    backend: &BackendChoice,
    verbose: u8,
    login: Option<BrowserMode>,
    read_only: bool,
) -> anyhow::Result<ExitCode> {
    preflight(paths, mountpoint).await?;
    // Reading the session here makes a keyring unlock dialog appear while this
    // terminal waits; the detached daemon has no terminal (D029).
    if *backend == BackendChoice::Proton && Credentials::new(paths).load()?.is_none() {
        let Some(browser) = login else {
            bail!(
                "profile {} is not logged in; run `pdmount login`",
                paths.profile
            );
        };
        println!("Profile {} is not logged in.", paths.profile);
        let logged_in = session::login(paths, browser).await?;
        println!(
            "Logged in as {} (session stored in {}).",
            logged_in.session.username, logged_in.storage
        );
    }
    let log_path = paths.log_file();
    if let Err(error) = rotate_log(&log_path, LOG_ROTATE_BYTES) {
        eprintln!("warning: cannot rotate {}: {error}", log_path.display());
    }
    let log = open_log(&log_path).with_context(|| format!("cannot open {}", log_path.display()))?;
    let log_start = log.metadata().map_or(0, |m| m.len());
    let mut command =
        Command::new(std::env::current_exe().context("cannot find the pdmount executable")?);
    command.arg("--profile").arg(paths.profile.as_str());
    // This launcher is not a supervised service: the daemon must not report
    // readiness to a supervisor that started the launcher (D093).
    command.env_remove("NOTIFY_SOCKET");
    command.args(crate::credentials::mode().flag());
    for _ in 0..verbose.max(1) {
        command.arg("-v");
    }
    command
        .arg("mount")
        .arg("--foreground")
        .arg("--backend")
        .arg(backend.arg())
        .args(read_only.then_some(["-o", "ro"]).into_iter().flatten())
        .arg(mountpoint)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log)
        // Its own process group: Ctrl-C in this terminal does not reach it.
        .process_group(0);
    let mut child = command.spawn().context("cannot start the mount daemon")?;
    let socket = paths.control_socket();
    let deadline = Instant::now() + START_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!(
                "the mount daemon exited ({status}):\n{}",
                log_tail(&log_path, log_start).trim_end()
            );
        }
        if control::call(&socket, Request::Ping, CONTROL_TIMEOUT)
            .await?
            .is_some()
        {
            println!(
                "Mounted Proton Drive (profile {}) at {}.",
                paths.profile,
                mountpoint.display()
            );
            println!("Log: {}", log_path.display());
            return Ok(ExitCode::SUCCESS);
        }
        if Instant::now() > deadline {
            bail!(
                "the mount daemon (pid {}) did not get ready within {START_TIMEOUT:?}; see {}",
                child.id(),
                log_path.display()
            );
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

pub(super) fn log_tail(path: &Path, from: u64) -> String {
    let Ok(text) = std::fs::read(path) else {
        return String::new();
    };
    let start = (from as usize).min(text.len());
    let tail = String::from_utf8_lossy(&text[start..]).into_owned();
    let lines: Vec<&str> = tail.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// Refuses to mount over a running daemon or an existing mount.
pub(super) async fn preflight(paths: &ProfilePaths, mountpoint: &Path) -> anyhow::Result<()> {
    if let Some(pid) = daemon_pid(paths) {
        bail!(
            "the daemon of profile {} (pid {pid}) is still running or shutting down; see `pdmount status`",
            paths.profile
        );
    }
    if control::call(&paths.control_socket(), Request::Ping, CONTROL_TIMEOUT)
        .await?
        .is_some()
    {
        bail!(
            "profile {} is already mounted; see `pdmount status`",
            paths.profile
        );
    }
    if let Some(entry) = mounts::find(mountpoint)? {
        if entry.is_pdmount() {
            // No daemon of this profile runs: a live mount is another's (D127).
            if let Some(refusal) = foreign_mount(paths, mountpoint, &entry, false) {
                bail!("{refusal}");
            }
            bail!(
                "{} is still mounted, but no daemon answers (stale mount); run `pdmount unmount --force {}`",
                mountpoint.display(),
                mountpoint.display()
            );
        }
        bail!(
            "{} is already a mount point ({})",
            mountpoint.display(),
            entry.fs_type
        );
    }
    Ok(())
}

/// The refusal for a live PdMount mount at `mountpoint` that is not profile
/// `profile`'s (D127); `owner` is the profile its source names, if any.
pub(crate) fn foreign_mount_text(profile: &str, mountpoint: &Path, owner: Option<&str>) -> String {
    let whose = owner.map_or_else(String::new, |owner| format!(" (profile {owner})"));
    format!(
        "another PdMount mount{whose} is at {}; give profile {profile} its own `mountpoint` in its \
         config (D127)",
        mountpoint.display()
    )
}

/// Whether a PdMount mount is the profile's: its source names the profile
/// (D127), or, for a mount of an earlier version that names none, a daemon
/// of the profile answers or runs (`daemon_present`).
fn is_own(profile: &str, entry: &mounts::MountEntry, daemon_present: bool) -> bool {
    entry
        .profile()
        .map_or(daemon_present, |owner| owner == profile)
}

/// The refusal of D127 when `entry`, the mount at `mountpoint`, is a live
/// PdMount mount that is not this profile's: such a mount is never
/// detached, whatever this profile's daemon does. `None` for any other
/// mount, this profile's, and a dead one (which is stale).
pub(super) fn foreign_mount(
    paths: &ProfilePaths,
    mountpoint: &Path,
    entry: &mounts::MountEntry,
    daemon_present: bool,
) -> Option<String> {
    let profile = paths.profile.as_str();
    (entry.is_pdmount() && !is_own(profile, entry, daemon_present) && !mounts::is_dead(mountpoint))
        .then(|| foreign_mount_text(profile, mountpoint, entry.profile()))
}

/// What `mount --foreground --clear-stale` and `unmount --if-stale` do with
/// a mountpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum StaleAction {
    /// Detach a dead pdmount mount with `fusermount3 -uz`.
    Detach,
    /// Touch nothing: no pdmount mount, a daemon of the profile answers or
    /// still runs, or the profile's own live mount; `preflight` reports it.
    Leave,
    /// Touch nothing and fail: a live mount of another profile (D127), whose
    /// source names it (`Some`) or, from an earlier version, nobody.
    Refuse(Option<String>),
}

/// Only a dead pdmount mount (its FUSE connection gone, `dead`) is stale,
/// and only while no daemon of the profile answers or still runs: a daemon
/// that is slow or shutting down keeps its mount (D093). A live mount is
/// never stale; one that is not this profile's is refused (D127). `dead` is
/// asked only when it matters.
pub(super) fn stale_action(
    profile: &str,
    daemon_answers: bool,
    daemon_running: bool,
    mount: Option<&mounts::MountEntry>,
    dead: impl FnOnce() -> bool,
) -> StaleAction {
    let Some(entry) = mount.filter(|entry| entry.is_pdmount()) else {
        return StaleAction::Leave;
    };
    if daemon_answers || daemon_running {
        return StaleAction::Leave;
    }
    if dead() {
        return StaleAction::Detach;
    }
    if is_own(profile, entry, false) {
        StaleAction::Leave
    } else {
        StaleAction::Refuse(entry.profile().map(str::to_owned))
    }
}

/// `--clear-stale`: the stale-mount check and the lazy unmount of
/// `unmount --force`, so that a supervisor's restart after a crash finds the
/// mountpoint usable; the further roots' mountpoints (`roots`) too, whose
/// mounts a crashed daemon leaves as well. Another profile's live mount
/// fails the start (D127).
pub(super) async fn clear_stale_mount(
    paths: &ProfilePaths,
    mountpoint: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<()> {
    let cleared = detach_if_stale(paths, mountpoint, roots).await?;
    if !cleared.refused.is_empty() {
        bail!("{}", cleared.refused.join("\n"));
    }
    Ok(())
}

/// What [`detach_if_stale`] did.
pub(super) struct Cleared {
    /// The mountpoints it detached.
    pub detached: Vec<PathBuf>,
    /// The refusals for other profiles' live mounts (D127).
    pub refused: Vec<String>,
}

/// The stale rule (`stale_action`) applied to `mountpoint` and the further
/// roots' `roots` and, for each stale mount, the lazy unmount.
pub(super) async fn detach_if_stale(
    paths: &ProfilePaths,
    mountpoint: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<Cleared> {
    let answers = !matches!(
        control::call(&paths.control_socket(), Request::Ping, CONTROL_TIMEOUT).await,
        Ok(None)
    );
    let running = daemon_pid(paths).is_some();
    let profile = paths.profile.as_str();
    let mut cleared = Cleared {
        detached: Vec::new(),
        refused: Vec::new(),
    };
    let mut seen = Vec::new();
    for target in std::iter::once(mountpoint).chain(roots.iter().map(PathBuf::as_path)) {
        if seen.contains(&target) {
            continue;
        }
        seen.push(target);
        let entry = mounts::find(target)?;
        match stale_action(profile, answers, running, entry.as_ref(), || {
            mounts::is_dead(target)
        }) {
            StaleAction::Detach => {
                tracing::warn!(mountpoint = %target.display(), "removing a stale mount");
                fusermount(target, true).with_context(|| {
                    format!("cannot remove the stale mount at {}", target.display())
                })?;
                cleared.detached.push(target.to_owned());
            }
            StaleAction::Refuse(owner) => {
                cleared
                    .refused
                    .push(foreign_mount_text(profile, target, owner.as_deref()));
            }
            StaleAction::Leave => {}
        }
    }
    Ok(cleared)
}

/// `pdmount unmount --if-stale`: what the systemd unit's `ExecStopPost=`
/// runs. Detaches the mountpoint, and those of the further roots (`roots`),
/// only where it is a dead pdmount mount and no daemon of the profile
/// answers or runs; a live daemon's mount, a mount of another kind and no
/// mount at all are left alone, exit 0 (D094). Another profile's live mount
/// is left alone too, but with an error and exit 1 (D127); the unit's
/// `ExecStopPost=-` ignores the code.
pub async fn unmount_if_stale(
    paths: &ProfilePaths,
    mountpoint: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<ExitCode> {
    let cleared = detach_if_stale(paths, mountpoint, roots).await?;
    for detached in &cleared.detached {
        println!("Removed the stale mount at {}.", detached.display());
    }
    for refusal in &cleared.refused {
        eprintln!("error: {refusal}");
    }
    Ok(if cleared.refused.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_large_log_moves_aside_and_replaces_the_old_one() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("p.log");
        let old = dir.path().join("p.log.1");
        rotate_log(&log, 4).expect("no log yet");
        std::fs::write(&log, "1234").expect("write");
        rotate_log(&log, 4).expect("at the limit");
        assert!(log.exists() && !old.exists());
        std::fs::write(&log, "12345").expect("write");
        rotate_log(&log, 4).expect("over the limit");
        assert!(!log.exists());
        assert_eq!(std::fs::read_to_string(&old).expect("old"), "12345");
        std::fs::write(&log, "abcdef").expect("write");
        rotate_log(&log, 4).expect("again");
        assert_eq!(std::fs::read_to_string(&old).expect("old"), "abcdef");
    }

    #[test]
    fn the_log_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let mode =
            |path: &Path| std::fs::metadata(path).expect("stat").permissions().mode() & 0o777;
        let old = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o022));
        let tmp = tempfile::tempdir().expect("tempdir");
        let dir = tmp.path().join("state/pdmount");
        let log = dir.join("work.log");
        open_log(&log).expect("open");
        assert_eq!((mode(&dir), mode(&log)), (0o700, 0o600));

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        std::fs::set_permissions(&log, std::fs::Permissions::from_mode(0o644)).expect("chmod");
        open_log(&log).expect("reopen");
        nix::sys::stat::umask(old);
        assert_eq!((mode(&dir), mode(&log)), (0o700, 0o600));
    }

    fn entry(fs_type: &str, source: &str) -> mounts::MountEntry {
        mounts::MountEntry {
            mount_point: "/mnt".into(),
            fs_type: fs_type.into(),
            source: source.into(),
            minor: 40,
        }
    }

    /// `stale_action` for profile `default`; `dead` must be asked only
    /// when no daemon of the profile answers or runs.
    fn action(
        answers: bool,
        running: bool,
        mount: Option<&mounts::MountEntry>,
        dead: bool,
    ) -> StaleAction {
        stale_action("default", answers, running, mount, || {
            assert!(
                !answers && !running,
                "liveness asked while a daemon is there"
            );
            dead
        })
    }

    /// `--clear-stale` acts only on a dead pdmount mount that no daemon of
    /// the profile answers for or runs behind (D093, D127).
    #[test]
    fn clear_stale_touches_only_a_dead_mount_without_a_daemon() {
        let own = entry("fuse.pdmount", "pdmount:default");
        assert_eq!(action(false, false, Some(&own), true), StaleAction::Detach);
        // A daemon answers: its mount is not stale.
        assert_eq!(action(true, true, Some(&own), true), StaleAction::Leave);
        assert_eq!(action(true, false, Some(&own), true), StaleAction::Leave);
        // A daemon that does not answer but runs (hung, shutting down).
        assert_eq!(action(false, true, Some(&own), true), StaleAction::Leave);
        // The profile's own live mount without a daemon here: `preflight` reports it.
        assert_eq!(action(false, false, Some(&own), false), StaleAction::Leave);
        // Somebody else's mount, or none.
        let sshfs = entry("fuse.sshfs", "host:/x");
        assert_eq!(action(false, false, Some(&sshfs), true), StaleAction::Leave);
        assert_eq!(action(false, false, None, true), StaleAction::Leave);
    }

    /// Two profiles on one mountpoint (V-6, D127): `work` is mounted and
    /// live; `default`, with no daemon, neither detaches it at a
    /// `--clear-stale` start nor at `unmount --if-stale`, but refuses. Once
    /// `work`'s mount is dead, it is cleared like any stale mount.
    #[test]
    fn another_profiles_live_mount_is_refused_and_a_dead_one_cleared() {
        let work = entry("fuse.pdmount", "pdmount:work");
        assert_eq!(
            action(false, false, Some(&work), false),
            StaleAction::Refuse(Some("work".into()))
        );
        assert_eq!(action(false, false, Some(&work), true), StaleAction::Detach);
        // A mount of a version before D127 names nobody: live, it is refused too.
        let older = entry("fuse.pdmount", "pdmount");
        assert_eq!(
            action(false, false, Some(&older), false),
            StaleAction::Refuse(None)
        );
        assert_eq!(
            action(false, false, Some(&older), true),
            StaleAction::Detach
        );
        // While this profile's daemon runs, `preflight` reports it instead.
        assert_eq!(action(false, true, Some(&work), false), StaleAction::Leave);
    }

    #[test]
    fn the_refusal_names_the_owner_and_the_fix() {
        let text = foreign_mount_text("default", Path::new("/home/u/ProtonDrive"), Some("work"));
        assert!(
            text.starts_with("another PdMount mount (profile work) is at /home/u/ProtonDrive;"),
            "{text}"
        );
        assert!(
            text.contains("give profile default its own `mountpoint`"),
            "{text}"
        );
        let text = foreign_mount_text("default", Path::new("/m"), None);
        assert!(
            text.starts_with("another PdMount mount is at /m;"),
            "{text}"
        );
    }

    /// A live mount is the profile's when its source names the profile;
    /// one of an earlier version only while the profile's daemon is there.
    #[test]
    fn ownership_follows_the_source() {
        let own = entry("fuse.pdmount", "pdmount:default");
        let work = entry("fuse.pdmount", "pdmount:work");
        let older = entry("fuse.pdmount", "pdmount");
        assert!(is_own("default", &own, false));
        assert!(!is_own("default", &work, true));
        assert!(is_own("default", &older, true));
        assert!(!is_own("default", &older, false));
    }

    #[tokio::test]
    async fn clear_stale_without_a_mount_does_nothing() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        clear_stale_mount(&paths, &dir.path().join("mnt"), &[dir.path().join("root")])
            .await
            .expect("nothing to clear");
    }
}
