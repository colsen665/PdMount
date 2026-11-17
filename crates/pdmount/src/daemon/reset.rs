//! `reset`: removes a profile's state and cache.

use super::*;

/// `pdmount reset --keep-login`: deletes the state database and the block
/// cache. The login, SDK caches and device identity stay. Refuses while
/// changes are not uploaded; with `orphan_trees`, the state of further
/// trees with such changes moves to `orphaned/` instead (a share that is
/// gone can no longer be mounted to upload them, D114).
pub async fn reset(
    paths: &ProfilePaths,
    mountpoint: &Path,
    orphan_trees: bool,
) -> anyhow::Result<ExitCode> {
    refuse_while_running(paths).await?;
    if mounts::find(mountpoint)?.is_some_and(|entry| entry.is_pdmount()) {
        bail!(
            "profile {} is mounted; run `pdmount unmount` first",
            paths.profile
        );
    }
    let pending =
        match pdmount_core::engine::pending_changes(&paths.state_db(), &paths.staging_dir()) {
            Ok(pending) => pending,
            // An unreadable state is what a reset repairs; staging files still
            // hold content that would be lost.
            Err(error) => {
                eprintln!("warning: cannot read the local state ({error}); resetting it");
                pdmount_core::staging::StagingDir::open(&paths.staging_dir())
                    .and_then(|staging| staging.staged_inos())
                    .map_or(0, |staged| staged.len() as u64)
            }
        };
    if pending > 0 {
        bail!(
            "profile {} has {pending} local changes that are not uploaded yet; mount it and run \
             `pdmount unmount` to upload them first",
            paths.profile
        );
    }
    // The further trees' too (D110): staging left there is content as well.
    let trees = trees_with_changes(paths);
    if !trees.is_empty() && !orphan_trees {
        let list: Vec<String> = trees
            .iter()
            .map(|(tree, pending)| format!("{} ({pending})", tree.display()))
            .collect();
        bail!(
            "further trees of profile {} have local changes that are not uploaded yet: {}; mount \
             their roots and run `pdmount unmount <mountpoint>` to upload them first, or, for a \
             share that is gone, `pdmount reset --keep-login --orphan-trees` to move their state \
             to {}",
            paths.profile,
            list.join(", "),
            paths.orphaned_dir().display()
        );
    }
    for (tree, pending) in &trees {
        let kept = orphan_tree(paths, tree, *pending)?;
        println!(
            "moved {} ({pending} not uploaded) to {}",
            tree.display(),
            kept.display()
        );
    }
    let mut removed = Vec::new();
    let trees_data = paths.trees_data_dir();
    if trees_data.exists() {
        std::fs::remove_dir_all(&trees_data)
            .with_context(|| format!("cannot remove {}", trees_data.display()))?;
        removed.push(trees_data);
    }
    let staging = paths.staging_dir();
    if staging.exists() {
        std::fs::remove_dir_all(&staging)
            .with_context(|| format!("cannot remove {}", staging.display()))?;
    }
    for path in pdmount_core::paths::sqlite_files(&paths.state_db()) {
        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("cannot remove {}", path.display()))?;
            removed.push(path);
        }
    }
    let blocks = paths.blocks_dir();
    let mut cache_size = 0;
    if blocks.exists() {
        cache_size = dir_size(&blocks);
        std::fs::remove_dir_all(&blocks)
            .with_context(|| format!("cannot remove {}", blocks.display()))?;
        removed.push(blocks);
    }
    let trees_cache = paths.trees_cache_dir();
    if trees_cache.exists() {
        cache_size += dir_size(&trees_cache);
        std::fs::remove_dir_all(&trees_cache)
            .with_context(|| format!("cannot remove {}", trees_cache.display()))?;
        removed.push(trees_cache);
    }
    if removed.is_empty() {
        println!("Nothing to reset for profile {}.", paths.profile);
    } else {
        for path in &removed {
            println!("removed {}", path.display());
        }
        println!(
            "Profile {} reset ({} of cached blocks freed); the login is kept.",
            paths.profile,
            ByteSize(cache_size).human()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The further trees' state directories (`trees/<key>/`) that hold changes
/// not uploaded, with how many.
fn trees_with_changes(paths: &ProfilePaths) -> Vec<(PathBuf, u64)> {
    let mut trees: Vec<(PathBuf, u64)> = std::fs::read_dir(paths.trees_data_dir())
        .map(|entries| {
            entries
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default()
        .into_iter()
        .map(|tree| {
            let pending = pdmount_core::engine::pending_changes(
                &tree.join("state.db"),
                &tree.join("staging"),
            )
            .unwrap_or_else(|_| {
                pdmount_core::staging::StagingDir::open(&tree.join("staging"))
                    .and_then(|staging| staging.staged_inos())
                    .map_or(0, |staged| staged.len() as u64)
            });
            (tree, pending)
        })
        .filter(|(_, pending)| *pending > 0)
        .collect();
    trees.sort();
    trees
}

/// Moves a further tree's state directory to `orphaned/tree <key>
/// (<time>)/state/`, with a manifest so that `status` lists it (D114).
/// Returns the new directory.
fn orphan_tree(paths: &ProfilePaths, tree: &Path, pending: u64) -> anyhow::Result<PathBuf> {
    use pdmount_core::engine::orphan::{OrphanEntry, OrphanManifest};
    let key = tree
        .file_name()
        .map_or_else(|| "tree".into(), |name| name.to_string_lossy().into_owned());
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let base = paths.orphaned_dir();
    let mut dir = base.join(format!("tree {key} ({stamp})"));
    let mut n = 2;
    while dir.exists() {
        dir = base.join(format!("tree {key} ({stamp}, {n})"));
        n += 1;
    }
    std::fs::create_dir_all(&dir).with_context(|| format!("cannot create {}", dir.display()))?;
    std::fs::rename(tree, dir.join("state"))
        .with_context(|| format!("cannot move {} to {}", tree.display(), dir.display()))?;
    let manifest = OrphanManifest::new(
        &format!("tree {key}"),
        &format!(
            "{pending} {} not uploaded, kept by `pdmount reset --orphan-trees`",
            if pending == 1 { "change" } else { "changes" }
        ),
        vec![OrphanEntry {
            path: "/".into(),
            change: "state".into(),
            file: None,
            error: Some("only the tree's raw state is kept".into()),
        }],
    );
    manifest
        .write(&dir)
        .with_context(|| format!("cannot write the list in {}", dir.display()))?;
    Ok(dir)
}

pub(super) fn dir_size(dir: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .map(|entry| match entry.metadata() {
            Ok(meta) if meta.is_dir() => dir_size(&entry.path()),
            Ok(meta) => meta.len(),
            Err(_) => 0,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn reset_removes_state_and_cache_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = pdmount_core::paths::BaseDirs {
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            state: dir.path().join("state"),
            runtime: None,
        };
        let paths = ProfilePaths::new(pdmount_core::Profile::default(), &base);
        std::fs::create_dir_all(paths.blocks_dir().join("ab")).expect("mkdir");
        std::fs::write(paths.blocks_dir().join("ab/x"), b"1234").expect("write");
        std::fs::create_dir_all(&paths.data_dir).expect("mkdir");
        std::fs::write(paths.state_db(), b"db").expect("write");
        std::fs::write(paths.credentials_file(), b"{}").expect("write");
        reset(&paths, &dir.path().join("mnt"), false)
            .await
            .expect("reset");
        assert!(!paths.state_db().exists());
        assert!(!paths.blocks_dir().exists());
        assert!(paths.credentials_file().exists());
    }

    #[tokio::test]
    async fn reset_refuses_while_changes_are_not_uploaded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = pdmount_core::paths::BaseDirs {
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            state: dir.path().join("state"),
            runtime: None,
        };
        let paths = ProfilePaths::new(pdmount_core::Profile::default(), &base);
        let staging =
            pdmount_core::staging::StagingDir::open(&paths.staging_dir()).expect("staging");
        staging.create(7, None, None, 3).expect("staged file");
        let error = reset(&paths, &dir.path().join("mnt"), false)
            .await
            .expect_err("refused");
        assert!(format!("{error:#}").contains("not uploaded yet"));
        assert!(staging.staged_inos().expect("list").contains(&7));
    }

    /// A gone share's kept state (`trees/<key>/` with changes not uploaded)
    /// never blocks a reset for good: the refusal names it, and
    /// `--orphan-trees` moves it to `orphaned/`, listed by `status` (D114).
    #[tokio::test]
    async fn a_further_tree_with_changes_is_named_and_can_be_moved_aside() {
        let dir = tempfile::tempdir().expect("tempdir");
        let base = pdmount_core::paths::BaseDirs {
            config: dir.path().join("config"),
            data: dir.path().join("data"),
            cache: dir.path().join("cache"),
            state: dir.path().join("state"),
            runtime: None,
        };
        let paths = ProfilePaths::new(pdmount_core::Profile::default(), &base);
        let key = "0123456789abcdef0123456789abcdef";
        let tree = paths.trees_data_dir().join(key);
        let staging =
            pdmount_core::staging::StagingDir::open(&tree.join("staging")).expect("staging");
        staging.create(9, None, None, 3).expect("staged file");
        // An unchanged tree beside it.
        std::fs::create_dir_all(paths.trees_data_dir().join("other")).expect("mkdir");
        let mnt = dir.path().join("mnt");

        let error = reset(&paths, &mnt, false).await.expect_err("refused");
        let text = format!("{error:#}");
        assert!(text.contains(&format!("{} (1)", tree.display())), "{text}");
        assert!(text.contains("--orphan-trees"), "{text}");
        assert!(!text.contains("other"), "{text}");
        assert!(tree.join("staging").exists());

        reset(&paths, &mnt, true).await.expect("reset");
        assert!(!paths.trees_data_dir().exists());
        let kept: Vec<PathBuf> = std::fs::read_dir(paths.orphaned_dir())
            .expect("orphaned")
            .map(|entry| entry.expect("entry").path())
            .collect();
        assert_eq!(kept.len(), 1, "{kept:?}");
        let moved = pdmount_core::staging::StagingDir::open(&kept[0].join("state/staging"))
            .expect("moved staging");
        assert!(moved.staged_inos().expect("list").contains(&9));
        let lines = crate::status::orphaned_lines(&paths.orphaned_dir());
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(&format!("1 change of tree {key} (1 change not uploaded")),
            "{}",
            lines[0]
        );
    }
}
