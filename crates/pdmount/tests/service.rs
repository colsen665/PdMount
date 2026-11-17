//! `pdmount service install|uninstall` against a temporary `XDG_CONFIG_HOME`,
//! with stub `rc-update`, `rc-service` and `systemctl` on `PATH` that record
//! their arguments (M5.4, M5.5). Nothing starts a real service: the sandbox
//! has no session, and the manual runs are M5.6.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const BINARY: &str = env!("CARGO_BIN_EXE_pdmount");

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let sandbox = Self {
            dir: tempfile::tempdir().expect("tempdir"),
        };
        std::fs::create_dir_all(sandbox.bin()).expect("mkdir");
        for tool in ["rc-update", "rc-service", "systemctl"] {
            let stub = sandbox.bin().join(tool);
            write_executable(
                &stub,
                "#!/bin/sh\n\
                 printf '%s\\n' \"$(basename \"$0\") $*\" >>\"$STUB_LOG\"\n\
                 [ -z \"$STUB_FAIL\" ] || { echo \"$STUB_FAIL\" >&2; exit 1; }\n",
            );
        }
        sandbox
    }

    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn bin(&self) -> PathBuf {
        self.root().join("bin")
    }

    fn config(&self) -> PathBuf {
        self.root().join("config")
    }

    fn log(&self) -> PathBuf {
        self.root().join("stub.log")
    }

    /// What the stubs were called with, one call per line.
    fn calls(&self) -> Vec<String> {
        std::fs::read_to_string(self.log())
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn pdmount(&self, args: &[&str]) -> Output {
        self.pdmount_with(args, &[])
    }

    fn pdmount_with(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!("{}:/usr/bin:/bin", self.bin().display());
        Command::new(BINARY)
            .args(args)
            .env_clear()
            .current_dir(self.root())
            .env("PATH", path)
            .env("HOME", self.root().join("home"))
            .env("XDG_CONFIG_HOME", self.config())
            .env("XDG_DATA_HOME", self.root().join("data"))
            .env("XDG_CACHE_HOME", self.root().join("cache"))
            .env("XDG_STATE_HOME", self.root().join("state"))
            .env("XDG_RUNTIME_DIR", self.root().join("run"))
            .env("STUB_LOG", self.log())
            .envs(env.iter().copied())
            .output()
            .expect("run pdmount")
    }

    /// Writes the profile's configuration file.
    fn write_profile_config(&self, profile: &str, text: &str) {
        let file = self.config().join(format!("pdmount/{profile}.toml"));
        std::fs::create_dir_all(file.parent().expect("parent")).expect("mkdir");
        std::fs::write(file, text).expect("write");
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[track_caller]
fn succeeds(output: &Output) {
    assert!(
        output.status.success(),
        "exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

/// Two profiles on one mountpoint (V-6, D127): `install` refuses and
/// touches nothing, `doctor` warns; with a mountpoint of its own it installs.
#[test]
fn install_refuses_a_mountpoint_that_another_profile_uses() {
    let sandbox = Sandbox::new();
    sandbox.write_profile_config("work", "[timeouts]\nshutdown_grace = \"45s\"\n");
    sandbox.write_profile_config("home", "[timeouts]\nshutdown_grace = \"45s\"\n");
    let install = || {
        sandbox.pdmount(&[
            "--profile",
            "work",
            "--force-file-secrets",
            "service",
            "install",
            "--manager",
            "openrc",
        ])
    };
    let output = install();
    assert!(!output.status.success(), "{}", stdout(&output));
    assert!(
        stderr(&output).contains("profile home also uses ")
            && stderr(&output).contains("its own `mountpoint`"),
        "{}",
        stderr(&output)
    );
    assert!(!sandbox.config().join("rc").exists());
    assert!(sandbox.calls().is_empty(), "{:?}", sandbox.calls());
    let doctor = sandbox.pdmount(&["--profile", "work", "--force-file-secrets", "doctor"]);
    assert!(
        stdout(&doctor).contains("[WARN] mountpoint   profile home also uses "),
        "{}",
        stdout(&doctor)
    );

    sandbox.write_profile_config("home", "mountpoint = \"~/Home\"\n");
    succeeds(&install());
    let doctor = sandbox.pdmount(&["--profile", "work", "--force-file-secrets", "doctor"]);
    assert!(
        stdout(&doctor).contains("[ok  ] mountpoint   "),
        "{}",
        stdout(&doctor)
    );
}

#[test]
fn openrc_install_writes_the_files_and_calls_the_tools() {
    let sandbox = Sandbox::new();
    sandbox.write_profile_config("work", "[timeouts]\nshutdown_grace = \"45s\"\n");
    let output = sandbox.pdmount(&[
        "--profile",
        "work",
        "--force-file-secrets",
        "service",
        "install",
        "--manager",
        "openrc",
    ]);
    succeeds(&output);
    let init = sandbox.config().join("rc/init.d");
    let script = std::fs::read_to_string(init.join("pdmount")).expect("script");
    assert!(script.starts_with("#!/sbin/openrc-run\n"), "{script}");
    assert!(script.contains(&format!("command={BINARY}\n")), "{script}");
    let conf =
        std::fs::read_to_string(sandbox.config().join("rc/conf.d/pdmount.work")).expect("conf.d");
    // 45 s grace + 30 s.
    assert!(conf.contains("retry=\"TERM/75/KILL/5\""), "{conf}");
    assert!(
        conf.contains("pdmount_secrets=--force-file-secrets"),
        "{conf}"
    );
    assert!(conf.contains("work.log"), "{conf}");
    assert_eq!(
        std::fs::read_link(init.join("pdmount.work")).expect("link"),
        Path::new("pdmount")
    );
    use std::os::unix::fs::PermissionsExt;
    let mode = |path: &Path| {
        std::fs::metadata(path)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(&init.join("pdmount")), 0o755);
    let log = sandbox.root().join("state/pdmount/work.log");
    assert_eq!(mode(&log), 0o600, "the log is private");
    assert_eq!(mode(log.parent().expect("parent")), 0o700);
    assert_eq!(
        sandbox.calls(),
        [
            "rc-update --user add pdmount.work default",
            "rc-service --user pdmount.work start"
        ]
    );
    // D091: the root drop-in is printed, never written.
    let text = stdout(&output);
    assert!(
        text.contains("InhibitDelayMaxSec=45") || text.contains("enough for the grace"),
        "{text}"
    );
    if text.contains("InhibitDelayMaxSec") {
        assert!(
            text.contains("/etc/elogind/logind.conf.d/pdmount.conf"),
            "{text}"
        );
    }

    // The state names it, and autostart now refuses.
    let state = sandbox.pdmount(&["--profile", "work", "service", "--manager", "openrc"]);
    succeeds(&state);
    assert!(
        stdout(&state).contains("OpenRC user service installed"),
        "{}",
        stdout(&state)
    );
    assert!(
        stdout(&state).contains("stop timeout: 75 s"),
        "{}",
        stdout(&state)
    );
    let autostart = sandbox.pdmount(&["--profile", "work", "autostart", "enable"]);
    assert!(!autostart.status.success());
    assert!(
        stderr(&autostart).contains("service uninstall"),
        "{}",
        stderr(&autostart)
    );
}

#[test]
fn openrc_reinstall_does_not_add_the_service_twice() {
    let sandbox = Sandbox::new();
    let install = || {
        sandbox.pdmount(&[
            "--profile",
            "work",
            "service",
            "install",
            "--manager",
            "openrc",
        ])
    };
    succeeds(&install());
    // `rc-update add` made this link in the runlevel.
    let runlevel = sandbox.config().join("rc/runlevels/default");
    std::fs::create_dir_all(&runlevel).expect("mkdir");
    std::os::unix::fs::symlink("../../init.d/pdmount.work", runlevel.join("pdmount.work"))
        .expect("symlink");
    std::fs::remove_file(sandbox.log()).expect("clear log");
    succeeds(&install());
    assert_eq!(sandbox.calls(), ["rc-service --user pdmount.work start"]);
}

#[test]
fn openrc_uninstall_removes_the_profile_and_the_script_with_the_last_one() {
    let sandbox = Sandbox::new();
    let run = |profile: &str, verb: &str| {
        sandbox.pdmount(&["--profile", profile, "service", verb, "--manager", "openrc"])
    };
    succeeds(&run("work", "install"));
    succeeds(&run("home", "install"));
    let init = sandbox.config().join("rc/init.d");
    let runlevel = sandbox.config().join("rc/runlevels/default");
    std::fs::create_dir_all(&runlevel).expect("mkdir");
    std::os::unix::fs::symlink("x", runlevel.join("pdmount.work")).expect("symlink");
    std::fs::remove_file(sandbox.log()).expect("clear log");

    let output = run("work", "uninstall");
    succeeds(&output);
    assert_eq!(
        sandbox.calls(),
        [
            "rc-service --user pdmount.work stop",
            "rc-update --user del pdmount.work default"
        ]
    );
    assert!(init.join("pdmount.work").symlink_metadata().is_err());
    assert!(!sandbox.config().join("rc/conf.d/pdmount.work").exists());
    assert!(
        init.join("pdmount").exists(),
        "the other profile still needs the script"
    );
    assert!(init.join("pdmount.home").symlink_metadata().is_ok());

    // Without --manager, the installed one is removed; the script goes last.
    succeeds(&sandbox.pdmount(&["--profile", "home", "service", "uninstall"]));
    assert!(!init.join("pdmount").exists());
    assert!(init.join("pdmount.home").symlink_metadata().is_err());
    // Autostart works again.
    succeeds(&sandbox.pdmount(&["--profile", "home", "autostart", "enable"]));
    // And a second uninstall says so.
    let again = sandbox.pdmount(&["--profile", "work", "service", "uninstall"]);
    succeeds(&again);
    assert!(
        stdout(&again).contains("No user service is installed"),
        "{}",
        stdout(&again)
    );
}

/// D102: `install` writes the entry that runs `session attach` at each
/// desktop login, with no secret flag; it is not autostart; `uninstall`
/// removes it, for each manager.
#[test]
fn install_writes_the_attach_entry_and_uninstall_removes_it() {
    for manager in ["openrc", "systemd"] {
        let sandbox = Sandbox::new();
        let output = sandbox.pdmount(&[
            "--profile",
            "work",
            "--force-file-secrets",
            "service",
            "install",
            "--manager",
            manager,
        ]);
        succeeds(&output);
        let entry = sandbox
            .config()
            .join("autostart/pdmount-work-attach.desktop");
        let text = std::fs::read_to_string(&entry).expect("attach entry");
        assert!(
            text.contains(&format!("\nExec={BINARY} --profile work session attach\n")),
            "{text}"
        );
        assert!(!text.contains("secrets"), "{text}");
        assert!(
            stdout(&output).contains("pdmount-work-attach.desktop"),
            "{}",
            stdout(&output)
        );
        // Not autostart: its state, and doctor's double-start check.
        let state = sandbox.pdmount(&["--profile", "work", "autostart"]);
        assert!(
            stdout(&state).contains("not enabled for profile work"),
            "{}",
            stdout(&state)
        );
        if manager == "systemd" {
            enable_link(&sandbox, "work");
        }
        let doctor = sandbox.pdmount(&["--profile", "work", "--force-file-secrets", "doctor"]);
        assert!(
            !stdout(&doctor).contains("would both start the daemon"),
            "{}",
            stdout(&doctor)
        );
        let service = sandbox.pdmount(&["--profile", "work", "service", "--manager", manager]);
        assert!(
            stdout(&service).contains("desktop bus: handed over at each login"),
            "{}",
            stdout(&service)
        );

        let output = sandbox.pdmount(&["--profile", "work", "service", "uninstall"]);
        succeeds(&output);
        assert!(!entry.exists(), "{manager}: the attach entry stays");
        assert!(
            stdout(&output).contains("Removed") && stdout(&output).contains("-attach.desktop"),
            "{}",
            stdout(&output)
        );
        // Autostart works again and is not confused with it.
        succeeds(&sandbox.pdmount(&["--profile", "work", "autostart", "enable"]));
        assert!(
            sandbox
                .config()
                .join("autostart/pdmount-work.desktop")
                .exists()
        );
    }
}

/// D102 amended: i3 does not run XDG autostart entries, so `install` and
/// `service` print the line for its configuration; XFCE runs them and gets
/// no such note. The entry is written either way (for `dex` users).
#[test]
fn a_desktop_without_xdg_autostart_gets_the_attach_line() {
    let line = format!("\n    exec --no-startup-id {BINARY} --profile work session attach\n");
    for (desktop, hinted) in [("i3", true), ("XFCE", false)] {
        let sandbox = Sandbox::new();
        let env = [("XDG_CURRENT_DESKTOP", desktop)];
        let install = sandbox.pdmount_with(
            &[
                "--profile",
                "work",
                "--force-file-secrets",
                "service",
                "install",
                "--manager",
                "openrc",
            ],
            &env,
        );
        succeeds(&install);
        assert!(
            sandbox
                .config()
                .join("autostart/pdmount-work-attach.desktop")
                .exists(),
            "{desktop}: no attach entry"
        );
        let service = sandbox.pdmount_with(
            &["--profile", "work", "service", "--manager", "openrc"],
            &env,
        );
        for output in [&install, &service] {
            let text = stdout(output);
            assert_eq!(text.contains(&line), hinted, "{desktop}: {text}");
            assert_eq!(
                text.contains("does not run XDG autostart"),
                hinted,
                "{desktop}: {text}"
            );
            assert!(!text.contains("XDG_CURRENT_DESKTOP is not set"), "{text}");
        }
    }
}

/// `session attach` without a daemon: one line, exit 0. Outside a desktop
/// session, or with an address of another transport, it fails.
#[test]
fn session_attach_without_a_daemon_is_not_an_error() {
    let sandbox = Sandbox::new();
    // Without a daemon the attach is saved for its next start (D102 amended
    // 2026-11-14), so a desktop bus that does not answer is refused, as it
    // is with a daemon: there is nothing worth saving.
    let output = sandbox.pdmount_with(
        &["--profile", "work", "session", "attach"],
        &[("DBUS_SESSION_BUS_ADDRESS", "unix:path=/nonexistent/bus")],
    );
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("cannot use that bus"),
        "{}",
        stderr(&output)
    );
    let unset = sandbox.pdmount(&["--profile", "work", "session", "attach"]);
    assert!(!unset.status.success());
    assert!(
        stderr(&unset).contains("DBUS_SESSION_BUS_ADDRESS"),
        "{}",
        stderr(&unset)
    );
    let tcp = sandbox.pdmount_with(
        &["--profile", "work", "session", "attach"],
        &[("DBUS_SESSION_BUS_ADDRESS", "tcp:host=localhost,port=1")],
    );
    assert!(!tcp.status.success());
    assert!(stderr(&tcp).contains("unix:path="), "{}", stderr(&tcp));
}

#[test]
fn install_refuses_while_autostart_is_enabled() {
    let sandbox = Sandbox::new();
    succeeds(&sandbox.pdmount(&["--profile", "work", "autostart", "enable"]));
    let output = sandbox.pdmount(&[
        "--profile",
        "work",
        "service",
        "install",
        "--manager",
        "openrc",
    ]);
    assert!(!output.status.success());
    assert!(
        stderr(&output).contains("autostart disable"),
        "{}",
        stderr(&output)
    );
    assert!(sandbox.calls().is_empty());
    assert!(!sandbox.config().join("rc").exists(), "nothing was written");
}

#[test]
fn a_failing_tool_is_reported_and_the_files_stay_for_uninstall() {
    let sandbox = Sandbox::new();
    let output = sandbox.pdmount_with(
        &[
            "--profile",
            "work",
            "service",
            "install",
            "--manager",
            "openrc",
        ],
        &[("STUB_FAIL", "no session")],
    );
    assert!(!output.status.success());
    assert!(stderr(&output).contains("rc-update"), "{}", stderr(&output));
    assert!(
        sandbox
            .config()
            .join("rc/init.d/pdmount.work")
            .symlink_metadata()
            .is_ok()
    );
    let output = sandbox.pdmount_with(
        &["--profile", "work", "service", "uninstall"],
        &[("STUB_FAIL", "no session")],
    );
    succeeds(&output);
    assert!(stderr(&output).contains("warning"), "{}", stderr(&output));
    assert!(
        sandbox
            .config()
            .join("rc/init.d/pdmount.work")
            .symlink_metadata()
            .is_err()
    );
}

#[test]
fn a_bad_profile_name_never_reaches_a_unit_file() {
    let sandbox = Sandbox::new();
    for name in ["a b", "x;y", "a\nb", "../x", "$(id)"] {
        let output = sandbox.pdmount(&[
            "--profile",
            name,
            "service",
            "install",
            "--manager",
            "openrc",
        ]);
        assert!(!output.status.success(), "{name:?}");
    }
    assert!(!sandbox.config().join("rc").exists());
}

/// What `systemctl enable` does for the template's `[Install]`.
fn enable_link(sandbox: &Sandbox, profile: &str) -> PathBuf {
    let wants = sandbox.config().join("systemd/user/default.target.wants");
    std::fs::create_dir_all(&wants).expect("mkdir");
    let link = wants.join(format!("pdmount@{profile}.service"));
    std::os::unix::fs::symlink("../pdmount@.service", &link).expect("symlink");
    link
}

#[test]
fn systemd_install_writes_the_template_and_the_drop_in() {
    let sandbox = Sandbox::new();
    sandbox.write_profile_config(
        "work",
        "mountpoint = \"/mnt/my drive\"\n[timeouts]\nshutdown_grace = \"45s\"\n",
    );
    let output = sandbox.pdmount(&[
        "--profile",
        "work",
        "--allow-file-secrets",
        "service",
        "install",
        "--manager",
        "systemd",
    ]);
    succeeds(&output);
    let units = sandbox.config().join("systemd/user");
    let template = std::fs::read_to_string(units.join("pdmount@.service")).expect("template");
    assert!(
        template.contains(&format!(
            "ExecStart={BINARY} --profile %i $PDMOUNT_SECRETS -v mount"
        )),
        "{template}"
    );
    let dropin = std::fs::read_to_string(units.join("pdmount@work.service.d/pdmount.conf"))
        .expect("drop-in");
    assert!(
        dropin.contains("Environment=PDMOUNT_SECRETS=--allow-file-secrets\n"),
        "{dropin}"
    );
    assert!(dropin.contains("TimeoutStopSec=75\n"), "{dropin}");
    // Only a stale mount is cleared: the unit's start fails over another
    // daemon's live mount, and ExecStopPost runs then too (D094).
    assert!(
        dropin.contains(&format!(
            "ExecStopPost=-{BINARY} --profile %i unmount --if-stale \"/mnt/my drive\"\n"
        )),
        "{dropin}"
    );
    assert!(!dropin.contains("fusermount3"), "{dropin}");
    assert_eq!(
        sandbox.calls(),
        ["systemctl --user enable --now pdmount@work"]
    );

    // With the link `enable` makes, the state sees the service.
    enable_link(&sandbox, "work");
    let state = sandbox.pdmount(&["--profile", "work", "service"]);
    succeeds(&state);
    assert!(
        stdout(&state).contains("systemd user service installed"),
        "{}",
        stdout(&state)
    );
    assert!(
        stdout(&state).contains("stop timeout: 75 s"),
        "{}",
        stdout(&state)
    );
    let autostart = sandbox.pdmount(&["--profile", "work", "autostart", "enable"]);
    assert!(!autostart.status.success());
    // A service of the other manager is refused.
    let other = sandbox.pdmount(&[
        "--profile",
        "work",
        "service",
        "install",
        "--manager",
        "openrc",
    ]);
    assert!(!other.status.success());
    assert!(
        stderr(&other).contains("service uninstall"),
        "{}",
        stderr(&other)
    );
}

#[test]
fn a_profile_without_a_secret_flag_gets_no_environment_line() {
    let sandbox = Sandbox::new();
    succeeds(&sandbox.pdmount(&[
        "--profile",
        "plain",
        "service",
        "install",
        "--manager",
        "systemd",
    ]));
    let dropin = std::fs::read_to_string(
        sandbox
            .config()
            .join("systemd/user/pdmount@plain.service.d/pdmount.conf"),
    )
    .expect("drop-in");
    assert!(!dropin.contains("Environment"), "{dropin}");
}

#[test]
fn systemd_uninstall_removes_the_template_with_the_last_profile() {
    let sandbox = Sandbox::new();
    let run = |profile: &str, verb: &str| {
        sandbox.pdmount(&[
            "--profile",
            profile,
            "service",
            verb,
            "--manager",
            "systemd",
        ])
    };
    succeeds(&run("work", "install"));
    succeeds(&run("home", "install"));
    let work_link = enable_link(&sandbox, "work");
    let home_link = enable_link(&sandbox, "home");
    std::fs::remove_file(sandbox.log()).expect("clear log");
    let units = sandbox.config().join("systemd/user");

    succeeds(&run("work", "uninstall"));
    assert_eq!(
        sandbox.calls(),
        [
            "systemctl --user disable --now pdmount@work",
            "systemctl --user daemon-reload"
        ]
    );
    assert!(
        work_link.symlink_metadata().is_err(),
        "the enable link is removed"
    );
    assert!(!units.join("pdmount@work.service.d").exists());
    assert!(
        units.join("pdmount@.service").exists(),
        "home still needs the template"
    );
    assert!(units.join("pdmount@home.service.d/pdmount.conf").exists());

    // Without --manager the installed one is found; the template goes last.
    succeeds(&sandbox.pdmount(&["--profile", "home", "service", "uninstall"]));
    assert!(home_link.symlink_metadata().is_err());
    assert!(!units.join("pdmount@.service").exists());
    succeeds(&sandbox.pdmount(&["--profile", "home", "autostart", "enable"]));
}

#[test]
fn systemd_uninstall_still_cleans_up_when_the_manager_is_not_running() {
    let sandbox = Sandbox::new();
    succeeds(&sandbox.pdmount(&[
        "--profile",
        "work",
        "service",
        "install",
        "--manager",
        "systemd",
    ]));
    let link = enable_link(&sandbox, "work");
    let output = sandbox.pdmount_with(
        &["--profile", "work", "service", "uninstall"],
        &[("STUB_FAIL", "Failed to connect to bus")],
    );
    succeeds(&output);
    assert!(stderr(&output).contains("warning"), "{}", stderr(&output));
    assert!(link.symlink_metadata().is_err());
    assert!(
        !sandbox
            .config()
            .join("systemd/user/pdmount@.service")
            .exists()
    );
}

/// A daemon of the profile that no service manager runs, as `daemon_pid` sees
/// it: a process named `pdmount` and a pid file. Dropped, it is killed.
struct HandDaemon(std::process::Child);

impl HandDaemon {
    fn start(sandbox: &Sandbox, profile: &str) -> Self {
        let named = sandbox.bin().join("pdmount");
        std::os::unix::fs::symlink("/bin/sleep", &named).expect("link sleep");
        let child = Command::new(&named).arg("300").spawn().expect("spawn");
        let pid_file = sandbox.root().join(format!("run/pdmount/{profile}.pid"));
        std::fs::create_dir_all(pid_file.parent().expect("parent")).expect("mkdir");
        std::fs::write(pid_file, format!("{}\n/mnt/hand\n", child.id())).expect("pid file");
        Self(child)
    }
}

impl Drop for HandDaemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn install_does_not_start_the_service_over_a_daemon_started_by_hand() {
    for manager in ["systemd", "openrc"] {
        let sandbox = Sandbox::new();
        let _daemon = HandDaemon::start(&sandbox, "work");
        let output = sandbox.pdmount(&[
            "--profile",
            "work",
            "service",
            "install",
            "--manager",
            manager,
        ]);
        succeeds(&output);
        assert!(
            stdout(&output).contains("started by hand"),
            "{manager}: {}",
            stdout(&output)
        );
        // Enabled, but not started: a second daemon would fail over the first one's mount.
        let calls = sandbox.calls();
        match manager {
            "systemd" => assert_eq!(calls, ["systemctl --user enable pdmount@work"]),
            _ => assert_eq!(calls, ["rc-update --user add pdmount.work default"]),
        }
    }
}

// ---- A failed start on a real OpenRC (D109) ------------------------------------------

/// The OpenRC tools the sandbox runs, where installed: the init script's
/// interpreter and the commands it and the test call.
fn openrc_installed() -> bool {
    Path::new("/sbin/openrc-run").is_file()
        && ["rc-service", "supervise-daemon"].iter().any(|tool| {
            ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
                .iter()
                .any(|dir| Path::new(dir).join(tool).is_file())
        })
}

/// A user service tree of OpenRC in the sandbox: `XDG_CONFIG_HOME` and
/// `XDG_RUNTIME_DIR` are the sandbox's, so neither the user's services nor
/// their state are touched. The installed script runs a stand-in daemon.
/// Dropped, it stops the supervisor and kills every process that names the
/// sandbox on its command line.
struct OpenRc {
    sandbox: Sandbox,
    profile: String,
}

impl OpenRc {
    fn new() -> Self {
        let sandbox = Sandbox::new();
        // Unique, as processes are looked up by the service's name.
        let profile = format!("sbx{}", std::process::id());
        let openrc = Self { sandbox, profile };
        let root = openrc.sandbox.root().to_owned();
        let output = openrc.sandbox.pdmount(&[
            "--profile",
            &openrc.profile,
            "--force-file-secrets",
            "service",
            "install",
            "--manager",
            "openrc",
        ]);
        succeeds(&output);
        // The generated script, with the stand-in as its command.
        let script = openrc.sandbox.config().join("rc/init.d/pdmount");
        let text = std::fs::read_to_string(&script).expect("script");
        let stand_in = root.join("daemon/stand-in");
        let text: String = text
            .lines()
            .map(|line| {
                if line.starts_with("command=") {
                    format!("command={}\n", stand_in.display())
                } else {
                    format!("{line}\n")
                }
            })
            .collect();
        write_executable(&script, &text);
        // Run 1 fails before it is ready; later runs write the daemon's pid
        // file (so that `pdmount status` sees a daemon under OpenRC), report
        // ready and sleep as a process named `pdmount`.
        std::fs::create_dir_all(root.join("daemon")).expect("mkdir");
        std::os::unix::fs::symlink("/bin/sleep", root.join("daemon/pdmount")).expect("link");
        let pid_file = root.join(format!("run/pdmount/{}.pid", openrc.profile));
        std::fs::create_dir_all(pid_file.parent().expect("parent")).expect("mkdir");
        write_executable(
            &stand_in,
            &format!(
                "#!/bin/sh\n\
                 n=$(( $(cat {root}/count 2>/dev/null || echo 0) + 1 ))\n\
                 echo $n >{root}/count\n\
                 [ $n -gt 1 ] || exit 1\n\
                 printf '%s\\n/mnt/sbx\\nservice openrc\\n' $$ >{pid_file}\n\
                 printf '\\n' >&3\n\
                 exec 3>&-\n\
                 exec {root}/daemon/pdmount 600\n",
                root = root.display(),
                pid_file = pid_file.display(),
            ),
        );
        // A stand-in for the user's dbus service, which the script needs.
        write_executable(
            &openrc.sandbox.config().join("rc/init.d/dbus"),
            "#!/sbin/openrc-run\nstart() { return 0; }\nstop() { return 0; }\n",
        );
        let state = root.join("run/openrc");
        std::fs::create_dir_all(&state).expect("mkdir");
        std::fs::write(state.join("softlevel"), "").expect("softlevel");
        openrc
    }

    fn name(&self) -> String {
        format!("pdmount.{}", self.profile)
    }

    /// A command with the sandbox's user-service environment.
    fn command(&self, program: &str) -> Command {
        let root = self.sandbox.root();
        let mut command = Command::new("timeout");
        command
            .arg("60")
            .arg(program)
            .env_clear()
            .current_dir(root)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .env("HOME", root.join("home"))
            .env("XDG_CONFIG_HOME", self.sandbox.config())
            .env("XDG_RUNTIME_DIR", root.join("run"));
        command
    }

    fn rc_service(&self, action: &str) -> Output {
        self.command("rc-service")
            .args(["--user", &self.name(), action])
            .output()
            .expect("rc-service")
    }

    /// The exit code of `rc-service … status`: 0 started, 3 stopped.
    fn status(&self) -> Option<i32> {
        self.rc_service("status").status.code()
    }

    /// The stop command that `pdmount status` names for a stray supervisor.
    fn stop_stray(&self) -> Output {
        self.command("supervise-daemon")
            .args([&self.name(), "-U", "--stop"])
            .env("RC_USER_SERVICES", "yes")
            .env("RC_SVCNAME", self.name())
            .output()
            .expect("supervise-daemon")
    }

    fn supervisor_pid_file(&self) -> PathBuf {
        self.sandbox
            .root()
            .join(format!("run/supervise-{}.pid", self.name()))
    }

    /// The supervisors of the service that run.
    fn supervisors(&self) -> Vec<u32> {
        let name = self.name();
        processes(|args| {
            args.first().map(String::as_str) == Some("supervise-daemon")
                && args.get(1) == Some(&name)
        })
    }

    fn pdmount_status(&self) -> Output {
        self.sandbox
            .pdmount(&["--profile", &self.profile, "status"])
    }

    fn runs(&self) -> u32 {
        std::fs::read_to_string(self.sandbox.root().join("count"))
            .ok()
            .and_then(|text| text.trim().parse().ok())
            .unwrap_or(0)
    }
}

impl Drop for OpenRc {
    fn drop(&mut self) {
        let _ = self.stop_stray();
        let root = self.sandbox.root().to_string_lossy().into_owned();
        for pid in processes(|args| args.iter().any(|arg| arg.contains(&root))) {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
}

/// The pids of the processes whose arguments match, this one excluded.
fn processes(matches: impl Fn(&[String]) -> bool) -> Vec<u32> {
    let own = std::process::id();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter(|&pid| pid != own)
        .filter(|pid| {
            let Ok(cmdline) = std::fs::read(format!("/proc/{pid}/cmdline")) else {
                return false;
            };
            let args: Vec<String> = cmdline
                .split(|b| *b == 0)
                .filter(|arg| !arg.is_empty())
                .map(|arg| String::from_utf8_lossy(arg).into_owned())
                .collect();
            matches(&args)
        })
        .collect()
}

#[track_caller]
fn rc_ok(output: &Output, what: &str) {
    assert!(
        output.status.success(),
        "{what}: exit {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout(output),
        stderr(output)
    );
}

/// D109 against the installed OpenRC: a start that fails before the daemon
/// is ready leaves no supervisor behind, so the service really is stopped
/// and the next start works; a supervisor left over from before (made here
/// with `zap`) is not stopped by a start, and `pdmount status` names the
/// command that stops it. About 8 s, most of it the failed start.
#[test]
fn openrc_a_failed_start_leaves_no_supervisor() {
    if !openrc_installed() {
        eprintln!("OpenRC (openrc-run, rc-service, supervise-daemon) not installed; skipped");
        return;
    }
    let openrc = OpenRc::new();

    // Run 1 exits before it is ready.
    let output = openrc.rc_service("start");
    assert!(
        !output.status.success(),
        "the failed start succeeded: {}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(
        openrc.supervisors(),
        Vec::<u32>::new(),
        "a supervisor is left"
    );
    assert!(!openrc.supervisor_pid_file().exists());
    assert_eq!(openrc.status(), Some(3));
    assert_eq!(openrc.runs(), 1, "the daemon was respawned");

    // The next start works, and a stop stops it.
    rc_ok(&openrc.rc_service("start"), "second start");
    assert_eq!(openrc.status(), Some(0));
    assert_eq!(openrc.supervisors().len(), 1);
    let output = openrc.pdmount_status();
    assert!(
        !stderr(&output).contains("outside OpenRC"),
        "{}",
        stderr(&output)
    );
    rc_ok(&openrc.rc_service("stop"), "stop");
    assert_eq!(openrc.status(), Some(3));
    assert_eq!(openrc.supervisors(), Vec::<u32>::new());

    // A stray: the supervisor runs while OpenRC holds the service as stopped.
    rc_ok(&openrc.rc_service("start"), "third start");
    rc_ok(&openrc.rc_service("zap"), "zap");
    assert_eq!(openrc.status(), Some(3));
    let stray = openrc.supervisors();
    assert_eq!(stray.len(), 1);
    let output = openrc.pdmount_status();
    let warning = stderr(&output);
    assert!(warning.contains("outside OpenRC's state"), "{warning}");
    assert!(
        warning.contains(&format!(
            "RC_USER_SERVICES=yes RC_SVCNAME={0} supervise-daemon {0} -U --stop",
            openrc.name()
        )),
        "{warning}"
    );
    // A start does not stop it, and fails as before.
    let output = openrc.rc_service("start");
    assert!(!output.status.success());
    assert!(
        format!("{}{}", stdout(&output), stderr(&output)).contains("already running"),
        "{}{}",
        stdout(&output),
        stderr(&output)
    );
    assert_eq!(openrc.supervisors(), stray, "the start stopped the stray");
    // The command the warning names stops it.
    rc_ok(&openrc.stop_stray(), "the stray's stop command");
    assert_eq!(openrc.supervisors(), Vec::<u32>::new());
    let output = openrc.pdmount_status();
    assert!(
        !stderr(&output).contains("outside OpenRC"),
        "{}",
        stderr(&output)
    );
    assert_eq!(openrc.runs(), 3);
}

// ---- A test build's fake drive (M5.6) ----------------------------------------------

/// `service install --backend fake:<dir>` exists in test builds only, as `mount --backend fake:`.
#[cfg(not(feature = "fake-backend"))]
#[test]
fn release_builds_do_not_offer_the_backend_option() {
    let sandbox = Sandbox::new();
    let output = sandbox.pdmount(&[
        "--profile",
        "work",
        "service",
        "install",
        "--manager",
        "openrc",
        "--backend",
        "fake:/tmp/x",
    ]);
    assert!(!output.status.success());
    assert!(!sandbox.config().join("rc").exists());
    assert!(sandbox.calls().is_empty());
}

#[cfg(feature = "fake-backend")]
mod fake_drive {
    use super::*;

    /// A fake drive directory in the sandbox.
    fn drive(sandbox: &Sandbox, name: &str) -> PathBuf {
        let dir = sandbox.root().join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }

    fn install(sandbox: &Sandbox, manager: &str, backend: &str) -> Output {
        sandbox.pdmount(&[
            "--profile",
            "work",
            "--force-file-secrets",
            "service",
            "install",
            "--manager",
            manager,
            "--backend",
            backend,
        ])
    }

    #[test]
    fn openrc_gets_the_backend_in_conf_d_and_it_reaches_the_command_line() {
        let sandbox = Sandbox::new();
        // Two spaces in a row: the split and the eval of supervise-daemon.sh must keep both.
        let dir = drive(&sandbox, "fake  drive");
        let output = install(&sandbox, "openrc", &format!("fake:{}", dir.display()));
        succeeds(&output);
        let conf = std::fs::read_to_string(sandbox.config().join("rc/conf.d/pdmount.work"))
            .expect("conf.d");
        assert!(
            conf.contains(&format!(
                "pdmount_mount_args='--backend=fake:{}/fake\\ \\ drive'\n",
                sandbox.root().display()
            )),
            "{conf}"
        );
        let script =
            std::fs::read_to_string(sandbox.config().join("rc/init.d/pdmount")).expect("script");
        assert!(
            script.contains("$pdmount_secrets -v mount $pdmount_mount_args --foreground"),
            "{script}"
        );
        // Source both files as openrc-run does, then expand `command_args` as
        // supervise-daemon.sh does (unquoted, inside eval).
        let words = std::process::Command::new("sh")
            .arg("-c")
            .arg(
                "RC_SVCNAME=pdmount.work; . \"$1\"; . \"$2\"; \
                 eval 'printf \"<%s>\"' $command_args",
            )
            .arg("sh")
            .arg(sandbox.config().join("rc/conf.d/pdmount.work"))
            .arg(sandbox.config().join("rc/init.d/pdmount"))
            .output()
            .expect("sh");
        assert!(words.status.success());
        assert_eq!(
            String::from_utf8_lossy(&words.stdout),
            format!(
                "<--profile><work><--force-file-secrets><-v><mount><--backend=fake:{}/fake  drive>\
                 <--foreground><--service><openrc><--clear-stale><--notify-fd><3>",
                sandbox.root().display()
            )
        );
        assert_eq!(
            sandbox.calls(),
            [
                "rc-update --user add pdmount.work default",
                "rc-service --user pdmount.work start"
            ]
        );
        // The state shows the backend once the service is installed.
        let state = sandbox.pdmount(&["--profile", "work", "service"]);
        succeeds(&state);
        assert!(
            stdout(&state).contains(&format!(
                "backend: fake:{}/fake  drive (test build)",
                sandbox.root().display()
            )),
            "{}",
            stdout(&state)
        );
    }

    #[test]
    fn systemd_gets_the_backend_in_the_drop_in() {
        let sandbox = Sandbox::new();
        let dir = drive(&sandbox, "fake drive");
        let output = install(&sandbox, "systemd", &format!("fake:{}", dir.display()));
        succeeds(&output);
        let units = sandbox.config().join("systemd/user");
        let template = std::fs::read_to_string(units.join("pdmount@.service")).expect("template");
        assert!(
            template.contains(
                " --profile %i $PDMOUNT_SECRETS -v mount $PDMOUNT_MOUNT_ARGS --foreground --service systemd"
            ),
            "{template}"
        );
        let dropin = std::fs::read_to_string(units.join("pdmount@work.service.d/pdmount.conf"))
            .expect("drop-in");
        // Quoted for `Environment=`, around a word quoted for the split in `ExecStart=`.
        assert!(
            dropin.contains(&format!(
                "Environment=\"PDMOUNT_MOUNT_ARGS=\\\"--backend=fake:{}/fake drive\\\"\"\n",
                sandbox.root().display()
            )),
            "{dropin}"
        );
        assert!(
            dropin.contains("Environment=PDMOUNT_SECRETS=--force-file-secrets\n"),
            "{dropin}"
        );
        assert_eq!(
            sandbox.calls(),
            ["systemctl --user enable --now pdmount@work"]
        );
        enable_link(&sandbox, "work");
        let state = sandbox.pdmount(&["--profile", "work", "service"]);
        succeeds(&state);
        assert!(
            stdout(&state).contains(&format!(
                "backend: fake:{}/fake drive (test build)",
                sandbox.root().display()
            )),
            "{}",
            stdout(&state)
        );
    }

    #[test]
    fn a_plain_path_needs_no_quoting_and_a_relative_one_is_made_absolute() {
        let sandbox = Sandbox::new();
        drive(&sandbox, "drive");
        succeeds(&install(&sandbox, "systemd", "fake:drive"));
        let dropin = std::fs::read_to_string(
            sandbox
                .config()
                .join("systemd/user/pdmount@work.service.d/pdmount.conf"),
        )
        .expect("drop-in");
        assert!(
            dropin.contains(&format!(
                "Environment=PDMOUNT_MOUNT_ARGS=--backend=fake:{}/drive\n",
                sandbox.root().display()
            )),
            "{dropin}"
        );
        succeeds(&install(&sandbox, "openrc", "fake:drive"));
        let conf = std::fs::read_to_string(sandbox.config().join("rc/conf.d/pdmount.work"))
            .expect("conf.d");
        assert!(
            conf.contains(&format!(
                "pdmount_mount_args=--backend=fake:{}/drive\n",
                sandbox.root().display()
            )),
            "{conf}"
        );
    }

    #[test]
    fn without_the_option_the_settings_have_no_backend() {
        let sandbox = Sandbox::new();
        succeeds(&sandbox.pdmount(&[
            "--profile",
            "work",
            "service",
            "install",
            "--manager",
            "openrc",
        ]));
        let conf = std::fs::read_to_string(sandbox.config().join("rc/conf.d/pdmount.work"))
            .expect("conf.d");
        assert!(conf.contains("pdmount_mount_args=\n"), "{conf}");
        let state = sandbox.pdmount(&["--profile", "work", "service"]);
        assert!(!stdout(&state).contains("backend:"), "{}", stdout(&state));
    }

    #[test]
    fn a_missing_drive_or_an_unquotable_path_writes_nothing() {
        let sandbox = Sandbox::new();
        let output = install(&sandbox, "openrc", "fake:/nonexistent/fake drive");
        assert!(!output.status.success());
        assert!(
            stderr(&output).contains("not a directory"),
            "{}",
            stderr(&output)
        );
        let quoted = drive(&sandbox, "it's");
        let output = install(&sandbox, "systemd", &format!("fake:{}", quoted.display()));
        assert!(!output.status.success());
        assert!(stderr(&output).contains("quote"), "{}", stderr(&output));
        let output = install(&sandbox, "systemd", "fake:");
        assert!(!output.status.success());
        assert!(!sandbox.config().join("rc").exists());
        assert!(!sandbox.config().join("systemd").exists());
        assert!(sandbox.calls().is_empty());
    }
}

/// Writes an executable stub through a `sh` child, so this process never
/// holds a write descriptor on a file it runs later: a child forked by
/// another test thread meanwhile would inherit it until its exec, and running
/// the stub would fail with "Text file busy".
fn write_executable(path: &Path, text: &str) {
    use std::io::Write;
    let mut child = std::process::Command::new("sh")
        .args(["-c", "cat >\"$1\" && chmod 755 \"$1\"", "sh"])
        .arg(path)
        .stdin(std::process::Stdio::piped())
        .spawn()
        .expect("spawn sh");
    child
        .stdin
        .take()
        .expect("piped")
        .write_all(text.as_bytes())
        .expect("write stub");
    assert!(
        child.wait().expect("wait").success(),
        "cannot write {path:?}"
    );
}
