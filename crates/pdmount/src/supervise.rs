//! The daemon under a supervisor (DESIGN.md §14.4, D074, D093): readiness
//! for `NOTIFY_SOCKET` and OpenRC's `notify="fd:N"`, how the daemon learns
//! that a service manager runs it, and the checks that keep a profile from
//! being started by two mechanisms (an autostart entry and a user service).

use anyhow::{Context, bail};
use pdmount_core::ProfilePaths;
use std::ffi::{OsStr, OsString};
use std::os::fd::RawFd;
use std::os::linux::net::SocketAddrExt;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The service managers `pdmount service` supports (D074). Also the value
/// of `mount --foreground --service` that unit files pass (D093).
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Manager {
    #[value(name = "openrc")]
    OpenRc,
    #[value(name = "systemd")]
    Systemd,
}

impl Manager {
    /// The word in the pid file and on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Self::OpenRc => "openrc",
            Self::Systemd => "systemd",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "openrc" => Some(Self::OpenRc),
            "systemd" => Some(Self::Systemd),
            _ => None,
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::OpenRc => "OpenRC",
            Self::Systemd => "systemd",
        }
    }

    /// The command that stops the profile's service.
    pub fn stop_command(self, profile: &str) -> String {
        match self {
            Self::OpenRc => format!("rc-service --user pdmount.{profile} stop"),
            Self::Systemd => format!("systemctl --user stop pdmount@{profile}"),
        }
    }

    /// The command that starts the profile's service again.
    pub fn start_command(self, profile: &str) -> String {
        match self {
            Self::OpenRc => format!("rc-service --user pdmount.{profile} start"),
            Self::Systemd => format!("systemctl --user start pdmount@{profile}"),
        }
    }
}

/// What `mount --foreground` was told about its supervisor (D093).
#[derive(Debug, Clone, Default)]
pub struct Supervision {
    /// `--service`: a service manager of this kind runs the daemon.
    pub service: Option<Manager>,
    /// `$NOTIFY_SOCKET` of the process.
    pub notify_socket: Option<OsString>,
    /// `--notify-fd`.
    pub notify_fd: Option<RawFd>,
    /// `--clear-stale`.
    pub clear_stale: bool,
}

/// The exit code of a daemon under `--service` whose start failed before it
/// reported ready (D109): `EX_CONFIG` of `sysexits.h`. The systemd unit's
/// `RestartPreventExitStatus=` names it, so that only a daemon that fails
/// after readiness is restarted; OpenRC's supervisor ignores exit codes.
pub const START_FAILED_EXIT: u8 = 78;

/// Set once the daemon has told its supervisor that it is ready.
static REPORTED_READY: AtomicBool = AtomicBool::new(false);

/// The exit code of a failed `pdmount` process: [`START_FAILED_EXIT`] for a
/// daemon under a service manager that never reported ready (D109), else 1.
pub fn failure_exit(service: Option<Manager>) -> ExitCode {
    failure_exit_code(service, REPORTED_READY.load(Ordering::SeqCst))
}

fn failure_exit_code(service: Option<Manager>, ready: bool) -> ExitCode {
    match service {
        Some(manager) if !ready => {
            eprintln!(
                "the start failed before the mount was ready (exit {START_FAILED_EXIT}): the {} \
                 service is not restarted; start it again once the cause above is fixed (D109)",
                manager.title()
            );
            ExitCode::from(START_FAILED_EXIT)
        }
        _ => ExitCode::FAILURE,
    }
}

impl Supervision {
    /// Tells the supervisor that the mount answers. Failures are logged and
    /// never stop the daemon: a mount that works matters more than the
    /// report. From here on a failure exits 1, and a service manager
    /// restarts the daemon (D109).
    pub async fn ready(&self) {
        REPORTED_READY.store(true, Ordering::SeqCst);
        let socket = self.notify_socket.clone();
        let fd = self.notify_fd;
        let result = tokio::task::spawn_blocking(move || {
            let mut told = Vec::new();
            if let Some(socket) = socket {
                match notify_socket_ready(&socket) {
                    Ok(()) => told.push("NOTIFY_SOCKET"),
                    Err(error) => tracing::warn!(%error, "cannot notify NOTIFY_SOCKET"),
                }
            }
            if let Some(fd) = fd {
                match notify_fd_ready(fd) {
                    Ok(()) => told.push("notify-fd"),
                    Err(error) => {
                        tracing::warn!(%error, fd, "cannot notify through the descriptor")
                    }
                }
            }
            told
        })
        .await;
        match result {
            Ok(told) if !told.is_empty() => {
                tracing::info!(via = %told.join(", "), "told the supervisor that the mount is ready");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "readiness notification failed"),
        }
    }
}

/// How long a readiness datagram may wait for room in the receiver's queue.
const NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);

/// Sends `READY=1` as one datagram to `address`, as `sd_notify` does: a
/// path, or `@name` for an abstract socket.
pub fn notify_socket_ready(address: &OsStr) -> std::io::Result<()> {
    let bytes = address.as_bytes();
    let target = match bytes.first() {
        Some(b'@') => SocketAddr::from_abstract_name(&bytes[1..])?,
        Some(b'/') => SocketAddr::from_pathname(Path::new(address))?,
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "NOTIFY_SOCKET is neither an absolute path nor @name",
            ));
        }
    };
    let socket = UnixDatagram::unbound()?;
    socket.set_write_timeout(Some(NOTIFY_TIMEOUT))?;
    socket.send_to_addr(b"READY=1", &target)?;
    Ok(())
}

/// Writes a newline to descriptor `fd` and closes it: what OpenRC's
/// `notify="fd:N"` waits for. Unsafe code is denied, so the descriptor is
/// written through its `/proc/self/fd` entry (a pipe, as supervise-daemon
/// makes it) and then closed by number.
///
/// Only a pipe is written and closed (D109): a daemon that `supervise-daemon`
/// respawns has no pipe, and the number may then be another file, such as
/// the log the supervisor opened; that is left as it is and reported.
pub fn notify_fd_ready(fd: RawFd) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::FileTypeExt;
    if fd < 3 {
        bail!("descriptor {fd} is stdin, stdout or stderr");
    }
    let entry = format!("/proc/self/fd/{fd}");
    let kind = std::fs::metadata(&entry)
        .with_context(|| format!("descriptor {fd} is not open"))?
        .file_type();
    if !kind.is_fifo() {
        let target = std::fs::read_link(&entry).unwrap_or_default();
        bail!(
            "descriptor {fd} is not a pipe but {}, so the supervisor does not wait on it (a \
             respawned daemon); left untouched",
            target.display()
        );
    }
    let mut pipe = std::fs::OpenOptions::new()
        .write(true)
        .open(&entry)
        .with_context(|| format!("descriptor {fd} is not open for writing"))?;
    let written = pipe.write_all(b"\n");
    drop(pipe);
    // Closing the original ends the supervisor's wait even after an error.
    let closed = nix::unistd::close(fd);
    written.context("cannot write to the notification descriptor")?;
    closed.context("cannot close the notification descriptor")?;
    Ok(())
}

/// The line of the pid file that says a service manager runs the daemon
/// (after the pid and the mountpoint).
pub fn pid_file_line(service: Option<Manager>) -> Option<String> {
    service.map(|manager| format!("service {}\n", manager.name()))
}

/// The service manager the running daemon of the profile was started by
/// (`mount --foreground --service`), from its pid file. `None` for no
/// daemon, a daemon that is not under a service, and a dead daemon's stale
/// pid file.
pub fn running_service(paths: &ProfilePaths) -> Option<Manager> {
    crate::daemon::daemon_pid(paths)?;
    let text = std::fs::read_to_string(paths.pid_file()).ok()?;
    recorded_service(&text)
}

fn recorded_service(pid_file: &str) -> Option<Manager> {
    pid_file
        .lines()
        .skip(2)
        .find_map(|line| Manager::from_name(line.strip_prefix("service ")?.trim()))
}

/// What `pdmount unmount` prints, and exits 1 on, while a service runs the
/// daemon (D093). What the supervisor does after `--force` differs: OpenRC's
/// `supervise-daemon` restarts the daemon after any exit, systemd's
/// `Restart=on-failure` only after a crash.
pub fn unmount_refusal(manager: Manager, profile: &str) -> String {
    let stop = manager.stop_command(profile);
    match manager {
        Manager::OpenRc => format!(
            "the daemon of profile {profile} runs under an OpenRC user service, which would start it \
             again after a plain unmount.\n\
             Stop the service instead: {stop}\n\
             (`pdmount unmount --force` still detaches the mount and stops the daemon; OpenRC then \
             restarts it.)"
        ),
        Manager::Systemd => format!(
            "the daemon of profile {profile} runs under a systemd user unit; stop it through the \
             unit, so that the unit and the daemon agree and a changed `Restart=` setting cannot \
             bring it back.\n\
             Stop the service instead: {stop}\n\
             (`pdmount unmount --force` still detaches the mount and stops the daemon; systemd does \
             not restart a daemon that exits normally, so the unit then stays stopped until `{start}`, \
             and stays enabled for the next login.)",
            start = manager.start_command(profile)
        ),
    }
}

/// What `pdmount unmount --force` adds after it stopped a daemon that ran
/// under a service.
pub fn force_note(manager: Manager, profile: &str) -> String {
    match manager {
        Manager::OpenRc => format!(
            "note: the OpenRC service of profile {profile} restarts the daemon; to keep it stopped: {}",
            manager.stop_command(profile)
        ),
        Manager::Systemd => format!(
            "note: the systemd unit of profile {profile} stays stopped, as it restarts the daemon \
             only after a crash (a daemon that had to be killed counts); start it again with: {}. \
             The unit stays enabled for the next login.",
            manager.start_command(profile)
        ),
    }
}

// ---- A daemon outside OpenRC's state (D109) ----------------------------------

/// The state directories in which OpenRC keeps a service whose daemon may
/// run: started, or on its way in or out.
const OPENRC_RUNNING_STATES: [&str; 4] = ["started", "starting", "stopping", "inactive"];

/// `$XDG_RUNTIME_DIR/openrc`, OpenRC's state for user services; `None` without
/// a runtime directory.
fn openrc_user_state_dir(paths: &ProfilePaths) -> Option<PathBuf> {
    Some(paths.runtime_dir.as_ref()?.parent()?.join("openrc"))
}

/// Whether OpenRC, with its state in `state_dir`, holds `pdmount.<profile>`
/// as stopped. Read from the state directory rather than through
/// `rc-service … status`, which runs the init script and may rebuild the
/// dependency cache first: `status` stays a few file lookups. `false`
/// when OpenRC keeps no user state there.
fn openrc_reports_stopped(state_dir: &Path, profile: &str) -> bool {
    let name = format!("pdmount.{profile}");
    state_dir.is_dir()
        && !OPENRC_RUNNING_STATES
            .iter()
            .any(|state| state_dir.join(state).join(&name).symlink_metadata().is_ok())
}

/// `status` and `doctor` (D109): the daemon of the profile runs under an
/// OpenRC supervisor while OpenRC holds the service as stopped. A start that
/// failed before D109 left its supervisor running, which respawned the
/// daemon; `rc-service` neither stops it nor starts the service over it.
pub fn openrc_stray(paths: &ProfilePaths) -> Option<String> {
    if running_service(paths)? != Manager::OpenRc {
        return None;
    }
    let profile = paths.profile.as_str();
    openrc_reports_stopped(&openrc_user_state_dir(paths)?, profile)
        .then(|| openrc_stray_text(profile))
}

fn openrc_stray_text(profile: &str) -> String {
    format!(
        "the daemon runs outside OpenRC's state: OpenRC holds pdmount.{profile} as stopped, but its \
         supervisor still runs the daemon (left by a start that failed before D109), so \
         `rc-service` cannot stop it and a start fails with \"already running\". Stop it with: \
         RC_USER_SERVICES=yes RC_SVCNAME=pdmount.{profile} supervise-daemon pdmount.{profile} -U --stop \
         (the daemon flushes and unmounts as at a normal stop), then: {}",
        Manager::OpenRc.start_command(profile)
    )
}

// ---- Double start: autostart entry and user service (D074, D093) --------

/// `$XDG_CONFIG_HOME`, from the profile's paths.
pub fn config_home(paths: &ProfilePaths) -> &Path {
    paths.config_dir.parent().unwrap_or(&paths.config_dir)
}

/// OpenRC: `$XDG_CONFIG_HOME/rc/init.d/pdmount.<profile>`, a link to the
/// shared `pdmount` script (`docs/plans/m5.md`, M5.4).
pub fn openrc_service_link(paths: &ProfilePaths) -> PathBuf {
    config_home(paths)
        .join("rc/init.d")
        .join(format!("pdmount.{}", paths.profile))
}

/// systemd: the template unit `$XDG_CONFIG_HOME/systemd/user/pdmount@.service`.
pub fn systemd_unit_template(paths: &ProfilePaths) -> PathBuf {
    config_home(paths).join("systemd/user/pdmount@.service")
}

/// systemd: the link `enable` makes for the profile, in whichever
/// `<target>.wants` directory of the user's unit directory the unit's
/// `[Install]` names.
pub fn systemd_enabled_link(paths: &ProfilePaths) -> Option<PathBuf> {
    let units = config_home(paths).join("systemd/user");
    let name = format!("pdmount@{}.service", paths.profile);
    std::fs::read_dir(&units)
        .ok()?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().as_bytes().ends_with(b".wants"))
        .map(|entry| entry.path().join(&name))
        .find(|link| link.symlink_metadata().is_ok())
}

/// A user service installed for the profile: its manager and the file that
/// shows it.
pub fn installed_service(paths: &ProfilePaths) -> Option<(Manager, PathBuf)> {
    let openrc = openrc_service_link(paths);
    if openrc.symlink_metadata().is_ok() {
        return Some((Manager::OpenRc, openrc));
    }
    if systemd_unit_template(paths).exists()
        && let Some(link) = systemd_enabled_link(paths)
    {
        return Some((Manager::Systemd, link));
    }
    None
}

/// `autostart enable`: refuses while a user service is installed for the
/// profile, since both would start the daemon at login.
pub fn refuse_autostart_over_service(paths: &ProfilePaths) -> anyhow::Result<()> {
    if let Some((manager, file)) = installed_service(paths) {
        bail!(
            "a {} user service is installed for profile {} ({}); autostart would start a second daemon. \
             Remove the service first (`pdmount service uninstall`)",
            manager.title(),
            paths.profile,
            file.display()
        );
    }
    Ok(())
}

/// `service install` (M5.4, M5.5): refuses while the autostart entry of the
/// profile is enabled.
pub fn refuse_service_over_autostart(paths: &ProfilePaths) -> anyhow::Result<()> {
    let entry = crate::autostart::entry_path(paths);
    if crate::autostart::enabled(paths) {
        bail!(
            "autostart is enabled for profile {} ({}); the service would start a second daemon. \
             Run `pdmount autostart disable` first",
            paths.profile,
            entry.display()
        );
    }
    Ok(())
}

/// `doctor`: the conflict between the two, if both are set.
pub fn double_start(paths: &ProfilePaths) -> Option<String> {
    let (manager, file) = installed_service(paths)?;
    let entry = crate::autostart::entry_path(paths);
    crate::autostart::enabled(paths).then(|| {
        format!(
            "conflict: autostart ({}) and the {} user service ({}) would both start the daemon; \
             keep one (`pdmount autostart disable` or `pdmount service uninstall`)",
            entry.display(),
            manager.title(),
            file.display()
        )
    })
}

/// One line for `doctor`: the user service of the profile, if any.
pub fn service_state_text(paths: &ProfilePaths) -> String {
    match installed_service(paths) {
        Some((manager, file)) => format!(
            "{} user service installed for profile {} ({})",
            manager.title(),
            paths.profile,
            file.display()
        ),
        None => format!("no user service installed for profile {}", paths.profile),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_core::paths::BaseDirs;
    use std::io::Read;
    use std::os::fd::IntoRawFd;

    fn paths_in(root: &Path, profile: &str) -> ProfilePaths {
        ProfilePaths::new(
            profile.parse().expect("profile"),
            &BaseDirs {
                config: root.join("config"),
                data: root.join("data"),
                cache: root.join("cache"),
                state: root.join("state"),
                runtime: Some(root.join("run")),
            },
        )
    }

    fn touch(path: &Path) {
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        std::fs::write(path, "").expect("write");
    }

    #[test]
    fn ready_goes_to_a_path_socket() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("notify");
        let receiver = UnixDatagram::bind(&path).expect("bind");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        notify_socket_ready(path.as_os_str()).expect("notify");
        let mut buf = [0u8; 64];
        let n = receiver.recv(&mut buf).expect("datagram");
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn ready_goes_to_an_abstract_socket() {
        let name = format!(
            "pdmount-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        );
        let receiver = UnixDatagram::bind_addr(
            &SocketAddr::from_abstract_name(name.as_bytes()).expect("address"),
        )
        .expect("bind");
        receiver
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        notify_socket_ready(OsStr::new(&format!("@{name}"))).expect("notify");
        let mut buf = [0u8; 64];
        let n = receiver.recv(&mut buf).expect("datagram");
        assert_eq!(&buf[..n], b"READY=1");
    }

    #[test]
    fn a_bad_or_dead_notify_socket_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(notify_socket_ready(OsStr::new("relative")).is_err());
        assert!(notify_socket_ready(OsStr::new("")).is_err());
        // A path nobody listens on.
        assert!(notify_socket_ready(dir.path().join("none").as_os_str()).is_err());
    }

    #[test]
    fn ready_writes_a_newline_and_closes_the_descriptor() {
        let (read, write) = nix::unistd::pipe().expect("pipe");
        let fd = write.into_raw_fd();
        notify_fd_ready(fd).expect("notify");
        // Both ends of the write side are closed: the reader sees the
        // newline, then end of file.
        let mut reader = std::fs::File::from(read);
        let mut text = String::new();
        reader.read_to_string(&mut text).expect("read");
        // The end of file shows that `fd` was closed too. It is not used again
        // here: another test's thread may have the number by now, and a
        // second call would close that descriptor.
        assert_eq!(text, "\n");
    }

    /// A descriptor that is not a pipe, such as the log a respawned daemon
    /// gets as descriptor 3 (D109), is neither written nor closed.
    #[test]
    fn a_descriptor_that_is_not_a_pipe_is_left_alone() {
        use std::io::{Seek, Write};
        use std::os::fd::AsRawFd;
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log");
        std::fs::write(&path, "first line\n").expect("write");
        let mut log = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .expect("open");
        let error = notify_fd_ready(log.as_raw_fd()).expect_err("not a pipe");
        assert!(format!("{error:#}").contains("not a pipe"), "{error:#}");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "first line\n"
        );
        // Still open: the log goes on.
        log.write_all(b"second line\n").expect("still open");
        log.rewind().expect("seek");
        assert_eq!(
            std::fs::read_to_string(&path).expect("read"),
            "first line\nsecond line\n"
        );
    }

    #[test]
    fn a_failure_before_readiness_under_a_service_is_final() {
        assert_eq!(
            failure_exit_code(Some(Manager::Systemd), false),
            ExitCode::from(START_FAILED_EXIT)
        );
        assert_eq!(
            failure_exit_code(Some(Manager::OpenRc), false),
            ExitCode::from(START_FAILED_EXIT)
        );
        // After readiness a failure is a crash, which the manager restarts.
        assert_eq!(
            failure_exit_code(Some(Manager::Systemd), true),
            ExitCode::FAILURE
        );
        // Not under a service: exit 1 as for any command.
        assert_eq!(failure_exit_code(None, false), ExitCode::FAILURE);
    }

    #[test]
    fn a_stray_is_a_service_that_openrc_holds_as_stopped() {
        let dir = tempfile::tempdir().expect("tempdir");
        let state = dir.path().join("openrc");
        // No OpenRC user state at all: nothing to compare with.
        assert!(!openrc_reports_stopped(&state, "work"));
        std::fs::create_dir_all(state.join("started")).expect("mkdir");
        assert!(openrc_reports_stopped(&state, "work"));
        for name in OPENRC_RUNNING_STATES {
            let link = state.join(name).join("pdmount.work");
            std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
            std::os::unix::fs::symlink("/nowhere/pdmount.work", &link).expect("symlink");
            assert!(!openrc_reports_stopped(&state, "work"), "{name}");
            // Another profile's service does not count.
            assert!(openrc_reports_stopped(&state, "other"), "{name}");
            std::fs::remove_file(&link).expect("remove");
        }
        let text = openrc_stray_text("work");
        assert!(
            text.contains(
                "RC_USER_SERVICES=yes RC_SVCNAME=pdmount.work supervise-daemon pdmount.work -U --stop"
            ),
            "{text}"
        );
        assert!(
            text.contains("rc-service --user pdmount.work start"),
            "{text}"
        );
        // Found next to the runtime directory of the profile's paths.
        let paths = paths_in(dir.path(), "work");
        assert_eq!(
            openrc_user_state_dir(&paths),
            Some(dir.path().join("run/openrc"))
        );
        // No daemon runs: no warning.
        assert_eq!(openrc_stray(&paths), None);
    }

    #[test]
    fn the_standard_descriptors_are_never_closed() {
        for fd in 0..3 {
            assert!(notify_fd_ready(fd).is_err());
        }
    }

    #[test]
    fn the_pid_file_names_the_service() {
        assert_eq!(pid_file_line(None), None);
        assert_eq!(
            pid_file_line(Some(Manager::OpenRc)).as_deref(),
            Some("service openrc\n")
        );
        assert_eq!(recorded_service("12\n/mnt\n"), None);
        assert_eq!(
            recorded_service("12\n/mnt\nservice systemd\n"),
            Some(Manager::Systemd)
        );
        assert_eq!(recorded_service("12\n/mnt\nservice other\n"), None);
    }

    #[test]
    fn the_refusal_names_the_stop_command() {
        let text = unmount_refusal(Manager::OpenRc, "work");
        assert!(
            text.contains("rc-service --user pdmount.work stop"),
            "{text}"
        );
        assert!(text.contains("--force"), "{text}");
        let text = unmount_refusal(Manager::Systemd, "work");
        assert!(
            text.contains("systemctl --user stop pdmount@work"),
            "{text}"
        );
        // The texts differ by manager: OpenRC restarts the daemon after --force, systemd does not.
        assert!(
            text.contains("systemctl --user start pdmount@work"),
            "{text}"
        );
        assert!(text.contains("does not restart"), "{text}");
        let openrc = unmount_refusal(Manager::OpenRc, "work");
        assert!(openrc.contains("OpenRC then restarts it"), "{openrc}");
        let note = force_note(Manager::Systemd, "work");
        assert!(note.contains("stays stopped"), "{note}");
        assert!(
            note.contains("systemctl --user start pdmount@work"),
            "{note}"
        );
        let note = force_note(Manager::OpenRc, "work");
        assert!(note.contains("restarts the daemon"), "{note}");
        assert!(
            note.contains("rc-service --user pdmount.work stop"),
            "{note}"
        );
    }

    #[test]
    fn a_service_is_found_by_its_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(dir.path(), "work");
        assert_eq!(installed_service(&paths), None);

        // OpenRC: the link, also a dangling one.
        let link = openrc_service_link(&paths);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink("nowhere", &link).expect("symlink");
        assert_eq!(
            installed_service(&paths),
            Some((Manager::OpenRc, link.clone()))
        );
        // Another profile's link is not this profile's.
        assert_eq!(installed_service(&paths_in(dir.path(), "other")), None);
        std::fs::remove_file(&link).expect("remove");
        assert_eq!(installed_service(&paths), None);

        // systemd: the template alone does not count; it must be enabled for the profile.
        touch(&systemd_unit_template(&paths));
        assert_eq!(installed_service(&paths), None);
        let wants = config_home(&paths).join("systemd/user/default.target.wants");
        std::fs::create_dir_all(&wants).expect("mkdir");
        std::os::unix::fs::symlink("../pdmount@.service", wants.join("pdmount@other.service"))
            .expect("symlink");
        assert_eq!(installed_service(&paths), None);
        let enabled = wants.join("pdmount@work.service");
        std::os::unix::fs::symlink("../pdmount@.service", &enabled).expect("symlink");
        assert_eq!(installed_service(&paths), Some((Manager::Systemd, enabled)));
    }

    #[test]
    fn autostart_and_a_service_exclude_each_other() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(dir.path(), "work");
        refuse_autostart_over_service(&paths).expect("nothing installed");
        refuse_service_over_autostart(&paths).expect("no autostart");
        assert_eq!(double_start(&paths), None);

        touch(&crate::autostart::entry_path(&paths));
        let error = refuse_service_over_autostart(&paths).expect_err("autostart is set");
        assert!(
            format!("{error:#}").contains("autostart disable"),
            "{error:#}"
        );
        refuse_autostart_over_service(&paths).expect("still no service");
        assert_eq!(double_start(&paths), None);

        let link = openrc_service_link(&paths);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink("nowhere", &link).expect("symlink");
        let conflict = double_start(&paths).expect("both are set");
        assert!(
            conflict.contains("conflict") && conflict.contains("OpenRC"),
            "{conflict}"
        );
        let error = refuse_autostart_over_service(&paths).expect_err("a service is installed");
        assert!(
            format!("{error:#}").contains("service uninstall"),
            "{error:#}"
        );

        std::fs::remove_file(crate::autostart::entry_path(&paths)).expect("remove");
        assert_eq!(double_start(&paths), None);
        assert!(service_state_text(&paths).contains("OpenRC"));
    }
}
