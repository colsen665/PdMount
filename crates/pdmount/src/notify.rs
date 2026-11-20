//! Desktop notifications (DESIGN.md §12.3, §10.8, §14.4, M6.1): the
//! `org.freedesktop.Notifications` interface over D-Bus, on the bus
//! notifications are routed to (the desktop's attached bus or the process's
//! own, D102 amended; `bus.rs`). Best effort: nothing here fails the caller,
//! and only [`desktop_sent`] waits, for a bounded time. No bus, or no
//! notification server on it: the call returns, and the first failure is
//! logged once.
//!
//! One connection per bus address lives on a runtime of its own (so it does
//! not depend on the caller's), is reused, and is made again after a failure.
//! Every call has a 2 s deadline ([`crate::bus::BUS_TIMEOUT`]). The server's
//! capabilities (`GetCapabilities`) are asked once per connection and again
//! after the service changes owner (another server, or one restarted with
//! another configuration): actions are only offered to a server that has
//! `actions`, and the body is escaped for a server with `body-markup` (names
//! in it come from other devices).
//!
//! For the tasks that act on a notification: [`send`] returns a [`Handle`]
//! (bus and id); [`close`] takes it back; [`subscribe`] delivers
//! [`Event::Action`] (`ActionInvoked`) and [`Event::Closed`]
//! (`NotificationClosed`) for the notifications this process sent.

use crate::bus::{BUS_TIMEOUT, Route, Service};
use futures::StreamExt;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

/// At most one notification in this time; the others are coalesced.
const MIN_GAP: Duration = Duration::from_secs(30);

/// Spaces notifications [`MIN_GAP`] (or another gap) apart: what arrives
/// in between is counted and announced together when the gap is over.
pub struct Coalescer {
    last: Option<Instant>,
    held: u32,
    gap: Duration,
}

impl Default for Coalescer {
    fn default() -> Self {
        Self::with_gap(MIN_GAP)
    }
}

impl Coalescer {
    pub fn with_gap(gap: Duration) -> Self {
        Self {
            last: None,
            held: 0,
            gap,
        }
    }

    /// `count` more events at `now`: how many to announce now, if a
    /// notification may go out.
    pub fn add_n(&mut self, now: Instant, count: u32) -> Option<u32> {
        self.held = self.held.saturating_add(count);
        self.flush(now)
    }

    /// The held events, when a notification may go out at `now`.
    pub fn flush(&mut self, now: Instant) -> Option<u32> {
        if self.held == 0
            || self
                .last
                .is_some_and(|last| now.duration_since(last) < self.gap)
        {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.held))
    }

    /// When held events may go out.
    pub fn deadline(&self) -> Option<Instant> {
        (self.held > 0).then(|| self.last.map_or_else(Instant::now, |last| last + self.gap))
    }
}

/// Lets up to `burst` notifications of their own go out in any `gap`
/// (M6.2: three conflicts per 30 s); what comes beyond that is held and
/// announced together in one notification once the window has room, which
/// counts against the limit too. Once something is held, later events wait
/// with it, so they are summed in order.
pub struct Burst {
    /// When the notifications in the last `gap` went out, oldest first.
    sent: VecDeque<Instant>,
    held: u32,
    burst: usize,
    gap: Duration,
}

impl Burst {
    pub fn new(burst: usize, gap: Duration) -> Self {
        Self {
            sent: VecDeque::new(),
            held: 0,
            burst: burst.max(1),
            gap,
        }
    }

    fn prune(&mut self, now: Instant) {
        while self
            .sent
            .front()
            .is_some_and(|first| now.saturating_duration_since(*first) >= self.gap)
        {
            self.sent.pop_front();
        }
    }

    /// One event at `now`: true when it may go out on its own now (it then
    /// counts as sent); false when it is held for the next summary.
    pub fn admit(&mut self, now: Instant) -> bool {
        self.prune(now);
        if self.held == 0 && self.sent.len() < self.burst {
            self.sent.push_back(now);
            true
        } else {
            self.held = self.held.saturating_add(1);
            false
        }
    }

    /// `count` events held without a try of their own (missed ones).
    pub fn hold(&mut self, count: u32) {
        self.held = self.held.saturating_add(count);
    }

    /// The held events, when their summary may go out at `now` (it then
    /// counts as sent).
    pub fn flush(&mut self, now: Instant) -> Option<u32> {
        self.prune(now);
        if self.held == 0 || self.sent.len() >= self.burst {
            return None;
        }
        self.sent.push_back(now);
        Some(std::mem::take(&mut self.held))
    }

    /// Takes back the last send that [`Burst::admit`] or [`Burst::flush`]
    /// counted: the server did not take it (M6.3), so it does not use up
    /// the window.
    pub fn retract(&mut self) {
        self.sent.pop_back();
    }

    /// When held events may go out.
    pub fn deadline(&self) -> Option<Instant> {
        if self.held == 0 {
            return None;
        }
        Some(match self.sent.len() >= self.burst {
            // The window has room again when its oldest send leaves it.
            true => self.sent[self.sent.len() - self.burst] + self.gap,
            false => Instant::now(),
        })
    }
}

// ---- The client -----------------------------------------------------------------

#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications",
    gen_blocking = false
)]
trait Notifications {
    #[allow(clippy::too_many_arguments)]
    fn notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: &[&str],
        hints: HashMap<&str, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;

    fn close_notification(&self, id: u32) -> zbus::Result<()>;

    fn get_capabilities(&self) -> zbus::Result<Vec<String>>;

    #[zbus(signal)]
    fn action_invoked(&self, id: u32, action_key: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn notification_closed(&self, id: u32, reason: u32) -> zbus::Result<()>;
}

/// What a notification server can do (`GetCapabilities`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Caps {
    /// `actions`: buttons that send `ActionInvoked`.
    pub actions: bool,
    /// `body-markup`: the body is read as markup, so it is escaped.
    pub markup: bool,
}

/// A notification to show.
#[derive(Clone, Debug)]
pub struct Notification {
    pub summary: String,
    /// Plain text; escaped here when the server reads markup.
    pub body: String,
    /// `(key, label)` pairs, offered when the server has `actions`; the key
    /// comes back in [`Event::Action`].
    pub actions: Vec<(String, String)>,
    /// A notification of this process to replace (a changed text under the
    /// same id); ignored when it was sent on another bus.
    pub replaces: Option<Handle>,
    /// Milliseconds: -1 the server's default, 0 never expires.
    pub expire_ms: i32,
}

impl Notification {
    pub fn new(summary: impl Into<String>, body: impl Into<String>) -> Self {
        Self {
            summary: summary.into(),
            body: body.into(),
            actions: Vec::new(),
            replaces: None,
            expire_ms: -1,
        }
    }

    pub fn action(mut self, key: impl Into<String>, label: impl Into<String>) -> Self {
        self.actions.push((key.into(), label.into()));
        self
    }

    pub fn replacing(mut self, handle: &Handle) -> Self {
        self.replaces = Some(handle.clone());
        self
    }
}

/// A notification that was sent: ids belong to one server, so the bus is
/// part of it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Handle {
    pub bus: String,
    pub id: u32,
}

/// A signal about a notification this process sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// `ActionInvoked`: the user chose the action with this key.
    Action { handle: Handle, key: String },
    /// `NotificationClosed`: `reason` is the server's (1 expired, 2
    /// dismissed by the user, 3 closed by [`close`], 4 undefined).
    Closed { handle: Handle, reason: u32 },
}

static EVENTS: LazyLock<broadcast::Sender<Event>> = LazyLock::new(|| broadcast::channel(64).0);

/// The signals about the notifications this process sent, from now on. A
/// receiver that falls behind misses the oldest (`RecvError::Lagged`).
pub fn subscribe() -> broadcast::Receiver<Event> {
    EVENTS.subscribe()
}

/// `&`, `<` and `>` escaped, for a body the server reads as markup.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            c => out.push(c),
        }
    }
    out
}

/// `name` (a file's, a share's, a device's: text from other devices or
/// users, D070) with every character that would split, reorder or hide a
/// notification's text, or make a name read as a path, replaced by U+FFFD
/// ([`hides`]). The one rule for every name in a notification (DESIGN.md
/// §10.8, §14.4); markup is escaped apart, when the notification is sent.
pub fn clean_name(name: &str) -> String {
    name.chars()
        .map(|c| if hides(c) { '\u{fffd}' } else { c })
        .collect()
}

/// The characters [`clean_name`] replaces: control characters (Cc); the
/// line and paragraph separators (U+2028, U+2029); the bidirectional
/// formatting characters (U+061C, U+200E, U+200F, U+202A–U+202E,
/// U+2066–U+2069); the invisible ones (U+00AD, U+034F, U+180B–U+180E,
/// U+200B–U+200D, U+2060–U+206F, U+FEFF, U+FFF9–U+FFFB, the tag characters
/// U+E0000–U+E007F, the variation selectors U+E0100–U+E01EF); the blanks
/// that are letters (U+115F, U+1160, U+3164, U+FFA0, U+2800), which push the
/// rest of a name out of sight; and the slash look-alikes (U+2044, U+2215,
/// U+29F8, U+FF0F), which make a name read as a path (M6.5 V-3). Emoji's
/// variation selectors (U+FE00–U+FE0F) stay: they show nothing apart.
fn hides(c: char) -> bool {
    c.is_control()
        || matches!(
            c,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{115f}'
                | '\u{1160}'
                | '\u{180b}'..='\u{180e}'
                | '\u{200b}'..='\u{200f}'
                | '\u{2028}'..='\u{202e}'
                | '\u{2044}'
                | '\u{2060}'..='\u{206f}'
                | '\u{2215}'
                | '\u{2800}'
                | '\u{29f8}'
                | '\u{3164}'
                | '\u{feff}'
                | '\u{ff0f}'
                | '\u{ffa0}'
                | '\u{fff9}'..='\u{fffb}'
                | '\u{e0000}'..='\u{e007f}'
                | '\u{e0100}'..='\u{e01ef}'
        )
}

/// The notification ids sent on a connection, to pass only their signals on.
/// Bounded: the oldest go when a daemon runs for months.
#[derive(Default)]
struct Sent(parking_lot::Mutex<(HashSet<u32>, VecDeque<u32>)>);

impl Sent {
    const MAX: usize = 512;

    fn add(&self, id: u32) {
        let mut sent = self.0.lock();
        if sent.0.insert(id) {
            sent.1.push_back(id);
            if sent.1.len() > Self::MAX
                && let Some(old) = sent.1.pop_front()
            {
                sent.0.remove(&old);
            }
        }
    }

    fn has(&self, id: u32) -> bool {
        self.0.lock().0.contains(&id)
    }

    fn forget(&self, id: u32) {
        let mut sent = self.0.lock();
        if sent.0.remove(&id) {
            sent.1.retain(|other| *other != id);
        }
    }
}

/// The server's capabilities as last asked; `None`: ask again (a new
/// connection, or the service changed owner since).
type CapsCache = Arc<parking_lot::Mutex<Option<Caps>>>;

/// A connection to one bus, with the server's capabilities and the listener
/// of its signals; the listener ends with the entry.
struct Entry {
    address: String,
    proxy: NotificationsProxy<'static>,
    caps: CapsCache,
    sent: Arc<Sent>,
    listener: tokio::task::JoinHandle<()>,
}

impl Drop for Entry {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

/// A failed call: the text for the log, and whether it ran out of time.
struct CallError {
    message: String,
    timed_out: bool,
}

impl CallError {
    fn new(what: &str, error: impl std::fmt::Display) -> Self {
        Self {
            message: format!("{what}: {error}"),
            timed_out: false,
        }
    }

    fn timeout(what: &str) -> Self {
        Self {
            message: format!("{what}: no answer within {} s", BUS_TIMEOUT.as_secs()),
            timed_out: true,
        }
    }
}

/// `call`, within the deadline (the connection's own method timeout is the
/// same; this one also covers a connection that stalls).
async fn within<T>(
    what: &str,
    call: impl std::future::Future<Output = zbus::Result<T>>,
) -> Result<T, CallError> {
    match tokio::time::timeout(BUS_TIMEOUT + Duration::from_millis(500), call).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(error)) => Err(CallError::new(what, error)),
        Err(_) => Err(CallError::timeout(what)),
    }
}

impl Entry {
    /// The server's capabilities: asked once, and again after the service
    /// changed owner ([`listen`] clears them), so that a server that reads
    /// markup never gets a body escaped for one that did not (M6.5 V-4).
    async fn caps(&self) -> Result<Caps, CallError> {
        if let Some(caps) = *self.caps.lock() {
            return Ok(caps);
        }
        let caps = within("GetCapabilities", self.proxy.get_capabilities()).await?;
        let caps = Caps {
            actions: caps.iter().any(|cap| cap == "actions"),
            markup: caps.iter().any(|cap| cap == "body-markup"),
        };
        *self.caps.lock() = Some(caps);
        Ok(caps)
    }

    async fn send(&self, notification: &Notification) -> Result<u32, CallError> {
        let caps = self.caps().await?;
        let body = if caps.markup {
            escape_markup(&notification.body)
        } else {
            notification.body.clone()
        };
        let actions: Vec<&str> = if caps.actions {
            notification
                .actions
                .iter()
                .flat_map(|(key, label)| [key.as_str(), label.as_str()])
                .collect()
        } else {
            Vec::new()
        };
        let replaces = notification
            .replaces
            .as_ref()
            .filter(|handle| handle.bus == self.address)
            .map_or(0, |handle| handle.id);
        let id = within(
            "Notify",
            self.proxy.notify(
                "PdMount",
                replaces,
                "",
                &notification.summary,
                &body,
                &actions,
                HashMap::new(),
                notification.expire_ms,
            ),
        )
        .await?;
        self.sent.add(id);
        Ok(id)
    }
}

/// The connections, by bus address.
static ENTRIES: LazyLock<tokio::sync::Mutex<HashMap<String, Arc<Entry>>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(HashMap::new()));

/// The runtime the connections and their listeners live on: the callers'
/// runtimes come and go (a blocking helper's lasts one call), a connection
/// must not.
fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("pdmount-notify")
            .enable_all()
            .build()
            .expect("the notification runtime")
    })
}

/// Runs `future` on [`runtime`] and waits for it from any runtime.
async fn on_runtime<T: Send + 'static>(
    future: impl std::future::Future<Output = T> + Send + 'static,
) -> Option<T> {
    runtime().spawn(future).await.ok()
}

/// The connection to `address`, made if there is none: with `true` when it
/// is new. A bus that is not this user's is refused (`bus::connect`). The
/// connection is made outside the lock (it may take the 2 s deadline twice),
/// so calls on other buses do not wait behind a bus that does not answer;
/// when two callers make one at once, the first kept wins.
async fn entry_for(address: &str) -> Result<(Arc<Entry>, bool), String> {
    if let Some(entry) = ENTRIES.lock().await.get(address) {
        return Ok((entry.clone(), false));
    }
    let connection = crate::bus::connect(address).await?;
    let proxy = NotificationsProxy::builder(&connection)
        .cache_properties(zbus::proxy::CacheProperties::No)
        .build()
        .await
        .map_err(|error| format!("cannot use the notification interface on {address}: {error}"))?;
    let mut entries = ENTRIES.lock().await;
    if let Some(entry) = entries.get(address) {
        // Made by another caller meanwhile: as new as this one.
        return Ok((entry.clone(), true));
    }
    let sent = Arc::new(Sent::default());
    let caps = CapsCache::default();
    let listener = tokio::spawn(listen(
        address.to_owned(),
        proxy.clone(),
        sent.clone(),
        caps.clone(),
    ));
    let entry = Arc::new(Entry {
        address: address.to_owned(),
        proxy,
        caps,
        sent,
        listener,
    });
    entries.insert(address.to_owned(), entry.clone());
    Ok((entry, true))
}

/// Forgets `entry`, if it is still the connection to its bus: the next call
/// connects again.
async fn drop_entry(entry: &Arc<Entry>) {
    let mut entries = ENTRIES.lock().await;
    if entries
        .get(&entry.address)
        .is_some_and(|current| Arc::ptr_eq(current, entry))
    {
        entries.remove(&entry.address);
    }
}

/// Passes `ActionInvoked` and `NotificationClosed` of this process's
/// notifications on to [`subscribe`]'s receivers, and forgets the server's
/// capabilities when the service changes owner, until the entry is dropped
/// or the connection ends.
async fn listen(
    address: String,
    proxy: NotificationsProxy<'static>,
    sent: Arc<Sent>,
    caps: CapsCache,
) {
    let (mut actions, mut closes, mut owners) = match (
        proxy.receive_action_invoked().await,
        proxy.receive_notification_closed().await,
        proxy.inner().receive_owner_changed().await,
    ) {
        (Ok(actions), Ok(closes), Ok(owners)) => (actions, closes, owners),
        (Err(error), _, _) | (_, Err(error), _) | (_, _, Err(error)) => {
            tracing::debug!(%error, address, "cannot listen for notification signals");
            return;
        }
    };
    loop {
        tokio::select! {
            Some(_) = owners.next() => {
                *caps.lock() = None;
            }
            Some(signal) = actions.next() => {
                if let Ok(args) = signal.args() && sent.has(*args.id()) {
                    let handle = Handle { bus: address.clone(), id: *args.id() };
                    let _ = EVENTS.send(Event::Action { handle, key: args.action_key().clone() });
                }
            }
            Some(signal) = closes.next() => {
                if let Ok(args) = signal.args() && sent.has(*args.id()) {
                    let handle = Handle { bus: address.clone(), id: *args.id() };
                    sent.forget(*args.id());
                    let _ = EVENTS.send(Event::Closed { handle, reason: *args.reason() });
                }
            }
            else => break,
        }
    }
}

/// Whether the last send failed: the first failure after a success is logged
/// at INFO, the following ones at debug level, so a desktop without a
/// notification server does not fill the log.
static FAILING: AtomicBool = AtomicBool::new(false);

fn failed(error: &str) {
    if FAILING.swap(true, Ordering::Relaxed) {
        tracing::debug!(error, "notification not sent");
    } else {
        tracing::info!(
            error,
            "notifications cannot be sent (logged once until one goes out)"
        );
    }
}

fn succeeded() {
    if FAILING.swap(false, Ordering::Relaxed) {
        tracing::info!("notifications are going out again");
    }
}

/// The bus the next notification goes to (D102 amended): the attached one or
/// the process's own; `None` without a bus or a server on either.
async fn target() -> Option<String> {
    match crate::bus::route(Service::Notifications).await {
        Route::Attached(address) => Some(address),
        Route::Own | Route::Default => crate::bus::own_address(),
        Route::None => None,
    }
}

/// After a failure on `address`: the next call chooses its bus afresh, and a
/// failure on the attached bus checks whether that bus is gone (logout).
async fn after_failure(address: &str) {
    crate::bus::invalidate();
    if crate::bus::attached().as_deref() == Some(address) {
        crate::bus::drop_if_gone_async(address).await;
    }
}

/// [`send_to`] on the notification runtime: a connection that has been idle
/// may be stale, so a failure on an old one is tried once more on a new one.
async fn send_to_inner(address: String, notification: Notification) -> Result<Handle, String> {
    for _ in 0..2 {
        let (entry, fresh) = entry_for(&address).await?;
        match entry.send(&notification).await {
            Ok(id) => return Ok(Handle { bus: address, id }),
            Err(error) => {
                drop_entry(&entry).await;
                if fresh || error.timed_out {
                    return Err(error.message);
                }
            }
        }
    }
    Err("the connection failed twice".to_owned())
}

/// Sends `notification` on `address` (a test names its private bus).
pub(crate) async fn send_to(address: &str, notification: &Notification) -> Result<Handle, String> {
    let (address, notification) = (address.to_owned(), notification.clone());
    let result = on_runtime(async move {
        let result = send_to_inner(address.clone(), notification).await;
        if result.is_err() {
            after_failure(&address).await;
        }
        result
    })
    .await
    .unwrap_or_else(|| Err("the notification task ended".to_owned()));
    match &result {
        Ok(_) => succeeded(),
        Err(error) => failed(error),
    }
    result
}

/// Sends a notification on the bus notifications are routed to; its handle,
/// or `None` when there is no bus, no server, or it did not answer in time
/// (the failure is logged once).
pub async fn send(notification: &Notification) -> Option<Handle> {
    let notification = notification.clone();
    on_runtime(async move {
        let address = target().await?;
        send_to(&address, &notification).await.ok()
    })
    .await?
}

/// Closes a notification (`CloseNotification`) on the bus it was sent on;
/// false when that bus or its server did not answer. A server that no longer
/// knows the id is not an error. A failure on an old connection is tried
/// once more on a new one, as a send is (an address reused by the next
/// login's bus).
pub async fn close(handle: &Handle) -> bool {
    let handle = handle.clone();
    on_runtime(async move {
        for _ in 0..2 {
            let (entry, fresh) = entry_for(&handle.bus).await.ok()?;
            match within(
                "CloseNotification",
                entry.proxy.close_notification(handle.id),
            )
            .await
            {
                Ok(()) => return Some(()),
                Err(error) => {
                    tracing::debug!(error = error.message, "notification not closed");
                    drop_entry(&entry).await;
                    if fresh || error.timed_out {
                        return None;
                    }
                }
            }
        }
        None
    })
    .await
    .flatten()
    .is_some()
}

/// The capabilities of the notification server on `address`, asked once per
/// connection and server.
pub(crate) async fn capabilities_on(address: &str) -> Option<Caps> {
    let address = address.to_owned();
    on_runtime(async move {
        let (entry, _) = entry_for(&address).await.ok()?;
        match entry.caps().await {
            Ok(caps) => Some(caps),
            Err(error) => {
                tracing::debug!(error = error.message, "cannot ask the notification server");
                drop_entry(&entry).await;
                None
            }
        }
    })
    .await
    .flatten()
}

/// The capabilities of the server notifications go to now; `None` without a
/// bus or server.
pub async fn capabilities() -> Option<Caps> {
    on_runtime(async move { capabilities_on(&target().await?).await })
        .await
        .flatten()
}

// ---- The callers' entry points -----------------------------------------------------

/// Whether a bus exists to send on, the attached one or the process's own.
fn available() -> bool {
    crate::bus::attached().is_some() || crate::bus::own_address().is_some()
}

/// Whether a notification would reach a desktop now: a bus with a
/// notification server that owns or can start its name, the attached one or
/// the process's own (D102 amended, D106).
pub async fn deliverable() -> bool {
    match crate::bus::route(Service::Notifications).await {
        Route::Attached(_) | Route::Own => true,
        Route::None => false,
        Route::Default => match crate::bus::own_address() {
            Some(own) => crate::bus::has_notification_server(&own).await,
            None => false,
        },
    }
}

/// Notifications waiting for [`desktop`]'s sender: sent in order, so that
/// "uploading" comes before "uploaded".
fn queue() -> &'static tokio::sync::mpsc::Sender<(String, String)> {
    static QUEUE: OnceLock<tokio::sync::mpsc::Sender<(String, String)>> = OnceLock::new();
    QUEUE.get_or_init(|| {
        let (sender, mut receiver) = tokio::sync::mpsc::channel::<(String, String)>(64);
        runtime().spawn(async move {
            while let Some((summary, body)) = receiver.recv().await {
                if let Some(address) = target().await {
                    let _ = send_to(&address, &Notification::new(summary, body)).await;
                }
            }
        });
        sender
    })
}

/// Shows a notification without waiting for it: queued for a task of the
/// notification runtime. Never fails; with no bus, nothing is queued.
pub fn desktop(summary: &str, body: &str) {
    if !available() {
        return;
    }
    // Full: the server is not answering; this one is dropped.
    let _ = queue().try_send((summary.to_owned(), body.to_owned()));
}

/// Like [`desktop`], but waits, at most [`SENT_TIMEOUT`], and says whether
/// the server took the notification: false when there is no bus or server,
/// it failed, or it did not answer in time. M5.2: a follow-up is sent only
/// if the first notification went out.
pub async fn desktop_sent(summary: &str, body: &str) -> bool {
    if !available() {
        return false;
    }
    let notification = Notification::new(summary, body);
    let sent = tokio::time::timeout(SENT_TIMEOUT, send(&notification)).await;
    matches!(sent, Ok(Some(_)))
}

/// How long [`desktop_sent`] waits: a hung session bus must not hold up
/// the caller, or the daemon's exit. A server that D-Bus starts on the first
/// call may need longer than one call's deadline.
const SENT_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::tests::{PrivateBus, private_bus};
    use crate::notify_stub::{Options, connect};

    #[test]
    fn events_inside_the_gap_are_held_and_announced_together() {
        let mut notices = Coalescer::default();
        let start = Instant::now();
        assert_eq!(notices.add_n(start, 1), Some(1));
        assert_eq!(notices.deadline(), None);
        assert_eq!(notices.add_n(start + Duration::from_secs(5), 1), None);
        assert_eq!(notices.add_n(start + Duration::from_secs(29), 1), None);
        assert_eq!(notices.deadline(), Some(start + MIN_GAP));
        assert_eq!(notices.flush(start + Duration::from_secs(29)), None);
        assert_eq!(notices.flush(start + MIN_GAP), Some(2));
        assert_eq!(notices.flush(start + MIN_GAP), None, "nothing is held");
        assert_eq!(
            notices.add_n(start + Duration::from_secs(31), 1),
            None,
            "the gap restarted"
        );
    }

    /// Three of their own per 30 s; the rest wait and go out as one, which
    /// counts against the next window too.
    #[test]
    fn a_burst_lets_three_through_and_sums_the_rest() {
        let gap = Duration::from_secs(30);
        let mut burst = Burst::new(3, gap);
        let start = Instant::now();
        assert!(burst.admit(start));
        assert!(burst.admit(start + Duration::from_secs(1)));
        assert_eq!(burst.deadline(), None);
        assert!(burst.admit(start + Duration::from_secs(2)));
        assert!(!burst.admit(start + Duration::from_secs(3)));
        assert!(!burst.admit(start + Duration::from_secs(4)));
        burst.hold(2);
        assert_eq!(burst.deadline(), Some(start + gap));
        assert_eq!(burst.flush(start + Duration::from_secs(29)), None);
        assert_eq!(burst.flush(start + gap), Some(4));
        assert_eq!(burst.flush(start + gap), None, "nothing is held");

        // The summary took the first one's place: two more fit until 31 s.
        let later = start + gap;
        assert!(!burst.admit(later), "the window is full again");
        assert_eq!(burst.deadline(), Some(start + Duration::from_secs(31)));
        assert_eq!(burst.flush(start + Duration::from_secs(31)), Some(1));
        // Once the window empties, single ones go out again.
        assert!(burst.admit(start + Duration::from_secs(70)));
    }

    /// A send the server did not take leaves its place in the window.
    #[test]
    fn a_retracted_send_leaves_the_window() {
        let mut burst = Burst::new(2, Duration::from_secs(30));
        let start = Instant::now();
        assert!(burst.admit(start));
        burst.retract();
        assert!(burst.admit(start));
        assert!(burst.admit(start));
        assert!(!burst.admit(start), "two sent are two sent");
    }

    #[test]
    fn markup_characters_are_escaped() {
        assert_eq!(
            escape_markup("a & b <i>c</i> > d"),
            "a &amp; b &lt;i&gt;c&lt;/i&gt; &gt; d"
        );
        assert_eq!(escape_markup("plain `code`"), "plain `code`");
    }

    /// Names cannot split or reorder the text: control, separator and
    /// bidirectional formatting characters become U+FFFD; the rest stays.
    #[test]
    fn names_are_cleaned_of_breaking_and_bidi_characters() {
        assert_eq!(
            clean_name("invoice\u{202e}fdp.exe"),
            "invoice\u{fffd}fdp.exe"
        );
        let all = "\u{0}\n\t\u{1b}\u{7f}\u{85}\u{2028}\u{2029}\u{061c}\u{200e}\u{200f}\
                   \u{202a}\u{202b}\u{202c}\u{202d}\u{202e}\u{2066}\u{2067}\u{2068}\u{2069}";
        assert_eq!(clean_name(all), "\u{fffd}".repeat(all.chars().count()));
        let kept = "Bericht für Café <b>&.odt \u{05d0}\u{0627} \u{1f600} \u{2764}\u{fe0f}";
        assert_eq!(clean_name(kept), kept);
    }

    /// Invisible characters, blanks that are letters and slash look-alikes
    /// are replaced too (M6.5 V-3): a name cannot hide its end or read as a
    /// path.
    #[test]
    fn names_are_cleaned_of_invisible_blank_and_slash_like_characters() {
        let all = "\u{ad}\u{34f}\u{115f}\u{1160}\u{180b}\u{180e}\u{200b}\u{200c}\u{200d}\
                   \u{2044}\u{2060}\u{206a}\u{206f}\u{2215}\u{2800}\u{29f8}\u{3164}\u{feff}\
                   \u{ff0f}\u{ffa0}\u{fff9}\u{fffb}\u{e0001}\u{e0041}\u{e007f}\u{e0100}\u{e01ef}";
        assert_eq!(clean_name(all), "\u{fffd}".repeat(all.chars().count()));
        assert_eq!(
            clean_name("Documents\u{2215}taxes\u{ff0f}2026.pdf"),
            "Documents\u{fffd}taxes\u{fffd}2026.pdf"
        );
        assert_eq!(
            clean_name(&format!("report.pdf{}.exe", "\u{3164}".repeat(40))),
            format!("report.pdf{}.exe", "\u{fffd}".repeat(40))
        );
    }

    /// A private bus in a temporary directory, or `None` without dbus-daemon.
    fn bus() -> Option<(tempfile::TempDir, PrivateBus)> {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return None;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = private_bus(&dbus_daemon, dir.path());
        Some((dir, bus))
    }

    /// The log's lines as JSON, of one op.
    fn lines(log: &std::path::Path, op: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| line["op"] == op)
            .collect()
    }

    #[tokio::test]
    async fn a_notification_carries_its_actions_and_an_escaped_body() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        let notification = Notification::new("Conflict", "a & b <c>.txt")
            .action("show", "Show")
            .action("later", "Later");
        let handle = send_to(&bus.address, &notification).await.expect("sent");
        assert_eq!(handle.bus, bus.address);
        let sent = lines(&log, "notify");
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0]["app"], "PdMount");
        assert_eq!(sent[0]["summary"], "Conflict");
        assert_eq!(sent[0]["body"], "a &amp; b &lt;c&gt;.txt");
        assert_eq!(
            sent[0]["actions"],
            serde_json::json!(["show", "Show", "later", "Later"])
        );
        assert_eq!(sent[0]["id"], handle.id);
        assert_eq!(
            capabilities_on(&bus.address).await,
            Some(Caps {
                actions: true,
                markup: true
            })
        );

        // The same text replaces the first notification.
        let again = Notification::new("Conflict", "two").replacing(&handle);
        let second = send_to(&bus.address, &again).await.expect("sent");
        assert_eq!(second.id, handle.id);
        assert_eq!(lines(&log, "notify")[1]["replaces_id"], handle.id);
    }

    #[tokio::test]
    async fn a_server_without_actions_or_markup_gets_neither() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let options = Options {
            no_actions: true,
            no_markup: true,
            ..Options::default()
        };
        let _stub = connect(Some(&bus.address), &log, options)
            .await
            .expect("stub");
        let notification = Notification::new("s", "a & b").action("show", "Show");
        send_to(&bus.address, &notification).await.expect("sent");
        let sent = lines(&log, "notify");
        assert_eq!(sent[0]["body"], "a & b");
        assert_eq!(sent[0]["actions"], serde_json::json!([]));
        assert_eq!(
            capabilities_on(&bus.address).await,
            Some(Caps {
                actions: false,
                markup: false
            })
        );
    }

    /// A server that takes the name over from one without `body-markup` is
    /// asked again: its bodies are escaped (M6.5 V-4).
    #[tokio::test]
    async fn another_server_is_asked_for_its_capabilities_again() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let plain = Options {
            no_markup: true,
            ..Options::default()
        };
        let first = connect(Some(&bus.address), &log, plain)
            .await
            .expect("stub");
        send_to(&bus.address, &Notification::new("s", "a & b"))
            .await
            .expect("sent");
        assert_eq!(lines(&log, "notify")[0]["body"], "a & b");
        drop(first);
        let _second = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        let deadline = Instant::now() + Duration::from_secs(5);
        while capabilities_on(&bus.address)
            .await
            .is_none_or(|caps| !caps.markup)
        {
            assert!(Instant::now() < deadline, "the old capabilities stayed");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        send_to(&bus.address, &Notification::new("s", "a & b"))
            .await
            .expect("sent");
        let sent = lines(&log, "notify");
        assert_eq!(sent.last().expect("sent")["body"], "a &amp; b");
    }

    /// The next event about a notification on `address`: the receivers
    /// are process-wide, and other tests send on their own buses meanwhile.
    async fn next_on(events: &mut broadcast::Receiver<Event>, address: &str) -> Event {
        let next = async {
            loop {
                let event = events.recv().await.expect("open");
                let (Event::Action { handle, .. } | Event::Closed { handle, .. }) = &event;
                if handle.bus == address {
                    return event;
                }
            }
        };
        tokio::time::timeout(Duration::from_secs(5), next)
            .await
            .expect("an event")
    }

    #[tokio::test]
    async fn actions_and_closes_of_sent_notifications_reach_subscribers() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        let mut events = subscribe();
        let first = send_to(
            &bus.address,
            &Notification::new("a", "b").action("show", "Show"),
        )
        .await
        .expect("sent");
        // Let the listener subscribe before the stub emits.
        tokio::time::sleep(Duration::from_millis(300)).await;

        // A signal about an id this process did not send is not passed on.
        crate::notify_stub::emit(
            Some(bus.address.clone()),
            first.id + 100,
            Some("x".into()),
            0,
        )
        .await
        .expect("emit");
        crate::notify_stub::emit(Some(bus.address.clone()), first.id, Some("show".into()), 0)
            .await
            .expect("emit");
        let event = next_on(&mut events, &bus.address).await;
        assert_eq!(
            event,
            Event::Action {
                handle: first.clone(),
                key: "show".into()
            }
        );

        assert!(close(&first).await);
        let event = next_on(&mut events, &bus.address).await;
        assert_eq!(
            event,
            Event::Closed {
                handle: first.clone(),
                reason: 3
            }
        );
        assert_eq!(lines(&log, "close")[0]["id"], first.id);
    }

    /// No server: nothing fails, nothing waits long, and once a server owns
    /// the name the next call connects again.
    #[tokio::test]
    async fn without_a_server_nothing_is_sent_and_the_next_call_recovers() {
        let Some((dir, bus)) = bus() else { return };
        let start = Instant::now();
        let error = send_to(&bus.address, &Notification::new("a", "b"))
            .await
            .expect_err("no server");
        assert!(start.elapsed() < Duration::from_secs(5), "{error}");
        assert_eq!(capabilities_on(&bus.address).await, None);

        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        send_to(&bus.address, &Notification::new("a", "b"))
            .await
            .expect("sent once the server runs");
    }

    /// A server that stops answering is given up on after the deadline.
    #[tokio::test]
    async fn a_hung_server_is_cut_off() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let options = Options {
            delay: Duration::from_secs(30),
            ..Options::default()
        };
        let _stub = connect(Some(&bus.address), &log, options)
            .await
            .expect("stub");
        let start = Instant::now();
        assert!(
            send_to(&bus.address, &Notification::new("a", "b"))
                .await
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(5));
    }

    /// A bus that takes its time to connect does not hold up a send on
    /// another: the connection is made outside the lock.
    #[tokio::test]
    async fn a_slow_bus_does_not_hold_up_another() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        // A socket that takes the connection and never answers: the
        // handshake waits for its deadline.
        let silent = dir.path().join("silent");
        let _listener = std::os::unix::net::UnixListener::bind(&silent).expect("listener");
        let address = format!("unix:path={}", silent.display());
        let slow =
            tokio::spawn(async move { send_to(&address, &Notification::new("a", "b")).await });
        tokio::time::sleep(Duration::from_millis(300)).await;
        let start = Instant::now();
        send_to(&bus.address, &Notification::new("a", "b"))
            .await
            .expect("sent");
        assert!(
            start.elapsed() < Duration::from_secs(1),
            "the send waited {:?}",
            start.elapsed()
        );
        assert!(slow.await.expect("the slow send").is_err());
    }

    /// A close on the connection to a bus that has gone, whose address the
    /// next bus took (a new login), goes out on a new connection.
    #[tokio::test]
    async fn a_close_after_the_bus_was_replaced_reaches_the_new_one() {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let first = private_bus(&dbus_daemon, dir.path());
        let log = dir.path().join("stub.log");
        let stub = connect(Some(&first.address), &log, Options::default())
            .await
            .expect("stub");
        let handle = send_to(&first.address, &Notification::new("a", "b"))
            .await
            .expect("sent");
        drop(stub);
        drop(first);
        let _ = std::fs::remove_file(dir.path().join("bus"));
        let second = private_bus(&dbus_daemon, dir.path());
        assert_eq!(second.address, handle.bus);
        let _stub = connect(Some(&second.address), &log, Options::default())
            .await
            .expect("stub again");
        assert!(close(&handle).await, "closed on a new connection");
        assert_eq!(lines(&log, "close").len(), 1);
    }

    /// A bus address nobody listens on: an error, at once.
    #[tokio::test]
    async fn an_unreachable_bus_is_an_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let address = format!("unix:path={}", dir.path().join("none").display());
        assert!(
            send_to(&address, &Notification::new("a", "b"))
                .await
                .is_err()
        );
        assert!(
            !close(&Handle {
                bus: address,
                id: 1
            })
            .await
        );
    }
}
