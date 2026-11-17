//! Opening an engine: [`Engine::open`], the first-run initialization and loading the stored tree.

use super::*;

/// Local changes of a profile that are not uploaded yet, from its stored
/// state (for `reset --keep-login`, which must not lose them). Staging
/// files count too.
pub fn pending_changes(state_db: &std::path::Path, staging_dir: &std::path::Path) -> Result<u64> {
    let mut pending = 0;
    if state_db.exists() {
        let store = Store::open(state_db)?;
        pending += store.read(crate::rows::count_pending)?;
    }
    if let Ok(staging) = StagingDir::open(staging_dir) {
        pending += staging.staged_inos()?.len() as u64;
    }
    Ok(pending)
}

/// First mount: persist the event cursor before listing anything (§9.1),
/// then the root.
pub(super) async fn initialize(
    store: &Store,
    backend: &dyn Backend,
    metrics: &Metrics,
) -> Result<()> {
    check_not_hot(metrics, "latest_event_id");
    let cursor = backend.latest_event_id().await?;
    check_not_hot(metrics, "root");
    let root = backend.root().await?;
    let tree = Tree::with_root(RemoteState::from_remote(&root));
    let row = tree.row(ROOT_INO).expect("root");
    store
        .write_durable(move |conn| {
            crate::events::write_cursor(conn, &cursor)?;
            write_rows(conn, &[(ROOT_INO, Some(row.clone()))])?;
            meta_set(conn, "ino_reserved", &ROOT_INO.to_string())?;
            meta_set(
                conn,
                "root_link",
                &row.remote.as_ref().expect("root is remote").link.0,
            )
        })
        .await?;
    Ok(())
}

/// Loads the stored tree, the inode reservation and the staged inodes.
pub(super) fn load(store: &Store, staging: &StagingDir) -> Result<(Tree, Ino, StagedMap)> {
    let (rows, reserved) = store.read(|conn| {
        let reserved: Ino = meta_get(conn, "ino_reserved")?
            .and_then(|value| value.parse().ok())
            .unwrap_or(ROOT_INO);
        Ok((load_rows(conn)?, reserved))
    })?;
    if !rows.iter().any(|row| row.ino == ROOT_INO) {
        return Err(CoreError::Store(
            "the state database has no root; run `pdmount reset --keep-login`".into(),
        ));
    }
    // Never hand out a number at or below one already in use.
    let reserved = rows
        .iter()
        .map(|row| row.ino)
        .max()
        .unwrap_or(ROOT_INO)
        .max(reserved);
    let (mut tree, stale) = Tree::from_rows(rows);
    if !stale.is_empty() {
        tracing::info!(count = stale.len(), "dropping unreachable stored nodes");
        // `write_rows` also drops the `remote` rows nobody refers to then.
        let rows: Vec<(Ino, Option<NodeRow>)> = stale.iter().map(|&ino| (ino, None)).collect();
        store.write(move |conn| write_rows(conn, &rows));
    }
    // Staging files are the content of changed files. Staging without a
    // node is left over from a node that is gone, and staging of a node
    // stored as clean remote content from an upload whose staging removal
    // had not run yet (§7.1): the remote revision may be newer by now.
    let mut staged = HashMap::new();
    for ino in staging.staged_inos()? {
        let keep = tree.get(ino).is_some_and(|node| {
            node.attached
                && node.kind == NodeKind::File
                // Rows do not store `Content::Staged`; the generations tell.
                && !(matches!(node.content, Content::Remote { .. })
                    && node.gens.content == node.gens.uploaded_content)
        });
        if !keep {
            staging.remove(ino);
            continue;
        }
        match staging.reopen(ino) {
            Ok(file) => {
                if let Some(node) = tree.get_mut(ino) {
                    node.content = Content::Staged;
                    node.size = file.size();
                    if node.gens.content == node.gens.uploaded_content && node.remote.is_none() {
                        node.gens.content += 1;
                    }
                }
                staged.insert(ino, Arc::new(StagingLock::new(Rank::Staging, file)));
            }
            Err(error) => {
                tracing::error!(ino, %error, "cannot reopen a staging file; its local changes are lost");
                staging.remove(ino);
            }
        }
    }
    // A new file whose staging is gone (it was empty) starts empty again.
    let missing: Vec<Ino> = tree
        .inos()
        .filter(|ino| {
            tree.get(*ino).is_some_and(|node| {
                node.attached && node.kind == NodeKind::File && node.content == Content::Staged
            }) && !staged.contains_key(ino)
        })
        .collect();
    for ino in missing {
        let file = staging.create(ino, None, None, 0)?;
        if let Some(node) = tree.get_mut(ino) {
            node.size = 0;
        }
        staged.insert(ino, Arc::new(StagingLock::new(Rank::Staging, file)));
    }
    Ok((tree, reserved, staged))
}

impl Engine {
    /// Opens the store, staging and cache. On the first mount of a profile
    /// it records the event cursor and the root (network); otherwise it uses
    /// only the stored state. Must run inside a tokio runtime.
    pub async fn open(
        backend: Arc<dyn Backend>,
        options: EngineOptions,
    ) -> Result<(Arc<Self>, mpsc::UnboundedReceiver<Invalidation>)> {
        let owns_health = options.health.is_none();
        let health = options.health.clone().unwrap_or_default();
        let metrics = Arc::new(Metrics::default());
        // Timed inside the monitor: calls the monitor refuses are not timed.
        let timed = Arc::new(TimedBackend::new(backend, Arc::clone(&metrics)));
        let monitored = Arc::new(Monitored::new(timed, Arc::clone(&health)));
        let backend: Arc<dyn Backend> = monitored.clone();
        let store = Arc::new(Store::open(&options.state_db)?);
        let account = backend.account().await?;
        let stored_root = store.meta_get("root_link")?;
        match stored_root {
            Some(root) if root != account.root.0 => {
                return Err(CoreError::Store(format!(
                    "{} belongs to another account or drive; run `pdmount reset --keep-login`",
                    options.state_db.display()
                )));
            }
            Some(_) => {}
            None => initialize(&store, backend.as_ref(), &metrics).await?,
        }
        let staging = StagingDir::open(&options.staging_dir)?;
        let (mut tree, reserved, staged) = load(&store, &staging)?;
        tree.set_namer(Namer {
            device: options.device.name.clone(),
            clock: Arc::clone(&options.clock),
        });
        let cursor = crate::events::load_cursor(&store)?;
        let pins = pins::Pins::load(&store)?;
        let markers = marker::Markers::load(&store)?;
        let access = store.read(|conn| Rules::load(conn, options.config.access.default))?;
        let conflicts = store.read(Conflicts::load)?;
        let scan = options.scan.clone().unwrap_or_else(|| {
            let cap = options
                .cache
                .shared
                .as_ref()
                .map_or(options.cache.disk_bytes, |shared| shared.budget.cap());
            ScanGuard::new(&options.config.scan, cap)
        });
        let limits = options
            .limits
            .clone()
            .unwrap_or_else(|| SessionLimits::new(&options.config));
        let mut cache_options = options.cache;
        cache_options.downloads = Some(Arc::clone(&limits.downloads));
        let cache = Arc::new(BlockCache::open(
            Arc::clone(&backend),
            Arc::clone(&store),
            Arc::clone(&metrics),
            cache_options,
            staging.protections().clone(),
        )?);
        let seat = {
            // Weak: the guard never keeps a closed tree's cache.
            let cache = Arc::downgrade(&cache);
            scan.join(Arc::new(move |key: &BlockKey| {
                if let Some(cache) = cache.upgrade() {
                    cache.evict_scan_block(key);
                }
            }))
        };
        let scan_tree = seat.id();
        cache.set_scan_seat(seat);
        let (sender, receiver) = mpsc::unbounded_channel();
        let journal = Journal::with_store(
            &options.config.upload,
            Arc::clone(&store),
            Arc::clone(&limits.uploads),
        );
        // The queue times of the last session, for the ages `status` shows (D105).
        let queued: Vec<(Ino, i64)> = store.read(|conn| {
            let mut select = conn.prepare("SELECT ino, queued_at_ms FROM queued")?;
            select
                .query_map([], |row| Ok((row.get::<_, i64>(0)? as Ino, row.get(1)?)))?
                .collect()
        })?;
        journal.restore(&queued, crate::journal::unix_ms(SystemTime::now()));
        let fsync_remote = options
            .config
            .fsync
            .remote_matcher()
            .map_err(|error| CoreError::Internal(format!("fsync.remote_paths: {error}")))?;
        let local_ignore = options
            .local_ignore
            .as_ref()
            .and_then(|path| std::fs::read_to_string(path).ok())
            .unwrap_or_default();
        let engine = Arc::new(Self {
            backend,
            monitored,
            health,
            store,
            metrics,
            cache,
            config: options.config,
            device: options.device,
            clock: options.clock,
            tree: lock_order::RwLock::new(Rank::Tree, tree),
            inos: parking_lot::Mutex::new(InoAllocator {
                next: reserved + 1,
                reserved,
            }),
            reserving: tokio::sync::Mutex::new(()),
            listings: SingleFlight::default(),
            listing_epoch: AtomicU64::new(0),
            invalidations: sender,
            dirs: parking_lot::Mutex::default(),
            files: parking_lot::Mutex::default(),
            next_handle: AtomicU64::new(1),
            staging,
            staged: Arc::new(lock_order::Mutex::new(Rank::StagedMap, staged)),
            staging_removals: Arc::default(),
            journal,
            storage: parking_lot::Mutex::default(),
            remote_clock: AtomicU64::new(0),
            pending_bytes: parking_lot::Mutex::default(),
            prefetched_dirs: parking_lot::Mutex::default(),
            ignore: lock_order::RwLock::new(
                Rank::Leaf,
                Arc::new(IgnoreMatcher::new([local_ignore.as_str()])),
            ),
            local_ignore,
            remote_ignore: options.remote_ignore,
            ignore_changed: tokio::sync::Notify::new(),
            access: lock_order::RwLock::new(Rank::Access, access),
            conflicts: lock_order::Mutex::new(Rank::Leaf, conflicts),
            notices: tokio::sync::broadcast::channel(64).0,
            gate: Arc::clone(&limits.gate),
            limits,
            scan,
            scan_tree,
            pins,
            markers,
            event_cursor: tokio::sync::Mutex::new(cursor),
            refresh: parking_lot::Mutex::default(),
            last_poll: parking_lot::Mutex::default(),
            read_only: options.read_only,
            write_gate: std::sync::atomic::AtomicBool::new(options.write_gate),
            owns_health,
            fsync_remote,
            background: parking_lot::Mutex::default(),
            rights: rights::Rights::default(),
            shutdown_wait_ended: std::sync::atomic::AtomicBool::new(false),
            retired: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            runtime: tokio::runtime::Handle::current(),
        });
        engine.queue_pending_work();
        engine.journal.finish_restore();
        Ok((engine, receiver))
    }
}
