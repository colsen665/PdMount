//! `pdmount pin|unpin` (DESIGN.md §5.3, §14.2): talks to the running daemon.

use crate::control::{self, Done, PinListReply, Request};
use pdmount_core::ProfilePaths;
use pdmount_core::units::ByteSize;
use serde::de::DeserializeOwned;
use std::process::ExitCode;
use std::time::Duration;

/// Listing a path may need the network.
const PINS_TIMEOUT: Duration = Duration::from_secs(60);

async fn ask<T: DeserializeOwned>(paths: &ProfilePaths, request: Request) -> anyhow::Result<T> {
    control::ask(paths, request, PINS_TIMEOUT, "pins are managed").await
}

pub async fn set(paths: &ProfilePaths, path: String) -> anyhow::Result<ExitCode> {
    let _: Done = ask(paths, Request::PinSet { path: path.clone() }).await?;
    println!("Pinned {path}; its files are fetched in the background (pdmount pin list).");
    Ok(ExitCode::SUCCESS)
}

pub async fn unset(paths: &ProfilePaths, path: String) -> anyhow::Result<ExitCode> {
    let _: Done = ask(paths, Request::PinUnset { path: path.clone() }).await?;
    println!("Unpinned {path}.");
    Ok(ExitCode::SUCCESS)
}

pub async fn list(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let reply: PinListReply = ask(paths, Request::PinList).await?;
    if reply.pins.is_empty() {
        println!("Nothing is pinned.");
    }
    for pin in reply.pins {
        let path = pin
            .path
            .unwrap_or_else(|| format!("{} (not reachable)", pin.set_as));
        println!(
            "{path}: {} files, {} of {}, {}",
            pin.files,
            ByteSize(pin.fetched_bytes).human(),
            ByteSize(pin.total_bytes).human(),
            pin.state
        );
    }
    Ok(ExitCode::SUCCESS)
}
