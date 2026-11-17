//! `.pdignore` rules and reclassification (DESIGN.md §7.7).

use super::*;

impl Engine {
    /// Drops the kernel's attributes for a subtree (mode bits changed).
    pub(super) fn invalidate_subtree(&self, ino: Ino) {
        let tree = self.tree.read();
        for current in tree.subtree(ino) {
            let _ = self
                .invalidations
                .send(Invalidation::Inode { ino: current });
        }
    }

    /// Whether a new entry `name` in `parent` stays local-only: matched by
    /// the rules, or inside a local-only folder.
    pub(super) fn is_ignored(&self, parent: Ino, name: &str, is_dir: bool) -> bool {
        let matcher = Arc::clone(&self.ignore.read());
        let tree = self.tree.read();
        if tree.get(parent).is_some_and(|dir| dir.local_only) {
            return true;
        }
        if matcher.is_empty() {
            return false;
        }
        let Some(dir) = tree.path(parent) else {
            return false;
        };
        matcher.is_ignored(&format!("{}/{name}", dir.trim_end_matches('/')), is_dir)
    }

    /// After a rename: entries never uploaded follow the rules of their new
    /// path. Returns the inodes whose flag changed.
    pub(super) fn reclassify(&self, ino: Ino) -> Vec<Ino> {
        let matcher = Arc::clone(&self.ignore.read());
        let updates: Vec<(Ino, bool)> = {
            let tree = self.tree.read();
            let mut updates = Vec::new();
            // Parents before children, so a folder's new flag is inherited.
            let mut queue = std::collections::VecDeque::from([(ino, false)]);
            while let Some((current, parent_local)) = queue.pop_front() {
                let Some(node) = tree.get(current) else {
                    continue;
                };
                let parent_local = parent_local
                    || (current == ino && tree.get(node.parent).is_some_and(|p| p.local_only));
                let local = if node.remote.is_some() {
                    false
                } else {
                    parent_local
                        || tree
                            .path(current)
                            .is_some_and(|path| matcher.is_ignored(&path, node.is_dir()))
                };
                if node.remote.is_none() && local != node.local_only {
                    updates.push((current, local));
                }
                queue.extend(node.children.values().map(|&child| (child, local)));
            }
            updates
        };
        let mut tree = self.tree.write();
        updates
            .into_iter()
            .filter_map(|(ino, local)| {
                tree.get_mut(ino).map(|node| {
                    node.local_only = local;
                    ino
                })
            })
            .collect()
    }

    pub(super) fn is_root_rules(&self, ino: Ino) -> bool {
        self.tree
            .read()
            .child(ROOT_INO, crate::pdignore::ROOT_FILE)
            .ok()
            .flatten()
            == Some(ino)
    }

    /// Asks for the rules to be recompiled (in the background: `/.pdignore`
    /// may have to be downloaded). Nothing to do in a tree that does not
    /// take the root's rules (D124): its matcher is the profile's file.
    pub(crate) fn refresh_ignore(&self) {
        if self.remote_ignore {
            self.ignore_changed.notify_one();
        }
    }

    pub(super) async fn keep_ignore_rules(self: Arc<Self>) {
        loop {
            self.ignore_changed.notified().await;
            // On another user's volume the root's `/.pdignore` is an
            // ordinary file (D124): its writers would decide what of the
            // user's stays local-only, and a `!` line would undo the user's
            // own rules.
            let root_rules = if self.remote_ignore {
                self.tree
                    .read()
                    .child(ROOT_INO, crate::pdignore::ROOT_FILE)
                    .ok()
                    .flatten()
            } else {
                None
            };
            let text = match root_rules {
                Some(ino) => match self.read_whole(ino, 1 << 20).await {
                    Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
                    Err(error) => {
                        tracing::warn!(%error, "cannot read /.pdignore; keeping the previous rules");
                        continue;
                    }
                },
                None => String::new(),
            };
            let matcher = IgnoreMatcher::new([self.local_ignore.as_str(), text.as_str()]);
            *self.ignore.write() = Arc::new(matcher);
            tracing::debug!("`.pdignore` rules reloaded");
        }
    }
}
