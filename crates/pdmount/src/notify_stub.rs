//! A stand-in for a desktop's notification server on a private bus, for the
//! scenarios and unit tests of `notify.rs` (test builds, DESIGN.md §15.3):
//! `pdmount fake notifications serve` owns `org.freedesktop.Notifications` on
//! the bus that `DBUS_SESSION_BUS_ADDRESS` (or `--bus`) names, answers
//! `GetCapabilities` and `GetServerInformation`, and writes what it is asked
//! to a log, one JSON object per line:
//!
//! - `{"op":"ready","bus":"<address>"}`
//! - `{"op":"notify","bus":…,"app":…,"summary":…,"body":…,"actions":[…],
//!   "replaces_id":n,"expire":n,"id":n}`: the `id` it answered
//! - `{"op":"dropped","bus":…,"summary":…}`: a `Notify` left unanswered
//!   (`Options::drop_first`, unit tests)
//! - `{"op":"close","bus":…,"id":n}`: a `CloseNotification` (it answers with
//!   `NotificationClosed(id, 3)`, as servers do)
//! - `{"op":"invoked","bus":…,"id":n,"action":"<key>"}` and
//!   `{"op":"closed","bus":…,"id":n,"reason":n}`: a signal sent on request
//!
//! `pdmount fake notifications invoke --id <n> --action <key>` and `… close
//! --id <n> [--reason <n>]` ask a running stub to send `ActionInvoked` or
//! `NotificationClosed` (the stub's own interface, `org.pdmount.TestNotifications`,
//! on the same object).
//!
//! `pdmount fake notifications file-manager` owns
//! `org.freedesktop.FileManager1` and logs `{"op":"show_items",…,"uris":[…]}`
//! for a conflict's **Show** (M6.2).

use anyhow::Context;
#[cfg(feature = "fake-backend")]
use futures::StreamExt;
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use zbus::zvariant::OwnedValue;
use zbus::{Connection, object_server::SignalEmitter};

pub const NAME: &str = "org.freedesktop.Notifications";
pub const PATH: &str = "/org/freedesktop/Notifications";
const TEST: &str = "org.pdmount.TestNotifications";

/// What the stub offers.
#[derive(Clone, Debug, Default)]
pub struct Options {
    /// `GetCapabilities` leaves out `actions`.
    pub no_actions: bool,
    /// `GetCapabilities` leaves out `body-markup`.
    pub no_markup: bool,
    /// Every `Notify` waits this long before it answers (a server that
    /// hangs).
    pub delay: Duration,
    /// The first this many `Notify` calls get no answer within any
    /// client's deadline (a call lost on the way; they fail after a
    /// minute), logged as `{"op":"dropped",…}`.
    pub drop_first: u32,
}

#[derive(Clone)]
struct Log {
    file: Arc<Mutex<std::fs::File>>,
    bus: String,
}

impl Log {
    fn open(path: &Path, bus: &str) -> anyhow::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .with_context(|| format!("cannot open {}", path.display()))?;
        Ok(Self {
            file: Arc::new(Mutex::new(file)),
            bus: bus.to_owned(),
        })
    }

    /// One line: `fields` (a JSON object's members) with the bus added. One
    /// `write`, so lines of several stubs on one file stay whole.
    fn line(&self, op: &str, mut fields: serde_json::Value) {
        fields["op"] = op.into();
        fields["bus"] = self.bus.clone().into();
        let mut text = fields.to_string();
        text.push('\n');
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(text.as_bytes());
        }
    }
}

struct Server {
    log: Log,
    options: Options,
    /// The last id given; atomics, so a call that hangs does not hold up
    /// the others (`&self` methods run side by side).
    next: AtomicU32,
    /// The `Notify` calls answered so far, or dropped.
    calls: AtomicU32,
}

#[zbus::interface(name = "org.freedesktop.Notifications")]
impl Server {
    fn get_capabilities(&self) -> Vec<String> {
        let mut caps = vec!["body".to_owned(), "persistence".to_owned()];
        if !self.options.no_actions {
            caps.push("actions".to_owned());
        }
        if !self.options.no_markup {
            caps.push("body-markup".to_owned());
        }
        caps
    }

    fn get_server_information(&self) -> (String, String, String, String) {
        (
            "pdmount-notification-stub".into(),
            "pdmount".into(),
            "0".into(),
            "1.2".into(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    async fn notify(
        &self,
        app_name: String,
        replaces_id: u32,
        _app_icon: String,
        summary: String,
        body: String,
        actions: Vec<String>,
        _hints: HashMap<String, OwnedValue>,
        expire_timeout: i32,
    ) -> zbus::fdo::Result<u32> {
        if self.calls.fetch_add(1, Ordering::SeqCst) < self.options.drop_first {
            self.log
                .line("dropped", serde_json::json!({ "summary": summary }));
            tokio::time::sleep(Duration::from_secs(60)).await;
            return Err(zbus::fdo::Error::Failed("dropped".into()));
        }
        if !self.options.delay.is_zero() {
            tokio::time::sleep(self.options.delay).await;
        }
        let id = if replaces_id != 0 {
            replaces_id
        } else {
            self.next.fetch_add(1, Ordering::SeqCst) + 1
        };
        self.log.line(
            "notify",
            serde_json::json!({
                "app": app_name, "summary": summary, "body": body, "actions": actions,
                "replaces_id": replaces_id, "expire": expire_timeout, "id": id,
            }),
        );
        Ok(id)
    }

    async fn close_notification(
        &self,
        id: u32,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<()> {
        self.log.line("close", serde_json::json!({ "id": id }));
        // 3: closed by a call of CloseNotification.
        Ok(Self::notification_closed(&emitter, id, 3).await?)
    }

    #[zbus(signal)]
    async fn notification_closed(
        emitter: &SignalEmitter<'_>,
        id: u32,
        reason: u32,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    async fn action_invoked(
        emitter: &SignalEmitter<'_>,
        id: u32,
        action_key: String,
    ) -> zbus::Result<()>;
}

/// The stub's own interface: signals on request.
struct Control {
    log: Log,
}

#[zbus::interface(name = "org.pdmount.TestNotifications")]
impl Control {
    async fn invoke(
        &self,
        id: u32,
        action: String,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<()> {
        let emitter = SignalEmitter::new(connection, PATH)?;
        Server::action_invoked(&emitter, id, action.clone()).await?;
        self.log
            .line("invoked", serde_json::json!({ "id": id, "action": action }));
        Ok(())
    }

    async fn close(
        &self,
        id: u32,
        reason: u32,
        #[zbus(connection)] connection: &Connection,
    ) -> zbus::fdo::Result<()> {
        let emitter = SignalEmitter::new(connection, PATH)?;
        Server::notification_closed(&emitter, id, reason).await?;
        self.log
            .line("closed", serde_json::json!({ "id": id, "reason": reason }));
        Ok(())
    }
}

/// Owns the name on `address` (a test passes its private bus; `None`:
/// `DBUS_SESSION_BUS_ADDRESS`) and logs `ready`; the stub serves while the
/// connection is kept.
pub async fn connect(
    address: Option<&str>,
    log: &Path,
    options: Options,
) -> anyhow::Result<Connection> {
    let address = address
        .map(str::to_owned)
        .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok())
        .context("no bus: DBUS_SESSION_BUS_ADDRESS is not set and no --bus given")?;
    let log = Log::open(log, &address)?;
    let connection = zbus::connection::Builder::address(address.as_str())?
        .serve_at(
            PATH,
            Server {
                log: log.clone(),
                options,
                next: AtomicU32::new(0),
                calls: AtomicU32::new(0),
            },
        )?
        .serve_at(PATH, Control { log: log.clone() })?
        .name(NAME)?
        .build()
        .await
        .with_context(|| format!("cannot own {NAME} on {address}"))?;
    log.line("ready", serde_json::json!({}));
    Ok(connection)
}

#[cfg(feature = "fake-backend")]
/// `pdmount fake notifications serve`: until killed.
pub async fn serve(
    address: Option<String>,
    log: std::path::PathBuf,
    options: Options,
) -> anyhow::Result<()> {
    let connection = connect(address.as_deref(), &log, options).await?;
    // Until the bus goes away (the scenarios start a stub through D-Bus
    // activation, which nobody else stops).
    let mut messages = zbus::MessageStream::from(&connection);
    while messages.next().await.is_some() {}
    Ok(())
}

#[cfg(any(test, feature = "fake-backend"))]
/// Asks a running stub on `address` (default: `DBUS_SESSION_BUS_ADDRESS`)
/// for a signal: `ActionInvoked(id, key)` when `action` is given, else
/// `NotificationClosed(id, reason)`.
pub async fn emit(
    address: Option<String>,
    id: u32,
    action: Option<String>,
    reason: u32,
) -> anyhow::Result<()> {
    let address = address
        .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok())
        .context("no bus: DBUS_SESSION_BUS_ADDRESS is not set and no --bus given")?;
    let connection = zbus::connection::Builder::address(address.as_str())?
        .build()
        .await
        .with_context(|| format!("cannot connect to {address}"))?;
    match action {
        Some(action) => {
            connection
                .call_method(Some(NAME), PATH, Some(TEST), "Invoke", &(id, action))
                .await?
        }
        None => {
            connection
                .call_method(Some(NAME), PATH, Some(TEST), "Close", &(id, reason))
                .await?
        }
    };
    Ok(())
}

// ---- A file manager (M6.2) ------------------------------------------------------

pub const FILE_MANAGER: &str = "org.freedesktop.FileManager1";
pub const FILE_MANAGER_PATH: &str = "/org/freedesktop/FileManager1";

/// A stand-in for a file manager's `org.freedesktop.FileManager1`: logs
/// `{"op":"show_items","bus":…,"uris":[…],"startup_id":…}` (and
/// `show_folders`), and `{"op":"fm_ready","bus":…}` once it owns the name.
struct FileManager {
    log: Log,
}

#[zbus::interface(name = "org.freedesktop.FileManager1")]
impl FileManager {
    fn show_items(&self, uris: Vec<String>, startup_id: String) {
        self.log.line(
            "show_items",
            serde_json::json!({ "uris": uris, "startup_id": startup_id }),
        );
    }

    fn show_folders(&self, uris: Vec<String>, startup_id: String) {
        self.log.line(
            "show_folders",
            serde_json::json!({ "uris": uris, "startup_id": startup_id }),
        );
    }
}

/// Owns `org.freedesktop.FileManager1` on `address` (`None`:
/// `DBUS_SESSION_BUS_ADDRESS`) and logs `fm_ready`; it serves while the
/// connection is kept.
pub async fn connect_file_manager(address: Option<&str>, log: &Path) -> anyhow::Result<Connection> {
    let address = address
        .map(str::to_owned)
        .or_else(|| std::env::var("DBUS_SESSION_BUS_ADDRESS").ok())
        .context("no bus: DBUS_SESSION_BUS_ADDRESS is not set and no --bus given")?;
    let log = Log::open(log, &address)?;
    let connection = zbus::connection::Builder::address(address.as_str())?
        .serve_at(FILE_MANAGER_PATH, FileManager { log: log.clone() })?
        .name(FILE_MANAGER)?
        .build()
        .await
        .with_context(|| format!("cannot own {FILE_MANAGER} on {address}"))?;
    log.line("fm_ready", serde_json::json!({}));
    Ok(connection)
}

#[cfg(feature = "fake-backend")]
/// `pdmount fake notifications file-manager`: until killed.
pub async fn serve_file_manager(
    address: Option<String>,
    log: std::path::PathBuf,
) -> anyhow::Result<()> {
    let connection = connect_file_manager(address.as_deref(), &log).await?;
    let mut messages = zbus::MessageStream::from(&connection);
    while messages.next().await.is_some() {}
    Ok(())
}
