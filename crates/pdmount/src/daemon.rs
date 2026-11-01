//! Mount daemon and mount management: `mount`, `unmount`, `reset`
//! (DESIGN.md §14).
//!
//! `pdmount mount` starts `pdmount mount --foreground` as a detached child in
//! its own process group, with output appended to the profile's log file,
//! and returns once the child's control socket answers. The foreground
//! process runs the engine, the FUSE session and the control socket until
//! SIGTERM/SIGINT, an `unmount` request, or an external unmount.

use crate::auth::Auth;
use crate::control::{self, Request};
use crate::credentials::Credentials;
use crate::mounts;
use crate::session::{self, LoggedIn, Resumed};
use anyhow::{Context, bail};
use pdmount_backend::Backend;
use pdmount_backend::fake::{FakeBackend, FakeClient};
use pdmount_backend::proton::ProtonBackend;
use pdmount_core::cache::CacheOptions;
use pdmount_core::config::BrowserMode;
use pdmount_core::conflicts::system_clock;
use pdmount_core::device::DeviceIdentity;
use pdmount_core::engine::{Engine, EngineOptions};
use pdmount_core::health::Connectivity;
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use serde_json::Value;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::signal::unix::{SignalKind, signal};

const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
/// The daemon waits up to 30 s for the sign-in link.
const LOGIN_LINK_TIMEOUT: Duration = Duration::from_secs(40);
const LOGIN_POLL: Duration = Duration::from_secs(5);
/// First mounts list the root and may refresh the session.
const START_TIMEOUT: Duration = Duration::from_secs(90);
const TOKEN_SAVE_INTERVAL: Duration = Duration::from_secs(60);
const SESSION_END_TIMEOUT: Duration = Duration::from_secs(10);
/// `unmount --force` sends SIGKILL this long after `timeouts.shutdown_grace`:
/// longer than the daemon's own wait for the session plus its store commit
/// and last token save.
const FORCE_KILL_MARGIN: Duration = Duration::from_secs(30);
/// A log above this is moved to `<profile>.log.1` at a background `mount`.
const LOG_ROTATE_BYTES: u64 = 16 << 20;
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

/// Opens the daemon log for appending: 0600 in a 0700 directory (DESIGN.md
/// §13.1), also when an older version created them with the umask's modes.
/// Keeps one old log: renames a log above `limit` to `<log>.1`, replacing
/// it. The daemon's output goes to a new file then.
fn rotate_log(path: &Path, limit: u64) -> std::io::Result<()> {
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

fn open_log(path: &Path) -> std::io::Result<std::fs::File> {
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
/// table to it now. Staging keeps up to a quarter of the limit open (D063;
/// the engine reads it after this), and each time the kernel grows the
/// table, every thread that opens a file waits for an RCU grace period
/// (30-80 ms measured, all FUSE threads at once).
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

/// `pdmount mount --foreground`: runs until unmounted. `textfile` is the
/// Prometheus textfile, if one is configured.
pub async fn run_foreground(
    paths: &ProfilePaths,
    config: &Config,
    mountpoint: &Path,
    choice: &BackendChoice,
    textfile: Option<PathBuf>,
    read_only: bool,
) -> anyhow::Result<ExitCode> {
    preflight(paths, mountpoint).await?;
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("cannot create {}", mountpoint.display()))?;
    grow_fd_table();
    let device = session::device_identity(paths, config)?;
    let limits = session::TransferLimits::of(config);
    let opened = open_backend(paths, choice, &device, limits).await?;
    let master_key: [u8; 32] = Credentials::new(paths)
        .cache_master_key()?
        .try_into()
        .map_err(|_| anyhow::anyhow!("the cache key has the wrong length"))?;
    let (engine, invalidations) = Engine::open(
        opened.backend,
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
            device: device.clone(),
            clock: system_clock(),
            read_only,
        },
    )
    .await
    .context("cannot open the local state")?;
    engine.health().set(opened.start_state);
    let mounted = {
        let engine = Arc::clone(&engine);
        let target = mountpoint.to_owned();
        tokio::task::spawn_blocking(move || pdmount_fuse::mount(engine, invalidations, &target))
            .await?
            .with_context(|| format!("cannot mount at {}", mountpoint.display()))?
    };
    engine.start_background();
    let auth = match opened.proton {
        Some((proton, logged_in)) => {
            let auth = Auth::new(
                paths.clone(),
                device.clone(),
                limits,
                proton,
                Arc::clone(engine.health()),
                *logged_in,
            )
            .await;
            auth.start().await;
            Some(auth)
        }
        None => {
            watch_fake_session(Arc::clone(engine.health()));
            None
        }
    };
    let notices = tokio::spawn(announce_conflicts(engine.subscribe_conflicts()));
    let textfile = textfile.map(|path| {
        crate::stats::spawn_textfile(Arc::clone(&engine), path, paths.profile.to_string())
    });
    tracing::info!(mountpoint = %mountpoint.display(), "mounted");

    let pid_file = paths.pid_file();
    let socket = paths.control_socket();
    // The PID file lives next to the socket, whose directory may not exist yet.
    control::create_dir(&socket)?;
    // The mountpoint lets `status` and `unmount` find the mount after a crash.
    std::fs::write(
        &pid_file,
        format!("{}\n{}\n", std::process::id(), mountpoint.display()),
    )
    .with_context(|| format!("cannot write {}", pid_file.display()))?;
    let (unmount_tx, unmount_rx) = tokio::sync::mpsc::channel::<()>(1);
    let handler = request_handler(
        Arc::clone(&engine),
        mountpoint.to_owned(),
        opened.username,
        opened.credential_storage,
        auth.clone(),
        unmount_tx,
    );
    let server = control::serve(&socket, handler)?;

    let reason = wait_for_stop(mountpoint, &mounted, auth.as_ref(), unmount_rx).await?;
    tracing::info!(reason, "shutting down");
    server.abort();
    notices.abort();
    if let Some(textfile) = textfile {
        textfile.abort();
    }
    control::remove(&socket);
    let stopped = shutdown(&engine, mounted, auth.as_ref(), mountpoint).await;
    let _ = std::fs::remove_file(&pid_file);
    stopped?;
    tracing::info!("stopped");
    Ok(ExitCode::SUCCESS)
}

/// The backend a mount runs on.
struct Opened {
    backend: Arc<dyn Backend>,
    /// The Proton backend and its session; `None` on the fake.
    proton: Option<(Arc<ProtonBackend>, Box<LoggedIn>)>,
    username: String,
    /// Where the session is stored.
    credential_storage: String,
    /// The connectivity the mount starts with.
    start_state: Connectivity,
}

async fn open_backend(
    paths: &ProfilePaths,
    choice: &BackendChoice,
    device: &DeviceIdentity,
    limits: session::TransferLimits,
) -> anyhow::Result<Opened> {
    let dir = match choice {
        BackendChoice::Proton => return open_proton(paths, device, limits).await,
        BackendChoice::Fake(dir) => dir,
    };
    let fake = FakeBackend::open(dir)
        .with_context(|| format!("cannot open the fake drive {}", dir.display()))?
        .with_client(FakeClient {
            uid: Some(device.client_uid()),
            ..FakeClient::default()
        });
    Ok(Opened {
        username: fake.account().await?.username,
        backend: Arc::new(fake),
        proton: None,
        credential_storage: "none (fake backend)".into(),
        start_state: Connectivity::Online,
    })
}

/// The stored root of a profile that has mounted before, so that it can come
/// up and wait for the sign-in (D042); for any other profile, an error that
/// asks for a login.
fn known_root_for_login(
    paths: &ProfilePaths,
    error: &anyhow::Error,
) -> anyhow::Result<session::KnownRoot> {
    session::KnownRoot::load(paths)
        .filter(|_| paths.state_db().exists())
        .with_context(|| format!("the session has expired; run `pdmount login` ({error:#})"))
}

/// Resumes the session; without the network, or with a rejected session,
/// a profile mounted online before starts from its stored root.
async fn open_proton(
    paths: &ProfilePaths,
    device: &DeviceIdentity,
    limits: session::TransferLimits,
) -> anyhow::Result<Opened> {
    let (logged_in, known, mut start_state) = match session::resume(paths).await? {
        Resumed::Active(logged_in) => (logged_in, None, Connectivity::Online),
        Resumed::Offline { logged_in, error } => {
            let Some(known) = session::KnownRoot::load(paths) else {
                return Err(error.context(
                    "cannot reach Proton, and this profile has not been mounted online yet",
                ));
            };
            tracing::warn!(error = %format!("{error:#}"), "cannot reach Proton; mounting offline");
            (logged_in, Some(known), Connectivity::Offline)
        }
        Resumed::NotLoggedIn => bail!(
            "profile {} is not logged in; run `pdmount login`",
            paths.profile
        ),
        Resumed::NeedsLogin { logged_in, error } => {
            // A mount that has run before comes up and waits for the
            // sign-in: local work goes on (D042).
            let known = known_root_for_login(paths, &error)?;
            tracing::warn!(error = %format!("{error:#}"), "the session was rejected; mounting until the sign-in");
            (logged_in, Some(known), Connectivity::NeedsLogin)
        }
    };
    let backend = match &known {
        Some(known) => session::proton_backend_offline(&logged_in, device, known, limits)?,
        None => {
            let online = async {
                let backend = session::proton_backend(&logged_in, device, limits).await?;
                let root = backend.account().await?.root;
                anyhow::Ok((backend, root))
            }
            .await;
            match online {
                Ok((backend, root)) => {
                    let known =
                        session::KnownRoot::of(&backend, &logged_in.session.username, &root);
                    if let Err(error) = known.save(paths) {
                        tracing::warn!(%error, "cannot store the drive's root for offline mounts");
                    }
                    backend
                }
                // The session resumed, but its keys cannot be unlocked (D085).
                Err(error) if session::needs_login(&error) => {
                    let known = known_root_for_login(paths, &error)?;
                    tracing::warn!(error = %format!("{error:#}"), "a new login is needed; mounting until the sign-in");
                    start_state = Connectivity::NeedsLogin;
                    session::proton_backend_offline(&logged_in, device, &known, limits)?
                }
                Err(error) => return Err(error),
            }
        }
    };
    let backend = Arc::new(backend);
    Ok(Opened {
        backend: Arc::clone(&backend) as Arc<dyn Backend>,
        username: logged_in.session.username.clone(),
        credential_storage: logged_in.storage.to_string(),
        proton: Some((backend, logged_in)),
        start_state,
    })
}

/// Serves until a signal, an `unmount` request or an external unmount;
/// returns which. Saves the tokens meanwhile.
async fn wait_for_stop(
    mountpoint: &Path,
    mounted: &pdmount_fuse::Mounted,
    auth: Option<&Arc<Auth>>,
    mut unmount_rx: tokio::sync::mpsc::Receiver<()>,
) -> anyhow::Result<&'static str> {
    let mut terminate = signal(SignalKind::terminate())?;
    let mut interrupt = signal(SignalKind::interrupt())?;
    let mut hangup = signal(SignalKind::hangup())?;
    // Rotations are stored as they happen; the timer is a fallback.
    let mut token_timer = tokio::time::interval(TOKEN_SAVE_INTERVAL);
    let mut table_unreadable = false;
    Ok(loop {
        tokio::select! {
            _ = terminate.recv() => break "SIGTERM",
            _ = interrupt.recv() => break "SIGINT",
            _ = hangup.recv() => tracing::info!("ignoring SIGHUP"),
            // `fusermount3 -u` succeeded in the handler.
            _ = unmount_rx.recv() => break "unmount request",
            _ = token_timer.tick() => {
                if let Some(auth) = auth {
                    auth.spawn_save();
                }
            }
            _ = tokio::time::sleep(Duration::from_millis(500)) => {
                // A copy of the mount in another mount namespace (e.g. a
                // Flatpak sandbox started while mounted) keeps the session
                // alive after an external unmount, so the mount table decides.
                // A table that cannot be read (EMFILE, say) proves nothing.
                if mounted.is_finished() {
                    break "unmounted";
                }
                match mounts::find(mountpoint) {
                    Ok(Some(_)) => table_unreadable = false,
                    Ok(None) => break "unmounted",
                    Err(error) => {
                        if !table_unreadable {
                            tracing::warn!(%error, "cannot read the mount table; checking again");
                        }
                        table_unreadable = true;
                    }
                }
            }
        }
    })
}

/// Unmounts, waits a bounded time for the FUSE session, commits the store
/// and saves the tokens; the tokens are saved even when the commit fails,
/// whose error is returned.
async fn shutdown(
    engine: &Engine,
    mounted: pdmount_fuse::Mounted,
    auth: Option<&Arc<Auth>>,
    mountpoint: &Path,
) -> pdmount_core::errors::Result<()> {
    engine.stop_starting();
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
    let stopped = engine.shutdown().await;
    if let Err(error) = &stopped {
        tracing::error!(%error, "could not commit the store at shutdown");
    }
    // One more keyring attempt even if it failed earlier: lost rotated
    // tokens force a new login.
    crate::credentials::retry_keyring();
    if let Some(auth) = auth {
        auth.shutdown();
        auth.save_tokens().await;
    }
    stopped
}

/// The notification hook (§10.8): says how to look at each new conflict, in
/// the log and, at most every 30 s, on the desktop ("N new conflicts" when
/// several come in between).
async fn announce_conflicts(
    mut notices: tokio::sync::broadcast::Receiver<pdmount_core::conflicts::ConflictNotice>,
) {
    use tokio::sync::broadcast::error::RecvError;
    let mut desktop = crate::notify::Coalescer::default();
    loop {
        let due = desktop.deadline().map(tokio::time::Instant::from_std);
        let held = async {
            match due {
                Some(due) => tokio::time::sleep_until(due).await,
                None => std::future::pending().await,
            }
        };
        let count = tokio::select! {
            () = held => desktop.flush(Instant::now()),
            notice = notices.recv() => match notice {
                Ok(notice) => {
                    tracing::info!(
                        id = notice.id,
                        kind = notice.kind.as_str(),
                        path = notice.path.as_deref().unwrap_or(""),
                        "conflict recorded; see `pdmount conflicts show {}`",
                        notice.id
                    );
                    desktop.add(Instant::now())
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::info!(missed, "conflicts recorded; see `pdmount conflicts list`");
                    desktop.add(Instant::now())
                }
                Err(RecvError::Closed) => return,
            },
        };
        match count {
            Some(1) => crate::notify::desktop(
                "PdMount: conflict",
                "A conflict was recorded; see `pdmount conflicts list`",
            ),
            Some(count) => crate::notify::desktop(
                "PdMount: conflicts",
                &format!("{count} new conflicts; see `pdmount conflicts list`"),
            ),
            None => {}
        }
    }
}

/// The fake backend has no sign-in, so a `needs_login` that its fault rule
/// caused would never end. This stands in for the user signing in again: it
/// probes while the state lasts, and the first answer after `pdmount fake
/// fault clear` ends it (the engine does the same after a real login).
fn watch_fake_session(health: Arc<pdmount_core::health::Health>) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(2)).await;
            if health.state() == pdmount_core::health::Connectivity::NeedsLogin {
                health.kick();
            }
        }
    });
}

/// The answer to `Request::Login`: the link of a login the daemon runs.
async fn login_reply(auth: Option<&Arc<Auth>>) -> Value {
    let Some(auth) = auth else {
        return control::failed("the fake backend has no login; use `pdmount fake fault clear`");
    };
    control::reply(
        auth.begin_sign_in()
            .await
            .map(|sign_in| control::LoginReply {
                url: sign_in.url,
                code: sign_in.code,
            }),
    )
}

fn request_handler(
    engine: Arc<Engine>,
    mountpoint: PathBuf,
    username: String,
    credential_storage: String,
    auth: Option<Arc<Auth>>,
    unmount_tx: tokio::sync::mpsc::Sender<()>,
) -> control::Handler {
    Arc::new(move |request| {
        let engine = Arc::clone(&engine);
        let mountpoint = mountpoint.clone();
        let username = username.clone();
        let credential_storage = credential_storage.clone();
        let auth = auth.clone();
        let unmount_tx = unmount_tx.clone();
        Box::pin(async move {
            match request {
                Request::Ping => control::ok(control::Pong {
                    pid: std::process::id(),
                }),
                Request::Status => {
                    let (cache_bytes, cache_blocks) = engine.cache_usage();
                    let storage = engine.storage();
                    let queue = engine.queue_counts();
                    let sign_in = auth.as_ref().and_then(|auth| auth.sign_in());
                    let pinned = engine.pin_summary().await.ok().flatten();
                    control::ok(control::StatusReply {
                        pid: std::process::id(),
                        mountpoint,
                        queue_pending: queue.pending as u64,
                        queue_parked: queue.parked as u64,
                        queue_oldest_secs: engine.queue_oldest().map(|age| age.as_secs()),
                        events_last_poll_secs: engine.events_last_poll().map(|age| age.as_secs()),
                        events_refreshing: engine.events_refreshing(),
                        read_only: engine.is_read_only(),
                        conflicts: engine.conflict_open_count() as u64,
                        connectivity: engine.health().state(),
                        connectivity_since: engine
                            .health()
                            .since()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_or(0, |d| d.as_secs()),
                        sign_in_url: sign_in.as_ref().map(|s| s.url.clone()),
                        sign_in_code: sign_in.as_ref().map(|s| s.code.clone()),
                        sign_in_error: auth.as_ref().and_then(|auth| auth.login_error()),
                        username,
                        credentials: credential_storage,
                        quota_used: storage.map(|s| s.used),
                        quota_max: storage.map(|s| s.max),
                        inodes: engine.inode_count() as u64,
                        cache_bytes,
                        cache_blocks: cache_blocks as u64,
                        pinned: pinned.map(|pinned| control::PinnedReply {
                            paths: pinned.paths,
                            fetched_bytes: pinned.fetched_bytes,
                            total_bytes: pinned.total_bytes,
                        }),
                    })
                }
                Request::Login => login_reply(auth.as_ref()).await,
                Request::Drain { timeout_secs } => {
                    let counts = engine.drain(Duration::from_secs(timeout_secs)).await;
                    control::ok(control::DrainReply {
                        pending: counts.pending as u64,
                        parked: counts.parked as u64,
                        parked_items: engine.parked(),
                    })
                }
                Request::AccessSet { path, mode } => control::reply(
                    engine
                        .access_set(&path, mode, pdmount_core::access::RuleSource::Cli)
                        .await
                        .map(|pending| control::AccessSetReply {
                            pending: pending as u64,
                        }),
                ),
                Request::AccessUnset { path } => match engine.access_unset(&path).await {
                    Ok(true) => control::ok(control::Done {}),
                    Ok(false) => control::failed(format!("no rule on {path}")),
                    Err(error) => control::failed(error),
                },
                Request::AccessList { path } => control::reply(
                    engine
                        .access_list(path.as_deref())
                        .await
                        .map(|(rules, default)| control::AccessListReply {
                            default,
                            rules: rules
                                .into_iter()
                                .map(|rule| control::RuleReply {
                                    mode: rule.mode,
                                    path: rule.path,
                                    set_as: rule.path_at_set,
                                    source: rule.source.as_str().to_owned(),
                                })
                                .collect(),
                        }),
                ),
                Request::AccessEffective { path } => control::reply(
                    engine
                        .access_effective(&path)
                        .await
                        .map(|(mode, rule)| control::AccessEffectiveReply { mode, rule }),
                ),
                Request::ConflictsList { path, all } => control::reply(
                    engine
                        .conflicts_list(path.as_deref(), all)
                        .await
                        .map(|views| control::ConflictsReply {
                            conflicts: views.iter().map(control::ConflictReply::from).collect(),
                        }),
                ),
                Request::ConflictsShow { target } => {
                    control::reply(engine.conflict_show(&target).await.map(|view| {
                        control::ConflictOneReply {
                            conflict: (&view).into(),
                        }
                    }))
                }
                Request::ConflictsResolve { target, choice } => {
                    control::reply(engine.conflict_resolve(&target, choice).await.map(|view| {
                        control::ConflictOneReply {
                            conflict: (&view).into(),
                        }
                    }))
                }
                Request::Stats { reset } => control::ok(crate::stats::reply(&engine, reset)),
                Request::PinSet { path } => {
                    control::reply(engine.pin_set(&path).await.map(|()| control::Done {}))
                }
                Request::PinUnset { path } => {
                    control::reply(engine.pin_unset(&path).await.map(|()| control::Done {}))
                }
                Request::PinList => control::reply(engine.pin_list().await.map(|pins| {
                    control::PinListReply {
                        pins: pins
                            .into_iter()
                            .map(|pin| control::PinReply {
                                path: pin.path,
                                set_as: pin.path_at_set,
                                files: pin.files,
                                fetched_bytes: pin.fetched_bytes,
                                total_bytes: pin.total_bytes,
                                state: pin.state.as_str().to_owned(),
                            })
                            .collect(),
                    }
                })),
                Request::QueueList { target } => {
                    control::reply(engine.queue_entries(&target).await.map(|entries| {
                        control::QueueListReply {
                            entries: entries.into_iter().map(Into::into).collect(),
                        }
                    }))
                }
                Request::QueueRetry { target } => {
                    control::reply(engine.queue_retry(&target).await.map(queue_acted))
                }
                Request::QueuePark { target } => {
                    control::reply(engine.queue_park(&target).await.map(queue_acted))
                }
                Request::QueueDrop { target, yes } => {
                    control::reply(engine.queue_drop(&target, yes).await.map(|items| {
                        control::QueueDropReply {
                            items: items
                                .into_iter()
                                .map(|item| control::QueueDropItem {
                                    id: item.id,
                                    path: item.path,
                                    effect: item.effect.to_owned(),
                                    refused: item.refused,
                                })
                                .collect(),
                        }
                    }))
                }
                Request::Sync { path, timeout_secs } => control::reply(
                    engine
                        .sync_path(path.as_deref(), Duration::from_secs(timeout_secs))
                        .await
                        .map(|outcome| control::SyncReply {
                            pending: outcome.pending as u64,
                            parked: outcome.parked,
                        }),
                ),
                Request::CacheStats => {
                    let stats = engine.cache_stats();
                    control::ok(control::CacheStatsReply {
                        bytes: stats.bytes,
                        max_bytes: stats.max_bytes,
                        blocks: stats.blocks,
                        pinned_bytes: stats.pinned_bytes,
                        protected_revisions: stats.protected_revisions,
                        pool: crate::stats::pool_reply(&engine),
                    })
                }
                Request::CacheGc { target } => {
                    let freed = engine.cache_gc(target).await;
                    control::reply(freed.map(|freed| control::CacheFreedReply {
                        blocks: freed.blocks,
                        bytes: freed.bytes,
                        left_bytes: engine.cache_stats().bytes,
                    }))
                }
                Request::CacheWipe { path } => {
                    let freed = engine.cache_wipe(path.as_deref()).await;
                    control::reply(freed.map(|freed| control::CacheFreedReply {
                        blocks: freed.blocks,
                        bytes: freed.bytes,
                        left_bytes: engine.cache_stats().bytes,
                    }))
                }
                Request::Unmount => {
                    let result =
                        tokio::task::spawn_blocking(move || fusermount(&mountpoint, false)).await;
                    match result {
                        Ok(Ok(())) => {
                            let _ = unmount_tx.try_send(());
                            control::ok(control::Done {})
                        }
                        Ok(Err(error)) => control::failed(format!("{error:#}")),
                        Err(error) => control::failed(error),
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

fn queue_acted(items: Vec<pdmount_core::engine::queue::QueueItem>) -> control::QueueActedReply {
    control::QueueActedReply {
        items: items.into_iter().map(|item| (item.id, item.path)).collect(),
    }
}

/// Uploads pending changes before an unmount. Parked ones stay in the
/// local state and are retried at the next mount.
async fn drain(socket: &Path) -> anyhow::Result<()> {
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
/// sign-in and keeps its mount (D042). `None`: no daemon answers.
pub async fn login_via_daemon(
    paths: &ProfilePaths,
    browser: BrowserMode,
) -> anyhow::Result<Option<ExitCode>> {
    let socket = paths.control_socket();
    let Some(reply) =
        control::call_ok::<control::LoginReply>(&socket, Request::Login, LOGIN_LINK_TIMEOUT)
            .await?
    else {
        return Ok(None);
    };
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

/// How long a stopping daemon may take: its grace for running steps and a
/// margin. An unreadable config counts as the default.
fn stop_limit(paths: &ProfilePaths) -> Duration {
    let config = Config::load(&paths.config_file()).unwrap_or_default();
    config.timeouts.shutdown_grace.0 + FORCE_KILL_MARGIN
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

/// `pdmount reset --keep-login`: deletes the state database and the block
/// cache. The login, SDK caches and device identity stay.
pub async fn reset(paths: &ProfilePaths, mountpoint: &Path) -> anyhow::Result<ExitCode> {
    refuse_while_running(paths).await?;
    if mounts::find(mountpoint)?.is_some_and(|entry| entry.is_pdmount()) {
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
    for path in pdmount_core::paths::sqlite_files(&paths.state_db()) {
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

    #[tokio::test]
    async fn the_fake_backend_has_no_login() {
        let reply = login_reply(None).await;
        assert_eq!(reply["ok"], false);
        assert!(
            reply["error"]
                .as_str()
                .is_some_and(|error| error.contains("fake fault clear"))
        );
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
