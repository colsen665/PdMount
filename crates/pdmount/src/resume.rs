//! The warning at start when the last session left changes (DESIGN.md
//! §14.4, M5.2, D075, D095).
//!
//! What a stop or a crash left in the journal is known at mount. A log line
//! says so, and a notification says that the changes are uploading; a
//! second one says when they have finished, but only if the first one went
//! out. Nothing here changes what uploads or when.

use pdmount_core::engine::Engine;
use pdmount_core::journal::FlushBarrier;
use std::sync::Arc;
use std::time::Duration;

/// How often the watch looks at the queue.
const POLL: Duration = Duration::from_secs(2);

/// The queue as the last session left it.
pub struct Left {
    /// The entries that are not parked: they upload by themselves.
    barrier: FlushBarrier,
    parked: usize,
}

impl Left {
    /// Reads the journal; call before the background work starts.
    pub fn of(engine: &Engine) -> Self {
        Self {
            barrier: engine.flush_barrier(),
            parked: engine.queue_counts().parked,
        }
    }
}

/// Logs the warning, sends the first notification, and watches the entries
/// until none is left to upload. Runs until then; abort it at a stop.
pub async fn announce(engine: Arc<Engine>, left: Left) {
    let uploading = left.barrier.len();
    if uploading == 0 && left.parked == 0 {
        return;
    }
    tracing::warn!(
        parked = left.parked,
        "{}",
        not_uploaded(uploading + left.parked)
    );
    // Parked entries do not upload by themselves, so they are no "still
    // uploading"; with none other, there is nothing to announce (D095).
    if uploading == 0 {
        return;
    }
    let (summary, body) = started(uploading);
    let sent = crate::notify::desktop_sent(&summary, &body).await;
    loop {
        tokio::time::sleep(POLL).await;
        let (pending, parked) = engine.barrier_queued(&left.barrier);
        if pending > 0 {
            continue;
        }
        tracing::info!(
            uploaded = uploading - parked,
            parked,
            "the changes of the last session are done"
        );
        if sent {
            let (summary, body) = finished(uploading, parked);
            crate::notify::desktop(&summary, &body);
        }
        return;
    }
}

fn entries(n: usize) -> String {
    match n {
        1 => "1 change".to_owned(),
        n => format!("{n} changes"),
    }
}

/// The log warning at start: the verb agrees with the count.
fn not_uploaded(n: usize) -> String {
    let verb = if n == 1 { "was" } else { "were" };
    format!(
        "{} from the last session {verb} not uploaded; `pdmount queue list` shows them",
        entries(n)
    )
}

fn started(n: usize) -> (String, String) {
    let summary = match n {
        1 => "PdMount: 1 change from the last session is still uploading".to_owned(),
        n => format!("PdMount: {n} changes from the last session are still uploading"),
    };
    (summary, "`pdmount status` shows the queue.".to_owned())
}

/// The message when none of the `total` is left to upload and `parked` of
/// them are parked.
fn finished(total: usize, parked: usize) -> (String, String) {
    if parked == 0 {
        let summary = match total {
            1 => "PdMount: the change from the last session is uploaded".to_owned(),
            n => format!("PdMount: the {n} changes from the last session are uploaded"),
        };
        (summary, String::new())
    } else {
        (
            format!(
                "PdMount: {} of {} from the last session uploaded",
                total - parked,
                entries(total)
            ),
            format!("{parked} need attention; `pdmount queue list` shows them."),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_warning_verb_agrees_with_the_count() {
        assert_eq!(
            not_uploaded(1),
            "1 change from the last session was not uploaded; `pdmount queue list` shows them"
        );
        assert_eq!(
            not_uploaded(2),
            "2 changes from the last session were not uploaded; `pdmount queue list` shows them"
        );
    }

    #[test]
    fn the_messages_count_and_agree_in_number() {
        assert_eq!(
            started(1).0,
            "PdMount: 1 change from the last session is still uploading"
        );
        assert_eq!(
            started(3).0,
            "PdMount: 3 changes from the last session are still uploading"
        );
        assert_eq!(
            finished(3, 0).0,
            "PdMount: the 3 changes from the last session are uploaded"
        );
        assert_eq!(
            finished(1, 0).0,
            "PdMount: the change from the last session is uploaded"
        );
        let (summary, body) = finished(5, 2);
        assert_eq!(
            summary,
            "PdMount: 3 of 5 changes from the last session uploaded"
        );
        assert!(body.starts_with("2 need attention"));
    }
}
