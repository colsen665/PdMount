//! Write gate, permission checks and the per-path access rules (DESIGN.md §11).

use super::*;

impl Engine {
    /// `mount -o ro`.
    pub fn is_read_only(&self) -> bool {
        self.read_only
    }

    /// Whether the write gate is open (D080, D110).
    pub fn write_gate_open(&self) -> bool {
        self.write_gate.load(Ordering::SeqCst)
    }

    /// Opens or closes the write gate. Closed, every write path answers
    /// EROFS, as `mount -o ro` does, and reported modes lose their write
    /// bits; work already accepted still uploads (§11.4). The kernel's
    /// cached attributes are invalidated, so the modes change at once.
    pub fn set_write_gate(&self, open: bool) {
        if self.write_gate.swap(open, Ordering::SeqCst) != open {
            tracing::info!(open, "write gate changed");
            self.invalidate_subtree(ROOT_INO);
        }
    }

    /// `mount -o ro` or a closed write gate: nothing may change.
    pub(super) fn writes_closed(&self) -> bool {
        self.read_only || !self.write_gate_open()
    }

    /// Whether `handle` is open for writing on `ino`.
    pub(super) fn writable_handle(&self, ino: Ino, handle: Option<u64>) -> bool {
        handle.is_some_and(|handle| {
            self.files
                .lock()
                .get(&handle)
                .is_some_and(|file| file.ino == ino && file.writable)
        })
    }

    /// EACCES unless the owner bits of the stored mode grant `want`. Only
    /// the mounting user reaches the mount (no `allow_other`), so these are
    /// the bits that apply; the kernel does not check them itself (D031).
    pub(super) fn ensure_allowed(&self, ino: Ino, want: u32) -> Result<()> {
        if want == 0 {
            return Ok(());
        }
        let mode = self.tree.read().get(ino).ok_or(CoreError::NotFound)?.mode;
        if (mode >> 6) & want == want {
            Ok(())
        } else {
            Err(CoreError::AccessDenied)
        }
    }

    /// Changing entries of a directory: EROFS in `ro` subtrees, then
    /// write and search permission.
    pub(super) fn ensure_dir_writable(&self, dir: Ino) -> Result<()> {
        self.ensure_writable(dir)?;
        self.ensure_allowed(dir, MAY_WRITE | MAY_EXEC)
    }

    /// Creating, removing or renaming `name` in `dir`: as
    /// [`Self::ensure_dir_writable`], and EROFS for the marker folder's
    /// name in the root (D076).
    pub(super) fn ensure_entry_writable(&self, dir: Ino, name: &str) -> Result<()> {
        self.ensure_dir_writable(dir)?;
        if marker::is_marker_name(dir, name) {
            return Err(CoreError::ReadOnly);
        }
        Ok(())
    }

    /// `access(2)`: `mask` of `MAY_*` bits; write access in a `ro` subtree
    /// is EROFS, like on a read-only file system.
    pub fn access(&self, ino: Ino, mask: u32) -> Result<()> {
        if mask & MAY_WRITE != 0 {
            self.ensure_writable(ino)?;
        }
        self.ensure_allowed(ino, mask & (MAY_READ | MAY_WRITE | MAY_EXEC))
    }

    /// EROFS when `ino` is in a `ro` subtree or the marker folder, or the
    /// mount is read-only or the write gate closed (D019, D076, D110).
    pub(super) fn ensure_writable(&self, ino: Ino) -> Result<()> {
        if self.writes_closed() || marker::in_marker_dir(&self.tree.read(), ino) {
            return Err(CoreError::ReadOnly);
        }
        let access = self.access.read();
        if access.is_empty() {
            return Ok(());
        }
        let tree = self.tree.read();
        if access.is_read_only(&tree, ino) {
            return Err(CoreError::ReadOnly);
        }
        Ok(())
    }

    /// Reported attributes lose their write bits in `ro` subtrees (§11.2).
    pub(super) fn masked(&self, mut attr: Attr) -> Attr {
        if self.writes_closed() {
            attr.perm &= !0o222;
            return attr;
        }
        let read_only = {
            let access = self.access.read();
            let tree = self.tree.read();
            marker::in_marker_dir(&tree, attr.ino)
                || (!access.is_empty() && access.is_read_only(&tree, attr.ino))
        };
        if read_only {
            attr.perm &= !0o222;
        }
        attr
    }

    /// Resolves a path below the mount root (`/a/b` or `a/b`), listing
    /// directories on the way.
    pub async fn resolve(self: &Arc<Self>, path: &str) -> Result<Ino> {
        let mut ino = ROOT_INO;
        for part in path.split('/').filter(|p| !p.is_empty() && *p != ".") {
            if part == ".." {
                return Err(CoreError::InvalidArgument);
            }
            self.ensure_listed(ino).await?;
            ino = self
                .tree
                .read()
                .child(ino, part)?
                .ok_or(CoreError::NotFound)?;
        }
        Ok(ino)
    }

    /// `access set ro|rw <path>` (§11.4). Returns how many pending journal
    /// entries are in the subtree; they still upload.
    pub async fn access_set(
        self: &Arc<Self>,
        path: &str,
        mode: AccessMode,
        source: RuleSource,
    ) -> Result<usize> {
        let ino = self.resolve(path).await?;
        let rule = {
            let tree = self.tree.read();
            Rule {
                ino,
                link: tree.link_of(ino).map(|link| link.0.clone()),
                path_at_set: tree.path(ino).unwrap_or_else(|| path.to_owned()),
                mode,
                source,
            }
        };
        let stored = rule.clone();
        self.store
            .write_durable(move |conn| crate::access::store_rule(conn, &stored, false))
            .await?;
        self.access.write().insert(rule);
        self.invalidate_subtree(ino);
        let tree = self.tree.read();
        Ok(self
            .journal
            .inos()
            .into_iter()
            .filter(|&pending| tree.is_within(pending, ino))
            .count())
    }

    /// `access unset <path>`: the rule on the entry at `path`, or an
    /// orphaned rule that was set on `path`. Returns whether one existed.
    pub async fn access_unset(self: &Arc<Self>, path: &str) -> Result<bool> {
        let resolved = self.resolve(path).await.ok();
        let ino = {
            let access = self.access.read();
            let tree = self.tree.read();
            let normalized = format!("/{}", path.trim_matches('/'));
            resolved
                .filter(|ino| access.get(*ino).is_some())
                .or_else(|| access.orphaned_ino(&tree, &normalized))
        };
        let Some(ino) = ino else {
            return Ok(false);
        };
        self.store
            .write_durable(move |conn| crate::access::delete_rule(conn, ino))
            .await?;
        self.access.write().remove(ino);
        if self.tree.read().get(ino).is_some() {
            self.invalidate_subtree(ino);
        }
        Ok(true)
    }

    /// `access list [<path>]`.
    pub async fn access_list(
        self: &Arc<Self>,
        path: Option<&str>,
    ) -> Result<(Vec<RuleView>, AccessMode)> {
        let under = match path {
            Some(path) => Some(self.resolve(path).await?),
            None => None,
        };
        let access = self.access.read();
        let views = access.views(&self.tree.read(), under);
        Ok((views, self.config.access.default))
    }

    /// `access effective <path>`: the mode and the deciding rule's path
    /// (`None`: the default).
    pub async fn access_effective(
        self: &Arc<Self>,
        path: &str,
    ) -> Result<(AccessMode, Option<String>)> {
        let ino = self.resolve(path).await?;
        let access = self.access.read();
        let tree = self.tree.read();
        let (mode, rule) = access.effective(&tree, ino);
        Ok((
            mode,
            rule.map(|rule| {
                tree.path(rule.ino)
                    .unwrap_or_else(|| rule.path_at_set.clone())
            }),
        ))
    }

    /// Imports `[[access.rule]]` from the config once per path; from then on
    /// the database is authoritative (§11.5).
    pub async fn import_config_rules(self: &Arc<Self>) {
        for rule in self.config.access.rule.clone() {
            let key = format!("access_config_imported:/{}", rule.path.trim_matches('/'));
            if self.store.meta_get(&key).ok().flatten().is_some() {
                continue;
            }
            match self
                .access_set(&rule.path, rule.mode, RuleSource::Config)
                .await
            {
                Ok(_) => {
                    let stored = key.clone();
                    if let Err(error) = self
                        .store
                        .write_durable(move |conn| meta_set(conn, &stored, "1"))
                        .await
                    {
                        tracing::warn!(%error, "cannot record an imported access rule");
                    }
                }
                Err(error) => {
                    tracing::warn!(path = rule.path, %error, "cannot import an access rule from the config");
                }
            }
        }
    }
}
