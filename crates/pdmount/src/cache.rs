//! `pdmount cache stats|gc|wipe` (DESIGN.md §14.2): talks to the running
//! daemon. Pinned blocks and the base revisions of staged files are never
//! removed. `wipe` reaches the cache of every mount: My Files, Photos and
//! the shares (M6.0.10).

use crate::control::{
    self, CacheFreedReply, CacheProfileReply, CacheStatsReply, CacheTreeFreed, Request,
};
use pdmount_core::ProfilePaths;
use pdmount_core::engine::Engine;
use pdmount_core::units::ByteSize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
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

/// `pdmount cache wipe [<path>]`: `path` is relative to My Files' mount root
/// (`/a/b`), or absolute (`absolute`) inside any mount of the profile.
pub async fn wipe(
    paths: &ProfilePaths,
    path: Option<String>,
    absolute: bool,
) -> anyhow::Result<ExitCode> {
    let reply: CacheFreedReply = ask(paths, Request::CacheWipe { path, absolute }).await?;
    print_wiped(reply);
    Ok(ExitCode::SUCCESS)
}

/// What the replies call My Files' tree.
const MY_FILES: &str = "My Files";

/// The daemon's answer to `CacheWipe`: with no path every tree's cache, with
/// a path the blocks at or below it in the tree of the mount it is in.
pub async fn wipe_reply(
    engine: &Arc<Engine>,
    mountpoint: &Path,
    roots: Option<&crate::roots::Roots>,
    path: Option<String>,
    absolute: bool,
) -> Value {
    let targets = match wipe_targets(engine, mountpoint, roots, path, absolute) {
        Ok(targets) => targets,
        Err(error) => return control::failed(error),
    };
    let mut trees: Vec<CacheTreeFreed> = Vec::new();
    for target in targets {
        let freed = match &target.tree {
            crate::signatures::Tree::Engine(tree) => tree.cache_wipe(target.path.as_deref()).await,
            crate::signatures::Tree::Photos(tree) => tree.cache_wipe(target.path.as_deref()).await,
        };
        match freed {
            Ok(freed) => trees.push(CacheTreeFreed {
                name: target.name,
                blocks: freed.blocks,
                bytes: freed.bytes,
            }),
            Err(error) => {
                // What the earlier trees freed stays freed; say so.
                let done = if trees.is_empty() {
                    String::new()
                } else {
                    let names: Vec<&str> = trees.iter().map(|tree| tree.name.as_str()).collect();
                    format!(" (wiped before: {})", names.join(", "))
                };
                return control::failed(format!("{}: {error}{done}", target.name));
            }
        }
    }
    let stats = engine.cache_stats();
    control::ok(CacheFreedReply {
        blocks: trees.iter().map(|tree| tree.blocks).sum(),
        bytes: trees.iter().map(|tree| tree.bytes).sum(),
        left_bytes: stats.bytes,
        profile: Some(CacheProfileReply::of(&stats)),
        trees,
    })
}

/// The trees a wipe reaches and the path in each.
fn wipe_targets(
    engine: &Arc<Engine>,
    mountpoint: &Path,
    roots: Option<&crate::roots::Roots>,
    path: Option<String>,
    absolute: bool,
) -> Result<Vec<crate::roots::CacheTarget>, String> {
    let my_files = |path: Option<String>| crate::roots::CacheTarget {
        name: MY_FILES.to_owned(),
        tree: crate::signatures::Tree::Engine(Arc::clone(engine)),
        path,
    };
    let Some(path) = path else {
        let mut targets = vec![my_files(None)];
        targets.extend(roots.map(|roots| roots.cache_trees()).unwrap_or_default());
        return Ok(targets);
    };
    if !absolute {
        return Ok(vec![my_files(Some(path))]);
    }
    let path = Path::new(&path);
    if !path.is_absolute() {
        return Err(format!("{} is not an absolute path", path.display()));
    }
    let inside = std::path::absolute(mountpoint)
        .map_err(|error| error.to_string())
        .map(|mountpoint| path.starts_with(mountpoint))?;
    if inside {
        let relative = crate::access::mount_relative(path, mountpoint)
            .map_err(|error| format!("{error:#}"))?;
        return Ok(vec![my_files(Some(relative))]);
    }
    match roots.map(|roots| roots.cache_trees_at(path)).transpose()? {
        Some(Some(targets)) => Ok(targets),
        _ => Err(format!(
            "{} is outside every mount of this profile",
            path.display()
        )),
    }
}

/// `wipe`'s answer: the total, what each tree freed when it was more than
/// My Files alone, then what is left of the profile's cache.
fn print_wiped(reply: CacheFreedReply) {
    let Some(profile) = reply.profile.filter(|_| !reply.trees.is_empty()) else {
        return print_freed(reply);
    };
    println!(
        "Freed {} in {} blocks.",
        ByteSize(reply.bytes).human(),
        reply.blocks
    );
    if reply.trees.len() > 1 || reply.trees.iter().any(|tree| tree.name != MY_FILES) {
        for tree in &reply.trees {
            println!(
                "  {}: {} in {} blocks",
                crate::status::printable(&tree.name),
                ByteSize(tree.bytes).human(),
                tree.blocks
            );
        }
    }
    println!(
        "{} left of {}{}.",
        ByteSize(profile.bytes).human(),
        ByteSize(profile.max_bytes).human(),
        if profile.shared() {
            format!(" in all {} trees", profile.trees)
        } else {
            String::new()
        }
    );
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
