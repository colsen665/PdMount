//! `pdmount queue` and `pdmount sync` (DESIGN.md §8.2, §5.6, D061): talk to
//! the running daemon.

use crate::control::{
    self, QueueActedReply, QueueDropReply, QueueEntryReply, QueueListReply, Request, SyncReply,
};
use pdmount_core::ProfilePaths;
use pdmount_core::engine::queue::QueueTarget;
use serde::de::DeserializeOwned;
use std::process::ExitCode;
use std::time::Duration;

/// A selected path may need listing, `drop` lists the folders it restores.
const QUEUE_TIMEOUT: Duration = Duration::from_secs(120);
/// On top of a `sync`'s own timeout.
const SYNC_MARGIN: Duration = Duration::from_secs(60);

async fn ask<T: DeserializeOwned>(paths: &ProfilePaths, request: Request) -> anyhow::Result<T> {
    control::ask(paths, request, QUEUE_TIMEOUT, "the queue is handled").await
}

/// `14:03:22 UTC` for unix seconds.
fn clock(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || "?".to_owned(),
        |time| time.format("%H:%M:%S UTC").to_string(),
    )
}

fn state_text(entry: &QueueEntryReply) -> String {
    match (&entry.reason, entry.until) {
        (Some(reason), _) => format!("parked: {reason}"),
        (None, Some(until)) => format!("backoff until {}", clock(until)),
        (None, None) => entry.state.clone(),
    }
}

fn format_list(entries: &[QueueEntryReply]) -> String {
    if entries.is_empty() {
        return "Nothing is queued.".to_owned();
    }
    let mut out = format!(
        "{:<10} {:<13} {:>8} {:>7}  {:<24} PATH\n",
        "ID", "KIND", "ATTEMPTS", "AGE", "STATE"
    );
    for entry in entries {
        out.push_str(&format!(
            "{:<10} {:<13} {:>8} {:>6}s  {:<24} {}\n",
            entry.id,
            entry.kind,
            entry.attempts,
            entry.age_secs,
            state_text(entry),
            entry.path,
        ));
    }
    out.truncate(out.trim_end().len());
    out
}

fn format_show(entry: &QueueEntryReply) -> String {
    let none = || "-".to_owned();
    let rows = [
        ("id", entry.id.to_string()),
        ("path", entry.path.clone()),
        ("kind", entry.kind.clone()),
        ("state", state_text(entry)),
        ("attempts", entry.attempts.to_string()),
        ("age", format!("{} s", entry.age_secs)),
        ("last error", entry.last_error.clone().unwrap_or_else(none)),
        ("base rev", entry.base_rev.clone().unwrap_or_else(none)),
        (
            "waits for",
            if entry.dependencies.is_empty() {
                none()
            } else {
                entry.dependencies.join(", ")
            },
        ),
    ];
    let lines: Vec<String> = rows
        .iter()
        .map(|(name, value)| format!("{name:<11}{value}"))
        .collect();
    lines.join("\n")
}

pub async fn list(paths: &ProfilePaths, target: QueueTarget) -> anyhow::Result<ExitCode> {
    let reply: QueueListReply = ask(paths, Request::QueueList { target }).await?;
    println!("{}", format_list(&reply.entries));
    Ok(ExitCode::SUCCESS)
}

pub async fn show(paths: &ProfilePaths, target: QueueTarget) -> anyhow::Result<ExitCode> {
    let reply: QueueListReply = ask(paths, Request::QueueList { target }).await?;
    if reply.entries.is_empty() {
        println!("Nothing is queued there.");
    }
    let shown: Vec<String> = reply.entries.iter().map(format_show).collect();
    if !shown.is_empty() {
        println!("{}", shown.join("\n\n"));
    }
    Ok(ExitCode::SUCCESS)
}

fn print_acted(reply: &QueueActedReply, verb: &str) {
    if reply.items.is_empty() {
        println!("Nothing is queued there.");
    }
    for (id, path) in &reply.items {
        println!("{verb} {id} {path}");
    }
}

pub async fn retry(paths: &ProfilePaths, target: QueueTarget) -> anyhow::Result<ExitCode> {
    let reply = ask(paths, Request::QueueRetry { target }).await?;
    print_acted(&reply, "retrying");
    Ok(ExitCode::SUCCESS)
}

pub async fn park(paths: &ProfilePaths, target: QueueTarget) -> anyhow::Result<ExitCode> {
    let reply = ask(paths, Request::QueuePark { target }).await?;
    print_acted(&reply, "parked");
    Ok(ExitCode::SUCCESS)
}

pub async fn drop(
    paths: &ProfilePaths,
    target: QueueTarget,
    yes: bool,
) -> anyhow::Result<ExitCode> {
    let reply: QueueDropReply = ask(paths, Request::QueueDrop { target, yes }).await?;
    if reply.items.is_empty() {
        println!("Nothing is queued there.");
        return Ok(ExitCode::SUCCESS);
    }
    let mut refused = false;
    for item in &reply.items {
        match &item.refused {
            Some(why) => {
                refused = true;
                println!("{} {}: not dropped: {why}", item.id, item.path);
            }
            None if yes => println!("{} {}: dropped: {}", item.id, item.path, item.effect),
            None => println!("{} {}: would {}", item.id, item.path, item.effect),
        }
    }
    if !yes {
        println!("Nothing was dropped. Run again with --yes to discard these local changes.");
        return Ok(ExitCode::from(1));
    }
    Ok(if refused {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    })
}

pub async fn sync(
    paths: &ProfilePaths,
    path: Option<String>,
    timeout_secs: u64,
) -> anyhow::Result<ExitCode> {
    let request = Request::Sync { path, timeout_secs };
    let timeout = Duration::from_secs(timeout_secs) + SYNC_MARGIN;
    let reply: SyncReply = control::ask(paths, request, timeout, "sync runs").await?;
    for (path, reason) in &reply.parked {
        println!("parked: {path}: {reason}");
    }
    if reply.pending > 0 {
        println!(
            "{} changes are still pending{}.",
            reply.pending,
            if reply.parked.is_empty() {
                format!(" after {timeout_secs} s")
            } else {
                ", held up by parked ones".to_owned()
            }
        );
    }
    Ok(if reply.pending == 0 && reply.parked.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    })
}
