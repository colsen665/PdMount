//! The daemon as a process and the mount as a kernel object: `fusermount`, the pid file, waiting for exit.

use super::*;

/// Runs `fusermount3 -u` (or `-uz`, lazy) on the mountpoint.
pub(crate) fn fusermount(mountpoint: &Path, lazy: bool) -> anyhow::Result<()> {
    let output = Command::new("fusermount3")
        .arg(if lazy { "-uz" } else { "-u" })
        .arg(mountpoint)
        .output()
        .context("cannot run fusermount3")?;
    if output.status.success() {
        Ok(())
    } else {
        bail!("{}", String::from_utf8_lossy(&output.stderr).trim())
    }
}

/// The running daemon of this profile, from its pid file (removed last at
/// shutdown). Never this process itself.
pub(crate) fn daemon_pid(paths: &ProfilePaths) -> Option<nix::unistd::Pid> {
    std::fs::read_to_string(paths.pid_file())
        .ok()
        .and_then(|text| text.lines().next()?.trim().parse::<i32>().ok())
        .filter(|&pid| pid as u32 != std::process::id())
        .map(nix::unistd::Pid::from_raw)
        .filter(|pid| nix::sys::signal::kill(*pid, None).is_ok() && is_pdmount_process(*pid))
}

/// Guards against a recycled pid in a stale pid file.
pub(super) fn is_pdmount_process(pid: nix::unistd::Pid) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| {
            let first = cmdline.split(|b| *b == 0).next().unwrap_or_default();
            Path::new(std::str::from_utf8(first).unwrap_or_default())
                .file_name()
                .is_some_and(|name| name == "pdmount")
        })
        .unwrap_or(false)
}

/// Fails while the profile's daemon answers or still runs: after its socket
/// is gone it may still commit the store and save tokens, so a command that
/// changes the local state or resumes the session must wait.
pub(crate) async fn refuse_while_running(paths: &ProfilePaths) -> anyhow::Result<()> {
    if control::call(&paths.control_socket(), Request::Ping, CONTROL_TIMEOUT)
        .await?
        .is_some()
    {
        bail!(
            "profile {} is mounted; run `pdmount unmount` first",
            paths.profile
        );
    }
    if let Some(pid) = daemon_pid(paths) {
        bail!(
            "the daemon of profile {} (pid {pid}) is still shutting down; try again when it has exited",
            paths.profile
        );
    }
    Ok(())
}

/// Waits until the daemon process is gone, not just its socket: it closes
/// the socket first and may need a few seconds to finish.
pub(super) async fn wait_for_daemon_exit(
    paths: &ProfilePaths,
    timeout: Duration,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    while daemon_pid(paths).is_some()
        || control::call(&paths.control_socket(), Request::Ping, CONTROL_TIMEOUT)
            .await?
            .is_some()
    {
        if Instant::now() > deadline {
            bail!("the daemon is still running after {timeout:?}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Ok(())
}

/// How long a stopping daemon may take: its grace for running steps and a
/// margin. An unreadable config counts as the default.
pub(super) fn stop_limit(paths: &ProfilePaths) -> Duration {
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    config.timeouts.shutdown_grace.0 + FORCE_KILL_MARGIN
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_test_runner_is_not_taken_for_a_daemon() {
        assert!(!is_pdmount_process(nix::unistd::Pid::this()));
    }
}
