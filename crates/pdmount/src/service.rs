//! `pdmount service` (DESIGN.md §14.4, D074, D091, D093): a user service for
//! the profile under OpenRC (M5.4) or systemd (M5.5).
//!
//! The unit files run `-v mount --foreground --service <manager> --clear-stale`
//! (`supervise.rs` has the daemon's side); `-v` logs at INFO, as a background
//! `mount` does, so the log shows the flush at logout and shutdown. What differs per profile (secret
//! storage, stop timeout, log) is kept apart from what is shared (the
//! binary, the command line), so that several profiles can use one script
//! or template: OpenRC reads `rc/conf.d/pdmount.<profile>`, systemd a drop-in
//! next to the template.

use crate::doctor::{Check, Level};
use crate::supervise::{self, Manager};
use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use pdmount_core::{Config, ProfilePaths};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

/// The stop timeout of the unit is the flush's grace plus this: the
/// daemon's own wait for the session, its store commit and its last token
/// save (the margin of `unmount --force`, D062).
const STOP_MARGIN_SECS: u64 = 30;

/// How long OpenRC waits after SIGKILL.
const OPENRC_KILL_WAIT_SECS: u64 = 5;

#[derive(Debug, Args)]
pub struct ServiceArgs {
    /// The service manager; by default the one found running (`/run/systemd/system`, then
    /// `/run/openrc`).
    #[arg(long, value_enum, global = true)]
    pub manager: Option<Manager>,
    #[command(subcommand)]
    pub command: Option<ServiceCommand>,
}

#[derive(Debug, Subcommand)]
pub enum ServiceCommand {
    /// Write the service files for this profile, enable the service and start it.
    Install(InstallArgs),
    /// Stop the service, disable it and remove its files.
    Uninstall,
}

#[derive(Debug, Args)]
pub struct InstallArgs {
    /// Test builds only: run the service's daemon on a fake drive (`fake:<dir>`, the directory
    /// must exist), for the manual runs of M5.6 (DESIGN.md §14.4). Absent from release builds.
    #[cfg(feature = "fake-backend")]
    #[arg(long, value_parser = crate::daemon::BackendChoice::parse, hide = true)]
    pub backend: Option<crate::daemon::BackendChoice>,
}

pub fn run(
    paths: &ProfilePaths,
    args: ServiceArgs,
    config: &Config,
    home: &Path,
) -> anyhow::Result<ExitCode> {
    match args.command {
        Some(ServiceCommand::Install(install_args)) => {
            let manager = pick_manager(args.manager)?;
            let backend = install_backend(&install_args)?;
            install(paths, manager, config, home, backend.as_deref())
        }
        Some(ServiceCommand::Uninstall) => {
            // Without an option, remove whatever is installed for the profile.
            let manager = args
                .manager
                .or_else(|| supervise::installed_service(paths).map(|(manager, _)| manager));
            match manager {
                Some(manager) => uninstall(paths, manager),
                None => {
                    println!(
                        "No user service is installed for profile {}.",
                        paths.profile
                    );
                    Ok(ExitCode::SUCCESS)
                }
            }
        }
        None => {
            show(paths, args.manager, config);
            Ok(ExitCode::SUCCESS)
        }
    }
}

/// The fake drive the service's daemon runs on, from `--backend fake:<dir>`:
/// the directory must exist. `--backend proton` is the default and stores nothing.
#[cfg(feature = "fake-backend")]
fn install_backend(args: &InstallArgs) -> anyhow::Result<Option<PathBuf>> {
    match &args.backend {
        Some(crate::daemon::BackendChoice::Fake(dir)) => {
            if !dir.is_dir() {
                bail!("the fake drive {} is not a directory", dir.display());
            }
            Ok(Some(dir.clone()))
        }
        Some(crate::daemon::BackendChoice::Proton) | None => Ok(None),
    }
}

#[cfg(not(feature = "fake-backend"))]
fn install_backend(_args: &InstallArgs) -> anyhow::Result<Option<PathBuf>> {
    Ok(None)
}

// ---- Manager detection -----------------------------------------------------

/// The manager running on this system: systemd when `<run>/systemd/system`
/// exists (as `sd_booted` checks), else OpenRC when `<run>/openrc` does.
fn detect_in(run: &Path) -> Option<Manager> {
    if run.join("systemd/system").is_dir() {
        Some(Manager::Systemd)
    } else if run.join("openrc").exists() {
        Some(Manager::OpenRc)
    } else {
        None
    }
}

fn pick_manager(requested: Option<Manager>) -> anyhow::Result<Manager> {
    requested.or_else(|| detect_in(Path::new("/run"))).context(
        "no running service manager found (/run/systemd/system, /run/openrc); \
             name one with --manager openrc|systemd",
    )
}

// ---- Settings and quoting --------------------------------------------------

/// What differs between the profiles' services.
struct Settings<'a> {
    profile: &'a str,
    /// The `pdmount` executable, absolute.
    binary: &'a str,
    /// `--allow-file-secrets` or `--force-file-secrets`, as the profile uses.
    secrets_flag: Option<&'a str>,
    /// The unit's stop timeout in seconds: the grace plus the margin.
    stop_timeout: u64,
    /// The configured mountpoint (cleared by the systemd unit after a stop).
    mountpoint: &'a str,
    /// The daemon's log file (OpenRC sends its output there).
    log: &'a str,
    /// The fake drive of a test build's `--backend fake:<dir>` (absolute, UTF-8, no control
    /// character, quote or backslash); `mount` gets it as `--backend=fake:<dir>`.
    backend: Option<&'a str>,
}

/// The word `mount` gets for the backend: one word, so that a single quoting
/// layer per manager is enough.
fn backend_word(dir: &str) -> String {
    format!("--backend=fake:{dir}")
}

/// Refuses what the quoting of the backend word does not cover.
fn check_backend_dir(dir: &str) -> anyhow::Result<()> {
    plain("the fake drive path", dir)?;
    if dir.contains(['"', '\'', '\\']) {
        bail!("the fake drive path contains a quote or a backslash: {dir:?}");
    }
    Ok(())
}

/// The stop timeout for a grace, rounded up to whole seconds.
pub fn stop_timeout(grace: Duration) -> u64 {
    grace.as_secs() + u64::from(grace.subsec_nanos() > 0) + STOP_MARGIN_SECS
}

/// Rejects control characters, which no quoting makes safe in a unit file.
fn plain(what: &str, text: &str) -> anyhow::Result<()> {
    if text.chars().any(char::is_control) {
        bail!("{what} contains a control character: {text:?}");
    }
    Ok(())
}

/// One word for a POSIX shell: unchanged when it has only safe characters,
/// else in single quotes.
fn sh_quote(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    if !text.is_empty() && text.chars().all(safe) {
        return text.to_owned();
    }
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A value for a variable that `supervise-daemon.sh` expands unquoted inside
/// `eval` (`command`, `output_log`): quoted for the assignment and again for
/// the `eval`.
fn eval_quote(text: &str) -> String {
    sh_quote(&sh_quote(text))
}

/// A word for a value that is expanded unquoted into `command_args` and then
/// goes through `eval` (`supervise-daemon.sh`): a backslash before every
/// character that is not safe. Single quotes would not do, since the
/// expansion is split at spaces first and `eval` joins the pieces with one
/// space, so a path with two spaces in a row would lose one.
fn eval_word(text: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@%+=:,./-".contains(c);
    let mut out = String::new();
    for c in text.chars() {
        if !safe(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// One argument of a systemd command line (`ExecStart=`, `ExecStopPost=`):
/// `%` and `$` doubled, since systemd expands specifiers and variables
/// there, and in double quotes when it has anything but safe characters.
fn systemd_quote(text: &str) -> String {
    let doubled = text.replace('%', "%%").replace('$', "$$");
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@+=:,./-%$".contains(c);
    if !doubled.is_empty() && doubled.chars().all(safe) {
        return doubled;
    }
    format!("\"{}\"", doubled.replace('\\', "\\\\").replace('"', "\\\""))
}

// ---- OpenRC ------------------------------------------------------------------

fn openrc_script_path(paths: &ProfilePaths) -> PathBuf {
    supervise::config_home(paths).join("rc/init.d/pdmount")
}

fn openrc_conf_path(paths: &ProfilePaths) -> PathBuf {
    supervise::config_home(paths)
        .join("rc/conf.d")
        .join(openrc_name(paths.profile.as_str()))
}

fn openrc_runlevel_link(paths: &ProfilePaths) -> PathBuf {
    supervise::config_home(paths)
        .join("rc/runlevels/default")
        .join(openrc_name(paths.profile.as_str()))
}

fn openrc_name(profile: &str) -> String {
    format!("pdmount.{profile}")
}

/// The shared init script. The profile is the part of the service's name
/// after `pdmount.`; `conf.d` is read before the script, so `retry`,
/// `pdmount_secrets` and `pdmount_mount_args` come from the profile's file.
/// `pdmount_mount_args` is empty except for a test build's fake drive.
fn openrc_script(binary: &str) -> String {
    format!(
        r#"#!/sbin/openrc-run
# Written by `pdmount service install`; the next install rewrites it.
# One service per profile: the link pdmount.<profile> next to this file points
# here, and ../conf.d/pdmount.<profile> holds the profile's settings.

profile=${{RC_SVCNAME#pdmount.}}
description="PdMount ($profile)"
supervisor=supervise-daemon
notify="fd:3"
command={command}
command_args="--profile $profile $pdmount_secrets -v mount $pdmount_mount_args --foreground --service openrc --clear-stale --notify-fd 3"
respawn_delay=5
respawn_max=3
respawn_period=60
export DBUS_SESSION_BUS_ADDRESS="unix:path=${{XDG_RUNTIME_DIR}}/bus"

depend() {{
	need dbus
}}

start_pre() {{
	case $RC_SVCNAME in
	pdmount.?*) ;;
	*)
		eerror "start a link named pdmount.<profile>, not $RC_SVCNAME"
		return 1
		;;
	esac
	if [ -z "$retry" ]; then
		eerror "no settings for $profile: run 'pdmount service install' for it"
		return 1
	fi
}}

# A start that fails before the daemon is ready is final (D109). The
# supervisor that default_start forked would respawn the daemon outside the
# service's state, so it is stopped as well. A supervisor that ran before
# this start (one left by an older script) is not: the start fails with
# "already running", and `pdmount status` says how to stop it.
start() {{
	local pidfile_ stray_ n_
	if yesno "$RC_USER_SERVICES"; then
		pidfile_="${{XDG_RUNTIME_DIR}}/supervise-${{RC_SVCNAME}}.pid"
	else
		pidfile_="/var/run/supervise-${{RC_SVCNAME}}.pid"
	fi
	stray_=
	if [ -s "$pidfile_" ] && kill -0 "$(cat "$pidfile_")" 2>/dev/null; then
		stray_=yes
	fi
	default_start && return 0
	if [ -z "$stray_" ]; then
		# The supervisor may write its pid file only after the start returned.
		n_=0
		while [ ! -s "$pidfile_" ] && [ "$n_" -lt 20 ]; do
			sleep 0.1
			n_=$((n_ + 1))
		done
		supervise-daemon "$RC_SVCNAME" --stop
	fi
	return 1
}}
"#,
        command = eval_quote(binary)
    )
}

/// `conf.d/pdmount.<profile>`: what differs per profile.
fn openrc_conf(settings: &Settings<'_>) -> String {
    let log = eval_quote(settings.log);
    format!(
        "# Written by `pdmount service install` for profile {profile}.\n\
         pdmount_secrets={secrets}\n\
         pdmount_mount_args={mount_args}\n\
         retry=\"TERM/{stop}/KILL/{kill}\"\n\
         output_log={log}\n\
         error_log={log}\n",
        profile = settings.profile,
        secrets = settings.secrets_flag.unwrap_or(""),
        // Expanded into `command_args`, which is evaluated again there, so it holds eval
        // words; the assignment quotes them once more.
        mount_args = settings
            .backend
            .map(|dir| sh_quote(&eval_word(&backend_word(dir))))
            .unwrap_or_default(),
        stop = settings.stop_timeout,
        kill = OPENRC_KILL_WAIT_SECS,
    )
}

/// The `N` of `retry="TERM/N/KILL/…"` in a `conf.d` file.
fn openrc_conf_stop_timeout(text: &str) -> Option<u64> {
    let value = text
        .lines()
        .find_map(|line| line.trim().strip_prefix("retry="))?
        .trim_matches('"');
    value.strip_prefix("TERM/")?.split('/').next()?.parse().ok()
}

/// What the root drop-in for elogind says (D091).
fn elogind_drop_in(grace: Duration) -> String {
    format!(
        "[Login]\nInhibitDelayMaxSec={}\n",
        grace.as_secs() + u64::from(grace.subsec_nanos() > 0)
    )
}

const ELOGIND_DROP_IN_PATH: &str = "/etc/elogind/logind.conf.d/pdmount.conf";

// ---- systemd -----------------------------------------------------------------

fn systemd_dropin_dir(paths: &ProfilePaths) -> PathBuf {
    supervise::config_home(paths)
        .join("systemd/user")
        .join(format!("pdmount@{}.service.d", paths.profile))
}

fn systemd_dropin_path(paths: &ProfilePaths) -> PathBuf {
    systemd_dropin_dir(paths).join("pdmount.conf")
}

fn systemd_name(profile: &str) -> String {
    format!("pdmount@{profile}")
}

/// The template `pdmount@.service`. `%i` is the instance as written (the
/// profile), not `%I`, which would turn a `-` into `/`. The secret-storage
/// flag and a test build's fake drive come from the drop-in's `Environment=`
/// (`$PDMOUNT_SECRETS`, `$PDMOUNT_MOUNT_ARGS`); unset, they expand to no argument.
fn systemd_template(binary: &str) -> String {
    format!(
        "# Written by `pdmount service install`; the next install rewrites it.\n\
         # One instance per profile: pdmount@<profile>.service. The profile's settings are in\n\
         # pdmount@<profile>.service.d/pdmount.conf.\n\
         [Unit]\n\
         Description=PdMount (profile %i)\n\
         After=dbus.socket dbus.service\n\
         \n\
         [Service]\n\
         Type=notify\n\
         ExecStart={binary} --profile %i $PDMOUNT_SECRETS -v mount $PDMOUNT_MOUNT_ARGS --foreground --service systemd --clear-stale\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # A start that fails before the mount is ready is final (D109).\n\
         RestartPreventExitStatus={start_failed}\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        binary = systemd_quote(binary),
        start_failed = supervise::START_FAILED_EXIT,
    )
}

/// The line that clears a stale mount after the daemon is gone. It applies
/// the stale rule of M5.3 (`unmount --if-stale`), so the mount of another
/// daemon of the profile, which made this unit's start fail, is left alone (D094).
fn systemd_stop_post(binary: &str, mountpoint: &str) -> String {
    format!(
        "ExecStopPost=-{} --profile %i{}",
        systemd_quote(binary),
        systemd_stop_post_tail(mountpoint)
    )
}

/// What the `ExecStopPost=` line ends with, whatever binary it names: the
/// part that `doctor` compares with the configured mountpoint.
fn systemd_stop_post_tail(mountpoint: &str) -> String {
    format!(" unmount --if-stale {}", systemd_quote(mountpoint))
}

/// The drop-in `pdmount@<profile>.service.d/pdmount.conf`.
fn systemd_dropin(settings: &Settings<'_>) -> String {
    let environment = settings
        .secrets_flag
        .map(|flag| format!("Environment=PDMOUNT_SECRETS={flag}\n"))
        .unwrap_or_default();
    let mount_args = settings
        .backend
        .map(|dir| format!("{}\n", systemd_mount_args_line(&backend_word(dir))))
        .unwrap_or_default();
    format!(
        "# Written by `pdmount service install` for profile {profile}.\n\
         [Service]\n\
         {environment}\
         {mount_args}\
         TimeoutStopSec={stop}\n\
         {stop_post}\n",
        profile = settings.profile,
        stop = settings.stop_timeout,
        stop_post = systemd_stop_post(settings.binary, settings.mountpoint),
    )
}

/// `Environment=PDMOUNT_MOUNT_ARGS=<word>`. systemd splits `$PDMOUNT_MOUNT_ARGS` in
/// `ExecStart=` at whitespace and respects double quotes there, so a word with
/// anything but safe characters is quoted; the assignment is then quoted for
/// `Environment=` itself. `%` is doubled (specifiers are expanded in
/// `Environment=`); `$` is not expanded there.
fn systemd_mount_args_line(word: &str) -> String {
    let safe = |c: char| c.is_ascii_alphanumeric() || "_@+=:,./-".contains(c);
    let word = word.replace('%', "%%");
    if word.chars().all(|c| safe(c) || c == '%') {
        return format!("Environment=PDMOUNT_MOUNT_ARGS={word}");
    }
    let inner = format!("\"{word}\"");
    format!(
        "Environment=\"PDMOUNT_MOUNT_ARGS={}\"",
        inner.replace('"', "\\\"")
    )
}

/// The fake drive (`fake:<dir>`) in an installed OpenRC `conf.d` file.
fn backend_of_openrc_conf(text: &str) -> Option<String> {
    let value = text
        .lines()
        .find_map(|line| line.strip_prefix("pdmount_mount_args="))?;
    let value = value.trim().trim_matches('\'');
    // Undo the backslashes of `eval_word`.
    let mut word = String::new();
    let mut escaped = false;
    for c in value.chars() {
        if c == '\\' && !escaped {
            escaped = true;
        } else {
            word.push(c);
            escaped = false;
        }
    }
    word.strip_prefix("--backend=").map(str::to_owned)
}

/// The fake drive (`fake:<dir>`) in an installed systemd drop-in.
fn backend_of_systemd_dropin(text: &str) -> Option<String> {
    let value = text.lines().find_map(|line| {
        let value = line.strip_prefix("Environment=")?;
        let value = match value.strip_prefix('"') {
            Some(quoted) => quoted.strip_suffix('"')?.replace("\\\"", "\""),
            None => value.to_owned(),
        };
        value.strip_prefix("PDMOUNT_MOUNT_ARGS=").map(str::to_owned)
    })?;
    let word = value.trim_matches('"').replace("%%", "%");
    word.strip_prefix("--backend=").map(str::to_owned)
}

/// The seconds of `TimeoutStopSec=` in the drop-in.
fn systemd_dropin_stop_timeout(text: &str) -> Option<u64> {
    ini_value(text, "Service", "TimeoutStopSec")??.parse().ok()
}

/// The links `systemctl enable` makes for instances of the template, in the
/// `*.wants` directories of the user's unit directory.
fn systemd_enabled_links(paths: &ProfilePaths) -> Vec<(String, PathBuf)> {
    let units = supervise::config_home(paths).join("systemd/user");
    let Ok(entries) = std::fs::read_dir(units) else {
        return Vec::new();
    };
    let mut links = Vec::new();
    for dir in entries.filter_map(Result::ok) {
        if !dir.file_name().to_string_lossy().ends_with(".wants") {
            continue;
        }
        let Ok(inner) = std::fs::read_dir(dir.path()) else {
            continue;
        };
        for link in inner.filter_map(Result::ok) {
            let name = link.file_name().to_string_lossy().into_owned();
            if let Some(profile) = name
                .strip_prefix("pdmount@")
                .and_then(|rest| rest.strip_suffix(".service"))
                .filter(|profile| !profile.is_empty())
            {
                links.push((profile.to_owned(), link.path()));
            }
        }
    }
    links
}

// ---- Files and commands ---------------------------------------------------------

/// Writes `text` to `path` through a temporary file and a rename, creating
/// the directory; a link or file already there is replaced.
fn write_file(path: &Path, text: &str, mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path.parent().context("a file without a directory")?;
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let temporary = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(mode)
            .open(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result.with_context(|| format!("cannot write {}", path.display()))
}

/// Removes a file or link; whether it was there.
fn remove_if_present(path: &Path) -> anyhow::Result<bool> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| format!("cannot remove {}", path.display())),
    }
}

/// Runs `program` with `args` on the user's `$PATH`, its output going to
/// ours. The manager's tools are looked up like any command the user types.
fn run_tool(program: &str, args: &[&str]) -> anyhow::Result<()> {
    let shown = format!("{program} {}", args.join(" "));
    let status = std::process::Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("cannot run `{shown}`"))?;
    if !status.success() {
        bail!("`{shown}` failed ({status})");
    }
    Ok(())
}

/// The running binary, for the unit files: absolute, UTF-8, still there.
fn current_binary() -> anyhow::Result<String> {
    let binary = std::env::current_exe().context("cannot find the pdmount executable")?;
    let binary = binary
        .to_str()
        .context("the path of the pdmount executable is not UTF-8")?;
    plain("the path of the pdmount executable", binary)?;
    if !Path::new(binary).is_file() {
        bail!("the pdmount executable {binary} is gone (replaced while running?); start it again");
    }
    Ok(binary.to_owned())
}

/// Creates the log file the OpenRC service writes to (0600 in a 0700
/// directory, as the daemon's own log, §13.1): `supervise-daemon` would
/// create it with the umask's mode.
fn prepare_log(path: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let dir = path.parent().context("the log file has no directory")?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::OpenOptions::new()
        .append(true)
        .create(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("cannot create {}", path.display()))?;
    Ok(())
}

/// Install refuses a second mechanism for the profile.
fn refuse_conflicts(paths: &ProfilePaths, manager: Manager) -> anyhow::Result<()> {
    supervise::refuse_service_over_autostart(paths)?;
    if let Some((other, file)) = supervise::installed_service(paths)
        && other != manager
    {
        bail!(
            "a {} user service is installed for profile {} ({}); a profile uses one service. \
             Run `pdmount service uninstall` first",
            other.title(),
            paths.profile,
            file.display()
        );
    }
    Ok(())
}

fn install(
    paths: &ProfilePaths,
    manager: Manager,
    config: &Config,
    home: &Path,
    backend: Option<&Path>,
) -> anyhow::Result<ExitCode> {
    refuse_conflicts(paths, manager)?;
    // A service starts at every login: on a mountpoint another profile uses,
    // one of the two would fail each time (D127).
    let sharing = crate::doctor::profiles_sharing_a_mountpoint(paths, config, home);
    if !sharing.is_empty() {
        bail!("not installed: {}", crate::doctor::sharing_text(&sharing));
    }
    let binary = current_binary()?;
    let mountpoint = crate::daemon::mountpoint(config, home, None);
    let mountpoint = mountpoint
        .to_str()
        .context("the mountpoint is not UTF-8")?
        .to_owned();
    plain("the mountpoint", &mountpoint)?;
    let log = paths.log_file();
    let log = log.to_str().context("the log path is not UTF-8")?;
    plain("the log path", log)?;
    let backend = backend
        .map(|dir| {
            let dir = dir.to_str().context("the fake drive path is not UTF-8")?;
            check_backend_dir(dir)?;
            Ok::<_, anyhow::Error>(dir)
        })
        .transpose()?;
    let grace = config.timeouts.shutdown_grace.0;
    let settings = Settings {
        profile: paths.profile.as_str(),
        binary: &binary,
        secrets_flag: crate::credentials::mode().flag(),
        stop_timeout: stop_timeout(grace),
        mountpoint: &mountpoint,
        log,
        backend,
    };
    let start = !hand_started_daemon(paths);
    if !start {
        println!(
            "note: a daemon of profile {} is running, started by hand (autostart or `pdmount \
             mount`). It holds the mount, so the service is set up and enabled but not started \
             now; it starts at the next login, or after `pdmount unmount` with `{}`.",
            paths.profile,
            manager.start_command(paths.profile.as_str())
        );
    }
    // Each desktop login hands its session bus to the daemon (D102).
    let attach = crate::autostart::write_attach_entry(paths, &binary)?;
    println!(
        "Wrote {}: each desktop login hands its session bus to the service, for the keyring and \
         notifications.",
        attach.display()
    );
    if let Some(hint) = attach_hint(&current_desktop(), paths.profile.as_str(), &binary) {
        println!("{hint}");
    }
    match manager {
        Manager::OpenRc => install_openrc(paths, &settings, grace, start),
        Manager::Systemd => install_systemd(paths, &settings, start),
    }
}

/// Window managers known not to run XDG autostart entries, so the attach
/// entry never runs at their login (D102 amended): the name in
/// `XDG_CURRENT_DESKTOP`, the configuration file, and the command that runs
/// a program when it starts. Kept short; the user guide covers others.
const NO_XDG_AUTOSTART: [(&str, &str, &str); 2] = [
    ("i3", "~/.config/i3/config", "exec --no-startup-id"),
    ("sway", "~/.config/sway/config", "exec"),
];

/// `XDG_CURRENT_DESKTOP` of this process, empty when unset.
fn current_desktop() -> String {
    std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default()
}

/// The note for a desktop that may not run the attach entry: for one of
/// [`NO_XDG_AUTOSTART`], the line to add to its configuration; with no
/// desktop named, both lines as examples. Nothing for other desktops, which
/// run XDG autostart. The configuration is never edited here.
fn attach_hint(current_desktop: &str, profile: &str, binary: &str) -> Option<String> {
    let command = format!(
        "{} --profile {} session attach",
        sh_quote(binary),
        sh_quote(profile)
    );
    let names: Vec<&str> = current_desktop
        .split(':')
        .filter(|name| !name.is_empty())
        .collect();
    if names.is_empty() {
        return Some(format!(
            "note: XDG_CURRENT_DESKTOP is not set. If your desktop is a window manager that does \
             not run XDG autostart entries, the attach entry never runs: run `{command}` at each \
             login from its configuration (i3: `exec --no-startup-id {command}`, sway: \
             `exec {command}`)."
        ));
    }
    let (name, file, exec) = NO_XDG_AUTOSTART
        .iter()
        .find(|(known, _, _)| names.iter().any(|name| name.eq_ignore_ascii_case(known)))?;
    Some(format!(
        "note: {name} does not run XDG autostart entries, so the attach entry does not run at \
         login (unless {file} runs `dex -a` or the like). Add this line to {file}, then log in \
         again:\n    {exec} {command}"
    ))
}

/// A daemon that no service manager runs holds the mount, so the service's
/// daemon would fail its start (D094). Starting it anyway would only make the
/// supervisor retry, so `install` leaves the start to the user.
fn hand_started_daemon(paths: &ProfilePaths) -> bool {
    crate::daemon::daemon_pid(paths).is_some() && supervise::running_service(paths).is_none()
}

fn install_openrc(
    paths: &ProfilePaths,
    settings: &Settings<'_>,
    grace: Duration,
    start: bool,
) -> anyhow::Result<ExitCode> {
    let script = openrc_script_path(paths);
    let conf = openrc_conf_path(paths);
    let link = supervise::openrc_service_link(paths);
    let name = openrc_name(settings.profile);
    write_file(&script, &openrc_script(settings.binary), 0o755)?;
    write_file(&conf, &openrc_conf(settings), 0o644)?;
    prepare_log(Path::new(settings.log))?;
    match link.symlink_metadata() {
        Ok(meta) if meta.file_type().is_symlink() => remove_if_present(&link).map(drop)?,
        Ok(_) => bail!(
            "{} exists and is not a link; remove it first",
            link.display()
        ),
        Err(_) => {}
    }
    std::os::unix::fs::symlink("pdmount", &link)
        .with_context(|| format!("cannot link {}", link.display()))?;
    println!(
        "Wrote {}, {} and the link {}.",
        script.display(),
        conf.display(),
        link.display()
    );
    if openrc_runlevel_link(paths).symlink_metadata().is_err() {
        run_tool("rc-update", &["--user", "add", &name, "default"])?;
    }
    if start {
        run_tool("rc-service", &["--user", &name, "start"])?;
    }
    println!(
        "OpenRC user service {name} is in the default runlevel{} (stop timeout {} s).",
        if start { " and started" } else { "" },
        settings.stop_timeout
    );
    print_logind_hint(grace, elogind_limit());
    Ok(ExitCode::SUCCESS)
}

/// D091: the root-owned file that gives a shutdown through logind the full
/// grace. Printed, never written.
fn print_logind_hint(grace: Duration, limit: Option<Duration>) {
    match limit {
        Some(limit) if limit >= grace => println!(
            "elogind holds a shutdown for up to {} s, enough for the grace of {} s.",
            limit.as_secs(),
            grace.as_secs()
        ),
        _ => {
            println!(
                "A shutdown through logind waits for the flush only as long as elogind's \
                 InhibitDelayMaxSec allows ({}). To give it the grace of {} s, create this file as \
                 root (pdmount does not write it); it takes effect at the next boot:\n\n  {}\n\n{}",
                limit.map_or("5 s by default".to_owned(), |limit| format!(
                    "{} s",
                    limit.as_secs()
                )),
                grace.as_secs(),
                ELOGIND_DROP_IN_PATH,
                elogind_drop_in(grace)
                    .lines()
                    .map(|line| format!("    {line}\n"))
                    .collect::<String>()
            );
        }
    }
}

fn install_systemd(
    paths: &ProfilePaths,
    settings: &Settings<'_>,
    start: bool,
) -> anyhow::Result<ExitCode> {
    let template = supervise::systemd_unit_template(paths);
    let dropin = systemd_dropin_path(paths);
    let name = systemd_name(settings.profile);
    write_file(&template, &systemd_template(settings.binary), 0o644)?;
    write_file(&dropin, &systemd_dropin(settings), 0o644)?;
    println!("Wrote {} and {}.", template.display(), dropin.display());
    // `enable` reloads the manager first, so the new files are read.
    let mut enable = vec!["--user", "enable"];
    if start {
        enable.push("--now");
    }
    enable.push(&name);
    run_tool("systemctl", &enable)?;
    println!(
        "systemd user unit {name} is enabled{} (stop timeout {} s). A daemon that was \
         already running keeps its old unit until `systemctl --user restart {name}`.",
        if start { " and started" } else { "" },
        settings.stop_timeout
    );
    Ok(ExitCode::SUCCESS)
}

fn uninstall(paths: &ProfilePaths, manager: Manager) -> anyhow::Result<ExitCode> {
    let code = match manager {
        Manager::OpenRc => uninstall_openrc(paths),
        Manager::Systemd => uninstall_systemd(paths),
    }?;
    if let Some(attach) = crate::autostart::remove_attach_entry(paths)? {
        println!("Removed {}.", attach.display());
    }
    Ok(code)
}

fn uninstall_systemd(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let own = paths.profile.as_str();
    let name = systemd_name(own);
    let enabled: Vec<PathBuf> = systemd_enabled_links(paths)
        .into_iter()
        .filter(|(profile, _)| profile == own)
        .map(|(_, link)| link)
        .collect();
    let dropin = systemd_dropin_path(paths);
    let template = supervise::systemd_unit_template(paths);
    let installed = !enabled.is_empty() || dropin.exists();
    if installed {
        println!(
            "Stopping {name}; this can take as long as pending changes need to upload (up to the \
             grace plus {STOP_MARGIN_SECS} s)."
        );
        if let Err(error) = run_tool("systemctl", &["--user", "disable", "--now", &name]) {
            eprintln!("warning: {error:#}; going on");
        }
    }
    let mut removed = Vec::new();
    // `disable` removed the links; this covers a manager that was not running.
    for link in enabled {
        if remove_if_present(&link)? {
            removed.push(link);
        }
    }
    if remove_if_present(&dropin)? {
        removed.push(dropin.clone());
    }
    let dir = systemd_dropin_dir(paths);
    if std::fs::remove_dir(&dir).is_ok() {
        removed.push(dir);
    }
    // The template goes with the last enabled profile.
    let others = systemd_enabled_links(paths)
        .iter()
        .any(|(profile, _)| profile != own);
    if !others && remove_if_present(&template)? {
        removed.push(template);
    }
    if installed && let Err(error) = run_tool("systemctl", &["--user", "daemon-reload"]) {
        eprintln!("warning: {error:#}; going on");
    }
    if removed.is_empty() {
        println!(
            "No systemd user service was installed for profile {}.",
            paths.profile
        );
    } else {
        for file in removed {
            println!("Removed {}.", file.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn uninstall_openrc(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let link = supervise::openrc_service_link(paths);
    let name = openrc_name(paths.profile.as_str());
    let installed = link.symlink_metadata().is_ok();
    if installed {
        println!(
            "Stopping {name}; this can take as long as pending changes need to upload (up to the \
             grace plus {STOP_MARGIN_SECS} s)."
        );
        if let Err(error) = run_tool("rc-service", &["--user", &name, "stop"]) {
            eprintln!("warning: {error:#}; going on");
        }
        if openrc_runlevel_link(paths).symlink_metadata().is_ok()
            && let Err(error) = run_tool("rc-update", &["--user", "del", &name, "default"])
        {
            eprintln!("warning: {error:#}; going on");
        }
    }
    let mut removed = Vec::new();
    for file in [&link, &openrc_conf_path(paths)] {
        if remove_if_present(file)? {
            removed.push(file.clone());
        }
    }
    // The shared script goes with the last profile.
    if !other_openrc_links(paths) && remove_if_present(&openrc_script_path(paths))? {
        removed.push(openrc_script_path(paths));
    }
    if removed.is_empty() {
        println!(
            "No OpenRC user service was installed for profile {}.",
            paths.profile
        );
    } else {
        for file in removed {
            println!("Removed {}.", file.display());
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// Whether the init directory has a `pdmount.<profile>` link besides this
/// profile's.
fn other_openrc_links(paths: &ProfilePaths) -> bool {
    let own = openrc_name(paths.profile.as_str());
    let Ok(entries) = std::fs::read_dir(supervise::config_home(paths).join("rc/init.d")) else {
        return false;
    };
    entries.filter_map(Result::ok).any(|entry| {
        let name = entry.file_name();
        let name = name.to_string_lossy();
        name.starts_with("pdmount.") && name != own
    })
}

// ---- State -----------------------------------------------------------------------

fn show(paths: &ProfilePaths, requested: Option<Manager>, config: &Config) {
    let detected = requested.or_else(|| detect_in(Path::new("/run")));
    println!("{}", supervise::service_state_text(paths));
    println!(
        "service manager: {}",
        detected.map_or("none found running", Manager::title)
    );
    let want = stop_timeout(config.timeouts.shutdown_grace.0);
    if let Some((manager, _)) = supervise::installed_service(paths) {
        let (have, file) = match manager {
            Manager::OpenRc => (
                std::fs::read_to_string(openrc_conf_path(paths))
                    .ok()
                    .and_then(|text| openrc_conf_stop_timeout(&text)),
                openrc_conf_path(paths),
            ),
            Manager::Systemd => (
                installed_systemd_stop_timeout(paths),
                systemd_dropin_path(paths),
            ),
        };
        match have {
            Some(have) => println!("stop timeout: {have} s (the config wants {want} s)"),
            None => println!("stop timeout: not found in {}", file.display()),
        }
        let backend = std::fs::read_to_string(&file)
            .ok()
            .and_then(|text| match manager {
                Manager::OpenRc => backend_of_openrc_conf(&text),
                Manager::Systemd => backend_of_systemd_dropin(&text),
            });
        if let Some(backend) = backend {
            println!("backend: {backend} (test build)");
        }
        let attach = crate::autostart::attach_entry_path(paths);
        if attach.exists() {
            println!(
                "desktop bus: handed over at each login ({})",
                attach.display()
            );
            let binary = current_binary().unwrap_or_else(|_| "pdmount".to_owned());
            if let Some(hint) = attach_hint(&current_desktop(), paths.profile.as_str(), &binary) {
                println!("{hint}");
            }
        } else {
            println!(
                "desktop bus: no attach entry ({} is missing; `service install` writes it)",
                attach.display()
            );
        }
    }
}

fn installed_systemd_stop_timeout(paths: &ProfilePaths) -> Option<u64> {
    std::fs::read_to_string(systemd_dropin_path(paths))
        .ok()
        .and_then(|text| systemd_dropin_stop_timeout(&text))
}

// ---- doctor ----------------------------------------------------------------------

/// The checks of `doctor` for the profile's service; none without one.
pub fn doctor_checks(paths: &ProfilePaths, config: &Config) -> Vec<Check> {
    let Some((manager, _)) = supervise::installed_service(paths) else {
        return Vec::new();
    };
    let grace = config.timeouts.shutdown_grace.0;
    let want = stop_timeout(grace);
    let mut checks = Vec::new();
    match manager {
        Manager::OpenRc => {
            let have = std::fs::read_to_string(openrc_conf_path(paths))
                .ok()
                .and_then(|text| openrc_conf_stop_timeout(&text));
            checks.push(stop_timeout_check(have, want));
            checks.push(logind_check(grace, elogind_limit()));
        }
        Manager::Systemd => {
            checks.push(stop_timeout_check(
                installed_systemd_stop_timeout(paths),
                want,
            ));
            if let Some(home) = std::env::var_os("HOME") {
                let mountpoint = crate::daemon::mountpoint(config, Path::new(&home), None);
                let expected = systemd_stop_post_tail(&mountpoint.to_string_lossy());
                let text = std::fs::read_to_string(systemd_dropin_path(paths)).unwrap_or_default();
                checks.push(mountpoint_check(&text, &expected));
            }
            checks.push(user_stop_check(want, user_unit_stop_limit()));
        }
    }
    checks
}

/// The drop-in clears the mountpoint the config has now. `expected_tail` is
/// what the `ExecStopPost=` line ends with; a drop-in of an older install
/// (`fusermount3 -uz`) does not match and is reported too.
fn mountpoint_check(dropin: &str, expected_tail: &str) -> Check {
    if dropin
        .lines()
        .any(|line| line.starts_with("ExecStopPost=") && line.ends_with(expected_tail))
    {
        Check::new(
            "service mount",
            Level::Ok,
            "the unit clears a stale mount at the configured mountpoint after a stop",
        )
    } else {
        Check::new(
            "service mount",
            Level::Warn,
            "the unit's ExecStopPost does not clear the configured mountpoint the way `unmount --if-stale` does; run `pdmount service install`",
        )
    }
}

/// D091: systemd stops a user's units within `user@.service`'s stop
/// timeout, whatever their own `TimeoutStopSec` says.
fn user_stop_check(want: u64, limit: (Duration, bool)) -> Check {
    let (limit, assumed) = limit;
    let shown = if limit == Duration::MAX {
        "no limit".to_owned()
    } else {
        format!(
            "{} s{}",
            limit.as_secs(),
            if assumed {
                " (assumed: no TimeoutStopSec found in user@.service or its drop-ins)"
            } else {
                ""
            }
        )
    };
    if limit >= Duration::from_secs(want) {
        Check::new(
            "user stop",
            Level::Ok,
            format!("user@.service stops after {shown}, enough for the service's {want} s"),
        )
    } else {
        Check::new(
            "user stop",
            Level::Warn,
            format!(
                "timeouts.shutdown_grace + {STOP_MARGIN_SECS} s is {want} s, but user@.service stops \
                 the user's units after {shown}, so a shutdown cuts the flush short. As root, raise \
                 TimeoutStopSec in a drop-in of user@.service (systemctl edit user@.service), or lower the grace"
            ),
        )
    }
}

fn stop_timeout_check(have: Option<u64>, want: u64) -> Check {
    match have {
        Some(have) if have == want => Check::new(
            "service stop",
            Level::Ok,
            format!(
                "the service stops after {have} s: timeouts.shutdown_grace + {STOP_MARGIN_SECS} s"
            ),
        ),
        Some(have) => Check::new(
            "service stop",
            Level::Warn,
            format!(
                "the service stops after {have} s, but timeouts.shutdown_grace + {STOP_MARGIN_SECS} s \
                 is {want} s; run `pdmount service install` to update it"
            ),
        ),
        None => Check::new(
            "service stop",
            Level::Warn,
            "the stop timeout is missing from the service's settings; run `pdmount service install`",
        ),
    }
}

/// D091: elogind holds a shutdown for at most `InhibitDelayMaxSec`.
fn logind_check(grace: Duration, limit: Option<Duration>) -> Check {
    match limit {
        None => Check::new(
            "logind delay",
            Level::Ok,
            "no elogind configuration found; not checked",
        ),
        Some(limit) if limit >= grace => Check::new(
            "logind delay",
            Level::Ok,
            format!(
                "elogind holds a shutdown for up to {} s (InhibitDelayMaxSec), enough for the grace of {} s",
                limit.as_secs(),
                grace.as_secs()
            ),
        ),
        Some(limit) => Check::new(
            "logind delay",
            Level::Warn,
            format!(
                "elogind holds a shutdown for at most {} s (InhibitDelayMaxSec), less than the grace of {} s, \
                 so a shutdown cuts the flush short. As root, create {ELOGIND_DROP_IN_PATH} with \
                 `InhibitDelayMaxSec={}` under [Login] (`pdmount service install` prints it)",
                limit.as_secs(),
                grace.as_secs(),
                grace.as_secs() + u64::from(grace.subsec_nanos() > 0)
            ),
        ),
    }
}

// ---- Reading logind's configuration -------------------------------------------------

/// elogind's default for `InhibitDelayMaxSec`.
const ELOGIND_DEFAULT_DELAY: Duration = Duration::from_secs(5);

/// What systemd gives `user@.service` to stop: its `TimeoutStopSec` from the
/// unit and its drop-ins in the usual directories; 120 s, the upstream
/// default, when none is found (`true`: assumed). `DefaultTimeoutStopSec` is
/// not read.
fn user_unit_stop_limit() -> (Duration, bool) {
    let dirs = [
        "/lib/systemd/system",
        "/usr/lib/systemd/system",
        "/usr/local/lib/systemd/system",
        "/run/systemd/system",
        "/etc/systemd/system",
    ]
    .map(PathBuf::from);
    let uid = nix::unistd::getuid();
    user_unit_stop_limit_in(
        &dirs,
        &[
            "user@.service.d".to_owned(),
            format!("user@{uid}.service.d"),
        ],
    )
}

/// `dirs` in ascending priority.
fn user_unit_stop_limit_in(dirs: &[PathBuf], drop_in_names: &[String]) -> (Duration, bool) {
    // The unit itself comes from the directory of highest priority that has it.
    let main: Vec<PathBuf> = dirs
        .iter()
        .rev()
        .map(|dir| dir.join("user@.service"))
        .find(|file| file.exists())
        .into_iter()
        .collect();
    let drop_in_dirs: Vec<PathBuf> = drop_in_names
        .iter()
        .flat_map(|name| dirs.iter().map(move |dir| dir.join(name)))
        .collect();
    match layered_value(&main, &drop_in_dirs, "Service", "TimeoutStopSec")
        .and_then(|value| parse_timespan(&value))
    {
        // 0 means no limit, as infinity does.
        Some(limit) if limit.is_zero() => (Duration::MAX, false),
        Some(limit) => (limit, false),
        None => (Duration::from_secs(120), true),
    }
}

/// The effective `InhibitDelayMaxSec` from `/etc/elogind` and the other
/// directories elogind reads; `None` when there is no elogind configuration.
fn elogind_limit() -> Option<Duration> {
    let main = [PathBuf::from("/etc/elogind/logind.conf")];
    let drop_ins = [
        "/usr/lib/elogind/logind.conf.d",
        "/usr/local/lib/elogind/logind.conf.d",
        "/run/elogind/logind.conf.d",
        "/etc/elogind/logind.conf.d",
    ]
    .map(PathBuf::from);
    inhibit_delay_in(&main, &drop_ins)
}

fn inhibit_delay_in(main: &[PathBuf], drop_in_dirs: &[PathBuf]) -> Option<Duration> {
    if !main.iter().any(|file| file.exists()) && !drop_in_dirs.iter().any(|dir| dir.is_dir()) {
        return None;
    }
    Some(
        layered_value(main, drop_in_dirs, "Login", "InhibitDelayMaxSec")
            .and_then(|value| parse_timespan(&value))
            .unwrap_or(ELOGIND_DEFAULT_DELAY),
    )
}

/// The value `key` of `section` ends with in a layered configuration, as
/// systemd and elogind read it: the main files in order, then the `*.conf`
/// drop-ins sorted by file name, where a file of the same name in a later
/// directory replaces an earlier one. The last assignment wins; an empty one
/// resets the key (`None`).
fn layered_value(
    main: &[PathBuf],
    drop_in_dirs: &[PathBuf],
    section: &str,
    key: &str,
) -> Option<String> {
    let mut drop_ins = std::collections::BTreeMap::new();
    for dir in drop_in_dirs {
        let Ok(entries) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in entries.filter_map(Result::ok) {
            let name = entry.file_name();
            if name.to_string_lossy().ends_with(".conf") {
                drop_ins.insert(name, entry.path());
            }
        }
    }
    let mut value = None;
    for file in main.iter().chain(drop_ins.values()) {
        if let Ok(text) = std::fs::read_to_string(file)
            && let Some(found) = ini_value(&text, section, key)
        {
            value = found;
        }
    }
    value
}

/// The last assignment of `key` in `[section]` of an ini-style file:
/// `Some(None)` for an empty one.
fn ini_value(text: &str, section: &str, key: &str) -> Option<Option<String>> {
    let mut in_section = false;
    let mut found = None;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = name == section;
        } else if in_section
            && let Some((name, value)) = line.split_once('=')
            && name.trim() == key
        {
            let value = value.trim();
            found = Some((!value.is_empty()).then(|| value.to_owned()));
        }
    }
    found
}

/// A systemd time span: `90`, `90s`, `2min`, `1h 30min`; `infinity` is
/// `Duration::MAX`. A bare number is seconds.
fn parse_timespan(text: &str) -> Option<Duration> {
    let text = text.trim();
    if text == "infinity" {
        return Some(Duration::MAX);
    }
    let mut rest = text;
    let mut total = 0.0f64;
    let mut any = false;
    while !rest.is_empty() {
        let digits = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let number: f64 = rest[..digits].parse().ok()?;
        rest = rest[digits..].trim_start();
        let unit_end = rest
            .find(|c: char| !c.is_ascii_alphabetic())
            .unwrap_or(rest.len());
        let scale = match &rest[..unit_end] {
            "" | "s" | "sec" | "second" | "seconds" => 1.0,
            "us" | "usec" => 1e-6,
            "ms" | "msec" => 1e-3,
            "m" | "min" | "minute" | "minutes" => 60.0,
            "h" | "hr" | "hour" | "hours" => 3600.0,
            "d" | "day" | "days" => 86400.0,
            "w" | "week" | "weeks" => 604_800.0,
            _ => return None,
        };
        total += number * scale;
        any = true;
        rest = rest[unit_end..].trim_start();
    }
    any.then(|| Duration::from_secs_f64(total))
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_core::paths::BaseDirs;

    /// D102 amended: the window managers that do not run XDG autostart get
    /// the line for their configuration; other desktops get nothing.
    #[test]
    fn desktops_without_xdg_autostart_get_the_attach_line() {
        let binary = "/usr/bin/pdmount";
        let i3 = attach_hint("i3", "work", binary).expect("i3");
        assert!(
            i3.ends_with(
                "\n    exec --no-startup-id /usr/bin/pdmount --profile work session attach"
            ),
            "{i3}"
        );
        assert!(i3.contains("~/.config/i3/config"), "{i3}");
        let sway = attach_hint("sway", "work", binary).expect("sway");
        assert!(
            sway.ends_with("\n    exec /usr/bin/pdmount --profile work session attach"),
            "{sway}"
        );
        // A list, in any case.
        assert!(attach_hint("X-Generic:SWAY", "work", binary).is_some());
        let unset = attach_hint("", "work", binary).expect("no desktop named");
        assert!(
            unset.contains("exec --no-startup-id /usr/bin/pdmount --profile work session attach")
        );
        for desktop in ["XFCE", "KDE", "GNOME", "ubuntu:GNOME", "LXQt"] {
            assert_eq!(attach_hint(desktop, "work", binary), None, "{desktop}");
        }
        // The binary is quoted for the shell that runs the line.
        let spaced = attach_hint("i3", "work", "/opt/pd mount/pdmount").expect("i3");
        assert!(
            spaced.ends_with(
                "exec --no-startup-id '/opt/pd mount/pdmount' --profile work session attach"
            ),
            "{spaced}"
        );
    }

    fn sample<'a>() -> Settings<'a> {
        Settings {
            profile: "work",
            binary: "/usr/bin/pdmount",
            secrets_flag: Some("--force-file-secrets"),
            stop_timeout: 120,
            mountpoint: "/home/user/ProtonDrive",
            log: "/home/user/.local/state/pdmount/work.log",
            backend: None,
        }
    }

    /// A test build's service on a fake drive whose path has a space.
    fn fake<'a>() -> Settings<'a> {
        Settings {
            backend: Some("/tmp/fake drive"),
            ..sample()
        }
    }

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

    fn golden(name: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/golden")
            .join(name);
        std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
    }

    /// Set `UPDATE_GOLDEN=1` to rewrite the files after a deliberate change.
    fn assert_golden(name: &str, actual: &str) {
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            let path = Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/golden")
                .join(name);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, actual).expect("write");
            return;
        }
        assert_eq!(
            actual,
            golden(name),
            "{name} differs; UPDATE_GOLDEN=1 rewrites it"
        );
    }

    #[test]
    fn the_openrc_files_match_their_golden_files() {
        assert_golden("openrc-pdmount", &openrc_script("/usr/bin/pdmount"));
        assert_golden("openrc-conf-work", &openrc_conf(&sample()));
        let plain = Settings {
            secrets_flag: None,
            ..sample()
        };
        assert_golden("openrc-conf-plain", &openrc_conf(&plain));
        assert_golden("openrc-conf-fake", &openrc_conf(&fake()));
    }

    /// The script and conf.d file must parse as POSIX shell, which is what
    /// `openrc-run` sources them with.
    #[test]
    fn the_openrc_files_parse_as_shell() {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, text) in [
            ("script", openrc_script("/opt/my tools/pd")),
            ("conf", openrc_conf(&sample())),
            ("conf-fake", openrc_conf(&fake())),
        ] {
            let file = dir.path().join(name);
            std::fs::write(&file, text).expect("write");
            let status = std::process::Command::new("sh")
                .arg("-n")
                .arg(&file)
                .status()
                .expect("sh");
            assert!(status.success(), "sh -n {name}");
        }
    }

    /// `supervise-daemon.sh` expands `$command` and `$output_log` unquoted
    /// inside `eval`: the words must come out as the original strings.
    #[test]
    fn eval_quoting_survives_the_assignment_and_the_eval() {
        let dir = tempfile::tempdir().expect("tempdir");
        for original in [
            "/usr/bin/pdmount",
            "/opt/my tools/pd",
            "/opt/it's/$HOME/`id`/pd;x",
            "/a\"b\\c d",
        ] {
            let file = dir.path().join("vars");
            std::fs::write(&file, format!("command={}\n", eval_quote(original))).expect("write");
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(". \"$1\"; eval 'printf \"<%s>\"' $command")
                .arg("sh")
                .arg(&file)
                .output()
                .expect("sh");
            assert_eq!(
                String::from_utf8_lossy(&output.stdout),
                format!("<{original}>"),
                "{original}"
            );
        }
    }

    #[test]
    fn the_openrc_script_uses_the_profile_conf_and_a_link() {
        let script = openrc_script("/usr/bin/pdmount");
        assert!(script.starts_with("#!/sbin/openrc-run\n"));
        // The command line never names a profile or a secrets flag of its own;
        // `-v` logs at INFO, as a background `mount` does.
        assert!(script.contains(
            "--profile $profile $pdmount_secrets -v mount $pdmount_mount_args --foreground"
        ));
        let conf = openrc_conf(&sample());
        assert!(conf.contains("retry=\"TERM/120/KILL/5\""));
        assert!(conf.contains("pdmount_secrets=--force-file-secrets"));
        assert_eq!(openrc_conf_stop_timeout(&conf), Some(120));
        assert_eq!(openrc_conf_stop_timeout("nothing"), None);
    }

    #[test]
    fn the_stop_timeout_is_the_grace_plus_the_margin() {
        assert_eq!(stop_timeout(Duration::from_secs(90)), 120);
        assert_eq!(stop_timeout(Duration::from_secs(30)), 60);
        assert_eq!(stop_timeout(Duration::from_millis(1500)), 32);
    }

    #[test]
    fn control_characters_are_refused() {
        assert!(plain("x", "/a/b").is_ok());
        assert!(plain("x", "/a\nb").is_err());
        assert!(plain("x", "/a\u{7f}").is_err());
    }

    #[test]
    fn the_manager_is_detected_from_run() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(detect_in(dir.path()), None);
        std::fs::create_dir(dir.path().join("openrc")).expect("mkdir");
        assert_eq!(detect_in(dir.path()), Some(Manager::OpenRc));
        std::fs::create_dir_all(dir.path().join("systemd/system")).expect("mkdir");
        assert_eq!(detect_in(dir.path()), Some(Manager::Systemd));
    }

    #[test]
    fn timespans_parse_like_systemd() {
        let secs = |text| parse_timespan(text).map(|d| d.as_secs());
        assert_eq!(secs("5"), Some(5));
        assert_eq!(secs("180s"), Some(180));
        assert_eq!(secs("2min"), Some(120));
        assert_eq!(secs("1h 30min"), Some(5400));
        assert_eq!(secs("90 sec"), Some(90));
        assert_eq!(parse_timespan("infinity"), Some(Duration::MAX));
        assert_eq!(secs(""), None);
        assert_eq!(secs("fast"), None);
        assert_eq!(secs("5 parsecs"), None);
    }

    #[test]
    fn logind_configuration_is_layered() {
        let dir = tempfile::tempdir().expect("tempdir");
        let main = dir.path().join("logind.conf");
        let low = dir.path().join("lib.d");
        let high = dir.path().join("etc.d");
        for d in [&low, &high] {
            std::fs::create_dir(d).expect("mkdir");
        }
        // Nothing there: no verdict.
        assert_eq!(
            inhibit_delay_in(&[dir.path().join("none")], &[dir.path().join("none.d")]),
            None
        );
        // A main file without the key: the default.
        std::fs::write(
            &main,
            "[Login]\n#InhibitDelayMaxSec=5\nHandleLidSwitch=ignore\n",
        )
        .expect("write");
        let limit = || inhibit_delay_in(std::slice::from_ref(&main), &[low.clone(), high.clone()]);
        assert_eq!(limit(), Some(Duration::from_secs(5)));
        std::fs::write(&main, "[Login]\nInhibitDelayMaxSec=20\n").expect("write");
        assert_eq!(limit(), Some(Duration::from_secs(20)));
        // A drop-in beats the main file; by file name, the later one wins.
        std::fs::write(low.join("50-a.conf"), "[Login]\nInhibitDelayMaxSec=30\n").expect("write");
        assert_eq!(limit(), Some(Duration::from_secs(30)));
        std::fs::write(low.join("60-b.conf"), "[Login]\nInhibitDelayMaxSec=40s\n").expect("write");
        assert_eq!(limit(), Some(Duration::from_secs(40)));
        // The same name in a higher directory replaces the lower file.
        std::fs::write(high.join("60-b.conf"), "[Login]\nInhibitDelayMaxSec=2min\n")
            .expect("write");
        assert_eq!(limit(), Some(Duration::from_secs(120)));
        // Other sections do not count; an empty value resets to the default.
        std::fs::write(high.join("60-b.conf"), "[Other]\nInhibitDelayMaxSec=9\n").expect("write");
        assert_eq!(limit(), Some(Duration::from_secs(30)));
        std::fs::write(high.join("70-c.conf"), "[Login]\nInhibitDelayMaxSec=\n").expect("write");
        assert_eq!(limit(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn doctor_judges_the_stop_timeout_and_the_logind_delay() {
        assert_eq!(stop_timeout_check(Some(120), 120).level, Level::Ok);
        assert_eq!(stop_timeout_check(Some(60), 120).level, Level::Warn);
        assert_eq!(stop_timeout_check(None, 120).level, Level::Warn);
        let grace = Duration::from_secs(90);
        assert_eq!(logind_check(grace, None).level, Level::Ok);
        assert_eq!(
            logind_check(grace, Some(Duration::from_secs(180))).level,
            Level::Ok
        );
        let low = logind_check(grace, Some(Duration::from_secs(5)));
        assert_eq!(low.level, Level::Warn);
        assert!(
            low.detail.contains("InhibitDelayMaxSec=90"),
            "{}",
            low.detail
        );
    }

    #[test]
    fn the_logind_drop_in_names_the_grace() {
        assert_eq!(
            elogind_drop_in(Duration::from_secs(90)),
            "[Login]\nInhibitDelayMaxSec=90\n"
        );
    }

    #[test]
    fn the_systemd_files_match_their_golden_files() {
        assert_golden(
            "systemd-pdmount@.service",
            &systemd_template("/usr/bin/pdmount"),
        );
        assert_golden("systemd-dropin-work.conf", &systemd_dropin(&sample()));
        let plain = Settings {
            secrets_flag: None,
            ..sample()
        };
        assert_golden("systemd-dropin-plain.conf", &systemd_dropin(&plain));
        assert_golden("systemd-dropin-fake.conf", &systemd_dropin(&fake()));
    }

    /// `pdmount_mount_args` goes through the assignment in `conf.d`, then into
    /// `command_args` (inside double quotes), then `supervise-daemon.sh` splits
    /// it unquoted and `eval`s it: the words must come out as written, including
    /// a path with two spaces in a row, and with no argument when it is empty.
    #[test]
    fn the_openrc_mount_args_survive_the_script_and_the_eval() {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut cases = vec![(None, "<mount><--foreground>".to_owned())];
        for path in [
            "/tmp/fake drive",
            "/tmp/two  spaces/x",
            "/tmp/it's/$HOME/`id`/a;b",
            "/tmp/a\"b\\c d",
        ] {
            cases.push((
                Some(path),
                format!("<mount><--backend=fake:{path}><--foreground>"),
            ));
        }
        for (backend, expected) in cases {
            let settings = Settings {
                backend,
                ..sample()
            };
            let file = dir.path().join("conf");
            std::fs::write(&file, openrc_conf(&settings)).expect("write");
            // What the script does with the variable, then what supervise-daemon.sh does.
            let output = std::process::Command::new("sh")
                .arg("-c")
                .arg(
                    ". \"$1\"; command_args=\"mount $pdmount_mount_args --foreground\"; \
                     eval 'printf \"<%s>\"' $command_args",
                )
                .arg("sh")
                .arg(&file)
                .output()
                .expect("sh");
            assert!(output.status.success(), "{backend:?}");
            assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
            // `install` refuses the paths with a quote or a backslash, so `show` reads
            // back only the others.
            if backend.is_none_or(|dir| check_backend_dir(dir).is_ok()) {
                assert_eq!(
                    backend_of_openrc_conf(&openrc_conf(&settings)).as_deref(),
                    backend.map(|dir| format!("fake:{dir}")).as_deref()
                );
            }
        }
    }

    #[test]
    fn the_systemd_mount_args_are_quoted_for_environment_and_exec_start() {
        let plain = systemd_dropin(&Settings {
            backend: Some("/tmp/fake"),
            ..sample()
        });
        assert!(
            plain.contains("Environment=PDMOUNT_MOUNT_ARGS=--backend=fake:/tmp/fake\n"),
            "{plain}"
        );
        let spaced = systemd_dropin(&fake());
        assert!(
            spaced.contains(
                "Environment=\"PDMOUNT_MOUNT_ARGS=\\\"--backend=fake:/tmp/fake drive\\\"\"\n"
            ),
            "{spaced}"
        );
        assert_eq!(
            systemd_mount_args_line("--backend=fake:/tmp/100%"),
            "Environment=PDMOUNT_MOUNT_ARGS=--backend=fake:/tmp/100%%"
        );
        // Without a fake drive the drop-in has no such line.
        assert!(!systemd_dropin(&sample()).contains("PDMOUNT_MOUNT_ARGS"));
        for settings in [
            Settings {
                backend: Some("/tmp/fake"),
                ..sample()
            },
            fake(),
            Settings {
                backend: Some("/tmp/100%"),
                ..sample()
            },
        ] {
            let text = systemd_dropin(&settings);
            assert_eq!(
                backend_of_systemd_dropin(&text).as_deref(),
                settings.backend.map(|dir| format!("fake:{dir}")).as_deref(),
                "{text}"
            );
        }
        assert_eq!(backend_of_systemd_dropin(&systemd_dropin(&sample())), None);
    }

    #[test]
    fn the_fake_drive_path_must_be_quotable() {
        assert!(check_backend_dir("/tmp/fake drive").is_ok());
        for bad in ["/tmp/a\"b", "/tmp/a'b", "/tmp/a\\b", "/tmp/a\nb"] {
            assert!(check_backend_dir(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn systemd_quoting_doubles_specifiers_and_quotes_what_it_must() {
        assert_eq!(systemd_quote("/usr/bin/pdmount"), "/usr/bin/pdmount");
        assert_eq!(systemd_quote("/opt/100%/pd"), "/opt/100%%/pd");
        assert_eq!(systemd_quote("/opt/$HOME/pd"), "/opt/$$HOME/pd");
        assert_eq!(systemd_quote("/opt/my tools/pd"), "\"/opt/my tools/pd\"");
        assert_eq!(systemd_quote("/a\"b\\c"), "\"/a\\\"b\\\\c\"");
        assert_eq!(systemd_quote("/a;b"), "\"/a;b\"");
        assert_eq!(systemd_quote("/it's"), "\"/it's\"");
        assert_eq!(systemd_quote("/a%b c"), "\"/a%%b c\"");
        // A quoted path is still one argument of the line.
        let line = systemd_stop_post("/usr/bin/pdmount", "/mnt/my drive/100%");
        assert_eq!(
            line,
            "ExecStopPost=-/usr/bin/pdmount --profile %i unmount --if-stale \"/mnt/my drive/100%%\""
        );
    }

    #[test]
    fn the_template_uses_the_raw_instance_and_the_environment_flag() {
        let unit = systemd_template("/usr/bin/pdmount");
        assert!(
            unit.contains(
                "--profile %i $PDMOUNT_SECRETS -v mount $PDMOUNT_MOUNT_ARGS --foreground"
            ),
            "{unit}"
        );
        assert!(!unit.contains("%I"), "{unit}");
        for line in [
            "Type=notify",
            "Restart=on-failure",
            "RestartSec=5",
            "RestartPreventExitStatus=78",
            "After=dbus.socket dbus.service",
        ] {
            assert!(unit.lines().any(|l| l == line), "{line}");
        }
        // The stop timeout and the mountpoint are per profile, in the drop-in.
        assert!(!unit.contains("TimeoutStopSec") && !unit.contains("ExecStopPost"));
        let dropin = systemd_dropin(&sample());
        assert_eq!(systemd_dropin_stop_timeout(&dropin), Some(120));
        assert_eq!(systemd_dropin_stop_timeout("[Service]\n"), None);
        assert!(dropin.contains("Environment=PDMOUNT_SECRETS=--force-file-secrets\n"));
    }

    /// `systemd-analyze verify`, where it is installed.
    #[test]
    fn systemd_analyze_accepts_the_template() {
        let Ok(path) = std::env::var("PATH") else {
            return;
        };
        let analyze = path
            .split(':')
            .map(|dir| Path::new(dir).join("systemd-analyze"))
            .find(|candidate| candidate.is_file());
        let Some(analyze) = analyze else {
            eprintln!("systemd-analyze not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        // An executable that exists, so that ExecStart is accepted.
        let unit = dir.path().join("pdmount@work.service");
        std::fs::write(&unit, systemd_template("/bin/true")).expect("write");
        let output = std::process::Command::new(analyze)
            .arg("verify")
            .arg(&unit)
            .output()
            .expect("systemd-analyze");
        assert!(
            output.status.success(),
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn the_stop_limit_of_user_units_comes_from_their_unit_and_drop_ins() {
        let dir = tempfile::tempdir().expect("tempdir");
        let lib = dir.path().join("lib");
        let etc = dir.path().join("etc");
        let dirs = [lib.clone(), etc.clone()];
        let names = ["user@.service.d".to_owned()];
        let limit = || user_unit_stop_limit_in(&dirs, &names);
        for d in [&lib, &etc] {
            std::fs::create_dir_all(d.join("user@.service.d")).expect("mkdir");
        }
        // Nothing says: 120 s, assumed.
        assert_eq!(limit(), (Duration::from_secs(120), true));
        std::fs::write(
            lib.join("user@.service"),
            "[Service]\nTimeoutStopSec=2min\n",
        )
        .expect("write");
        assert_eq!(limit(), (Duration::from_secs(120), false));
        std::fs::write(
            etc.join("user@.service.d/90-local.conf"),
            "[Service]\nTimeoutStopSec=300\n",
        )
        .expect("write");
        assert_eq!(limit(), (Duration::from_secs(300), false));
        std::fs::write(
            etc.join("user@.service.d/90-local.conf"),
            "[Service]\nTimeoutStopSec=infinity\n",
        )
        .expect("write");
        assert_eq!(limit(), (Duration::MAX, false));
        std::fs::write(
            etc.join("user@.service.d/90-local.conf"),
            "[Service]\nTimeoutStopSec=0\n",
        )
        .expect("write");
        assert_eq!(limit(), (Duration::MAX, false));
    }

    #[test]
    fn doctor_compares_the_grace_with_the_stop_limit_of_user_units() {
        let ok = user_stop_check(120, (Duration::from_secs(120), false));
        assert_eq!(ok.level, Level::Ok);
        assert_eq!(
            user_stop_check(120, (Duration::MAX, false)).level,
            Level::Ok
        );
        let warn = user_stop_check(150, (Duration::from_secs(120), true));
        assert_eq!(warn.level, Level::Warn);
        assert!(warn.detail.contains("assumed"), "{}", warn.detail);
        assert!(warn.detail.contains("150 s"), "{}", warn.detail);
        let dropin = systemd_dropin(&sample());
        assert_eq!(
            mountpoint_check(&dropin, &systemd_stop_post_tail("/home/user/ProtonDrive")).level,
            Level::Ok
        );
        assert_eq!(
            mountpoint_check(&dropin, &systemd_stop_post_tail("/mnt/other")).level,
            Level::Warn
        );
        // A drop-in of the first M5.5 install detached any mount of the path.
        let old = "[Service]\nExecStopPost=-fusermount3 -uz /home/user/ProtonDrive\n";
        assert_eq!(
            mountpoint_check(old, &systemd_stop_post_tail("/home/user/ProtonDrive")).level,
            Level::Warn
        );
    }

    #[test]
    fn a_service_of_another_manager_blocks_an_install() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = paths_in(dir.path(), "work");
        refuse_conflicts(&paths, Manager::Systemd).expect("nothing installed");
        let link = supervise::openrc_service_link(&paths);
        std::fs::create_dir_all(link.parent().expect("parent")).expect("mkdir");
        std::os::unix::fs::symlink("pdmount", &link).expect("symlink");
        refuse_conflicts(&paths, Manager::OpenRc).expect("same manager reinstalls");
        let error = refuse_conflicts(&paths, Manager::Systemd).expect_err("other manager");
        assert!(
            format!("{error:#}").contains("service uninstall"),
            "{error:#}"
        );
    }
}
