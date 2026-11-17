//! `pdmount cache stats|gc|wipe` (DESIGN.md §14.2): talks to the running
//! daemon. Pinned blocks and the base revisions of staged files are never
//! removed.

use crate::control::{self, CacheFreedReply, CacheStatsReply, Request};
use pdmount_core::ProfilePaths;
use pdmount_core::units::ByteSize;
use serde::de::DeserializeOwned;
use std::process::ExitCode;
use std::time::Duration;

/// Removing many files, and `wipe <path>` may need to list it.
const CACHE_TIMEOUT: Duration = Duration::from_secs(300);

async fn ask<T: DeserializeOwned>(paths: &ProfilePaths, request: Request) -> anyhow::Result<T> {
    control::ask(paths, request, CACHE_TIMEOUT, "the cache is managed").await
}

pub async fn stats(paths: &ProfilePaths) -> anyhow::Result<ExitCode> {
    let reply: CacheStatsReply = ask(paths, Request::CacheStats).await?;
    match reply.profile.filter(|profile| profile.shared()) {
        // Further trees on the cap (D110 amended): My Files' part, then all.
        Some(profile) => {
            println!(
                "Disk:       {} in {} blocks (My Files)",
                ByteSize(reply.bytes).human(),
                reply.blocks
            );
            println!("{}", crate::status::cache_profile_line(&profile));
        }
        None => println!(
            "Disk:       {} of {}, {} blocks",
            ByteSize(reply.bytes).human(),
            ByteSize(reply.max_bytes).human(),
            reply.blocks
        ),
    }
    println!(
        "Pinned:     {} (not counted against the cap)",
        ByteSize(reply.pinned_bytes).human()
    );
    println!("Protected:  {} revisions", reply.protected_revisions);
    let slots: Vec<String> = reply
        .pool
        .iter()
        .map(|class| format!("{} {}/{}", class.class, class.in_use, class.slots))
        .collect();
    println!("Pool slots: {}", slots.join(", "));
    Ok(ExitCode::SUCCESS)
}

pub async fn gc(paths: &ProfilePaths, target: Option<ByteSize>) -> anyhow::Result<ExitCode> {
    let request = Request::CacheGc {
        target: target.map(|target| target.0),
    };
    print_freed(ask(paths, request).await?);
    Ok(ExitCode::SUCCESS)
}

pub async fn wipe(paths: &ProfilePaths, path: Option<String>) -> anyhow::Result<ExitCode> {
    print_freed(ask(paths, Request::CacheWipe { path }).await?);
    Ok(ExitCode::SUCCESS)
}

fn print_freed(reply: CacheFreedReply) {
    // With further trees on the cap, the profile's total too (D110 amended).
    let profile = match reply.profile.filter(|profile| profile.shared()) {
        Some(profile) => format!(
            " (My Files; all {} trees: {} of {})",
            profile.trees,
            ByteSize(profile.bytes).human(),
            ByteSize(profile.max_bytes).human()
        ),
        None => String::new(),
    };
    println!(
        "Freed {} in {} blocks; {} left{profile}.",
        ByteSize(reply.bytes).human(),
        reply.blocks,
        ByteSize(reply.left_bytes).human()
    );
}
