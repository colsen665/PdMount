//! The control request handler of the foreground daemon.

use super::*;

/// Whether `Request::Login` starts a sign-in once the daemon is past its
/// keyring wait (D108): only for a rejected session, or when forced. Online
/// and offline both count as signed in.
pub(super) fn sign_in_wanted(state: Connectivity, force: bool) -> bool {
    force || state == Connectivity::NeedsLogin
}

/// The answer to `Request::Login`: the link of a login the daemon runs. A
/// daemon waiting for the keyring reads it first (D106); then, unless
/// `force`, a session that is not rejected needs no sign-in (D108).
pub(super) async fn login_reply(
    health: &pdmount_core::health::Health,
    late: &Late,
    username: &str,
    force: bool,
) -> Value {
    if late.wait().is_some() {
        crate::credentials::retry_keyring();
        match late.retry_and_wait(login_attempt_wait()).await {
            keywait::Attempt::Running => {
                return control::failed(
                    "the daemon is still trying to finish its start (reading the keyring, then \
                     reaching Proton); see `pdmount status`",
                );
            }
            keywait::Attempt::Done(Some(wait)) => return control::failed(wait.login_advice()),
            keywait::Attempt::Done(None) => {}
        }
    }
    if !sign_in_wanted(health.state(), force) {
        return control::ok(control::LoginReply {
            url: String::new(),
            code: String::new(),
            not_needed: true,
            account: username.to_owned(),
        });
    }
    let Some(auth) = late.auth() else {
        return control::failed("the fake backend has no login; use `pdmount fake fault clear`");
    };
    control::reply(
        auth.begin_sign_in()
            .await
            .map(|sign_in| control::LoginReply {
                url: sign_in.url,
                code: sign_in.code,
                not_needed: false,
                account: String::new(),
            }),
    )
}

pub(super) fn request_handler(
    engine: Arc<Engine>,
    mountpoint: PathBuf,
    username: String,
    late: Arc<Late>,
    unmount_tx: tokio::sync::mpsc::Sender<()>,
    roots: Option<Arc<crate::roots::Roots>>,
) -> control::Handler {
    let checks = Arc::new(crate::signatures::Checks::default());
    Arc::new(move |request| {
        let roots = roots.clone();
        let checks = Arc::clone(&checks);
        let engine = Arc::clone(&engine);
        let mountpoint = mountpoint.clone();
        let username = username.clone();
        let late = Arc::clone(&late);
        let unmount_tx = unmount_tx.clone();
        Box::pin(async move {
            match request {
                Request::Ping => control::ok(control::Pong {
                    pid: std::process::id(),
                }),
                Request::Status => {
                    let (cache_bytes, cache_blocks) = engine.cache_usage();
                    let cache_profile = control::CacheProfileReply::of(&engine.cache_stats());
                    let storage = engine.storage();
                    let queue = engine.queue_counts();
                    let auth = late.auth();
                    let sign_in = auth.and_then(|auth| auth.sign_in());
                    let pinned = engine.pin_summary().await.ok().flatten();
                    let roots = match &roots {
                        Some(roots) => roots.status(),
                        None => Vec::new(),
                    };
                    control::ok(control::StatusReply {
                        roots,
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
                        sign_in_error: auth.and_then(|auth| auth.login_error()),
                        username,
                        credentials: late.credentials(),
                        quota_used: storage.map(|s| s.used),
                        quota_max: storage.map(|s| s.max),
                        inodes: engine.inode_count() as u64,
                        cache_bytes,
                        cache_blocks: cache_blocks as u64,
                        cache_profile: Some(cache_profile),
                        pinned: pinned.map(|pinned| control::PinnedReply {
                            paths: pinned.paths,
                            fetched_bytes: pinned.fetched_bytes,
                            total_bytes: pinned.total_bytes,
                        }),
                        incoming: engine.incoming_markers(),
                        incoming_not_read: engine.incoming_not_read(),
                        keyring_wait: late.waiting(),
                        desktop_bus: crate::bus::status_text(),
                        attached_bus: crate::bus::attached(),
                        tokens_unsaved: auth.and_then(|auth| auth.unsaved()),
                        server_time_offset: pdmount_backend::proton::server_time_offset(),
                        signatures_found: engine.signature_counts().0,
                        signatures_refused: engine.signature_counts().1,
                    })
                }
                Request::Login { force } => {
                    login_reply(engine.health(), &late, &username, force).await
                }
                Request::AttachBus { address, display } => {
                    keywait::handle_attach(address, display, &late).await
                }
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
                        profile: Some(control::CacheProfileReply::of(&stats)),
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
                        profile: Some(control::CacheProfileReply::of(&engine.cache_stats())),
                        trees: Vec::new(),
                    }))
                }
                Request::CacheWipe { path, absolute } => {
                    crate::cache::wipe_reply(&engine, &mountpoint, roots.as_deref(), path, absolute)
                        .await
                }
                Request::SignaturesList => {
                    crate::signatures::list_reply(&engine, roots.as_deref()).await
                }
                Request::SignaturesAccept { id } => {
                    crate::signatures::accept_reply(&engine, roots.as_deref(), &id).await
                }
                Request::SignaturesCheck { path, start } => {
                    crate::signatures::check_reply(&engine, roots.as_deref(), &checks, path, start)
                }
                Request::UnmountRoot { mountpoint } => match &roots {
                    Some(roots) => control::reply(
                        roots
                            .unmount(&mountpoint)
                            .await
                            .map(|()| control::Done {})
                            .map_err(|error| format!("{error:#}")),
                    ),
                    None => control::failed("no further root is configured"),
                },
                Request::MountRoot {
                    mountpoint,
                    read_only,
                } => match &roots {
                    Some(roots) => control::reply(
                        roots
                            .mount(&mountpoint, read_only)
                            .await
                            .map(|()| control::Done {})
                            .map_err(|error| format!("{error:#}")),
                    ),
                    None => control::failed("no further root is configured"),
                },
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

/// What further trees open over (D110), and My Files' volume: the
/// session's Proton backend or fake drive; `None` while the start waits for
/// the keyring (D106).
pub(crate) fn tree_source(
    proton: Option<&Arc<ProtonBackend>>,
    fake: Option<&FakeBackend>,
) -> Option<(crate::roots::TreeSource, String)> {
    match (proton, fake) {
        (Some(proton), _) => Some((
            crate::roots::TreeSource::Proton(Arc::clone(proton)),
            proton.volume_id().to_owned(),
        )),
        (None, Some(fake)) => Some((
            crate::roots::TreeSource::Fake(fake.clone()),
            pdmount_backend::fake::MAIN_VOLUME.to_owned(),
        )),
        (None, None) => None,
    }
}

pub(super) fn queue_acted(
    items: Vec<pdmount_core::engine::queue::QueueItem>,
) -> control::QueueActedReply {
    control::QueueActedReply {
        items: items.into_iter().map(|item| (item.id, item.path)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_rejected_session_or_force_starts_a_sign_in() {
        use Connectivity::*;
        for (state, force, wanted) in [
            (Online, false, false),
            (Offline, false, false),
            (NeedsLogin, false, true),
            (Online, true, true),
            (Offline, true, true),
            (NeedsLogin, true, true),
        ] {
            assert_eq!(
                sign_in_wanted(state, force),
                wanted,
                "{state:?} force={force}"
            );
        }
    }

    #[tokio::test]
    async fn an_online_daemon_needs_no_sign_in_and_the_fake_has_none() {
        let late = Late::new(String::new(), None);
        let health = pdmount_core::health::Health::default();
        health.set(Connectivity::Online);
        let reply = login_reply(&health, &late, "user@example.com", false).await;
        assert_eq!(reply["ok"], true, "{reply}");
        assert_eq!(reply["not_needed"], true);
        assert_eq!(reply["account"], "user@example.com");
        assert_eq!(reply["url"], "");

        for (state, force) in [
            (Connectivity::Online, true),
            (Connectivity::NeedsLogin, false),
        ] {
            health.set(state);
            let reply = login_reply(&health, &late, "user@example.com", force).await;
            assert_eq!(reply["ok"], false, "{state:?}: {reply}");
            assert!(
                reply["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("fake fault clear"))
            );
        }
    }
}
