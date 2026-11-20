//! Noticing that a notification service is there (DESIGN.md §10.8, M6.3):
//! at the start, when the desktop's bus is attached or the bus becomes
//! reachable, and whenever `org.freedesktop.Notifications` gets an owner
//! (`NameOwnerChanged`), a callback runs. The conflict announcer
//! (`conflict_notify.rs`) shows the conflicts it missed then.
//!
//! The bus watched is the one notifications are routed to (D102: the
//! attached bus, else the process's own), or, when neither has a server,
//! the attached or own bus, so that a server that starts later is seen. It
//! is chosen again every [`RETRY`] and at once after an attach or a drop
//! ([`crate::bus::attach_changes`]). A daemon with no reachable bus tries to
//! connect every [`RETRY`]: one cached name lookup at most, no log line per
//! try (debug level only).

use crate::bus::{self, Route, Service};
use futures::StreamExt;
use std::time::Duration;
use zbus::fdo::DBusProxy;

/// How often a missing bus is tried again and the bus is chosen afresh.
pub const RETRY: Duration = Duration::from_secs(60);

/// Which bus to watch.
#[derive(Clone, Debug)]
pub enum Target {
    /// The bus notifications are routed to (the daemon).
    Routed,
    /// A fixed bus (tests).
    #[cfg(test)]
    Fixed(String),
}

impl Target {
    async fn address(&self) -> Option<String> {
        match self {
            Self::Routed => match bus::route(Service::Notifications).await {
                Route::Attached(address) => Some(address),
                Route::Own | Route::Default => bus::own_address(),
                // No server on either: watch where one may come up.
                Route::None => bus::attached().or_else(bus::own_address),
            },
            #[cfg(test)]
            Self::Fixed(address) => Some(address.clone()),
        }
    }
}

/// Runs `up` whenever a notification service is available on the watched
/// bus: once the bus is reached with the service owned or activatable, and
/// again each time the service gets an owner. Never returns; it ends when
/// its task is dropped.
pub async fn watch(target: Target, retry: Duration, up: impl Fn()) {
    // Kept across the loop, so an attach while a bus is chosen is not lost.
    let mut changes = bus::attach_changes();
    loop {
        changes.mark_unchanged();
        let again = match target.address().await {
            Some(address) => follow(&address, &target, retry, &up, &mut changes).await,
            None => false,
        };
        if !again {
            tokio::select! {
                () = tokio::time::sleep(retry) => {}
                _ = changes.changed() => {}
            }
        }
    }
}

/// Whether the name is owned, or D-Bus can start an owner.
async fn available(proxy: &DBusProxy<'_>) -> zbus::Result<bool> {
    let name = zbus::names::BusName::try_from(Service::Notifications.bus_name())?;
    if proxy.name_has_owner(name).await? {
        return Ok(true);
    }
    Ok(proxy
        .list_activatable_names()
        .await?
        .iter()
        .any(|activatable| activatable.as_str() == Service::Notifications.bus_name()))
}

/// Watches the bus at `address` until it ends or fails (false: wait
/// [`RETRY`]) or the bus to watch changed (true: look again at once).
async fn follow(
    address: &str,
    target: &Target,
    retry: Duration,
    up: &impl Fn(),
    attach: &mut tokio::sync::watch::Receiver<u64>,
) -> bool {
    let connection = match bus::connect(address).await {
        Ok(connection) => connection,
        Err(error) => {
            tracing::debug!(
                error,
                "no session bus to watch for the notification service"
            );
            return false;
        }
    };
    let watching = async {
        let proxy = DBusProxy::new(&connection).await?;
        // Subscribed before the state is read, so no change is missed.
        let changes = proxy
            .receive_name_owner_changed_with_args(&[(0, Service::Notifications.bus_name())])
            .await?;
        let there = available(&proxy).await?;
        Ok::<_, zbus::Error>((changes, there))
    };
    let (mut changes, there) = match tokio::time::timeout(bus::BUS_TIMEOUT, watching).await {
        Ok(Ok(watching)) => watching,
        Ok(Err(error)) => {
            tracing::debug!(%error, address, "cannot watch the bus for the notification service");
            return false;
        }
        Err(_) => return false,
    };
    if there {
        up();
    }
    loop {
        tokio::select! {
            change = changes.next() => match change {
                Some(signal) => {
                    if signal.args().is_ok_and(|args| args.new_owner().is_some()) {
                        up();
                    }
                }
                // The connection ended: the bus is gone.
                None => return false,
            },
            () = tokio::time::sleep(retry) => {
                if target.address().await.as_deref() != Some(address) {
                    return true;
                }
            }
            // An attach or a drop may route notifications elsewhere; the same
            // bus is kept, without a second `up` for a service already seen.
            _ = attach.changed() => {
                if target.address().await.as_deref() != Some(address) {
                    return true;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::tests::private_bus;
    use crate::notify_stub::{Options, connect};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Instant;

    async fn wait_for(count: &AtomicUsize, at_least: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while count.load(Ordering::SeqCst) < at_least {
            assert!(
                Instant::now() < deadline,
                "up ran {} times, not {at_least}",
                count.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// A bus that is not there yet is tried again; a service that is not
    /// running is waited for; one that comes, goes and comes again is seen
    /// each time.
    #[tokio::test]
    async fn the_service_appearing_is_seen() {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let address = format!("unix:path={}", dir.path().join("bus").display());
        let ups = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ups);
        let task = tokio::spawn(watch(
            Target::Fixed(address.clone()),
            Duration::from_millis(100),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        ));
        // No bus yet.
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(ups.load(Ordering::SeqCst), 0);
        let bus = private_bus(&dbus_daemon, dir.path());
        assert_eq!(bus.address, address);
        // A bus without a service.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(ups.load(Ordering::SeqCst), 0);
        let log = dir.path().join("stub.log");
        let stub = connect(Some(&address), &log, Options::default())
            .await
            .expect("stub");
        wait_for(&ups, 1).await;
        drop(stub);
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(
            ups.load(Ordering::SeqCst),
            1,
            "a service that left is not an appearance"
        );
        let _stub = connect(Some(&address), &log, Options::default())
            .await
            .expect("stub again");
        wait_for(&ups, 2).await;
        task.abort();
    }

    /// A service that runs when the watch starts counts at once.
    #[tokio::test]
    async fn a_service_that_runs_is_seen_at_once() {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = private_bus(&dbus_daemon, dir.path());
        let _stub = connect(
            Some(&bus.address),
            &dir.path().join("stub.log"),
            Options::default(),
        )
        .await
        .expect("stub");
        let ups = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&ups);
        let task = tokio::spawn(watch(
            Target::Fixed(bus.address.clone()),
            Duration::from_secs(60),
            move || {
                counter.fetch_add(1, Ordering::SeqCst);
            },
        ));
        wait_for(&ups, 1).await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(ups.load(Ordering::SeqCst), 1);
        task.abort();
    }
}
