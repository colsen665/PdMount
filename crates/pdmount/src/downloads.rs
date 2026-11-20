//! The slow-download notification (DESIGN.md §14.4, D137, M6.4): when a
//! read a user waits on has waited 5 s for a file's blocks, in any tree
//! (the engine's report, `pdmount_core::slow`), "PdMount: downloading" names
//! the file (not its path) and its size. At most one per file per
//! [`PER_FILE`], and [`BURST`] per [`WINDOW`] through [`Burst`]; the rest
//! are summed in one ("2 more files downloading"), which counts as one of
//! the [`BURST`]. A notification is closed when its wait ends, and every
//! one still open when the daemon stops. `notify.downloads = false` sends
//! none: the daemon then does not subscribe, and the engine measures
//! nothing.

use crate::notify::{Burst, Handle, Notification};
use pdmount_core::slow::{FileKey, SlowRead};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot};

/// A file is named at most once in this time.
const PER_FILE: Duration = Duration::from_secs(10 * 60);
/// At most this many notifications in [`WINDOW`], a sum included.
const BURST: usize = 3;
const WINDOW: Duration = Duration::from_secs(30);

const SUMMARY: &str = "PdMount: downloading";

/// What to show for a report.
#[derive(Debug, PartialEq, Eq)]
enum Show {
    Nothing,
    /// The file, by name.
    One,
    /// The held files, summed: their waits and files.
    Sum(HashMap<u64, FileKey>),
}

/// The limits, apart from the bus: which reports become which notification.
struct Policy {
    /// Files named or summed, and when.
    shown: HashMap<FileKey, Instant>,
    /// Three of their own per window; once one is held, later ones wait
    /// with it, and their sum counts as one of the three.
    burst: Burst,
    /// Waits over the burst, not shown yet and not ended, with their files.
    held: HashMap<u64, FileKey>,
}

impl Policy {
    fn new(window: Duration) -> Self {
        Self {
            shown: HashMap::new(),
            burst: Burst::new(BURST, window),
            held: HashMap::new(),
        }
    }

    /// Wait `wait` of `file` began at `now`.
    fn began(&mut self, now: Instant, wait: u64, file: FileKey) -> Show {
        self.shown
            .retain(|_, at| now.duration_since(*at) < PER_FILE);
        if self.shown.contains_key(&file) {
            return Show::Nothing;
        }
        self.drop_empty_hold(now);
        self.shown.insert(file, now);
        if self.burst.admit(now) {
            return Show::One;
        }
        self.held.insert(wait, file);
        Show::Nothing
    }

    /// The server did not take `file`'s own notification: the file is not
    /// named, and the send does not use up the window.
    fn not_taken(&mut self, file: FileKey) {
        self.shown.remove(&file);
        self.burst.retract();
    }

    /// The server did not take the sum of `files`, as [`Policy::not_taken`].
    fn sum_not_taken(&mut self, files: impl IntoIterator<Item = FileKey>) {
        for file in files {
            self.shown.remove(&file);
        }
        self.burst.retract();
    }

    /// Wait `wait` ended: a held one is no longer summed.
    fn ended(&mut self, wait: u64) {
        self.held.remove(&wait);
    }

    /// Reports were missed: the held waits may have ended unseen.
    fn lagged(&mut self) {
        self.held.clear();
    }

    /// When held waits may be summed.
    fn deadline(&self) -> Option<Instant> {
        self.burst.deadline()
    }

    /// At the deadline: the sum of the held waits, if any are left.
    fn due(&mut self, now: Instant) -> Show {
        match self.burst.flush(now) {
            None => Show::Nothing,
            Some(_) if self.held.is_empty() => {
                // Every held wait ended: nothing goes out, nothing counts.
                self.burst.retract();
                Show::Nothing
            }
            Some(_) => Show::Sum(std::mem::take(&mut self.held)),
        }
    }

    /// Lets the burst forget a hold whose waits all ended, once the window
    /// has room, so the next file is not held behind nothing.
    fn drop_empty_hold(&mut self, now: Instant) {
        if self.held.is_empty()
            && self.burst.deadline().is_some()
            && self.burst.flush(now).is_some()
        {
            self.burst.retract();
        }
    }
}

/// A size as people read it: "633 MiB", "4.9 GiB".
fn size_text(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value < 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.0} {}", UNITS[unit])
    }
}

/// One file's notification. The name comes from another device perhaps
/// (D070): it is cleaned ([`crate::notify::clean_name`]), and `notify`
/// escapes it for a server that reads markup.
fn one(name: &str, size: u64) -> Notification {
    Notification::new(
        SUMMARY,
        format!(
            "{} ({}) is downloading; it opens as its blocks arrive.",
            crate::notify::clean_name(name),
            size_text(size)
        ),
    )
}

/// The sum of `count` files over the burst.
fn sum(count: usize) -> Notification {
    let body = if count == 1 {
        "1 more file downloading; it opens as its blocks arrive.".to_owned()
    } else {
        format!("{count} more files downloading; they open as their blocks arrive.")
    };
    Notification::new(SUMMARY, body)
}

/// Announces the session's slow reads until `stop` fires (or its sender
/// is dropped), then closes the notifications still open; or until the
/// receiver closes.
pub async fn announce(events: broadcast::Receiver<SlowRead>, stop: oneshot::Receiver<()>) {
    announce_on(None, WINDOW, events, stop).await;
}

/// [`announce`] on the bus `address` (a test's), or on the route of every
/// notification, with `window` in place of [`WINDOW`].
async fn announce_on(
    address: Option<String>,
    window: Duration,
    mut events: broadcast::Receiver<SlowRead>,
    mut stop: oneshot::Receiver<()>,
) {
    use broadcast::error::RecvError;
    let target = Target(address);
    let mut policy = Policy::new(window);
    // The notifications open now: one per wait, and the sum with its waits.
    let mut open: HashMap<u64, Handle> = HashMap::new();
    let mut summed: Option<(Handle, HashSet<u64>)> = None;
    loop {
        let deadline = policy.deadline();
        let event = tokio::select! {
            _ = &mut stop => {
                close_all(&mut open, &mut summed).await;
                return;
            }
            event = events.recv() => event,
            () = sleep_until(deadline), if deadline.is_some() => {
                if let Show::Sum(waits) = policy.due(Instant::now()) {
                    show_sum(&target, &mut policy, &mut summed, waits).await;
                }
                continue;
            }
        };
        match event {
            Ok(SlowRead::Began {
                wait,
                file,
                name,
                size,
            }) => match policy.began(Instant::now(), wait, file) {
                Show::Nothing => {}
                Show::One => {
                    tracing::debug!(wait, "a slow download is announced");
                    match target.send(&one(&name, size)).await {
                        Some(handle) => {
                            open.insert(wait, handle);
                        }
                        None => policy.not_taken(file),
                    }
                }
                Show::Sum(waits) => show_sum(&target, &mut policy, &mut summed, waits).await,
            },
            Ok(SlowRead::Ended { wait }) => {
                policy.ended(wait);
                if let Some(handle) = open.remove(&wait) {
                    crate::notify::close(&handle).await;
                }
                let all_ended = summed.as_mut().is_some_and(|(_, waits)| {
                    waits.remove(&wait);
                    waits.is_empty()
                });
                if all_ended && let Some((handle, _)) = summed.take() {
                    crate::notify::close(&handle).await;
                }
            }
            // The missed reports may have been ends: what is open is
            // closed, rather than left to count finished files.
            Err(RecvError::Lagged(missed)) => {
                tracing::debug!(
                    missed,
                    "slow-read reports were missed; closing what is open"
                );
                policy.lagged();
                close_all(&mut open, &mut summed).await;
            }
            Err(RecvError::Closed) => return,
        }
    }
}

/// Closes every notification still open, the sum included, at once.
async fn close_all(open: &mut HashMap<u64, Handle>, summed: &mut Option<(Handle, HashSet<u64>)>) {
    let handles: Vec<Handle> = open
        .drain()
        .map(|(_, handle)| handle)
        .chain(summed.take().map(|(handle, _)| handle))
        .collect();
    futures::future::join_all(handles.iter().map(crate::notify::close)).await;
}

/// Sends the sum of `waits` and of the waits still open in the sum on
/// screen, replacing it (its waits stay counted until they end). A sum the
/// server did not take leaves the one on screen as it was.
async fn show_sum(
    target: &Target,
    policy: &mut Policy,
    summed: &mut Option<(Handle, HashSet<u64>)>,
    waits: HashMap<u64, FileKey>,
) {
    let mut all: HashSet<u64> = summed
        .as_ref()
        .map(|(_, open)| open.clone())
        .unwrap_or_default();
    all.extend(waits.keys().copied());
    let mut notification = sum(all.len());
    if let Some((previous, _)) = summed.as_ref() {
        notification = notification.replacing(previous);
    }
    match target.send(&notification).await {
        Some(handle) => *summed = Some((handle, all)),
        None => policy.sum_not_taken(waits.into_values()),
    }
}

/// Where the notifications go: a test's bus, or the route of every
/// notification.
struct Target(Option<String>);

impl Target {
    async fn send(&self, notification: &Notification) -> Option<Handle> {
        match &self.0 {
            Some(address) => crate::notify::send_to(address, notification).await.ok(),
            None => crate::notify::send(notification).await,
        }
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: FileKey = (1, 10);

    fn held(waits: &[u64]) -> Show {
        Show::Sum(waits.iter().map(|&n| (n, (1, n))).collect())
    }

    #[test]
    fn a_file_is_named_once_per_ten_minutes() {
        let mut policy = Policy::new(WINDOW);
        let start = Instant::now();
        assert_eq!(policy.began(start, 1, A), Show::One);
        policy.ended(1);
        assert_eq!(
            policy.began(start + Duration::from_secs(60), 2, A),
            Show::Nothing
        );
        assert_eq!(policy.began(start + PER_FILE, 3, A), Show::One);
    }

    /// Three in the window, the sum one of them: the fourth file waits for
    /// the window, and the sum takes a place in the next one.
    #[test]
    fn three_per_window_the_sum_included() {
        let mut policy = Policy::new(WINDOW);
        let start = Instant::now();
        for n in 0..3 {
            assert_eq!(policy.began(start, n, (1, n)), Show::One, "file {n}");
        }
        for n in 3..7 {
            assert_eq!(policy.began(start, n, (1, n)), Show::Nothing, "file {n}");
        }
        policy.ended(6);
        assert_eq!(policy.deadline(), Some(start + WINDOW));
        assert_eq!(policy.due(start + Duration::from_secs(10)), Show::Nothing);
        assert_eq!(
            policy.due(start + WINDOW),
            held(&[3, 4, 5]),
            "the ended one is not counted"
        );
        // The next window holds the sum: two more of their own.
        let next = start + WINDOW;
        assert_eq!(policy.began(next, 7, (1, 7)), Show::One);
        assert_eq!(policy.began(next, 8, (1, 8)), Show::One);
        assert_eq!(policy.began(next, 9, (1, 9)), Show::Nothing);
        assert_eq!(policy.deadline(), Some(next + WINDOW));
    }

    #[test]
    fn a_sum_whose_waits_all_ended_is_not_sent_and_takes_no_place() {
        let mut policy = Policy::new(WINDOW);
        let start = Instant::now();
        for n in 0..4 {
            policy.began(start, n, (1, n));
        }
        policy.ended(3);
        assert_eq!(policy.due(start + WINDOW), Show::Nothing);
        assert_eq!(policy.deadline(), None);
        let next = start + WINDOW;
        for n in 4..7 {
            assert_eq!(policy.began(next, n, (1, n)), Show::One, "file {n}");
        }
    }

    /// A held wait that ended does not hold back the next file once the
    /// window has room.
    #[test]
    fn an_empty_hold_does_not_hold_the_next_file() {
        let mut policy = Policy::new(WINDOW);
        let start = Instant::now();
        for n in 0..4 {
            policy.began(start, n, (1, n));
        }
        policy.ended(3);
        assert_eq!(policy.began(start + WINDOW, 4, (1, 4)), Show::One);
    }

    /// A send the server did not take neither names the file nor uses up
    /// the window; neither does a sum.
    #[test]
    fn sends_not_taken_leave_no_trace() {
        let mut policy = Policy::new(WINDOW);
        let start = Instant::now();
        assert_eq!(policy.began(start, 0, (1, 0)), Show::One);
        policy.not_taken((1, 0));
        assert_eq!(policy.began(start, 1, (1, 0)), Show::One, "named again");
        for n in 2..4 {
            assert_eq!(policy.began(start, n, (1, n)), Show::One, "file {n}");
        }
        assert_eq!(policy.began(start, 4, (1, 4)), Show::Nothing);
        let Show::Sum(waits) = policy.due(start + WINDOW) else {
            panic!("a sum is due");
        };
        policy.sum_not_taken(waits.into_values());
        assert_eq!(policy.began(start + WINDOW, 5, (1, 4)), Show::One);
    }

    #[test]
    fn missed_reports_drop_the_hold() {
        let mut policy = Policy::new(WINDOW);
        let start = Instant::now();
        for n in 0..5 {
            policy.began(start, n, (1, n));
        }
        policy.lagged();
        assert_eq!(policy.due(start + WINDOW), Show::Nothing);
    }

    #[test]
    fn texts_name_the_file_and_its_size() {
        assert_eq!(size_text(512), "512 B");
        assert_eq!(size_text(633 << 20), "633 MiB");
        assert_eq!(size_text((49 << 30) / 10), "4.9 GiB");
        let note = one("clip\n.mp4", 633 << 20);
        assert_eq!(note.summary, "PdMount: downloading");
        assert_eq!(
            note.body,
            "clip\u{fffd}.mp4 (633 MiB) is downloading; it opens as its blocks arrive."
        );
        assert_eq!(
            one("invoice\u{202e}fdp.exe", 1).body,
            "invoice\u{fffd}fdp.exe (1 B) is downloading; it opens as its blocks arrive."
        );
        assert_eq!(
            sum(2).body,
            "2 more files downloading; they open as their blocks arrive."
        );
        assert_eq!(
            sum(1).body,
            "1 more file downloading; it opens as its blocks arrive."
        );
    }

    // ---- On a private bus with the stub ----

    use crate::bus::tests::{PrivateBus, private_bus};
    use crate::notify_stub::{Options, connect};

    fn bus() -> Option<(tempfile::TempDir, PrivateBus)> {
        let Some(dbus_daemon) = crate::doctor::find_in_path("dbus-daemon") else {
            eprintln!("dbus-daemon not installed; skipped");
            return None;
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let bus = private_bus(&dbus_daemon, dir.path());
        Some((dir, bus))
    }

    fn lines(log: &std::path::Path, op: &str) -> Vec<serde_json::Value> {
        std::fs::read_to_string(log)
            .unwrap_or_default()
            .lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .filter(|line| line["op"] == op)
            .collect()
    }

    async fn wait_lines(log: &std::path::Path, op: &str, count: usize) -> Vec<serde_json::Value> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let found = lines(log, op);
            if found.len() >= count || Instant::now() > deadline {
                return found;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn began(wait: u64, file: FileKey, name: &str) -> SlowRead {
        SlowRead::Began {
            wait,
            file,
            name: name.into(),
            size: 8 << 20,
        }
    }

    /// A slow read is named, escaped, and closed when it ends; a second
    /// report of the file sends nothing.
    #[tokio::test]
    async fn a_report_is_shown_escaped_and_closed_at_its_end() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        let (sender, events) = broadcast::channel(16);
        let (_stop, stop) = oneshot::channel();
        let task = tokio::spawn(announce_on(Some(bus.address.clone()), WINDOW, events, stop));
        sender.send(began(1, A, "<b>&.mp4")).expect("send");
        sender.send(SlowRead::Ended { wait: 1 }).expect("send");
        sender.send(began(2, A, "<b>&.mp4")).expect("send");
        sender.send(SlowRead::Ended { wait: 2 }).expect("send");
        let closes = wait_lines(&log, "close", 1).await;
        tokio::time::sleep(Duration::from_millis(200)).await;
        let notes = lines(&log, "notify");
        assert_eq!(notes.len(), 1, "{notes:?}");
        assert_eq!(notes[0]["summary"], "PdMount: downloading");
        assert_eq!(
            notes[0]["body"],
            "&lt;b&gt;&amp;.mp4 (8.0 MiB) is downloading; it opens as its blocks arrive."
        );
        assert_eq!(closes.len(), 1);
        assert_eq!(closes[0]["id"], notes[0]["id"]);
        task.abort();
    }

    /// Five files at once: three named (a bidi name cleaned), the other two
    /// summed when the window has room; the sum is closed when both of its
    /// waits have ended.
    #[tokio::test]
    async fn a_burst_is_named_then_summed_and_the_sum_closed() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        let (sender, events) = broadcast::channel(16);
        let (_stop, stop) = oneshot::channel();
        let window = Duration::from_secs(1);
        let task = tokio::spawn(announce_on(Some(bus.address.clone()), window, events, stop));
        sender
            .send(began(0, (1, 0), "invoice\u{202e}fdp.exe"))
            .expect("send");
        for n in 1..5 {
            sender
                .send(began(n, (1, n), &format!("f{n}.bin")))
                .expect("send");
        }
        let notes = wait_lines(&log, "notify", 3).await;
        assert_eq!(notes.len(), 3, "{notes:?}");
        assert_eq!(
            notes[0]["body"],
            "invoice\u{fffd}fdp.exe (8.0 MiB) is downloading; it opens as its blocks arrive."
        );
        assert_eq!(
            notes[2]["body"],
            "f2.bin (8.0 MiB) is downloading; it opens as its blocks arrive."
        );
        tokio::time::sleep(window / 2).await;
        assert_eq!(
            lines(&log, "notify").len(),
            3,
            "the sum waits for the window"
        );
        let notes = wait_lines(&log, "notify", 4).await;
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert_eq!(
            notes[3]["body"],
            "2 more files downloading; they open as their blocks arrive."
        );
        sender.send(SlowRead::Ended { wait: 3 }).expect("send");
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(
            lines(&log, "close").is_empty(),
            "one wait of the sum goes on"
        );
        sender.send(SlowRead::Ended { wait: 4 }).expect("send");
        let closes = wait_lines(&log, "close", 1).await;
        assert_eq!(closes.len(), 1, "{closes:?}");
        assert_eq!(closes[0]["id"], notes[3]["id"]);
        task.abort();
    }

    /// The stop closes every notification still open; missed reports close
    /// the open ones too.
    #[tokio::test]
    async fn the_stop_and_missed_reports_close_what_is_open() {
        let Some((dir, bus)) = bus() else { return };
        let log = dir.path().join("stub.log");
        let _stub = connect(Some(&bus.address), &log, Options::default())
            .await
            .expect("stub");
        // Lagged: two reports fit; the task reads nothing while the test
        // sends (a current-thread runtime).
        let (sender, events) = broadcast::channel(2);
        let (_stop, stop) = oneshot::channel();
        let task = tokio::spawn(announce_on(Some(bus.address.clone()), WINDOW, events, stop));
        sender.send(began(1, (1, 1), "a.bin")).expect("send");
        let notes = wait_lines(&log, "notify", 1).await;
        assert_eq!(notes.len(), 1, "{notes:?}");
        for wait in 100..105 {
            sender.send(SlowRead::Ended { wait }).expect("send");
        }
        let closes = wait_lines(&log, "close", 1).await;
        assert_eq!(closes.len(), 1, "{closes:?}");
        assert_eq!(closes[0]["id"], notes[0]["id"]);
        task.abort();

        // The stop: three files of their own and a sum of one are open.
        let (sender, events) = broadcast::channel(16);
        let (stop_tx, stop) = oneshot::channel();
        let task = tokio::spawn(announce_on(
            Some(bus.address.clone()),
            Duration::from_secs(1),
            events,
            stop,
        ));
        for n in 2..6 {
            sender.send(began(n, (1, n), "b.bin")).expect("send");
        }
        let notes = wait_lines(&log, "notify", 5).await;
        assert_eq!(notes.len(), 5, "{notes:?}");
        assert_eq!(
            notes[4]["body"],
            "1 more file downloading; it opens as its blocks arrive."
        );
        stop_tx.send(()).expect("the task runs");
        tokio::time::timeout(Duration::from_secs(10), task)
            .await
            .expect("the task ends at the stop")
            .expect("no panic");
        let closes = lines(&log, "close");
        assert_eq!(closes.len(), 5, "{closes:?}");
        let mut closed: Vec<_> = closes[1..].iter().map(|line| line["id"].clone()).collect();
        let mut sent: Vec<_> = notes[1..].iter().map(|line| line["id"].clone()).collect();
        closed.sort_by_key(|id| id.as_u64());
        sent.sort_by_key(|id| id.as_u64());
        assert_eq!(closed, sent);
    }
}
