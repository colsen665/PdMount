//! Exporting the queued changes of a tree that goes away with them
//! (DESIGN.md §14.5; D080, D114, D124).
//!
//! A share that is revoked while changes to it are queued, or while it holds
//! staged content no entry uploads, leaves the mount, but nothing staged is
//! deleted (RQ3): before its engine stops, the daemon asks it to write the
//! content of every queued file and every other staged file the drive does
//! not have (a local-only file, or an edit of an uploaded file in a
//! local-only folder, §7.7: never in the journal, its content only in the
//! staging) to `orphaned/<tree>/files/<path in the tree>` and to list each
//! in [`MANIFEST`]. A file's content is read the way an upload reads it,
//! from the staging file and the base blocks it builds on (block cache, then
//! the server); one whose content cannot be written is listed with the
//! reason, and the daemon keeps the tree's raw state beside it.

use super::queue::{QueueKind, entry_path, kind_of};
use super::*;
use serde::{Deserialize, Serialize};

/// The list of an export, in its directory.
pub const MANIFEST: &str = "pdmount-orphaned.json";
/// The folder of an export that holds the files' content.
pub const FILES_DIR: &str = "files";
/// [`OrphanEntry::change`] of a local-only file (D124).
pub const LOCAL_ONLY: &str = "local-only";
/// [`OrphanEntry::change`] of a file with a remote whose staged content
/// was never queued: an edit in a local-only folder (§7.7, M6.0.3 V-1).
pub const NOT_UPLOADED: &str = "not-uploaded";
const MANIFEST_VERSION: u64 = 1;

/// One queued change of an export.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanEntry {
    /// Its path in the tree.
    pub path: String,
    /// What was queued: `upload`, `create-folder`, `move`, `trash`,
    /// `metadata`; or, for a staged file no entry uploads, [`LOCAL_ONLY`]
    /// (no remote: it was never to upload) or [`NOT_UPLOADED`] (a remote
    /// file whose staged edit stayed local, in a local-only folder).
    pub change: String,
    /// Where its content is, relative to the export's directory.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// Why its content could not be exported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// [`MANIFEST`]: what an export holds and why.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrphanManifest {
    pub version: u64,
    /// The tree's name in its mount.
    pub tree: String,
    /// Why it went, e.g. "the share was revoked".
    pub reason: String,
    /// Unix seconds of the export.
    pub time: u64,
    pub entries: Vec<OrphanEntry>,
}

impl OrphanManifest {
    pub fn new(tree: &str, reason: &str, entries: Vec<OrphanEntry>) -> Self {
        Self {
            version: MANIFEST_VERSION,
            tree: tree.to_owned(),
            reason: reason.to_owned(),
            time: SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_secs()),
            entries,
        }
    }

    /// Entries whose content could not be exported.
    pub fn incomplete(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.error.is_some())
            .count()
    }

    /// Reads the manifest of an export directory.
    pub fn read(dir: &Path) -> Option<Self> {
        let text = std::fs::read(dir.join(MANIFEST)).ok()?;
        serde_json::from_slice(&text).ok()
    }

    /// Writes it into `dir`.
    pub fn write(&self, dir: &Path) -> std::io::Result<()> {
        let text = serde_json::to_vec_pretty(self).map_err(std::io::Error::other)?;
        std::fs::write(dir.join(MANIFEST), text)
    }
}

/// An attached file with content the drive does not have, if no journal
/// entry uploads it: local-only, or staged content not uploaded.
fn is_unqueued_content(node: &crate::vfs::Node) -> bool {
    node.kind == NodeKind::File
        && node.attached
        && !node.deleted
        && (node.local_only || node.content_pending())
}

/// A file that never uploads: local-only, attached, no remote (§7.7).
fn is_local_only_file(node: &crate::vfs::Node) -> bool {
    node.kind == NodeKind::File
        && node.local_only
        && node.remote.is_none()
        && node.attached
        && !node.deleted
}

/// `/a/b` as a relative path, or `None` for anything but plain names.
fn relative(path: &str) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for part in path.split('/').filter(|part| !part.is_empty()) {
        if part == "." || part == ".." {
            return None;
        }
        out.push(part);
    }
    (!out.as_os_str().is_empty()).then_some(out)
}

impl Engine {
    /// Staged files whose content no journal entry will upload (§7.7,
    /// D114): attached files, not in the journal, with a local-only flag or
    /// content not uploaded, whatever the reason they are not queued (a
    /// local-only file, an edit in a local-only folder). The queue's counts
    /// leave them out; their content is only in the staging.
    pub fn unqueued_staged(&self) -> Vec<Ino> {
        let queued: std::collections::HashSet<Ino> = self.journal.inos().into_iter().collect();
        let mut staged: Vec<Ino> = self.staged.lock().keys().copied().collect();
        staged.retain(|ino| !queued.contains(ino));
        let tree = self.tree.read();
        staged.retain(|ino| tree.get(*ino).is_some_and(is_unqueued_content));
        staged
    }

    /// The changes a gone tree must not delete: the queued ones and
    /// [`Engine::unqueued_staged`]. With none, its state may go (§14.5).
    pub fn changes_to_keep(&self) -> usize {
        let counts = self.queue_counts();
        counts.pending + counts.parked + self.unqueued_staged().len()
    }

    /// Writes the content of every queued file and every staged file no
    /// entry uploads ([`Engine::unqueued_staged`]) under `dir`/[`FILES_DIR`]
    /// at its path in the tree, makes the folders queued for creation, and
    /// returns every queued change and unqueued file, each once: a
    /// [`LOCAL_ONLY`] file has no remote, a [`NOT_UPLOADED`] one has. A
    /// staged file of the tree whose content could not be written has an
    /// [`OrphanEntry::error`]. Hold the queue first
    /// ([`Engine::withdraw_write_access`]), so no step runs meanwhile.
    pub async fn export_queued(self: &Arc<Self>, dir: &Path) -> Result<Vec<OrphanEntry>> {
        let files = dir.join(FILES_DIR);
        std::fs::create_dir_all(&files)?;
        let mut inos = self.journal.inos();
        let unqueued: std::collections::HashSet<Ino> = self.unqueued_staged().into_iter().collect();
        inos.extend(unqueued.iter().copied());
        inos.sort_unstable();
        inos.dedup();
        let mut out = Vec::new();
        for ino in inos {
            let (path, kind, place, local_only) = {
                let tree = self.tree.read();
                let node = tree.get(ino);
                let present = node.is_some_and(|node| node.attached && !node.deleted);
                let place = tree
                    .path(ino)
                    .filter(|_| present)
                    .and_then(|path| relative(&path));
                (
                    entry_path(&tree, ino),
                    kind_of(&tree, ino),
                    present.then(|| (place, node.map(|node| node.kind))),
                    node.is_some_and(is_local_only_file),
                )
            };
            let staged = self.staged_entry(ino).is_some();
            if kind == QueueKind::Nothing && !staged {
                continue;
            }
            let mut entry = OrphanEntry {
                path,
                change: if local_only {
                    LOCAL_ONLY.to_owned()
                } else if unqueued.contains(&ino) {
                    NOT_UPLOADED.to_owned()
                } else {
                    kind.as_str().to_owned()
                },
                file: None,
                error: None,
            };
            match place {
                Some((Some(place), Some(NodeKind::Folder))) if kind == QueueKind::CreateFolder => {
                    std::fs::create_dir_all(files.join(&place))?;
                    entry.file = Some(Path::new(FILES_DIR).join(&place).display().to_string());
                }
                Some((Some(place), Some(NodeKind::File))) if staged => {
                    match self.export_file(ino, &files.join(&place)).await {
                        Ok(()) => {
                            entry.file =
                                Some(Path::new(FILES_DIR).join(&place).display().to_string());
                        }
                        Err(error) => entry.error = Some(error),
                    }
                }
                // A staged file of the tree with no plain path: its content
                // stays in the state the daemon keeps.
                Some((None, Some(NodeKind::File))) if staged => {
                    entry.error = Some("it has no path to export it to".into());
                }
                _ => {}
            }
            out.push(entry);
        }
        Ok(out)
    }

    /// One file's whole content to `dest`, with its mode and mtime.
    async fn export_file(&self, ino: Ino, dest: &Path) -> std::result::Result<(), String> {
        let (snapshot, mode, mtime) = self
            .snapshot_content(ino)
            .await
            .map_err(|error| format!("the content it builds on cannot be read: {error}"))?;
        let moved = (|| -> std::io::Result<()> {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            if std::fs::rename(&snapshot, dest).is_err() {
                std::fs::copy(&snapshot, dest)?;
                let _ = std::fs::remove_file(&snapshot);
            }
            use std::os::unix::fs::PermissionsExt;
            // The time first: a read-only mode would refuse the open.
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(0o600))?;
            std::fs::File::options()
                .write(true)
                .open(dest)?
                .set_modified(mtime)?;
            std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode & 0o777))?;
            Ok(())
        })();
        moved.map_err(|error| format!("cannot write {}: {error}", dest.display()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_plain_names_make_a_place() {
        assert_eq!(relative("/a/b"), Some(PathBuf::from("a/b")));
        assert_eq!(relative("/"), None);
        assert_eq!(relative("/a/../b"), None);
        assert_eq!(relative("(inode 7)"), Some(PathBuf::from("(inode 7)")));
    }
}
