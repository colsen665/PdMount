//! `pdmount conflicts list|show|resolve` (DESIGN.md §10.8): talks to the
//! running daemon. A target is a conflict id (all digits) or a path, which
//! names the newest unresolved conflict of that file; the path is the last
//! argument (D009).

use crate::control::{self, ConflictOneReply, ConflictReply, ConflictsReply, Request};
use pdmount_core::ProfilePaths;
use pdmount_core::conflicts::Resolution;
use serde::de::DeserializeOwned;
use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

/// `resolve keep-remote` waits for the local copy's upload (up to 120 s).
const CONFLICTS_TIMEOUT: Duration = Duration::from_secs(150);

/// A conflict id stays as it is; anything else is a path, taken relative to
/// the mount root.
pub fn target(text: &str, mountpoint: &Path) -> anyhow::Result<String> {
    if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(text.to_owned());
    }
    crate::access::mount_relative(Path::new(text), mountpoint)
}

async fn ask<T: DeserializeOwned>(paths: &ProfilePaths, request: Request) -> anyhow::Result<T> {
    control::ask(paths, request, CONFLICTS_TIMEOUT, "conflicts are handled").await
}

pub async fn list(
    paths: &ProfilePaths,
    path: Option<String>,
    all: bool,
) -> anyhow::Result<ExitCode> {
    let reply: ConflictsReply = ask(paths, Request::ConflictsList { path, all }).await?;
    println!("{}", format_list(&reply.conflicts));
    Ok(ExitCode::SUCCESS)
}

pub async fn show(paths: &ProfilePaths, target: String) -> anyhow::Result<ExitCode> {
    let reply: ConflictOneReply = ask(paths, Request::ConflictsShow { target }).await?;
    println!("{}", format_show(&reply.conflict));
    Ok(ExitCode::SUCCESS)
}

pub async fn resolve(
    paths: &ProfilePaths,
    target: String,
    choice: Resolution,
) -> anyhow::Result<ExitCode> {
    let reply: ConflictOneReply = ask(paths, Request::ConflictsResolve { target, choice }).await?;
    let conflict = &reply.conflict;
    println!(
        "conflict {} ({}) resolved: {}",
        conflict.id,
        conflict.kind,
        match choice {
            Resolution::KeepBoth => "both copies kept",
            Resolution::KeepLocal => "the local copy kept",
            Resolution::KeepRemote => "the other copy kept",
        }
    );
    Ok(ExitCode::SUCCESS)
}

/// `2026-10-22 10:00:00 UTC` for unix seconds.
fn when(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0).map_or_else(
        || "?".to_owned(),
        |time| time.format("%Y-%m-%d %H:%M:%S UTC").to_string(),
    )
}

fn format_list(conflicts: &[ConflictReply]) -> String {
    if conflicts.is_empty() {
        return "no conflicts".to_owned();
    }
    let mut out = format!(
        "{:<4} {:<8} {:<28} {:<23} PATH\n",
        "ID", "STATE", "KIND", "WHEN"
    );
    for conflict in conflicts {
        out.push_str(&format!(
            "{:<4} {:<8} {:<28} {:<23} {}\n",
            conflict.id,
            conflict.state,
            conflict.kind,
            when(conflict.created_at),
            conflict.path.as_deref().unwrap_or("-"),
        ));
        if let Some(other) = &conflict.other_path {
            out.push_str(&format!(
                "{:<4} {:<8} {:<28} {:<23} other: {other}\n",
                "", "", "", ""
            ));
        }
    }
    out.truncate(out.trim_end().len());
    out
}

fn format_show(conflict: &ConflictReply) -> String {
    let mut rows: Vec<(&str, String)> = vec![
        ("Conflict", conflict.id.to_string()),
        ("Kind", conflict.kind.clone()),
        ("State", conflict.state.clone()),
        ("When", when(conflict.created_at)),
        (
            "Local copy",
            conflict.path.clone().unwrap_or_else(|| "(gone)".into()),
        ),
        (
            "Other copy",
            conflict
                .other_path
                .clone()
                .unwrap_or_else(|| "(none)".into()),
        ),
        ("Original name", conflict.orig_name.clone()),
        ("Local device", conflict.local_device.clone()),
    ];
    for (label, value) in [
        ("Local revision", &conflict.local_rev),
        ("Remote revision", &conflict.remote_rev),
        ("Detail", &conflict.detail),
    ] {
        if let Some(value) = value {
            rows.push((label, value.clone()));
        }
    }
    let mut out: String = rows
        .iter()
        .map(|(label, value)| format!("{:<17}{value}\n", format!("{label}:")))
        .collect();
    out.push_str(match conflict.kind.as_str() {
        "edit_edit" | "create_create" | "name_taken" => {
            "Resolve with keep-both (dismiss), keep-local (the local copy takes the name) \
             or keep-remote (the local copy is removed)."
        }
        "local_delete_remote_edit" => {
            "Resolve with keep-both or keep-remote (dismiss), or keep-local (remove the file again)."
        }
        _ => "Nothing to decide; dismiss it with keep-both.",
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_stay_and_paths_become_mount_relative() {
        let mnt = Path::new("/home/u/ProtonDrive");
        assert_eq!(target("12", mnt).expect("id"), "12");
        assert_eq!(target("docs/a.txt", mnt).expect("relative"), "/docs/a.txt");
        assert_eq!(target("./2024", mnt).expect("dot"), "/2024");
        assert_eq!(
            target("/home/u/ProtonDrive/docs", mnt).expect("absolute"),
            "/docs"
        );
        assert!(target("/etc/passwd", mnt).is_err());
    }

    fn conflict(id: i64, kind: &str) -> ConflictReply {
        ConflictReply {
            id,
            kind: kind.to_owned(),
            state: "open".to_owned(),
            path: None,
            other_path: None,
            orig_name: String::new(),
            conflict_name: None,
            local_rev: None,
            remote_rev: None,
            local_device: String::new(),
            created_at: 0,
            detail: None,
        }
    }

    #[test]
    fn the_list_names_both_copies() {
        let reply = [ConflictReply {
            path: Some("/docs/new (conflict dev-a 2026-10-22 10-00-00 UTC).txt".into()),
            other_path: Some("/docs/new.txt".into()),
            created_at: 1_792_663_200,
            ..conflict(3, "create_create")
        }];
        let text = format_list(&reply);
        assert!(text.starts_with("ID   STATE    KIND"), "{text}");
        assert!(text.contains("3    open     create_create"), "{text}");
        assert!(text.contains("2026-10-22 10:00:00 UTC"), "{text}");
        assert!(text.contains("/docs/new (conflict dev-a"), "{text}");
        assert!(
            text.lines()
                .nth(2)
                .is_some_and(|l| l.ends_with("/docs/new.txt"))
        );
        assert_eq!(format_list(&[]), "no conflicts");
    }

    #[test]
    fn show_lists_what_is_known_and_how_to_resolve() {
        let text = format_show(&ConflictReply {
            path: Some("/a (conflict).txt".into()),
            orig_name: "a.txt".into(),
            local_device: "laptop".into(),
            remote_rev: Some("r2".into()),
            ..conflict(7, "edit_edit")
        });
        assert!(text.contains("Conflict:        7"), "{text}");
        assert!(text.contains("Other copy:      (none)"), "{text}");
        assert!(text.contains("Remote revision: r2"), "{text}");
        assert!(text.contains("keep-local"), "{text}");
    }
}
