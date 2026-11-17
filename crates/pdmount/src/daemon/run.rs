//! The foreground daemon: [`run_foreground`] sets up the engine, the FUSE mount and the control socket, then waits for a stop.

use super::*;

/// Raises the soft open-file limit to the hard one and grows the descriptor
/// table to it now. Staging keeps up to a quarter of the limit open (D063;
/// the engine reads it after this), and each time the kernel grows the
/// table, every thread that opens a file waits for an RCU grace period
/// (30-80 ms measured, all FUSE threads at once).
pub(super) fn grow_fd_table() {
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
    supervision: &Supervision,
) -> anyhow::Result<ExitCode> {
    let home = crate::home_dir()?;
    // Every mount of this daemon names its profile in its source (D127).
    pdmount_fuse::set_profile(paths.profile.as_str());
    if supervision.clear_stale {
        clear_stale_mount(paths, mountpoint, &config.root_mountpoints(&home)).await?;
    }
    preflight(paths, mountpoint).await?;
    crate::roots::check_mountpoints(config, &home, mountpoint)?;
    std::fs::create_dir_all(mountpoint)
        .with_context(|| format!("cannot create {}", mountpoint.display()))?;
    grow_fd_table();
    let device = session::device_identity(paths, config)?;
    let limits = session::TransferLimits::of(config);
    // The desktop's bus that the last daemon had (D102 amended): before the
    // keyring is first tried, so its calls and the start's notifications
    // already go to the bus each service runs on.
    keywait::restore_saved_attach(paths).await;
    // Under a service manager the start neither waits for an unlock dialog
    // nor spends long on the keyring: a supervisor's start timeout would
    // restart it into a new dialog (D106).
    let held = match supervision.service {
        Some(_) => keywait::guard_service_start(paths).await,
        None => false,
    };
    let opened = open_with_key(paths, choice, &device, limits).await;
    crate::credentials::limit_keyring_until(None);
    if held {
        crate::credentials::retry_keyring();
    }
    let start = match opened {
        Ok((opened, key)) => Start {
            backend: opened.backend,
            master_key: Some(key),
            state: opened.start_state,
            username: opened.username,
            late: Late::new(opened.credential_storage, None),
            proton: opened.proton,
            fake: opened.fake,
            waiting: None,
        },
        // A keyring that does not answer is waited for by a profile that has
        // mounted before (D106): no exit, so no respawn under a supervisor.
        Err(error) => match keywait::waiting_start(paths, &error) {
            Some((known, reason)) => {
                tracing::warn!(
                    reason,
                    "the keyring is unavailable; mounting from the stored state until it answers \
                     (retried at `pdmount session attach`, `pdmount login` and every 60 s)"
                );
                let deferred = Arc::new(keywait::Deferred::new(known.account()));
                Start {
                    backend: Arc::clone(&deferred) as Arc<dyn Backend>,
                    master_key: None,
                    state: Connectivity::NeedsLogin,
                    username: known.username.clone(),
                    late: Late::new(
                        "keyring unavailable".into(),
                        Some(keywait::Wait::Keyring(reason)),
                    ),
                    proton: None,
                    fake: None,
                    waiting: Some((deferred, known.root)),
                }
            }
            None => return Err(error),
        },
    };
    let late = Arc::clone(&start.late);
    let master_key = start.master_key;
    // Further trees open over the session's backend (M5.10, D110); while
    // the start waits for the keyring, once the wait ends (D110 amended).
    let source = tree_source(
        start.proton.as_ref().map(|(proton, _)| proton),
        start.fake.as_ref(),
    );
    if !config.roots.is_empty() && source.is_none() {
        tracing::warn!("the further roots mount once the keyring answers");
    }
    // From here on no cache key is made in this process: a new one would
    // delete the caches under the mount (D106).
    crate::credentials::caches_open();
    // One disk cap for My Files and every further tree (D110 amended).
    let budget = DiskBudget::new(config.cache.max_bytes.0);
    let (engine, invalidations) = Engine::open(
        start.backend,
        EngineOptions {
            state_db: paths.state_db(),
            staging_dir: paths.staging_dir(),
            local_ignore: Some(paths.local_ignore_file()),
            remote_ignore: true,
            cache: CacheOptions {
                dir: paths.blocks_dir(),
                master_key: start.master_key,
                ram_bytes: config.cache.ram_bytes.0,
                disk_bytes: config.cache.max_bytes.0,
                shared: Some(SharedCap {
                    budget: Arc::clone(&budget),
                    keeps_half: true,
                }),
                downloads: None,
            },
            config: config.clone(),
            device: device.clone(),
            clock: system_clock(),
            read_only,
            write_gate: true,
            health: None,
            // My Files makes the session's bulk-read guard (D111) and
            // transfer limits (D033, D110).
            scan: None,
            limits: None,
        },
    )
    .await
    .context("cannot open the local state")?;
    engine.health().set(start.state);
    // Before the background work starts: what the last session left (M5.2).
    let left = crate::resume::Left::of(&engine);
    let mounted = {
        let engine = Arc::clone(&engine);
        let target = mountpoint.to_owned();
        tokio::task::spawn_blocking(move || pdmount_fuse::mount(engine, invalidations, &target))
            .await?
            .with_context(|| format!("cannot mount at {}", mountpoint.display()))?
    };
    // With further roots, every volume has one poller, My Files' included
    // (D110); without, My Files polls by itself as it always did. After a
    // keyring wait My Files keeps its own event task, and the further trees
    // get the pollers of their volumes when the wait ends.
    let pollers = Arc::new(pdmount_core::poller::Pollers::new());
    let roots = (!config.roots.is_empty()).then(|| {
        crate::roots::Roots::new(
            crate::roots::RootsContext {
                paths: paths.clone(),
                config: config.clone(),
                device: device.clone(),
                health: Arc::clone(engine.health()),
                scan: Arc::clone(engine.scan_guard()),
                limits: Arc::clone(engine.limits()),
                budget,
                read_only,
                home: home.clone(),
            },
            Arc::clone(&pollers),
        )
    });
    match (&roots, source) {
        (Some(roots), Some((source, volume))) => {
            engine.start_background_polled();
            pollers.add(&volume, true, Arc::clone(&engine));
            roots.set_session(crate::roots::Session { source, master_key });
        }
        _ => engine.start_background(),
    }
    let resumed = tokio::spawn(crate::resume::announce(Arc::clone(&engine), left));
    match start.proton {
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
            late.set_auth(auth);
        }
        // The wait starts it once the keyring answers.
        None if start.waiting.is_some() => {}
        None => watch_fake_session(Arc::clone(engine.health())),
    }
    let notices = tokio::spawn(announce_conflicts(engine.subscribe_conflicts()));
    let incoming = tokio::spawn(crate::incoming::announce(engine.subscribe_incoming()));
    let textfile = textfile.map(|path| {
        crate::stats::spawn_textfile(Arc::clone(&engine), path, paths.profile.to_string())
    });
    tracing::info!(mountpoint = %mountpoint.display(), "mounted");

    let pid_file = paths.pid_file();
    let socket = paths.control_socket();
    // The PID file lives next to the socket, whose directory may not exist yet.
    control::create_dir(&socket)?;
    // The mountpoint lets `status` and `unmount` find the mount after a crash.
    // A third line says that a service manager runs the daemon (D093).
    std::fs::write(
        &pid_file,
        format!(
            "{}\n{}\n{}",
            std::process::id(),
            mountpoint.display(),
            supervise::pid_file_line(supervision.service).unwrap_or_default()
        ),
    )
    .with_context(|| format!("cannot write {}", pid_file.display()))?;
    let (unmount_tx, unmount_rx) = tokio::sync::mpsc::channel::<()>(1);
    let handler = request_handler(
        Arc::clone(&engine),
        mountpoint.to_owned(),
        start.username,
        Arc::clone(&late),
        unmount_tx,
        roots.clone(),
    );
    let server = control::serve(&socket, handler)?;
    // An attach saved by `session attach` while this daemon started, too
    // late for the read above and too early for the socket (D102 amended).
    keywait::restore_late_attach(&late).await;
    let waiter = start.waiting.map(|(deferred, root)| {
        tokio::spawn(
            keywait::Waiter {
                paths: paths.clone(),
                choice: choice.clone(),
                device: device.clone(),
                limits,
                engine: Arc::clone(&engine),
                deferred,
                late: Arc::clone(&late),
                root,
                roots: roots.clone(),
            }
            .run(),
        )
    });
    let mut flusher = crate::flush::Flusher::new(
        Arc::clone(&engine),
        mountpoint.to_owned(),
        Some(crate::logind::Inhibitor::start()),
    );
    let mut signals = StopSignals::new()?;
    supervision.ready().await;
    // The further roots mount after the readiness report, in the
    // background: a slow or failing one shows in `status` and the log, and
    // never delays or fails the start (D109).
    if let Some(roots) = &roots {
        roots.start();
    }

    let reason = wait_for_stop(
        mountpoint,
        &mounted,
        &late,
        unmount_rx,
        &mut flusher,
        &mut signals,
    )
    .await?;
    if let Some(waiter) = waiter {
        waiter.abort();
    }
    let deadline = flusher.deadline();
    tracing::info!(reason, "shutting down");
    // A signal during the stop ends its waits, as a second one during the
    // flush does (D092).
    let hurried = Arc::new(std::sync::OnceLock::new());
    if flusher.hurried() {
        let _ = hurried.set(Instant::now());
        engine.end_shutdown_wait();
        if let Some(roots) = &roots {
            roots.hurry();
        }
    }
    let hurry = tokio::spawn(hurry_on_signal(
        signals,
        Arc::clone(&engine),
        Arc::clone(&hurried),
        roots.clone(),
    ));
    server.abort();
    notices.abort();
    resumed.abort();
    incoming.abort();
    if let Some(textfile) = textfile {
        textfile.abort();
    }
    control::remove(&socket);
    if let Some(roots) = &roots {
        roots.shutdown(deadline).await;
    }
    pollers.stop();
    let stopped = shutdown(
        &engine,
        mounted,
        late.auth(),
        mountpoint,
        deadline,
        &hurried,
    )
    .await;
    hurry.abort();
    flusher.end();
    crate::flush::log_left(&engine);
    let _ = std::fs::remove_file(&pid_file);
    stopped?;
    tracing::info!("stopped");
    Ok(ExitCode::SUCCESS)
}

/// How the mount starts: with its backend and cache key, or waiting for the
/// keyring (D106).
pub(super) struct Start {
    pub(super) backend: Arc<dyn Backend>,
    pub(super) master_key: Option<[u8; 32]>,
    pub(super) state: Connectivity,
    pub(super) username: String,
    pub(super) late: Arc<Late>,
    pub(super) proton: Option<(Arc<ProtonBackend>, Box<LoggedIn>)>,
    /// The fake drive, for further trees on it (test builds).
    pub(super) fake: Option<FakeBackend>,
    /// While the keyring does not answer: the stand-in backend and the root
    /// the engine opens with.
    pub(super) waiting: Option<(Arc<keywait::Deferred>, String)>,
}
