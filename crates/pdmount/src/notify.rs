//! Desktop notifications (DESIGN.md §12.3, §10.8): `notify-send`, when a
//! session bus and the tool exist. Best effort: nothing here fails or waits.

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

/// Whether notifications can work in this process.
fn available() -> bool {
    std::env::var_os("DBUS_SESSION_BUS_ADDRESS").is_some_and(|value| !value.is_empty())
        && crate::doctor::find_in_path("notify-send").is_some()
}

/// Shows a notification: spawns `notify-send` and does not wait for it.
pub fn desktop(summary: &str, body: &str) {
    if !available() {
        return;
    }
    let spawned = Command::new("notify-send")
        .arg("--app-name=PdMount")
        .arg("--")
        .arg(summary)
        .arg(body)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    match spawned {
        // Reaped in the background so no zombie is left.
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        Err(error) => tracing::debug!(%error, "cannot run notify-send"),
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
}
