//! `pdmount access set|unset|list|effective` (DESIGN.md §11.5): talks to the
//! running daemon. Paths are absolute inside the mountpoint, or relative to
//! the mount root; the path is the last argument (D009).

use crate::control::{self, AccessListReply, Request};
use anyhow::{Context, bail};
use pdmount_core::ProfilePaths;
use pdmount_core::access::mode_str;
use pdmount_core::config::AccessMode;
use serde::de::DeserializeOwned;
use std::path::{Component, Path};
use std::process::ExitCode;
use std::time::Duration;

/// Listing a path may need the network.
const ACCESS_TIMEOUT: Duration = Duration::from_secs(60);

pub fn parse_mode(value: &str) -> Result<AccessMode, String> {
    pdmount_core::access::parse_mode(value)
        .ok_or_else(|| format!("expected ro or rw, got {value:?}"))
}

/// `path` as the daemon expects it: `/a/b` below the mount root.
pub fn mount_relative(path: &Path, mountpoint: &Path) -> anyhow::Result<String> {
    let relative = if path.is_absolute() {
        let path = std::path::absolute(path)?;
        let mountpoint = std::path::absolute(mountpoint)?;
        path.strip_prefix(&mountpoint)
            .with_context(|| {
                format!(
                    "{} is outside the mount at {}",
                    path.display(),
                    mountpoint.display()
                )
            })?
            .to_owned()
    } else {
        path.to_owned()
    };
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(
                part.to_str()
                    .with_context(|| format!("{} is not valid UTF-8", path.display()))?
                    .to_owned(),
            ),
            Component::CurDir => {}
            _ => bail!("{} must not contain `..`", path.display()),
        }
    }
    Ok(format!("/{}", parts.join("/")))
}

async fn ask<T: DeserializeOwned>(paths: &ProfilePaths, request: Request) -> anyhow::Result<T> {
    control::ask(paths, request, ACCESS_TIMEOUT, "access rules are changed").await
}

pub async fn set(paths: &ProfilePaths, path: String, mode: AccessMode) -> anyhow::Result<ExitCode> {
    let reply: control::AccessSetReply = ask(
        paths,
        Request::AccessSet {
            path: path.clone(),
            mode,
        },
    )
    .await?;
    let pending = reply.pending;
    println!("{} {path}", mode_str(mode));
    if mode == AccessMode::Ro && pending > 0 {
        println!("{pending} pending changes in it were accepted before and still upload.");
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn unset(paths: &ProfilePaths, path: String) -> anyhow::Result<ExitCode> {
    let _: control::Done = ask(paths, Request::AccessUnset { path: path.clone() }).await?;
    println!("removed the rule on {path}");
    Ok(ExitCode::SUCCESS)
}

pub async fn list(paths: &ProfilePaths, path: Option<String>) -> anyhow::Result<ExitCode> {
    let reply = ask(paths, Request::AccessList { path }).await?;
    println!("{}", format_list(&reply));
    Ok(ExitCode::SUCCESS)
}

pub async fn effective(paths: &ProfilePaths, path: String) -> anyhow::Result<ExitCode> {
    let reply: control::AccessEffectiveReply =
        ask(paths, Request::AccessEffective { path }).await?;
    let mode = mode_str(reply.mode);
    match reply.rule {
        Some(rule) => println!("{mode}  (rule: {rule})"),
        None => println!("{mode}  (default)"),
    }
    Ok(ExitCode::SUCCESS)
}

fn format_list(reply: &AccessListReply) -> String {
    let mut out = format!(
        "{:<5} {:<25} {:<25} {:<7} STATUS\n",
        "MODE", "PATH", "SET AS", "SOURCE"
    );
    for rule in &reply.rules {
        out.push_str(&format!(
            "{:<5} {:<25} {:<25} {:<7} {}\n",
            mode_str(rule.mode),
            rule.path.as_deref().unwrap_or("-"),
            rule.set_as,
            rule.source,
            if rule.path.is_some() {
                "active"
            } else {
                "orphaned"
            }
        ));
    }
    out.push_str(&format!("(default: {})", mode_str(reply.default)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_are_taken_relative_to_the_mount_root() {
        let mnt = Path::new("/home/u/ProtonDrive");
        assert_eq!(
            mount_relative(Path::new("/home/u/ProtonDrive/Archive/x"), mnt).expect("inside"),
            "/Archive/x"
        );
        assert_eq!(
            mount_relative(Path::new("Archive/./x"), mnt).expect("relative"),
            "/Archive/x"
        );
        assert_eq!(
            mount_relative(Path::new("/home/u/ProtonDrive"), mnt).expect("root"),
            "/"
        );
        assert!(mount_relative(Path::new("/etc"), mnt).is_err());
        assert!(mount_relative(Path::new("a/../b"), mnt).is_err());
    }

    #[test]
    fn the_list_shows_orphaned_rules_and_the_default() {
        let rule = |path: Option<&str>, set_as: &str| control::RuleReply {
            mode: AccessMode::Ro,
            path: path.map(str::to_owned),
            set_as: set_as.to_owned(),
            source: "cli".to_owned(),
        };
        let reply = AccessListReply {
            default: AccessMode::Rw,
            rules: vec![
                rule(Some("/Archive"), "/Archive"),
                rule(None, "/Scratch/tmp"),
            ],
        };
        let text = format_list(&reply);
        assert!(text.contains("ro    /Archive"));
        assert!(text.contains("-                         /Scratch/tmp"));
        assert!(text.contains("orphaned"));
        assert!(text.ends_with("(default: rw)"));
    }
}
