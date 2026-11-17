//! Desktop notifications (DESIGN.md §12.3, §10.8): `notify-send`, when a
//! session bus and the tool exist. Best effort: nothing here fails, and only
//! [`desktop_sent`] waits, for a bounded time. With the desktop's bus attached
//! (D102, `bus.rs`), the bus is chosen for notifications on their own (D102
//! amended): the attached bus runs `notify-send` with that bus in its
//! environment, the daemon's own bus without, and with neither none is sent.

use crate::bus::Route;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// At most one notification in this time; the others are coalesced.
const MIN_GAP: Duration = Duration::from_secs(30);

/// Spaces notifications [`MIN_GAP`] apart: what arrives in between is
/// counted and announced together when the gap is over.
#[derive(Default)]
pub struct Coalescer {
    last: Option<Instant>,
    held: u32,
}

impl Coalescer {
    /// One more event at `now`: how many to announce now, if a notification
    /// may go out.
    pub fn add(&mut self, now: Instant) -> Option<u32> {
        self.held += 1;
        self.flush(now)
    }

    /// The held events, when a notification may go out at `now`.
    pub fn flush(&mut self, now: Instant) -> Option<u32> {
        if self.held == 0
            || self
                .last
                .is_some_and(|last| now.duration_since(last) < MIN_GAP)
        {
            return None;
        }
        self.last = Some(now);
        Some(std::mem::take(&mut self.held))
    }

    /// When held events may go out.
    pub fn deadline(&self) -> Option<Instant> {
        (self.held > 0).then(|| self.last.map_or_else(Instant::now, |last| last + MIN_GAP))
    }
}

/// Whether notifications can work in this process: a bus, the attached one
/// or the process's own, and the tool.
fn available() -> bool {
    (crate::bus::attached().is_some() || crate::bus::own_address().is_some())
        && crate::doctor::find_in_path("notify-send").is_some()
}

/// Whether a notification would reach a desktop now: a bus with a
/// notification server that owns or can start its name, the attached one or
/// the process's own (D102 amended, D106).
pub async fn deliverable() -> bool {
    if crate::doctor::find_in_path("notify-send").is_none() {
        return false;
    }
    match crate::bus::route(crate::bus::Service::Notifications).await {
        Route::Attached(_) | Route::Own => true,
        Route::None => false,
        Route::Default => match crate::bus::own_address() {
            Some(own) => crate::bus::has_notification_server(&own).await,
            None => false,
        },
    }
}

/// The bus for `notify-send`: `Some(Some(address))` the attached one,
/// `Some(None)` the process's own, `None` no bus to send on.
fn bus_of(route: Route) -> Option<Option<String>> {
    match route {
        Route::Attached(bus) => Some(Some(bus)),
        Route::Own => Some(None),
        Route::Default => crate::bus::own_address().map(|_| None),
        Route::None => None,
    }
}

/// `notify-send` with the attached bus, if any, in its environment.
fn command(program: &str, summary: &str, body: &str, bus: Option<&str>) -> Command {
    let mut command = Command::new(program);
    command
        .args(argv(summary, body))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if let Some(bus) = bus {
        command.env("DBUS_SESSION_BUS_ADDRESS", bus);
    }
    command
}

/// `notify-send`'s arguments. Other local users can read them in
/// `/proc/<pid>/cmdline`, so they never carry secrets (D071).
pub fn argv<'a>(summary: &'a str, body: &'a str) -> [&'a str; 4] {
    ["--app-name=PdMount", "--", summary, body]
}

/// Shows a notification: runs `notify-send` on a thread of its own and does
/// not wait for it.
pub fn desktop(summary: &str, body: &str) {
    if !available() {
        return;
    }
    let (summary, body) = (summary.to_owned(), body.to_owned());
    std::thread::spawn(move || {
        // A chosen attached bus was checked just now (still there, still
        // this user's; D102): GDBus does not check an address's `guid=`.
        let Some(bus) = bus_of(crate::bus::route_blocking(
            crate::bus::Service::Notifications,
        )) else {
            return;
        };
        let mut child = match command("notify-send", &summary, &body, bus.as_deref()).spawn() {
            Ok(child) => child,
            Err(error) => {
                tracing::debug!(%error, "cannot run notify-send");
                return;
            }
        };
        // Reaped here so no zombie is left. After a failure the next
        // notification chooses its bus afresh, and a failure on the attached
        // bus checks whether that bus is gone (logout).
        let failed = !child.wait().is_ok_and(|status| status.success());
        if failed {
            crate::bus::invalidate();
            if let Some(bus) = bus {
                crate::bus::drop_if_gone(&bus);
            }
        }
    });
}

/// Like [`desktop`], but waits for `notify-send`, at most [`SENT_TIMEOUT`],
/// and says whether it succeeded: false when there is no session bus or
/// tool, it failed, or it did not finish in time (it is then killed). M5.2:
/// a follow-up is sent only if the first notification went out.
pub async fn desktop_sent(summary: &str, body: &str) -> bool {
    if !available() {
        return false;
    }
    let Some(bus) = bus_of(crate::bus::route(crate::bus::Service::Notifications).await) else {
        return false;
    };
    let sent = sent_by("notify-send", summary, body, bus.as_deref(), SENT_TIMEOUT).await;
    if !sent {
        crate::bus::invalidate();
        if let Some(bus) = bus {
            crate::bus::drop_if_gone_async(&bus).await;
        }
    }
    sent
}

/// How long [`desktop_sent`] waits: a hung session bus must not hold up
/// the caller, or the daemon's exit.
const SENT_TIMEOUT: Duration = Duration::from_secs(10);

async fn sent_by(
    program: &str,
    summary: &str,
    body: &str,
    bus: Option<&str>,
    timeout: Duration,
) -> bool {
    // Not a blocking thread: the runtime waits for those when it is dropped.
    let status = tokio::process::Command::from(command(program, summary, body, bus))
        .kill_on_drop(true)
        .status();
    match tokio::time::timeout(timeout, status).await {
        Ok(status) => status.is_ok_and(|status| status.success()),
        Err(_) => {
            // Dropping the future killed it; tokio reaps it.
            tracing::debug!(?timeout, "notify-send did not finish; not sent");
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_inside_the_gap_are_held_and_announced_together() {
        let mut notices = Coalescer::default();
        let start = Instant::now();
        assert_eq!(notices.add(start), Some(1));
        assert_eq!(notices.deadline(), None);
        assert_eq!(notices.add(start + Duration::from_secs(5)), None);
        assert_eq!(notices.add(start + Duration::from_secs(29)), None);
        assert_eq!(notices.deadline(), Some(start + MIN_GAP));
        assert_eq!(notices.flush(start + Duration::from_secs(29)), None);
        assert_eq!(notices.flush(start + MIN_GAP), Some(2));
        assert_eq!(notices.flush(start + MIN_GAP), None, "nothing is held");
        assert_eq!(
            notices.add(start + Duration::from_secs(31)),
            None,
            "the gap restarted"
        );
    }

    /// A `notify-send` that hangs is killed at the limit and counts as not
    /// sent; one that exits 0 counts as sent.
    #[tokio::test]
    async fn a_hung_notify_send_is_cut_off() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hang = dir.path().join("hang");
        let pid_file = dir.path().join("pid");
        crate::test_support::write_executable(
            &hang,
            &format!(
                "#!/bin/sh\necho $$ >{}\nexec sleep 60\n",
                pid_file.display()
            ),
        );
        let start = Instant::now();
        let hang = hang.to_str().expect("utf-8");
        assert!(!sent_by(hang, "s", "b", None, Duration::from_millis(300)).await);
        assert!(start.elapsed() < Duration::from_secs(5));
        let pid = std::fs::read_to_string(&pid_file).expect("pid");
        let stat = format!("/proc/{}/stat", pid.trim());
        // Gone, or a zombie until tokio reaps it.
        let running = || {
            std::fs::read_to_string(&stat).is_ok_and(|stat| {
                stat.rsplit_once(") ")
                    .is_some_and(|(_, rest)| !rest.starts_with('Z'))
            })
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        while running() {
            assert!(Instant::now() < deadline, "the child was not killed");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(sent_by("true", "s", "b", None, Duration::from_secs(5)).await);
        assert!(!sent_by("false", "s", "b", None, Duration::from_secs(5)).await);
    }

    /// D102: with the desktop's bus attached, `notify-send` gets that bus;
    /// without, the process's own environment.
    #[tokio::test]
    async fn notify_send_runs_on_the_attached_bus() {
        let dir = tempfile::tempdir().expect("tempdir");
        let stub = dir.path().join("notify-send");
        let script = "#!/bin/sh\nprintf '%s|%s\\n' \"${DBUS_SESSION_BUS_ADDRESS-unset}\" \"$*\" >>\"$0.env\"\n";
        crate::test_support::write_executable(&stub, script);
        let stub = stub.to_str().expect("utf-8");
        let desktop = "unix:path=/run/user/1000/desktop-bus";
        assert!(sent_by(stub, "s", "b", Some(desktop), Duration::from_secs(5)).await);
        let own = std::env::var("DBUS_SESSION_BUS_ADDRESS").unwrap_or_else(|_| "unset".into());
        assert!(sent_by(stub, "s", "b", None, Duration::from_secs(5)).await);
        let text = std::fs::read_to_string(format!("{stub}.env")).expect("read");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], format!("{desktop}|--app-name=PdMount -- s b"));
        assert_eq!(lines[1], format!("{own}|--app-name=PdMount -- s b"));
    }
}
