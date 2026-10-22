//! Mount daemon and mount management: `mount`, `unmount`, `stats`, `reset`
//! (DESIGN.md §14).
//!
//! `pdmount mount` starts `pdmount mount --foreground` as a detached child in
//! its own process group, with output appended to the profile's log file,
//! and returns once the child's control socket answers. The foreground
//! process runs the engine, the FUSE session and the control socket until
//! SIGTERM/SIGINT, an `unmount` request, or an external unmount.

use crate::control::{self, Request};
use crate::credentials::Credentials;
use crate::mounts;
use crate::session::{self, Resumed};
use anyhow::{Context, bail};
use pdmount_backend::Backend;
use pdmount_backend::fake::FakeBackend;
use pdmount_core::cache::CacheOptions;
use pdmount_core::engine::{Engine, EngineOptions};
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use serde_json::{Value, json};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// First mounts list the root and may refresh the session.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const TOKEN_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(10);
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

/// `pdmount mount` without `--foreground`.
pub async fn mount_background(
    paths: &ProfilePaths,
    mountpoint: &Path,
    backend: &BackendChoice,
    verbose: u8,
) -> anyhow::Result<ExitCode> {
    preflight(paths, mountpoint).await?;
    // Reading the session here makes a keyring unlock dialog appear while this
    // terminal waits; the detached daemon has no terminal (D029).
    if *backend == BackendChoice::Proton && Credentials::new(paths).load()?.is_none() {
        bail!(
            "profile {} is not logged in; run `pdmount login`",
            paths.profile
        );
    }
    let log_path = paths.log_file();
    if let Some(dir) = log_path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .with_context(|| format!("cannot open {}", log_path.display()))?;
    let log_start = log.metadata().map_or(0, |m| m.len());
    let mut command =
        Command::new(std::env::current_exe().context("cannot find the pdmount executable")?);
    command.arg("--profile").arg(paths.profile.as_str());
    command.args(crate::credentials::mode().flag());
    for _ in 0..verbose.max(1) {
        command.arg("-v");
    }
    command
        .arg("mount")
        .arg("--foreground")
        .arg("--backend")
        .arg(backend.arg())
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

fn log_tail(path: &Path, from: u64) -> String {
    let Ok(text) = std::fs::read(path) else {
        return String::new();
    };
    let start = (from as usize).min(text.len());
    let tail = String::from_utf8_lossy(&text[start..]).into_owned();
    let lines: Vec<&str> = tail.lines().collect();
    lines[lines.len().saturating_sub(20)..].join("\n")
}

/// Refuses to mount over a running daemon or an existing mount.
async fn preflight(paths: &ProfilePaths, mountpoint: &Path) -> anyhow::Result<()> {
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

/// Raises the soft open-file limit to the hard one and grows the descriptor
/// table to it now. Every staged file holds a descriptor, and each time the
/// kernel grows the table, every thread that opens a file waits for an RCU
/// grace period (30-80 ms measured, all FUSE threads at once).
fn grow_fd_table() {
    use nix::fcntl::{FcntlArg, fcntl};
    use nix::sys::resource::{Resource, getrlimit, setrlimit};
    let Ok((soft, hard)) = getrlimit(Resource::RLIMIT_NOFILE) else {
        return;
    };
    let limit = hard.min(FD_TABLE_MAX).max(soft);
    if soft < limit
        && let Err(error) = setrlimit(Resource::RLIMIT_NOFILE, limit, hard)
    {
        tracing::warn!(%error, "cannot raise the open-file limit");
        return;
    }
    // The table grows to hold the highest descriptor; closing it keeps the size.
    let highest = i32::try_from(limit.min(FD_TABLE_MAX) - 1).unwrap_or(i32::MAX);
    let grown = std::fs::File::open("/dev/null")
        .map_err(|e| e.to_string())
        .and_then(|null| {
            fcntl(&null, FcntlArg::F_DUPFD_CLOEXEC(highest)).map_err(|e| e.to_string())
        });
    match grown {
        Ok(fd) => {
            let _ = nix::unistd::close(fd);
        }
        Err(error) => tracing::warn!(%error, "cannot grow the descriptor table"),
    }
    tracing::debug!(limit, "open-file limit");
}

/// `pdmount mount --foreground`: runs until unmounted.
pub async fn run_foreground(
    paths: &ProfilePaths,
    config: &Config,
    mountpoint: &Path,
    choice: &BackendChoice,
) -> anyhow::Result<ExitCode> {
    preflight(paths, mountpoint).await?;
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("cannot create {}", mountpoint.display()))?;
    grow_fd_table();
    let (backend, logged_in, username, credential_storage): (Arc<dyn Backend>, _, _, _) =
        match choice {
            BackendChoice::Proton => {
                let logged_in = match session::resume(paths).await? {
                    Resumed::Active(logged_in) => logged_in,
                    Resumed::NotLoggedIn => bail!(
                        "profile {} is not logged in; run `pdmount login`",
                        paths.profile
                    ),
                    Resumed::NeedsLogin(error) => {
                        bail!("the session has expired; run `pdmount login` ({error:#})")
                    }
                };
                let device = session::device_identity(paths, config)?;
                let backend = session::proton_backend(&logged_in, &device).await?;
                let username = logged_in.session.username.clone();
                let storage = logged_in.storage.to_string();
                (Arc::new(backend), Some(logged_in), username, storage)
            }
            BackendChoice::Fake(dir) => {
                let fake = FakeBackend::open(dir)
                    .with_context(|| format!("cannot open the fake drive {}", dir.display()))?;
                let username = fake.account().await?.username;
                (Arc::new(fake), None, username, "none (fake backend)".into())
            }
        };
    let master_key: [u8; 32] = Credentials::new(paths)
        .cache_master_key()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the cache key has the wrong length"))?;
    let (engine, invalidations) = Engine::open(
        backend,
        EngineOptions {
            state_db: paths.state_db(),
            staging_dir: paths.staging_dir(),
            local_ignore: Some(paths.local_ignore_file()),
            cache: CacheOptions {
                dir: paths.blocks_dir(),
                master_key,
                ram_bytes: config.cache.ram_bytes.0,
                disk_bytes: config.cache.max_bytes.0,
            },
            config: config.clone(),
        },
    )
    .await
    .context("cannot open the local state")?;
    let mounted = {
        let engine = Arc::clone(&engine);
        let target = mountpoint.to_owned();
        tokio::task::spawn_blocking(move || pdmount_fuse::mount(engine, invalidations, &target))
            .await?
            .with_context(|| format!("cannot mount at {}", mountpoint.display()))?
    };
    engine.start_background();
    tracing::info!(mountpoint = %mountpoint.display(), "mounted");

    let pid_file = paths.pid_file();
    // The mountpoint lets `status` and `unmount` find the mount after a crash.
    std::fs::write(
        &pid_file,
        format!("{}\n{}\n", std::process::id(), mountpoint.display()),
    )?;
    let socket = paths.control_socket();
    let (unmount_tx, mut unmount_rx) = tokio::sync::mpsc::channel::<()>(1);
    let handler = request_handler(
        Arc::clone(&engine),
        mountpoint.to_owned(),
        username,
        credential_storage,
        unmount_tx,
    );
    let server = control::serve(&socket, handler)?;

    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    let mut tokens = match &logged_in {
        Some(logged_in) => Some(session::TokenSaver::new(&logged_in.session).await),
        None => None,
    };
    let mut token_timer = tokio::time::interval(TOKEN_SAVE_INTERVAL);
    let reason = loop {
        tokio::select! {
            _ = terminate.recv() => break "SIGTERM",
            _ = interrupt.recv() => break "SIGINT",
            _ = hangup.recv() => tracing::info!("ignoring SIGHUP"),
            // `fusermount3 -u` succeeded in the handler.
            _ = unmount_rx.recv() => break "unmount request",
            _ = token_timer.tick() => {
                if let (Some(tokens), Some(logged_in)) = (&mut tokens, &logged_in)
                    && let Err(error) = tokens.save(paths, &logged_in.session).await
                {
                    tracing::warn!(error = %format!("{error:#}"), "cannot store refreshed session tokens");
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // A copy of the mount in another mount namespace (e.g. a
                // Flatpak sandbox started while mounted) keeps the session
                // alive after an external unmount, so the mount table decides.
                if mounted.is_finished() || !matches!(mounts::find(mountpoint), Ok(Some(_))) {
                    break "unmounted";
                }
            }
        }
    };
    tracing::info!(reason, "shutting down");

    server.abort();
    control::remove(&socket);
    if matches!(mounts::find(mountpoint), Ok(Some(_))) {
        // Busy mounts are detached; the kernel finishes them when unused.
        if fusermount(mountpoint, false).is_err() {
            let _ = fusermount(mountpoint, true);
        }
    }
    // The session threads end once the kernel drops the connection. Copies
    // of the mount in other namespaces keep it alive; exiting closes
    // /dev/fuse, which ends it for them too. Never wait on them unbounded.
    let deadline = Instant::now() + SESSION_END_TIMEOUT;
    while !mounted.is_finished() && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if !mounted.is_finished() {
        tracing::warn!(
            "the FUSE session is still referenced (another mount namespace holds a copy of the mount); exiting anyway"
        );
    }
    // Dropping the session does not join its threads; process exit ends them.
    drop(mounted);
    engine.shutdown().await?;
    // One more keyring attempt even if it failed earlier: lost rotated
    // tokens force a new login.
    crate::credentials::retry_keyring();
    if let (Some(tokens), Some(logged_in)) = (&mut tokens, &logged_in)
        && let Err(error) = tokens.save(paths, &logged_in.session).await
    {
        tracing::warn!(error = %format!("{error:#}"), "cannot store session tokens");
    }
    let _ = std::fs::remove_file(&pid_file);
    tracing::info!("stopped");
    Ok(ExitCode::SUCCESS)
}

fn request_handler(
    engine: Arc<Engine>,
    mountpoint: PathBuf,
    username: String,
    credential_storage: String,
    unmount_tx: tokio::sync::mpsc::Sender<()>,
) -> control::Handler {
    Arc::new(move |request| {
        let engine = Arc::clone(&engine);
        let mountpoint = mountpoint.clone();
        let username = username.clone();
        let credential_storage = credential_storage.clone();
        let unmount_tx = unmount_tx.clone();
        Box::pin(async move {
            match request {
                Request::Ping => json!({ "ok": true, "pid": std::process::id() }),
                Request::Status => {
                    let (cache_bytes, cache_blocks) = engine.cache_usage();
                    let storage = engine.storage();
                    json!({
                        "ok": true,
                        "pid": std::process::id(),
                        "mountpoint": mountpoint,
                        "read_only": false,
                        "queue_pending": engine.queue_counts().pending,
                        "queue_parked": engine.queue_counts().parked,
                        "username": username,
                        "credentials": credential_storage,
                        "quota_used": storage.map(|s| s.used),
                        "quota_max": storage.map(|s| s.max),
                        "inodes": engine.inode_count(),
                        "cache_bytes": cache_bytes,
                        "cache_blocks": cache_blocks,
                    })
                }
                Request::Drain { timeout_secs } => {
                    let counts = engine.drain(Duration::from_secs(timeout_secs)).await;
                    json!({
                        "ok": true,
                        "pending": counts.pending,
                        "parked": counts.parked,
                        "parked_items": engine.parked(),
                    })
                }
                Request::AccessSet { path, mode } => {
                    match engine
                        .access_set(&path, mode, pdmount_core::access::RuleSource::Cli)
                        .await
                    {
                        Ok(pending) => json!({ "ok": true, "pending": pending }),
                        Err(error) => json!({ "ok": false, "error": error.to_string() }),
                    }
                }
                Request::AccessUnset { path } => match engine.access_unset(&path).await {
                    Ok(true) => json!({ "ok": true }),
                    Ok(false) => json!({ "ok": false, "error": format!("no rule on {path}") }),
                    Err(error) => json!({ "ok": false, "error": error.to_string() }),
                },
                Request::AccessList { path } => match engine.access_list(path.as_deref()).await {
                    Ok((rules, default)) => json!({
                        "ok": true,
                        "default": pdmount_core::access::mode_str(default),
                        "rules": rules
                            .iter()
                            .map(|rule| json!({
                                "mode": pdmount_core::access::mode_str(rule.mode),
                                "path": rule.path,
                                "set_as": rule.path_at_set,
                                "source": match rule.source {
                                    pdmount_core::access::RuleSource::Cli => "cli",
                                    pdmount_core::access::RuleSource::Config => "config",
                                },
                            }))
                            .collect::<Vec<_>>(),
                    }),
                    Err(error) => json!({ "ok": false, "error": error.to_string() }),
                },
                Request::AccessEffective { path } => match engine.access_effective(&path).await {
                    Ok((mode, rule)) => json!({
                        "ok": true,
                        "mode": pdmount_core::access::mode_str(mode),
                        "rule": rule,
                    }),
                    Err(error) => json!({ "ok": false, "error": error.to_string() }),
                },
                Request::Stats => {
                    let counters: serde_json::Map<String, Value> = engine
                        .metrics()
                        .snapshot()
                        .into_iter()
                        .map(|(name, value)| (name.to_owned(), json!(value)))
                        .collect();
                    json!({ "ok": true, "counters": counters })
                }
                Request::Unmount => {
                    let result =
                        tokio::task::spawn_blocking(move || fusermount(&mountpoint, false)).await;
                    match result {
                        Ok(Ok(())) => {
                            let _ = unmount_tx.try_send(());
                            json!({ "ok": true })
                        }
                        Ok(Err(error)) => json!({ "ok": false, "error": format!("{error:#}") }),
                        Err(error) => json!({ "ok": false, "error": error.to_string() }),
                    }
                }
            }
        })
    })
}

/// Runs `fusermount3 -u` (or `-uz`, lazy) on the mountpoint.
fn fusermount(mountpoint: &Path, lazy: bool) -> anyhow::Result<()> {
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

/// `pdmount unmount [--force]`.
pub async fn unmount(
    paths: &ProfilePaths,
    mountpoint: &Path,
    force: bool,
) -> anyhow::Result<ExitCode> {
    let socket = paths.control_socket();
    if force {
        return force_unmount(paths, mountpoint).await;
    }
    let was_mounted = mounts::find(mountpoint)?.is_some_and(|entry| entry.is_pdmount());
    drain(&socket).await?;
    match control::call(&socket, Request::Unmount, CONTROL_TIMEOUT).await? {
        Some(response) if response["ok"] == true => {
            wait_for_daemon_exit(paths, Duration::from_secs(60)).await?;
            println!("Unmounted {}.", mountpoint.display());
            Ok(ExitCode::SUCCESS)
        }
        Some(response) => {
            let error = response["error"].as_str().unwrap_or("unknown error");
            eprintln!("error: cannot unmount {}: {error}", mountpoint.display());
            eprintln!("Close the files open under it, or use `pdmount unmount --force`.");
            Ok(ExitCode::FAILURE)
        }
        None => match mounts::find(mountpoint)? {
            Some(entry) if entry.is_pdmount() => {
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
async fn drain(socket: &Path) -> anyhow::Result<()> {
    let request = Request::Drain {
        timeout_secs: DRAIN_TIMEOUT.as_secs(),
    };
    let Some(reply) = control::call(socket, request, DRAIN_TIMEOUT + CONTROL_TIMEOUT).await? else {
        return Ok(());
    };
    let pending = reply["pending"].as_u64().unwrap_or(0);
    if pending > 0 {
        eprintln!(
            "warning: {pending} changes are still uploading after {} s; they continue at the next mount",
            DRAIN_TIMEOUT.as_secs()
        );
    }
    if let Some(parked) = reply["parked_items"].as_array().filter(|p| !p.is_empty()) {
        eprintln!(
            "warning: {} changes could not be uploaded; they are retried at the next mount:",
            parked.len()
        );
        for item in parked {
            eprintln!(
                "  {}: {}",
                item[0].as_str().unwrap_or("?"),
                item[1].as_str().unwrap_or("?")
            );
        }
    }
    Ok(())
}

/// Waits until the daemon process is gone, not just its socket: it closes
/// the socket first and may need a few seconds to finish.
async fn wait_for_daemon_exit(paths: &ProfilePaths, timeout: Duration) -> anyhow::Result<()> {
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

/// `unmount --force` (§14.4): lazy unmount, stop the daemon (TERM, then
/// KILL), and abort the FUSE connection if it is still there. Never needs
/// a reboot.
async fn force_unmount(paths: &ProfilePaths, mountpoint: &Path) -> anyhow::Result<ExitCode> {
    use nix::sys::signal::{Signal, kill};

    let entry = mounts::find(mountpoint)?;
    if entry.is_some() {
        match fusermount(mountpoint, true) {
            Ok(()) => println!("Detached {}.", mountpoint.display()),
            Err(error) => eprintln!("warning: fusermount3 -uz failed: {error:#}"),
        }
    }
    if let Some(pid) = daemon_pid(paths) {
        let _ = kill(pid, Signal::SIGTERM);
        let deadline = Instant::now() + SESSION_END_TIMEOUT;
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
fn is_pdmount_process(pid: nix::unistd::Pid) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|cmdline| {
            let first = cmdline.split(|b| *b == 0).next().unwrap_or_default();
            Path::new(std::str::from_utf8(first).unwrap_or_default())
                .file_name()
                .is_some_and(|name| name == "pdmount")
        })
        .unwrap_or(false)
}

/// `pdmount stats`.
pub async fn stats(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let Some(response) =
        control::call(&paths.control_socket(), Request::Stats, CONTROL_TIMEOUT).await?
    else {
        eprintln!("Profile {} is not mounted.", paths.profile);
        return Ok(ExitCode::FAILURE);
    };
    if let Some(counters) = response["counters"].as_object() {
        let width = counters.keys().map(String::len).max().unwrap_or(0);
        for (name, value) in counters {
            println!("{name:width$}  {value}");
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `pdmount reset --keep-login`: deletes the state database and the block
/// cache. The login, SDK caches and device identity stay.
pub async fn reset(paths: &ProfilePaths, mountpoint: &Path) -> anyhow::Result<ExitCode> {
    if control::call(&paths.control_socket(), Request::Ping, CONTROL_TIMEOUT)
        .await?
        .is_some()
        || mounts::find(mountpoint)?.is_some_and(|entry| entry.is_pdmount())
    {
        bail!(
            "profile {} is mounted; run `pdmount unmount` first",
            paths.profile
        );
    }
    let pending =
        match pdmount_core::engine::pending_changes(&paths.state_db(), &paths.staging_dir()) {
            Ok(pending) => pending,
            // An unreadable state is what a reset repairs; staging files still
            // hold content that would be lost.
            Err(error) => {
                eprintln!("warning: cannot read the local state ({error}); resetting it");
                pdmount_core::staging::StagingDir::open(&paths.staging_dir())
                    .and_then(|staging| staging.staged_inos())
                    .map_or(0, |staged| staged.len() as u64)
            }
        };
    if pending > 0 {
        bail!(
            "profile {} has {pending} local changes that are not uploaded yet; mount it and run \
             `pdmount unmount` to upload them first",
            paths.profile
        );
    }
    let mut removed = Vec::new();
    let staging = paths.staging_dir();
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("cannot remove {}", staging.display()))?;
    }
    for suffix in ["", "-wal", "-shm"] {
        let mut path = paths.state_db().into_os_string();
        path.push(suffix);
        let path = PathBuf::from(path);
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            removed.push(path);
        }
    }
    let blocks = paths.blocks_dir();
    let mut cache_size = 0;
    if blocks.exists() {
        cache_size = dir_size(&blocks);
        std::fs::remove_dir_all(&blocks)
            .with_context(|| format!("cannot remove {}", blocks.display()))?;
        removed.push(blocks);
    }
    if removed.is_empty() {
        println!("Nothing to reset for profile {}.", paths.profile);
    } else {
        for path in &removed {
            println!("removed {}", path.display());
        }
        println!(
            "Profile {} reset ({} of cached blocks freed); the login is kept.",
            paths.profile,
            ByteSize(cache_size).human()
        );
    }
    Ok(ExitCode::SUCCESS)
}

fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
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

    #[test]
    fn the_test_runner_is_not_taken_for_a_daemon() {
        assert!(!is_pdmount_process(nix::unistd::Pid::this()));
    }

    #[tokio::test]
    async fn reset_removes_state_and_cache_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = pdmount_core::paths::BaseDirs {
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            state: dir.path().join("state"),
            runtime: None,
        };
        let paths = ProfilePaths::new(pdmount_core::Profile::default(), &base);
        std::fs::create_dir_all(paths.blocks_dir().join("ab")).expect("mkdir");
        std::fs::write(paths.blocks_dir().join("ab/x"), b"1234").expect("write");
        std::fs::create_dir_all(&paths.data_dir).expect("mkdir");
        std::fs::write(paths.state_db(), b"db").expect("write");
        std::fs::write(paths.credentials_file(), b"{}").expect("write");
        reset(&paths, &dir.path().join("mnt")).await.expect("reset");
        assert!(!paths.state_db().exists());
        assert!(!paths.blocks_dir().exists());
        assert!(paths.credentials_file().exists());
    }

    #[tokio::test]
    async fn reset_refuses_while_changes_are_not_uploaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = pdmount_core::paths::BaseDirs {
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            state: dir.path().join("state"),
            runtime: None,
        };
        let paths = ProfilePaths::new(pdmount_core::Profile::default(), &base);
        let staging =
            pdmount_core::staging::StagingDir::open(&paths.staging_dir()).expect("staging");
        staging.create(7, None, None, 3).expect("staged file");
        let error = reset(&paths, &dir.path().join("mnt"))
            .await
            .expect_err("refused");
        assert!(format!("{error:#}").contains("not uploaded yet"));
        assert!(staging.staged_inos().expect("list").contains(&7));
    }
}
