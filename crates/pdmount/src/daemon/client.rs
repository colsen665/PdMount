//! The CLI side of talking to a running daemon: `unmount`, `drain`, `login` and a forced unmount.

use super::*;

/// `pdmount unmount [--force]`.
pub async fn unmount(
    paths: &ProfilePaths,
    mountpoint: &Path,
    force: bool,
) -> anyhow::Result<ExitCode> {
    let socket = paths.control_socket();
    let service = supervise::running_service(paths);
    if force {
        let code = force_unmount(paths, mountpoint).await?;
        if let Some(manager) = service {
            println!("{}", supervise::force_note(manager, paths.profile.as_str()));
        }
        return Ok(code);
    }
    // A supervisor would restart the daemon after a plain unmount (D093).
    if let Some(manager) = service {
        eprintln!(
            "error: {}",
            supervise::unmount_refusal(manager, paths.profile.as_str())
        );
        return Ok(ExitCode::FAILURE);
    }
    let was_mounted = mounts::find(mountpoint)?.is_some_and(|entry| entry.is_pdmount());
    drain(&socket).await?;
    match control::call_ok::<control::Done>(&socket, Request::Unmount, CONTROL_TIMEOUT).await {
        Ok(Some(_)) => {
            wait_for_daemon_exit(paths, stop_limit(paths).max(Duration::from_secs(60))).await?;
            println!("Unmounted {}.", mountpoint.display());
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!("error: cannot unmount {}: {error:#}", mountpoint.display());
            eprintln!("Close the files open under it, or use `pdmount unmount --force`.");
            Ok(ExitCode::FAILURE)
        }
        Ok(None) => match mounts::find(mountpoint)? {
            Some(entry) if entry.is_pdmount() => {
                // Not this profile's live mount: never removed (D127).
                let present = daemon_pid(paths).is_some();
                if let Some(refusal) = foreign_mount(paths, mountpoint, &entry, present) {
                    eprintln!("error: {refusal}");
                    return Ok(ExitCode::FAILURE);
                }
                fusermount(mountpoint, false).with_context(|| {
                    format!("no daemon answers and the stale mount at {} cannot be removed; try `pdmount unmount --force`", mountpoint.display())
                })?;
                println!("Removed the stale mount at {}.", mountpoint.display());
                Ok(ExitCode::SUCCESS)
            }
            // The daemon may have unmounted and exited before answering.
            _ if was_mounted => {
                println!("Unmounted {}.", mountpoint.display());
                Ok(ExitCode::SUCCESS)
            }
            _ => {
                println!("{} is not mounted.", mountpoint.display());
                Ok(ExitCode::SUCCESS)
            }
        },
    }
}

/// Uploads pending changes before an unmount. Parked ones stay in the
/// local state and are retried at the next mount.
pub(super) async fn drain(socket: &Path) -> anyhow::Result<()> {
    let request = Request::Drain {
        timeout_secs: DRAIN_TIMEOUT.as_secs(),
    };
    let Some(reply) =
        control::call_ok::<control::DrainReply>(socket, request, DRAIN_TIMEOUT + CONTROL_TIMEOUT)
            .await?
    else {
        return Ok(());
    };
    let pending = reply.pending;
    if pending > 0 {
        eprintln!(
            "warning: {pending} changes are still uploading after {} s; they continue at the next mount",
            DRAIN_TIMEOUT.as_secs()
        );
    }
    if !reply.parked_items.is_empty() {
        eprintln!(
            "warning: {} changes could not be uploaded; they are retried at the next mount:",
            reply.parked_items.len()
        );
        for (path, reason) in &reply.parked_items {
            eprintln!("  {path}: {reason}");
        }
    }
    Ok(())
}

/// `pdmount login` while the profile is mounted: the daemon runs the
/// sign-in and keeps its mount (D042); without `force` only when its session
/// is rejected (D108). `None`: no daemon answers.
pub async fn login_via_daemon(
    paths: &ProfilePaths,
    browser: BrowserMode,
    force: bool,
) -> anyhow::Result<Option<ExitCode>> {
    let socket = paths.control_socket();
    // A daemon waiting for the keyring reads it first, which may show an
    // unlock dialog (D106).
    let timeout = LOGIN_LINK_TIMEOUT + login_attempt_wait();
    let Some(reply) =
        control::call_ok::<control::LoginReply>(&socket, Request::Login { force }, timeout).await?
    else {
        return Ok(None);
    };
    if reply.not_needed {
        println!(
            "{}",
            already_signed_in(&paths.profile.to_string(), &reply.account)
        );
        return Ok(Some(ExitCode::SUCCESS));
    }
    println!("{}", session::THIRD_PARTY_NOTICE);
    session::present_link(&reply.url, &reply.code, browser);
    let deadline = Instant::now() + crate::auth::SIGN_IN_TIME + Duration::from_secs(60);
    loop {
        tokio::time::sleep(LOGIN_POLL).await;
        let Some(status) =
            control::call_ok::<control::StatusReply>(&socket, Request::Status, CONTROL_TIMEOUT)
                .await?
        else {
            bail!("the daemon stopped while waiting for the sign-in");
        };
        if status.sign_in_url.is_none() {
            if let Some(error) = status.sign_in_error {
                eprintln!("error: {error}");
                return Ok(Some(ExitCode::FAILURE));
            }
            println!("Logged in as {}; the mount continues.", status.username);
            return Ok(Some(ExitCode::SUCCESS));
        }
        if Instant::now() > deadline {
            bail!("the sign-in did not complete in time");
        }
    }
}

/// `login`'s answer when the daemon needs no sign-in (D108); an older daemon
/// does not name the account.
pub(super) fn already_signed_in(profile: &str, account: &str) -> String {
    if account.is_empty() {
        format!("Profile {profile} is mounted and needs no sign-in.")
    } else {
        format!(
            "Profile {profile} is already signed in as {account}; the mount continues \
             (`pdmount login --force` signs in anew)."
        )
    }
}

/// `unmount --force` (§14.4): lazy unmount, stop the daemon (TERM, then
/// KILL), and abort the FUSE connection if it is still there. Never needs
/// a reboot.
pub(super) async fn force_unmount(
    paths: &ProfilePaths,
    mountpoint: &Path,
) -> anyhow::Result<ExitCode> {
    use nix::sys::signal::{Signal, kill};

    let entry = mounts::find(mountpoint)?;
    // A live mount whose source names another profile is that profile's to
    // force (D127); one of an earlier version names nobody and is detached.
    if let Some(entry) = &entry
        && let Some(owner) = entry.profile()
        && owner != paths.profile.as_str()
        && !mounts::is_dead(mountpoint)
    {
        eprintln!(
            "error: {}; `pdmount --profile {owner} unmount --force {}` detaches it",
            foreign_mount_text(paths.profile.as_str(), mountpoint, Some(owner)),
            mountpoint.display()
        );
        return Ok(ExitCode::FAILURE);
    }
    if entry.is_some() {
        match fusermount(mountpoint, true) {
            Ok(()) => println!("Detached {}.", mountpoint.display()),
            Err(error) => eprintln!("warning: fusermount3 -uz failed: {error:#}"),
        }
    }
    if let Some(pid) = daemon_pid(paths) {
        let _ = kill(pid, Signal::SIGTERM);
        let deadline = Instant::now() + stop_limit(paths);
        while kill(pid, None).is_ok() && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if kill(pid, None).is_ok() {
            let _ = kill(pid, Signal::SIGKILL);
            println!("Killed the unresponsive daemon (pid {pid}).");
        } else {
            println!("Stopped the daemon (pid {pid}).");
        }
    }
    if let Some(entry) = &entry {
        let connection = entry.connection_dir();
        if connection.exists() {
            match std::fs::write(connection.join("abort"), "1") {
                Ok(()) => println!("Aborted FUSE connection {}.", entry.minor),
                Err(error) => eprintln!(
                    "warning: cannot abort FUSE connection {} ({error}); as root: echo 1 > {}",
                    entry.minor,
                    connection.join("abort").display()
                ),
            }
        }
    }
    control::remove(&paths.control_socket());
    let _ = std::fs::remove_file(paths.pid_file());
    if mounts::find(mountpoint)?.is_some() {
        eprintln!("error: {} is still mounted", mountpoint.display());
        return Ok(ExitCode::FAILURE);
    }
    println!("{} is unmounted.", mountpoint.display());
    Ok(ExitCode::SUCCESS)
}

#[cfg(test)]
mod tests {
    use super::process::is_pdmount_process;
    use super::*;

    #[test]
    fn already_signed_in_names_the_account_and_force() {
        let text = already_signed_in("default", "user@example.com");
        assert!(text.starts_with("Profile default "), "{text}");
        assert!(
            text.contains("already signed in as user@example.com"),
            "{text}"
        );
        assert!(text.contains("--force"), "{text}");
        assert!(already_signed_in("default", "").contains("needs no sign-in"));
    }

    /// A process whose `argv[0]` is `pdmount`, standing in for the daemon.
    fn fake_daemon(paths: &ProfilePaths, mountpoint: &Path, line: &str) -> std::process::Child {
        let child = Command::new("sleep")
            .arg0("pdmount")
            .arg("60")
            .spawn()
            .expect("spawn");
        // `spawn` returns once the child's old address space is gone, which is
        // before the kernel has set up the new `argv` area: `/proc/<pid>/cmdline`
        // is empty (or still the test binary's) for a moment, and
        // `is_pdmount_process` would reject the stand-in.
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid"));
        let deadline = Instant::now() + Duration::from_secs(10);
        while !is_pdmount_process(pid) {
            assert!(
                Instant::now() < deadline,
                "the stand-in never showed argv[0]"
            );
            std::thread::sleep(Duration::from_millis(2));
        }
        control::create_dir(&paths.control_socket()).expect("run dir");
        std::fs::write(
            paths.pid_file(),
            format!("{}\n{}\n{line}", child.id(), mountpoint.display()),
        )
        .expect("pid file");
        child
    }

    /// Under a service manager `unmount` refuses and touches nothing;
    /// `--force` still stops the daemon (D093).
    #[tokio::test]
    async fn unmount_under_a_service_refuses_and_force_still_works() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        let mountpoint = dir.path().join("mnt");
        let child = fake_daemon(&paths, &mountpoint, "service openrc\n");
        let pid = nix::unistd::Pid::from_raw(i32::try_from(child.id()).expect("pid"));
        // Reaps the stand-in as soon as it ends, as init would the daemon.
        let reaper = std::thread::spawn(move || {
            let mut child = child;
            child.wait()
        });
        assert_eq!(
            supervise::running_service(&paths),
            Some(supervise::Manager::OpenRc)
        );
        let code = unmount(&paths, &mountpoint, false).await.expect("unmount");
        assert_eq!(code, ExitCode::FAILURE);
        assert!(
            nix::sys::signal::kill(pid, None).is_ok(),
            "the daemon was stopped"
        );
        assert!(paths.pid_file().exists());

        let code = unmount(&paths, &mountpoint, true).await.expect("force");
        assert_eq!(code, ExitCode::SUCCESS);
        reaper
            .join()
            .expect("reaper")
            .expect("the daemon was stopped");
        assert!(!paths.pid_file().exists());
    }

    /// Without the service line `unmount` goes on as before.
    #[tokio::test]
    async fn unmount_without_a_service_is_not_refused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = test_paths(dir.path());
        let mountpoint = dir.path().join("mnt");
        let mut child = fake_daemon(&paths, &mountpoint, "");
        assert_eq!(supervise::running_service(&paths), None);
        // No socket answers and nothing is mounted: "not mounted", exit 0.
        let code = unmount(&paths, &mountpoint, false).await.expect("unmount");
        assert_eq!(code, ExitCode::SUCCESS);
        child.kill().expect("kill");
        child.wait().expect("wait");
        // A dead daemon's pid file names no service.
        std::fs::write(paths.pid_file(), "1\n/mnt\nservice openrc\n").expect("write");
        assert_eq!(supervise::running_service(&paths), None);
    }
}
