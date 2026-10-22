//! `pdmount access set|unset|list|effective` (DESIGN.md §11.5): talks to the
//! running daemon. Paths are absolute inside the mountpoint, or relative to
//! the mount root; the path is the last argument (D009).

use crate::control::{self, Request};
use anyhow::{Context, bail};
use pdmount_core::ProfilePaths;
use pdmount_core::config::AccessMode;
use serde_json::Value;
use std::path::{Component, Path};
use std::process::ExitCode;
use std::time::Duration;

/// Listing a path may need the network.
const ACCESS_TIMEOUT: Duration = Duration::from_secs(60);

pub fn parse_mode(value: &str) -> Result<AccessMode, String> {
    match value {
        "ro" => Ok(AccessMode::Ro),
        "rw" => Ok(AccessMode::Rw),
        _ => Err(format!("expected ro or rw, got {value:?}")),
    }
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

async fn ask(paths: &ProfilePaths, request: Request) -> anyhow::Result<Value> {
    let reply = control::call(&paths.control_socket(), request, ACCESS_TIMEOUT)
        .await?
        .with_context(|| {
            format!(
                "profile {} is not mounted; access rules are changed through the running daemon",
                paths.profile
            )
        })?;
    if reply["ok"] != true {
        bail!("{}", reply["error"].as_str().unwrap_or("unknown error"));
    }
    Ok(reply)
}

pub async fn set(paths: &ProfilePaths, path: String, mode: AccessMode) -> anyhow::Result<ExitCode> {
    let reply = ask(
        paths,
        Request::AccessSet {
            path: path.clone(),
            mode,
        },
    )
    .await?;
    let pending = reply["pending"].as_u64().unwrap_or(0);
    println!("{} {path}", pdmount_core::access::mode_str(mode));
    if mode == AccessMode::Ro && pending > 0 {
        println!("{pending} pending changes in it were accepted before and still upload.");
    }
    Ok(ExitCode::SUCCESS)
}

pub async fn unset(paths: &ProfilePaths, path: String) -> anyhow::Result<ExitCode> {
    ask(paths, Request::AccessUnset { path: path.clone() }).await?;
    println!("removed the rule on {path}");
    Ok(ExitCode::SUCCESS)
}

pub async fn list(paths: &ProfilePaths, path: Option<String>) -> anyhow::Result<ExitCode> {
    let reply = ask(paths, Request::AccessList { path }).await?;
    println!("{}", format_list(&reply));
    Ok(ExitCode::SUCCESS)
}

pub async fn effective(paths: &ProfilePaths, path: String) -> anyhow::Result<ExitCode> {
    let reply = ask(paths, Request::AccessEffective { path }).await?;
    let mode = reply["mode"].as_str().unwrap_or("?");
    match reply["rule"].as_str() {
        Some(rule) => println!("{mode}  (rule: {rule})"),
        None => println!("{mode}  (default)"),
    }
    Ok(ExitCode::SUCCESS)
}

fn format_list(reply: &Value) -> String {
    let mut out = format!(
        "{:<5} {:<25} {:<25} {:<7} STATUS\n",
        "MODE", "PATH", "SET AS", "SOURCE"
    );
    for rule in reply["rules"].as_array().into_iter().flatten() {
        let path = rule["path"].as_str();
        out.push_str(&format!(
            "{:<5} {:<25} {:<25} {:<7} {}\n",
            rule["mode"].as_str().unwrap_or("?"),
            path.unwrap_or("-"),
            rule["set_as"].as_str().unwrap_or("?"),
            rule["source"].as_str().unwrap_or("?"),
            if path.is_some() { "active" } else { "orphaned" }
        ));
    }
    out.push_str(&format!(
        "(default: {})",
        reply["default"].as_str().unwrap_or("rw")
    ));
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
        let reply = serde_json::json!({
            "default": "rw",
            "rules": [
                { "mode": "ro", "path": "/Archive", "set_as": "/Archive", "source": "cli" },
                { "mode": "ro", "path": null, "set_as": "/Scratch/tmp", "source": "cli" },
            ],
        });
        let text = format_list(&reply);
        assert!(text.contains("ro    /Archive"));
        assert!(text.contains("-                         /Scratch/tmp"));
        assert!(text.contains("orphaned"));
        assert!(text.ends_with("(default: rw)"));
    }
}
