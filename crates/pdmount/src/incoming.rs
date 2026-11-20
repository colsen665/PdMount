//! Pending markers of other devices (DESIGN.md §14.4, D076, D126): one
//! desktop notification per tree and pass that found new markers, and the
//! `Incoming:` lines of `status`. The engine reads and checks the markers
//! (`pdmount_core::engine::marker`), at most `MAX_MARKERS_READ` per tree;
//! names in a notification are cleaned once more of the characters that
//! would reorder its text (`notify::clean_name`).

use pdmount_core::engine::marker::{IncomingMarker, IncomingNotice};
use pdmount_core::units::ByteSize;
use std::time::SystemTime;

/// A notification names at most this many devices (D126).
const MAX_NAMES: usize = 3;
/// `status` shows at most this many `Incoming:` lines per tree, then
/// `+N more` (D126).
pub const MAX_STATUS_LINES: usize = 5;

/// Announces My Files' notices, until the engine is gone.
pub async fn announce(notices: tokio::sync::broadcast::Receiver<IncomingNotice>) {
    announce_in(None, notices).await;
}

/// Announces each notice the engine sends, one notification each, naming
/// the tree when it is a further one; until the engine is gone.
pub async fn announce_in(
    tree: Option<String>,
    mut notices: tokio::sync::broadcast::Receiver<IncomingNotice>,
) {
    use tokio::sync::broadcast::error::RecvError;
    loop {
        match notices.recv().await {
            Ok(notice) => {
                if let Some((summary, body)) = notification(tree.as_deref(), &notice) {
                    crate::notify::desktop(&summary, &body);
                }
            }
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

/// The notification's summary and body; `None` for a notice without
/// markers. The summary is plain text in the notification protocol and
/// names at most [`MAX_NAMES`] devices; the body may be read as markup, so
/// it carries no text from the markers but numbers.
fn notification(tree: Option<&str>, notice: &IncomingNotice) -> Option<(String, String)> {
    let markers = &notice.markers;
    let first = markers.first()?;
    let place = tree.map_or_else(String::new, |name| {
        format!(" in {}", crate::notify::clean_name(name))
    });
    let summary = if markers.len() == 1 {
        format!(
            "PdMount: changes on their way{place} from {}",
            crate::notify::clean_name(&first.device_label())
        )
    } else {
        let mut names: Vec<String> = markers
            .iter()
            .take(MAX_NAMES)
            .map(|marker| crate::notify::clean_name(&marker.device_label()))
            .collect();
        if markers.len() > MAX_NAMES {
            names.push(format!("{} more", markers.len() - MAX_NAMES));
        }
        format!(
            "PdMount: changes on their way{place} from {} devices: {}",
            markers.len(),
            names.join(", ")
        )
    };
    let entries = markers
        .iter()
        .fold(0u64, |sum, marker| sum.saturating_add(marker.entries));
    let bytes = markers
        .iter()
        .fold(0u64, |sum, marker| sum.saturating_add(marker.bytes));
    let oldest = markers.iter().map(|marker| marker.time).min().unwrap_or(0);
    let since = if markers.len() == 1 {
        "since"
    } else {
        "the oldest since"
    };
    let not_read = match notice.not_read {
        0 => String::new(),
        1 => " 1 more marker was not read.".to_owned(),
        n => format!(" {n} more markers were not read."),
    };
    let body = format!(
        "{} left to upload, {since} {}.{not_read} See `pdmount status`.",
        changes(entries, bytes),
        crate::status::local_time(oldest)
    );
    Some((summary, body))
}

/// `3 changes (24.0 MiB)`.
fn changes(entries: u64, bytes: u64) -> String {
    let noun = if entries == 1 { "change" } else { "changes" };
    format!("{entries} {noun} ({})", ByteSize(bytes).human())
}

/// One line per marker for `status`: device, changes, since when, and how
/// long ago; `stale` after 7 days. At most [`MAX_STATUS_LINES`], then
/// `+N more` with the rest and the `not_read` markers the cap left out.
pub fn status_lines(markers: &[IncomingMarker], not_read: u64, now: SystemTime) -> Vec<String> {
    let mut lines: Vec<String> = markers
        .iter()
        .take(MAX_STATUS_LINES)
        .map(|marker| {
            let age = crate::status::short_age(marker.age(now).as_secs());
            let stale = if marker.is_stale(now) { ", stale" } else { "" };
            format!(
                "{}: {}, since {} ({age} ago{stale})",
                marker.device_label(),
                changes(marker.entries, marker.bytes),
                crate::status::local_time(marker.time)
            )
        })
        .collect();
    let more = (markers.len().saturating_sub(MAX_STATUS_LINES) as u64).saturating_add(not_read);
    if more > 0 {
        lines.push(format!("+{more} more"));
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, UNIX_EPOCH};

    fn marker(device: Option<&str>, time: u64, entries: u64) -> IncomingMarker {
        IncomingMarker {
            device_id: "0b5f6c1e-6a7d-4c2b-9f1e-2a3b4c5d6e7f".into(),
            device: device.map(str::to_owned),
            time,
            entries,
            bytes: 25 << 20,
        }
    }

    fn notice(markers: Vec<IncomingMarker>, not_read: u64) -> IncomingNotice {
        IncomingNotice { markers, not_read }
    }

    #[test]
    fn status_lines_show_device_changes_and_age() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let lines = status_lines(
            &[
                marker(Some("laptop"), 1_800_000_000 - 300, 3),
                marker(None, 1_800_000_000 - 8 * 24 * 3600, 1),
            ],
            0,
            now,
        );
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].starts_with("laptop: 3 changes (25.0 MiB), since "),
            "{}",
            lines[0]
        );
        assert!(lines[0].ends_with(" (5m ago)"), "{}", lines[0]);
        assert!(
            lines[1].starts_with("device 0b5f6c1e: 1 change (25.0 MiB), since "),
            "{}",
            lines[1]
        );
        assert!(lines[1].ends_with(" (192h ago, stale)"), "{}", lines[1]);
    }

    /// D126: at most `MAX_STATUS_LINES`, then one line with the rest and
    /// the markers the engine did not read.
    #[test]
    fn status_lines_are_capped_with_a_count_of_the_rest() {
        let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let many: Vec<IncomingMarker> = (0..16)
            .map(|n| marker(Some(&format!("d{n}")), 1_800_000_000, 1))
            .collect();
        let lines = status_lines(&many, 24, now);
        assert_eq!(lines.len(), MAX_STATUS_LINES + 1, "{lines:?}");
        assert_eq!(lines.last().map(String::as_str), Some("+35 more"));
        // Only markers not read: one line for them.
        assert_eq!(status_lines(&[], 3, now), vec!["+3 more".to_owned()]);
        // A marker whose time no clock holds does not panic (V-8).
        let lines = status_lines(&[marker(Some("x"), u64::MAX, 1)], 0, now);
        assert!(lines[0].contains("an unknown time"), "{lines:?}");
    }

    #[test]
    fn the_notification_body_holds_no_marker_text() {
        let (summary, body) = notification(
            None,
            &notice(vec![marker(Some("<b>x</b>"), 1_800_000_000, 2)], 0),
        )
        .expect("a notification");
        assert_eq!(summary, "PdMount: changes on their way from <b>x</b>");
        assert!(body.starts_with("2 changes (25.0 MiB) left to upload, since "));
        assert!(!body.contains('<'));
    }

    /// D126: one notification for a pass, with the count, at most three
    /// names, the sums, and how many markers the cap left out.
    #[test]
    fn one_notification_names_the_count_and_a_few_devices() {
        let markers: Vec<IncomingMarker> = (0..16)
            .map(|n| marker(Some(&format!("dev{n}")), 1_800_000_000 + n, 2))
            .collect();
        let (summary, body) =
            notification(Some("Team"), &notice(markers, 24)).expect("a notification");
        assert_eq!(
            summary,
            "PdMount: changes on their way in Team from 16 devices: dev0, dev1, dev2, 13 more"
        );
        assert!(
            body.starts_with("32 changes (400.0 MiB) left to upload, the oldest since "),
            "{body}"
        );
        assert!(body.contains(" 24 more markers were not read."), "{body}");
        // Two: both named, no "more".
        let (summary, _) = notification(
            None,
            &notice(vec![marker(Some("a"), 1, 1), marker(None, 2, u64::MAX)], 0),
        )
        .expect("a notification");
        assert_eq!(
            summary,
            "PdMount: changes on their way from 2 devices: a, device 0b5f6c1e"
        );
        // A tree's and a device's name lose control and bidi characters.
        let (summary, _) = notification(
            Some("x\u{1b}[2Jy"),
            &notice(vec![marker(Some("a\u{202e}b"), 1, 1)], 0),
        )
        .expect("a notification");
        assert_eq!(
            summary,
            "PdMount: changes on their way in x\u{fffd}[2Jy from a\u{fffd}b"
        );
        assert!(notification(None, &notice(Vec::new(), 5)).is_none());
    }
}
