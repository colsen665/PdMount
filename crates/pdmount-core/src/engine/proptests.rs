//! Journal property tests (DESIGN.md §15.1): random sequences of namespace
//! and content changes, restarts and crashes, checked against a POSIX
//! model.
//!
//! After every step the engine's result (success or errno) must equal the
//! model's. After the final drain:
//! - the fake drive equals the model: names, kinds, bytes, modes, and
//!   mtimes that were set explicitly;
//! - the journal is empty;
//! - a fresh engine with an empty state over the same fake sees the same
//!   tree.
//!
//! Another device's changes (`Op::Remote`) are made at clean points, after
//! a drain, and must arrive through the event stream (§9, M2.10).
//!
//! Two devices and an app (M3.17) are tested further down, with their own
//! invariants.
//!
//! `PROPTEST_CASES` sets the number of cases (default 32; the M2 exit check
//! runs 10 000).

use super::*;
use crate::fake_fs::fake_tree;
use crate::units::HumanDuration;
use bytes::Bytes;
use pdmount_backend::fake::{FakeBackend, FakeClient};
use pdmount_backend::{FileMeta, UploadSource};
use proptest::prelude::*;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::UNIX_EPOCH;

const NAMES: [&str; 3] = ["a", "b", "d"];
const PARENTS: [&str; 3] = ["", "/d", "/b"];
const MODES: [u32; 3] = [0o600, 0o644, 0o755];

#[derive(Clone)]
enum Op {
    Create(String),
    Write(String, u64, Vec<u8>),
    Truncate(String, u64),
    Rename(String, String, bool),
    Unlink(String),
    Mkdir(String),
    Rmdir(String),
    Chmod(String, u32),
    Touch(String, u64),
    Drain,
    Restart,
    Crash,
    Remote(Remote),
}

/// A change by another device, applied to the fake directly.
#[derive(Clone)]
enum Remote {
    Write(String, Vec<u8>),
    Create(String, Vec<u8>),
    Rename(String, String),
    Trash(String),
}

impl std::fmt::Debug for Op {
    /// Compact, so a shrunk failing case is readable: data as its length
    /// and first byte.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Create(p) => write!(f, "Create({p:?})"),
            Self::Write(p, o, d) => write!(
                f,
                "Write({p:?}, {o}, len {} first {:?})",
                d.len(),
                d.first()
            ),
            Self::Truncate(p, s) => write!(f, "Truncate({p:?}, {s})"),
            Self::Rename(a, b, n) => write!(f, "Rename({a:?}, {b:?}, noreplace {n})"),
            Self::Unlink(p) => write!(f, "Unlink({p:?})"),
            Self::Mkdir(p) => write!(f, "Mkdir({p:?})"),
            Self::Rmdir(p) => write!(f, "Rmdir({p:?})"),
            Self::Chmod(p, m) => write!(f, "Chmod({p:?}, {m:o})"),
            Self::Touch(p, t) => write!(f, "Touch({p:?}, {t})"),
            Self::Drain => write!(f, "Drain"),
            Self::Restart => write!(f, "Restart"),
            Self::Crash => write!(f, "Crash"),
            Self::Remote(change) => write!(f, "Remote({change:?})"),
        }
    }
}

impl std::fmt::Debug for Remote {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Write(p, d) => write!(f, "Write({p:?}, len {})", d.len()),
            Self::Create(p, d) => write!(f, "Create({p:?}, len {})", d.len()),
            Self::Rename(a, b) => write!(f, "Rename({a:?}, {b:?})"),
            Self::Trash(p) => write!(f, "Trash({p:?})"),
        }
    }
}

fn path() -> impl Strategy<Value = String> {
    (0..PARENTS.len(), 0..NAMES.len()).prop_map(|(p, n)| format!("{}/{}", PARENTS[p], NAMES[n]))
}

/// The namespace and content operations of one device.
fn local_op() -> impl Strategy<Value = Op> {
    prop_oneof![
        3 => path().prop_map(Op::Create),
        4 => (path(), 0u64..12_000, proptest::collection::vec(any::<u8>(), 0..6_000))
            .prop_map(|(p, o, d)| Op::Write(p, o, d)),
        1 => (path(), 0u64..15_000).prop_map(|(p, s)| Op::Truncate(p, s)),
        3 => (path(), path(), any::<bool>()).prop_map(|(a, b, n)| Op::Rename(a, b, n)),
        2 => path().prop_map(Op::Unlink),
        2 => path().prop_map(Op::Mkdir),
        1 => path().prop_map(Op::Rmdir),
        1 => (path(), 0..MODES.len()).prop_map(|(p, m)| Op::Chmod(p, MODES[m])),
        1 => (path(), 1_000_000u64..2_000_000).prop_map(|(p, t)| Op::Touch(p, t)),
    ]
}

/// A change by another device or an app.
fn remote() -> impl Strategy<Value = Remote> {
    prop_oneof![
        (path(), proptest::collection::vec(any::<u8>(), 0..3_000))
            .prop_map(|(p, d)| Remote::Write(p, d)),
        (path(), proptest::collection::vec(any::<u8>(), 0..3_000))
            .prop_map(|(p, d)| Remote::Create(p, d)),
        (path(), path()).prop_map(|(a, b)| Remote::Rename(a, b)),
        path().prop_map(Remote::Trash),
    ]
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        18 => local_op(),
        1 => Just(Op::Drain),
        1 => Just(Op::Restart),
        1 => Just(Op::Crash),
        4 => remote().prop_map(Op::Remote),
    ]
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Entry {
    Dir,
    File {
        data: Vec<u8>,
        mode: u32,
        /// Known only after an explicit touch with no later write.
        mtime: Option<SystemTime>,
    },
}

/// The POSIX model: paths to entries, the root implied.
#[derive(Debug, Default)]
struct Model {
    entries: BTreeMap<String, Entry>,
}

fn parent_of(path: &str) -> (&str, &str) {
    let (parent, name) = path.rsplit_once('/').expect("absolute");
    (parent, name)
}

const ENOENT: i32 = nix::libc::ENOENT;
const EEXIST: i32 = nix::libc::EEXIST;
const ENOTDIR: i32 = nix::libc::ENOTDIR;
const EISDIR: i32 = nix::libc::EISDIR;
const ENOTEMPTY: i32 = nix::libc::ENOTEMPTY;
const EINVAL: i32 = nix::libc::EINVAL;

impl Model {
    /// The parent directory must exist and be a directory.
    fn check_parent(&self, path: &str) -> std::result::Result<(), i32> {
        let (parent, _) = parent_of(path);
        match parent {
            "" => Ok(()),
            parent => match self.entries.get(parent) {
                Some(Entry::Dir) => Ok(()),
                Some(Entry::File { .. }) => Err(ENOTDIR),
                None => Err(ENOENT),
            },
        }
    }

    fn file(&mut self, path: &str) -> std::result::Result<&mut Entry, i32> {
        self.check_parent(path)?;
        match self.entries.get_mut(path) {
            Some(entry @ Entry::File { .. }) => Ok(entry),
            Some(Entry::Dir) => Err(EISDIR),
            None => Err(ENOENT),
        }
    }

    fn has_children(&self, dir: &str) -> bool {
        let prefix = format!("{dir}/");
        self.entries.keys().any(|path| path.starts_with(&prefix))
    }

    fn apply(&mut self, op: &Op) -> std::result::Result<(), i32> {
        match op {
            Op::Create(path) => {
                self.check_parent(path)?;
                if self.entries.contains_key(path) {
                    return Err(EEXIST);
                }
                self.entries.insert(
                    path.clone(),
                    Entry::File {
                        data: Vec::new(),
                        mode: 0o644,
                        mtime: None,
                    },
                );
                Ok(())
            }
            Op::Write(path, offset, bytes) => {
                if let Entry::File { data, mtime, .. } = self.file(path)? {
                    // A zero-length write changes nothing (POSIX).
                    if bytes.is_empty() {
                        return Ok(());
                    }
                    let end = *offset as usize + bytes.len();
                    if data.len() < end {
                        data.resize(end, 0);
                    }
                    data[*offset as usize..end].copy_from_slice(bytes);
                    *mtime = None;
                }
                Ok(())
            }
            Op::Truncate(path, size) => {
                if let Entry::File { data, mtime, .. } = self.file(path)? {
                    data.resize(*size as usize, 0);
                    *mtime = None;
                }
                Ok(())
            }
            Op::Chmod(path, new_mode) => {
                self.check_parent(path)?;
                match self.entries.get_mut(path) {
                    Some(Entry::File { mode, .. }) => *mode = *new_mode,
                    Some(Entry::Dir) => {}
                    None => return Err(ENOENT),
                }
                Ok(())
            }
            Op::Touch(path, secs) => {
                self.check_parent(path)?;
                match self.entries.get_mut(path) {
                    Some(Entry::File { mtime, .. }) => {
                        *mtime = Some(UNIX_EPOCH + Duration::from_secs(*secs));
                    }
                    Some(Entry::Dir) => {}
                    None => return Err(ENOENT),
                }
                Ok(())
            }
            Op::Mkdir(path) => {
                self.check_parent(path)?;
                if self.entries.contains_key(path) {
                    return Err(EEXIST);
                }
                self.entries.insert(path.clone(), Entry::Dir);
                Ok(())
            }
            Op::Unlink(path) => {
                self.check_parent(path)?;
                match self.entries.get(path) {
                    Some(Entry::File { .. }) => {
                        self.entries.remove(path);
                        Ok(())
                    }
                    Some(Entry::Dir) => Err(EISDIR),
                    None => Err(ENOENT),
                }
            }
            Op::Rmdir(path) => {
                self.check_parent(path)?;
                match self.entries.get(path) {
                    Some(Entry::Dir) if self.has_children(path) => Err(ENOTEMPTY),
                    Some(Entry::Dir) => {
                        self.entries.remove(path);
                        Ok(())
                    }
                    Some(Entry::File { .. }) => Err(ENOTDIR),
                    None => Err(ENOENT),
                }
            }
            Op::Rename(from, to, noreplace) => {
                // The kernel resolves both parents before looking at names.
                self.check_parent(from)?;
                self.check_parent(to)?;
                let source = self.entries.get(from).cloned().ok_or(ENOENT)?;
                if from == to {
                    return Ok(());
                }
                let source_dir = source == Entry::Dir;
                if source_dir && to.starts_with(&format!("{from}/")) {
                    return Err(EINVAL);
                }
                if let Some(target) = self.entries.get(to) {
                    if *noreplace {
                        return Err(EEXIST);
                    }
                    match (source_dir, target == &Entry::Dir) {
                        (true, false) => return Err(ENOTDIR),
                        (false, true) => return Err(EISDIR),
                        (true, true) if self.has_children(to) => return Err(ENOTEMPTY),
                        _ => {}
                    }
                }
                self.entries.remove(to);
                let prefix = format!("{from}/");
                let moved: Vec<(String, Entry)> = self
                    .entries
                    .iter()
                    .filter(|(path, _)| *path == from || path.starts_with(&prefix))
                    .map(|(path, entry)| (path.clone(), entry.clone()))
                    .collect();
                for (path, entry) in moved {
                    self.entries.remove(&path);
                    let new_path = format!("{to}{}", &path[from.len()..]);
                    self.entries.insert(new_path, entry);
                }
                Ok(())
            }
            Op::Drain | Op::Restart | Op::Crash | Op::Remote(_) => Ok(()),
        }
    }

    /// Applies another device's change if the server would accept it;
    /// returns whether it did.
    fn apply_remote(&mut self, change: &Remote) -> bool {
        match change {
            Remote::Write(path, bytes) => match self.entries.get_mut(path) {
                Some(Entry::File { data, mtime, .. }) => {
                    *data = bytes.clone();
                    *mtime = None;
                    true
                }
                _ => false,
            },
            Remote::Create(path, bytes) => {
                if self.check_parent(path).is_err() || self.entries.contains_key(path) {
                    return false;
                }
                let entry = Entry::File {
                    data: bytes.clone(),
                    mode: REMOTE_MODE,
                    mtime: None,
                };
                self.entries.insert(path.clone(), entry);
                true
            }
            Remote::Rename(from, to) => {
                let valid = self.entries.contains_key(from)
                    && from != to
                    && !to.starts_with(&format!("{from}/"))
                    && self.check_parent(to).is_ok()
                    && !self.entries.contains_key(to);
                valid
                    && self
                        .apply(&Op::Rename(from.clone(), to.clone(), true))
                        .is_ok()
            }
            Remote::Trash(path) => {
                if !self.entries.contains_key(path) {
                    return false;
                }
                let prefix = format!("{path}/");
                self.entries
                    .retain(|entry, _| entry != path && !entry.starts_with(&prefix));
                true
            }
        }
    }
}

/// Mode of files another device creates.
const REMOTE_MODE: u32 = 0o640;

/// The remote node at `path` ("" is the root), if there is one.
async fn find_remote(backend: &FakeBackend, path: &str) -> Option<pdmount_backend::RemoteNode> {
    let mut node = backend.root().await.ok()?;
    for name in path.split('/').filter(|name| !name.is_empty()) {
        node = backend
            .list_children(&node.link_id)
            .await
            .ok()?
            .into_iter()
            .find(|child| child.name == name)?;
    }
    Some(node)
}

/// The remote node at `path`; the model has already accepted the change.
async fn remote_at(backend: &FakeBackend, path: &str) -> pdmount_backend::RemoteNode {
    find_remote(backend, path)
        .await
        .unwrap_or_else(|| panic!("no remote {path}"))
}

/// Makes `change` on the fake, as another device would. The model has
/// already accepted it.
async fn change_remote(backend: &FakeBackend, change: &Remote, model: &Model) {
    let meta = |mode| FileMeta {
        mtime: UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        mode: Some(mode),
        media_type: "application/octet-stream".into(),
        sha1: None,
    };
    let source = |data: &[u8]| UploadSource::Bytes(Bytes::copy_from_slice(data));
    match change {
        Remote::Write(path, data) => {
            let node = remote_at(backend, path).await;
            let Some(Entry::File { mode, .. }) = model.entries.get(path) else {
                unreachable!("checked by the model");
            };
            backend
                .upload_revision(
                    &node.link_id,
                    node.active_revision.as_ref().expect("rev"),
                    source(data),
                    meta(*mode),
                )
                .await
                .expect("remote write");
        }
        Remote::Create(path, data) => {
            let (parent, name) = parent_of(path);
            let parent = remote_at(backend, parent).await;
            backend
                .upload_new(&parent.link_id, name, source(data), meta(REMOTE_MODE))
                .await
                .expect("remote create");
        }
        Remote::Rename(from, to) => {
            let node = remote_at(backend, from).await;
            let (parent, name) = parent_of(to);
            let parent = remote_at(backend, parent).await;
            backend
                .rename(&node.link_id, Some(&parent.link_id), name, None)
                .await
                .expect("remote rename");
        }
        Remote::Trash(path) => {
            let node = remote_at(backend, path).await;
            let results = backend.trash(&[node.link_id]).await.expect("trash");
            results.into_iter().for_each(|r| r.expect("remote trash"));
        }
    }
}

fn config(debounce: Duration) -> Config {
    let mut config = Config::default();
    config.upload.debounce = HumanDuration(debounce);
    config
}

/// Upload debounce of the single-device test and of the concurrent
/// property test.
const DEBOUNCE: Duration = Duration::from_millis(5);

async fn open(
    backend: &FakeBackend,
    dir: &std::path::Path,
    device: &str,
    debounce: Duration,
) -> Arc<Engine> {
    let mut options = EngineOptions::in_dir(
        dir,
        config(debounce),
        super::tests::test_device(device),
        super::tests::fixed_clock(),
    );
    options.cache.disk_bytes = 8 << 20;
    let (engine, _inval) = Engine::open(Arc::new(backend.clone()), options)
        .await
        .expect("engine");
    // The fake needs no pacing; every event sync would wait for its turns.
    engine.gate.set_interval(Duration::ZERO);
    engine.start_background();
    engine
}

fn errno_of<T>(result: Result<T>) -> std::result::Result<T, i32> {
    result.map_err(|error| error.errno())
}

/// Parent inode and name of a path, listing on the way.
async fn locate(engine: &Arc<Engine>, path: &str) -> std::result::Result<(Ino, String), i32> {
    let (parent, name) = parent_of(path);
    let ino = errno_of(engine.resolve(parent).await)?;
    let kind = engine.getattr(ino).map(|a| a.kind).map_err(|e| e.errno())?;
    if kind != NodeKind::Folder {
        return Err(ENOTDIR);
    }
    errno_of(engine.ensure_listed(ino).await)?;
    Ok((ino, name.to_owned()))
}

async fn existing(engine: &Arc<Engine>, path: &str) -> std::result::Result<Ino, i32> {
    let (parent, name) = locate(engine, path).await?;
    let child = engine
        .tree
        .read()
        .child(parent, &name)
        .map_err(|e| e.errno())?;
    child.ok_or(ENOENT)
}

/// Reserves inode numbers when the hot path ran out, as the FUSE slow
/// path does.
async fn with_inos<T>(engine: &Arc<Engine>, call: impl Fn() -> Result<T>) -> Result<T> {
    match call() {
        Err(CoreError::NeedsInos) => {
            engine.reserve_inos(16).await?;
            call()
        }
        other => other,
    }
}
/// Applies `op`; for a create, write or truncate, the inode it acted on.
async fn apply(engine: &Arc<Engine>, op: &Op) -> std::result::Result<Option<Ino>, i32> {
    let write = OpenMode {
        read: false,
        write: true,
        truncate: false,
    };
    match op {
        Op::Create(path) => {
            let (parent, name) = locate(engine, path).await?;
            let created = with_inos(engine, || {
                engine.create(parent, name.as_bytes(), 0o644, true, write)
            })
            .await;
            let (attr, handle) = errno_of(created)?;
            engine.release_file(handle);
            Ok(Some(attr.ino))
        }
        Op::Write(path, offset, data) => {
            let ino = existing(engine, path).await?;
            let (handle, _) = errno_of(engine.open_file(ino, write))?;
            let result = match engine.write_local(handle, *offset, data) {
                Err(CoreError::NeedsMaterialize(_)) => {
                    errno_of(engine.write(handle, *offset, data.clone()).await)
                }
                other => errno_of(other),
            };
            engine.release_file(handle);
            result.map(|_| Some(ino))
        }
        Op::Truncate(path, size) => {
            let ino = existing(engine, path).await?;
            if engine
                .getattr(ino)
                .is_ok_and(|a| a.kind == NodeKind::Folder)
            {
                return Err(EISDIR);
            }
            let change = SetAttr {
                size: Some(*size),
                ..SetAttr::default()
            };
            errno_of(engine.setattr_slow(ino, change).await).map(|_| Some(ino))
        }
        Op::Chmod(path, mode) => {
            let ino = existing(engine, path).await?;
            // Directory modes would need permission modeling; the model
            // treats them as no-ops, and file modes keep owner rw.
            if engine
                .getattr(ino)
                .is_ok_and(|a| a.kind == NodeKind::Folder)
            {
                return Ok(None);
            }
            let change = SetAttr {
                mode: Some(*mode),
                ..SetAttr::default()
            };
            errno_of(engine.setattr(ino, &change)).map(|_| None)
        }
        Op::Touch(path, secs) => {
            let ino = existing(engine, path).await?;
            let change = SetAttr {
                mtime: Some(UNIX_EPOCH + Duration::from_secs(*secs)),
                ..SetAttr::default()
            };
            errno_of(engine.setattr(ino, &change)).map(|_| None)
        }
        Op::Mkdir(path) => {
            let (parent, name) = locate(engine, path).await?;
            errno_of(with_inos(engine, || engine.mkdir(parent, name.as_bytes(), 0o755)).await)
                .map(|_| None)
        }
        Op::Unlink(path) => {
            let (parent, name) = locate(engine, path).await?;
            errno_of(engine.unlink(parent, name.as_bytes())).map(|()| None)
        }
        Op::Rmdir(path) => {
            let (parent, name) = locate(engine, path).await?;
            let result = engine.rmdir(parent, name.as_bytes());
            match result {
                Err(CoreError::NeedsListing(dir)) => {
                    errno_of(engine.ensure_listed(dir).await)?;
                    errno_of(engine.rmdir(parent, name.as_bytes()))
                }
                other => errno_of(other),
            }
            .map(|()| None)
        }
        Op::Rename(from, to, noreplace) => {
            let (from_parent, from_name) = locate(engine, from).await?;
            let (to_parent, to_name) = locate(engine, to).await?;
            let mode = if *noreplace {
                RenameMode::NoReplace
            } else {
                RenameMode::Replace
            };
            let rename = || {
                engine.rename(
                    from_parent,
                    from_name.as_bytes(),
                    to_parent,
                    to_name.as_bytes(),
                    mode,
                )
            };
            // A replaced directory must be listed to know it is empty.
            match rename() {
                Err(CoreError::NeedsListing(dir)) => {
                    errno_of(engine.ensure_listed(dir).await)?;
                    errno_of(rename())
                }
                other => errno_of(other),
            }
            .map(|_| None)
        }
        Op::Drain | Op::Restart | Op::Crash | Op::Remote(_) => Ok(None),
    }
}

async fn assert_fake_matches(backend: &FakeBackend, model: &Model) {
    let fake = fake_tree(backend).await;
    let paths: Vec<&String> = fake.keys().collect();
    let expected: Vec<&String> = model.entries.keys().collect();
    assert_eq!(paths, expected, "names on the fake");
    for (path, entry) in &model.entries {
        let remote = &fake[path];
        match entry {
            Entry::Dir => assert_eq!(remote.kind, NodeKind::Folder, "{path}"),
            Entry::File { data, mode, mtime } => {
                assert_eq!(remote.kind, NodeKind::File, "{path}");
                assert_eq!(&remote.content, data, "content of {path}");
                assert_eq!(remote.mode, Some(*mode), "mode of {path}");
                if let Some(mtime) = mtime {
                    assert_eq!(remote.mtime, Some(*mtime), "mtime of {path}");
                }
            }
        }
    }
}

async fn assert_engine_matches(engine: &Arc<Engine>, model: &Model) {
    for (path, entry) in &model.entries {
        let ino = match existing(engine, path).await {
            Ok(ino) => ino,
            Err(errno) => {
                let tree = engine.tree.read();
                let root = tree.get(ROOT_INO).expect("root");
                let children: Vec<(String, Ino, bool)> = root
                    .children
                    .iter()
                    .map(|(name, ino)| (name.clone(), *ino, tree.get(*ino).is_some()))
                    .collect();
                let rows = engine.store.read(crate::rows::load_rows).expect("rows");
                let rows: Vec<(Ino, Option<Ino>, Option<String>, bool)> = rows
                    .iter()
                    .map(|r| (r.ino, r.parent, r.name.clone(), r.remote.is_some()))
                    .collect();
                panic!(
                    "{path} missing in the engine: errno {errno}; root listed {} children {children:?}; \
                     stored rows {rows:?}",
                    root.listed
                );
            }
        };
        let attr = engine.getattr(ino).expect("attr");
        match entry {
            Entry::Dir => assert_eq!(attr.kind, NodeKind::Folder, "{path}"),
            Entry::File { data, mode, .. } => {
                assert_eq!(attr.perm, *mode, "mode of {path}");
                assert_eq!(&crate::fake_fs::read_all(engine, ino).await, data, "{path}");
            }
        }
    }
    // Nothing more: every listed directory holds exactly the model's names.
    let mut dirs = vec![(ROOT_INO, String::new())];
    while let Some((dir, prefix)) = dirs.pop() {
        engine.ensure_listed(dir).await.expect("list");
        let children: Vec<(String, Ino)> = engine
            .tree
            .read()
            .get(dir)
            .map(|node| node.children.iter().map(|(n, i)| (n.clone(), *i)).collect())
            .unwrap_or_default();
        for (name, ino) in children {
            let path = format!("{prefix}/{name}");
            assert!(model.entries.contains_key(&path), "unexpected {path}");
            if engine
                .getattr(ino)
                .is_ok_and(|a| a.kind == NodeKind::Folder)
            {
                dirs.push((ino, path));
            }
        }
    }
}

/// State in tmpfs where available: thousands of cases with durable
/// commits would otherwise wait on disk flushes.
fn temp_dir() -> tempfile::TempDir {
    let shm = std::path::Path::new("/dev/shm");
    if shm.is_dir()
        && let Ok(dir) = tempfile::tempdir_in(shm)
    {
        return dir;
    }
    tempfile::tempdir().expect("tempdir")
}

/// One engine with its own runtime: dropping the runtime ends every task
/// the engine spawned, like the exit of the daemon process.
struct Instance {
    runtime: tokio::runtime::Runtime,
    engine: Arc<Engine>,
}

impl Instance {
    fn open(
        backend: &FakeBackend,
        dir: &std::path::Path,
        device: &str,
        debounce: Duration,
    ) -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("runtime");
        let engine = runtime.block_on(open(backend, dir, device, debounce));
        Self { runtime, engine }
    }

    fn run<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }

    /// Clean stop: everything persisted, then the process ends.
    fn stop(self) {
        self.run(self.engine.shutdown()).expect("shutdown");
        self.kill();
    }

    /// kill -9 after the last reply: FUSE replies wait for the store commit
    /// (§4.3), so queued store writes commit first and the model expects
    /// every step to survive; nothing else happens. Losing queued writes is
    /// tested in `staging_outlives_a_crash_before_its_rows_are_committed`.
    fn kill(self) {
        let Self { runtime, engine } = self;
        runtime.block_on(engine.store.flush()).expect("flush");
        // Every task ends, so the old store writer is gone before the
        // database is opened again (one process, one writer).
        runtime.shutdown_timeout(Duration::from_secs(10));
        assert_eq!(
            Arc::strong_count(&engine),
            1,
            "engine still referenced after the kill"
        );
        drop(engine);
    }
}

fn run_case(ops: Vec<Op>) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("PDMOUNT_TEST_LOG"))
        .with_test_writer()
        .try_init();
    let temp = temp_dir();
    let backend = FakeBackend::new();
    backend.set_block_size(4096);
    let mut model = Model::default();
    let mut generation = 0;
    let state = |n: u32| temp.path().join(format!("state-{n}"));
    let mut instance = Instance::open(&backend, &state(generation), "dev-a", DEBOUNCE);
    for op in &ops {
        let started = std::time::Instant::now();
        let _timing = Timing(op, started);
        match op {
            Op::Drain => {
                let engine = &instance.engine;
                let counts = instance.run(engine.drain(Duration::from_secs(5)));
                assert_eq!(
                    (counts.pending, counts.parked),
                    (0, 0),
                    "{:#?}",
                    engine.describe_pending()
                );
                instance.run(assert_fake_matches(&backend, &model));
            }
            Op::Restart => {
                instance.stop();
                instance = Instance::open(&backend, &state(generation), "dev-a", DEBOUNCE);
            }
            Op::Crash => {
                instance.kill();
                instance = Instance::open(&backend, &state(generation), "dev-a", DEBOUNCE);
            }
            Op::Remote(change) => {
                let engine = &instance.engine;
                let counts = instance.run(engine.drain(Duration::from_secs(5)));
                assert_eq!(
                    (counts.pending, counts.parked),
                    (0, 0),
                    "{:#?}",
                    engine.describe_pending()
                );
                let before = model.entries.clone();
                if model.apply_remote(change) {
                    let accepted = Model { entries: before };
                    instance.run(change_remote(&backend, change, &accepted));
                    instance.run(engine.sync_events()).expect("events");
                    instance.run(assert_engine_matches(engine, &model));
                }
            }
            op => {
                let expected = model.apply(op);
                let actual = instance.run(apply(&instance.engine, op)).map(drop);
                assert_eq!(actual, expected, "{op:?}");
            }
        }
        check_tree(&instance.engine, || format!("{op:?}"));
    }
    let engine = Arc::clone(&instance.engine);
    let counts = instance.run(engine.drain(Duration::from_secs(5)));
    assert_eq!(
        (counts.pending, counts.parked),
        (0, 0),
        "{:#?}",
        engine.describe_pending()
    );
    instance.run(assert_fake_matches(&backend, &model));
    instance.run(assert_engine_matches(&engine, &model));
    drop(engine);
    instance.stop();
    // A fresh core from an empty database sees the same tree.
    generation += 1;
    let fresh = Instance::open(&backend, &state(generation), "dev-a", DEBOUNCE);
    fresh.run(assert_engine_matches(&fresh.engine, &model));
    fresh.stop();
}

/// The tree's internal consistency after a step.
fn check_tree(engine: &Arc<Engine>, step: impl Fn() -> String) {
    if let Err(problem) = engine.tree.read().check_invariants() {
        panic!("tree invariant broken after {}: {problem}", step());
    }
}

struct Timing<'a>(&'a Op, std::time::Instant);

impl Drop for Timing<'_> {
    fn drop(&mut self) {
        if std::env::var_os("PDMOUNT_PROPTEST_TIMING").is_some()
            && self.1.elapsed() > Duration::from_millis(30)
        {
            eprintln!("slow step {:?}: {:?}", self.0, self.1.elapsed());
        }
    }
}

// Two devices and an app (M3.17).
//
// Two engines (`dev-a`, `dev-b`) share one fake drive, each as its own
// client, next to an app that uploads without a SHA-1 or a mode. Their
// operations interleave with pending local changes: nothing drains before
// a remote change. There is no model of the outcome, since conflict
// handling decides it. After the system has settled:
// 1. nothing is parked;
// 2. both cores and the fake show the same tree;
// 3. no acknowledged content is unreachable: every content a device
//    wrote and did not delete itself, and every content the app uploaded,
//    is still a revision somewhere on the drive (names, conflict copies,
//    history, trash);
// 4. a fresh core with an empty database sees that tree.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum Dev {
    A,
    B,
}

impl Dev {
    const ALL: [Dev; 2] = [Dev::A, Dev::B];

    fn index(self) -> usize {
        self as usize
    }

    fn label(self) -> &'static str {
        match self {
            Dev::A => "dev-a",
            Dev::B => "dev-b",
        }
    }
}

#[derive(Clone, Debug)]
enum Step {
    Local(Dev, Op),
    Sync(Dev),
    Drain(Dev),
    Restart(Dev),
    Crash(Dev),
    /// A change made by the official app.
    App(Remote),
}

fn dev() -> impl Strategy<Value = Dev> {
    prop_oneof![Just(Dev::A), Just(Dev::B)]
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        24 => (dev(), local_op()).prop_map(|(d, op)| Step::Local(d, op)),
        3 => dev().prop_map(Step::Sync),
        2 => dev().prop_map(Step::Drain),
        1 => dev().prop_map(Step::Restart),
        1 => dev().prop_map(Step::Crash),
        4 => remote().prop_map(Step::App),
    ]
}

/// What a failing local operation may report; never EIO or an internal
/// error.
const ALLOWED_ERRNOS: [i32; 6] = [ENOENT, EEXIST, ENOTDIR, EISDIR, ENOTEMPTY, EINVAL];

type Sha1Hash = [u8; 20];

fn sha1_of(data: &[u8]) -> Sha1Hash {
    use sha1::Digest;
    sha1::Sha1::digest(data).into()
}

/// The inode a successful `op` deletes on purpose: an unlinked file or the
/// target of a replacing rename.
async fn deleted_by(engine: &Arc<Engine>, op: &Op) -> Option<Ino> {
    match op {
        Op::Unlink(path) => existing(engine, path).await.ok(),
        Op::Rename(from, to, _) => {
            let target = existing(engine, to).await.ok()?;
            (existing(engine, from).await.ok() != Some(target)).then_some(target)
        }
        _ => None,
    }
}

/// The whole content of a file, or `None` if the node is gone.
async fn read_if_present(engine: &Arc<Engine>, ino: Ino) -> Option<Vec<u8>> {
    let size = engine.getattr(ino).ok()?.size;
    let (handle, _) = engine.open_file(ino, OpenMode::default()).ok()?;
    let mut out = Vec::new();
    let mut complete = true;
    while (out.len() as u64) < size {
        match engine.read(handle, out.len() as u64, 1 << 20).await {
            Ok(chunk) if !chunk.is_empty() => out.extend_from_slice(&chunk),
            Ok(_) => break,
            Err(_) => {
                complete = false;
                break;
            }
        }
    }
    engine.release_file(handle);
    complete.then_some(out)
}

/// Every node the engine shows, by path.
async fn engine_tree(engine: &Arc<Engine>) -> BTreeMap<String, Entry> {
    let mut out = BTreeMap::new();
    let mut dirs = vec![(ROOT_INO, String::new())];
    while let Some((dir, prefix)) = dirs.pop() {
        engine.ensure_listed(dir).await.expect("list");
        let children: Vec<(String, Ino)> = engine
            .tree
            .read()
            .get(dir)
            .map(|node| node.children.iter().map(|(n, i)| (n.clone(), *i)).collect())
            .unwrap_or_default();
        for (name, ino) in children {
            let path = format!("{prefix}/{name}");
            let attr = engine.getattr(ino).expect("attr");
            if attr.kind == NodeKind::Folder {
                out.insert(path.clone(), Entry::Dir);
                dirs.push((ino, path));
            } else {
                let data = crate::fake_fs::read_all(engine, ino).await;
                let mode = attr.perm;
                out.insert(
                    path,
                    Entry::File {
                        data,
                        mode,
                        mtime: None,
                    },
                );
            }
        }
    }
    out
}

/// Names, kinds and bytes equal; modes where the drive records one (the
/// app leaves it out).
async fn assert_tree_is_fake(who: &str, tree: &BTreeMap<String, Entry>, backend: &FakeBackend) {
    let fake = fake_tree(backend).await;
    let paths: Vec<&String> = tree.keys().collect();
    let expected: Vec<&String> = fake.keys().collect();
    assert_eq!(paths, expected, "names of {who} against the fake");
    for (path, remote) in &fake {
        match &tree[path] {
            Entry::Dir => assert_eq!(remote.kind, NodeKind::Folder, "{who}: {path}"),
            Entry::File { data, mode, .. } => {
                assert_eq!(remote.kind, NodeKind::File, "{who}: {path}");
                assert_eq!(&remote.content, data, "{who}: content of {path}");
                if let Some(remote_mode) = remote.mode {
                    assert_eq!(*mode, remote_mode, "{who}: mode of {path}");
                }
            }
        }
    }
}

/// The app makes `change` if the drive accepts it; returns the SHA-1 of
/// the content it uploaded.
async fn change_app(app: &FakeBackend, change: &Remote) -> Option<Sha1Hash> {
    let meta = FileMeta {
        mtime: UNIX_EPOCH + Duration::from_secs(1_800_000_000),
        mode: None,
        media_type: "application/octet-stream".into(),
        sha1: None,
    };
    let source = |data: &[u8]| UploadSource::Bytes(Bytes::copy_from_slice(data));
    match change {
        Remote::Write(path, data) => {
            let node = find_remote(app, path).await?;
            if node.kind != NodeKind::File {
                return None;
            }
            app.upload_revision(
                &node.link_id,
                node.active_revision.as_ref()?,
                source(data),
                meta,
            )
            .await
            .ok()?;
            Some(sha1_of(data))
        }
        Remote::Create(path, data) => {
            let (parent, name) = parent_of(path);
            let parent = find_remote(app, parent).await?;
            if parent.kind != NodeKind::Folder {
                return None;
            }
            app.upload_new(&parent.link_id, name, source(data), meta)
                .await
                .ok()?;
            Some(sha1_of(data))
        }
        Remote::Rename(from, to) => {
            let node = find_remote(app, from).await?;
            let (parent, name) = parent_of(to);
            let parent = find_remote(app, parent).await?;
            app.rename(&node.link_id, Some(&parent.link_id), name, None)
                .await
                .ok()?;
            None
        }
        Remote::Trash(path) => {
            let node = find_remote(app, path).await?;
            let _ = app.trash(&[node.link_id]).await;
            None
        }
    }
}

struct Fleet {
    backend: FakeBackend,
    app: FakeBackend,
    handles: [FakeBackend; 2],
    dirs: [std::path::PathBuf; 2],
    instances: [Option<Instance>; 2],
    /// Runs the app's calls, whichever instance is restarting.
    helper: tokio::runtime::Runtime,
    debounce: Duration,
    /// The content last written through each device's inode.
    ack: HashMap<(Dev, Ino), Sha1Hash>,
    app_written: HashSet<Sha1Hash>,
}

impl Fleet {
    fn new(root: &std::path::Path, debounce: Duration) -> Self {
        let backend = FakeBackend::new();
        backend.set_block_size(4096);
        backend.set_revision_limit(1000);
        let client = |uid: &str, own: bool| {
            backend.with_client(FakeClient {
                uid: Some(uid.to_owned()),
                claims_sha1: own,
                claims_mode: own,
            })
        };
        let handles = [client("pdmount-a", true), client("pdmount-b", true)];
        let app = client("web", false);
        let dirs = Dev::ALL.map(|dev| root.join(dev.label()));
        let instances = Dev::ALL.map(|dev| {
            Some(Instance::open(
                &handles[dev.index()],
                &dirs[dev.index()],
                dev.label(),
                debounce,
            ))
        });
        let helper = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        Self {
            backend,
            app,
            handles,
            dirs,
            instances,
            helper,
            debounce,
            ack: HashMap::new(),
            app_written: HashSet::new(),
        }
    }

    fn instance(&self, dev: Dev) -> &Instance {
        self.instances[dev.index()].as_ref().expect("running")
    }

    fn engine(&self, dev: Dev) -> Arc<Engine> {
        Arc::clone(&self.instance(dev).engine)
    }

    fn reopen(&mut self, dev: Dev) {
        self.instances[dev.index()] = Some(Instance::open(
            &self.handles[dev.index()],
            &self.dirs[dev.index()],
            dev.label(),
            self.debounce,
        ));
    }

    fn step(&mut self, step: &Step) {
        match step {
            Step::Local(dev, op) => self.local(*dev, op),
            Step::Sync(dev) => {
                let engine = self.engine(*dev);
                self.instance(*dev)
                    .run(engine.sync_events())
                    .unwrap_or_else(|e| panic!("{step:?}: events failed: {e}"));
            }
            Step::Drain(dev) => {
                let engine = self.engine(*dev);
                self.instance(*dev)
                    .run(engine.drain(Duration::from_secs(2)));
            }
            Step::Restart(dev) => {
                self.instances[dev.index()].take().expect("running").stop();
                self.reopen(*dev);
            }
            Step::Crash(dev) => {
                self.instances[dev.index()].take().expect("running").kill();
                self.reopen(*dev);
            }
            Step::App(change) => {
                if let Some(hash) = self.helper.block_on(change_app(&self.app, change))
                    && matches!(change, Remote::Write(..) | Remote::Create(..))
                {
                    self.app_written.insert(hash);
                }
            }
        }
        for dev in Dev::ALL {
            check_tree(&self.engine(dev), || format!("{dev:?} {step:?}"));
        }
    }

    fn local(&mut self, dev: Dev, op: &Op) {
        let engine = self.engine(dev);
        let instance = self.instance(dev);
        let deleted = instance.run(deleted_by(&engine, op));
        match instance.run(apply(&engine, op)) {
            Err(errno) => assert!(
                ALLOWED_ERRNOS.contains(&errno),
                "{dev:?} {op:?} failed with errno {errno}"
            ),
            Ok(target) => {
                // The node may be gone again already (a remote delete won);
                // then there is nothing to acknowledge. It is read by inode:
                // a conflict may have renamed it since.
                let written = target.and_then(|ino| {
                    instance.run(async {
                        let data = read_if_present(&engine, ino).await?;
                        Some((ino, sha1_of(&data)))
                    })
                });
                if let Some(ino) = deleted {
                    self.ack.remove(&(dev, ino));
                }
                if let Some((ino, hash)) = written {
                    self.ack.insert((dev, ino), hash);
                }
            }
        }
    }

    /// Drains and syncs both devices until nothing changes any more.
    fn quiesce(&self) {
        let mut last = None;
        for _ in 0..20 {
            for dev in Dev::ALL {
                let engine = self.engine(dev);
                self.instance(dev).run(engine.drain(Duration::from_secs(5)));
            }
            for dev in Dev::ALL {
                let engine = self.engine(dev);
                self.instance(dev)
                    .run(engine.sync_events())
                    .expect("events");
            }
            let idle = Dev::ALL.iter().all(|dev| {
                let counts = self.engine(*dev).journal.counts();
                (counts.pending, counts.parked) == (0, 0)
            });
            let mark = self.backend.changes_since(0).last().map(|(seq, _)| *seq);
            if idle && last == Some(mark) {
                return;
            }
            last = Some(mark);
        }
        panic!(
            "no quiescence after 20 rounds: A {:#?}, B {:#?}",
            self.engine(Dev::A).describe_pending(),
            self.engine(Dev::B).describe_pending()
        );
    }

    fn check_settled(&mut self, root: &std::path::Path) {
        self.quiesce();
        for dev in Dev::ALL {
            let engine = self.engine(dev);
            let counts = self
                .instance(dev)
                .run(async { engine.drain(Duration::ZERO).await });
            assert_eq!(
                (counts.pending, counts.parked),
                (0, 0),
                "{dev:?}: {:#?}",
                engine.describe_pending()
            );
        }
        let a = self.instance(Dev::A).run(engine_tree(&self.engine(Dev::A)));
        let b = self.instance(Dev::B).run(engine_tree(&self.engine(Dev::B)));
        self.helper
            .block_on(assert_tree_is_fake("core A", &a, &self.backend));
        self.helper
            .block_on(assert_tree_is_fake("core B", &b, &self.backend));
        assert_eq!(a, b, "two cores converge");

        let known = self.backend.all_revision_sha1s();
        for ((dev, ino), hash) in &self.ack {
            let path = self.engine(*dev).tree.read().path(*ino);
            assert!(
                known.contains(hash),
                "content acknowledged on {dev:?} at {path:?} (inode {ino}) is unreachable"
            );
        }
        for hash in &self.app_written {
            assert!(known.contains(hash), "content of the app is unreachable");
        }

        // A fresh core from an empty database sees the same tree.
        let fresh = Instance::open(
            &self.handles[0],
            &root.join("fresh"),
            "dev-a",
            self.debounce,
        );
        let seen = fresh.run(engine_tree(&fresh.engine));
        assert_eq!(seen, a, "a fresh core sees the same tree");
        fresh.stop();
    }

    fn finish(mut self) {
        for slot in &mut self.instances {
            slot.take().expect("running").stop();
        }
    }
}

fn run_concurrent_case(steps: &[Step], debounce: Duration) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("PDMOUNT_TEST_LOG"))
        .with_test_writer()
        .try_init();
    let temp = temp_dir();
    let mut fleet = Fleet::new(temp.path(), debounce);
    for step in steps {
        let started = std::time::Instant::now();
        fleet.step(step);
        if std::env::var_os("PDMOUNT_PROPTEST_TIMING").is_some()
            && started.elapsed() > Duration::from_millis(30)
        {
            eprintln!("slow step {step:?}: {:?}", started.elapsed());
        }
    }
    fleet.check_settled(temp.path());
    fleet.finish();
}

fn cases() -> u32 {
    std::env::var("PROPTEST_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(32)
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: cases(),
        failure_persistence: None,
        .. ProptestConfig::default()
    })]

    #[test]
    fn random_changes_converge_to_the_model(ops in proptest::collection::vec(op(), 1..40)) {
        run_case(ops);
    }

    #[test]
    fn two_devices_and_an_app_converge(steps in proptest::collection::vec(step(), 1..40)) {
        run_concurrent_case(&steps, DEBOUNCE);
    }
}

/// Cases the property test found, replayed exactly.
#[test]
fn regression_cases() {
    use Op::*;
    let s = |p: &str| p.to_owned();
    run_case(vec![
        Rename(s("/d/a"), s("/a"), true),
        Crash,
        Mkdir(s("/b")),
        Rename(s("/d/a"), s("/d"), true),
        Unlink(s("/d/b")),
        Drain,
        Write(s("/d/d"), 1996, vec![5; 5164]),
        Mkdir(s("/a")),
        Write(s("/a"), 8636, vec![142; 88]),
        Chmod(s("/b/b"), 0o644),
        Create(s("/a")),
        Touch(s("/d/a"), 1_051_727),
        Rename(s("/b"), s("/b/a"), true),
        Restart,
        Chmod(s("/d"), 0o755),
    ]);
    // A directory created after a rename onto a free name went missing
    // after a restart and a crash.
    run_case(vec![
        Crash,
        Create(s("/a")),
        Write(s("/a"), 8128, vec![178; 4939]),
        Crash,
        Restart,
        Rename(s("/a"), s("/d"), true),
        Mkdir(s("/b")),
        Drain,
        Restart,
        Crash,
        Rmdir(s("/d")),
    ]);
    // An atomic save's new mode must survive a restart's revalidation.
    run_case(vec![
        Create(s("/b")),
        Write(s("/b"), 0, vec![1; 100]),
        Chmod(s("/b"), 0o600),
        Drain,
        Create(s("/a")),
        Touch(s("/b"), 1_559_634),
        Rename(s("/a"), s("/b"), false),
        Restart,
        Rename(s("/b"), s("/d"), true),
    ]);
    // A write after an upload, before that upload's staging removal ran,
    // was lost by a crash.
    run_case(vec![
        Create(s("/b")),
        Drain,
        Write(s("/b"), 0, vec![46; 5269]),
        Crash,
    ]);
    // Another device's revision of a file uploaded just before must win
    // over the leftover staging after a crash.
    run_case(vec![
        Create(s("/a")),
        Remote(super::proptests::Remote::Write(s("/a"), vec![7])),
        Crash,
    ]);
}

/// Debounce of the replayed interleavings: nothing uploads before the
/// `Drain` that says so, so the steps meet in the order written.
const SLOW_DEBOUNCE: Duration = Duration::from_millis(400);

/// Cases the concurrent property test found, replayed.
#[test]
fn concurrent_regression_cases() {
    use self::Remote as R;
    use Dev::*;
    use Op::*;
    let s = |p: &str| p.to_owned();
    let a = |op| Step::Local(A, op);
    let b = |op| Step::Local(B, op);
    let w = |p: &str, byte: u8| Write(s(p), 0, vec![byte; 100]);
    let run = |steps: Vec<Step>| run_concurrent_case(&steps, SLOW_DEBOUNCE);
    // A restart while B listed the folder after yielding /b to the app's /b
    // aborted the step before the conflict was recorded: nothing listed the
    // folder again, and the app's /b stayed hidden (D055; data shortened
    // from the 10 000-case run, failed within 2 to 47 replays before).
    for _ in 0..30 {
        run_concurrent_case(
            &[
                Step::App(R::Create(s("/d"), vec![1; 2042])),
                a(Write(s("/d/d"), 6679, vec![98; 3740])),
                b(Write(s("/b"), 6652, vec![40; 1797])),
                a(Truncate(s("/b/d"), 8339)),
                b(Write(s("/d"), 3851, vec![79; 2545])),
                Step::Crash(A),
                Step::App(R::Create(s("/a"), vec![2; 405])),
                a(Write(s("/d/b"), 10092, vec![212; 4557])),
                Step::Sync(B),
                b(Rename(s("/b"), s("/b/d"), false)),
                b(Create(s("/b"))),
                Step::Crash(B),
                Step::Drain(A),
                Step::Sync(A),
                Step::App(R::Create(s("/b"), vec![3; 2150])),
                b(Rename(s("/d/a"), s("/a"), true)),
                b(Rename(s("/d/b"), s("/b/a"), false)),
                Step::App(R::Write(s("/d/a"), vec![4; 950])),
                Step::App(R::Create(s("/a"), vec![5; 2343])),
                a(Create(s("/a"))),
                a(Write(s("/b/d"), 3571, vec![197; 573])),
                Step::Sync(B),
                a(Create(s("/b/b"))),
                Step::Sync(B),
                Step::Restart(B),
                a(Chmod(s("/d/d"), 0o600)),
                b(Rename(s("/d/a"), s("/b/a"), false)),
                Step::App(R::Write(s("/b/b"), vec![6; 1843])),
            ],
            DEBOUNCE,
        );
    }
    // The server no longer lists a trashed folder; the event page that
    // reported it failed and was repeated for good.
    for _ in 0..20 {
        run_concurrent_case(
            &[
                b(Mkdir(s("/b"))),
                Step::Crash(B),
                a(Unlink(s("/b/a"))),
                Step::Sync(A),
                b(Mkdir(s("/b/b"))),
                b(Create(s("/b/a"))),
                Step::App(R::Trash(s("/b"))),
            ],
            DEBOUNCE,
        );
    }
    // A local file that is unlinked before it uploads must not hide the
    // remote folder of its name for good.
    run(vec![
        b(Mkdir(s("/d"))),
        a(Create(s("/d"))),
        Step::Sync(A),
        a(Unlink(s("/d"))),
    ]);
    // A node renamed into a folder that is trashed meanwhile stays where it
    // is on the drive: its rename loses, not the node.
    for _ in 0..20 {
        run_concurrent_case(
            &[
                a(Mkdir(s("/d"))),
                a(Mkdir(s("/a"))),
                b(Write(s("/b/a"), 338, vec![76; 692])),
                a(Rename(s("/a"), s("/d/b"), false)),
                Step::App(R::Trash(s("/d"))),
            ],
            DEBOUNCE,
        );
    }
    // A folder renamed onto a name a local file holds keeps only its pending
    // items locally; its clean child stays in the renamed remote folder.
    run(vec![
        a(Mkdir(s("/d"))),
        Step::Drain(A),
        Step::App(R::Create(s("/d/b"), vec![7; 100])),
        b(Create(s("/d/d"))),
        a(Rename(s("/d"), s("/a"), false)),
        Step::Drain(A),
        b(Create(s("/a"))),
        Step::Sync(B),
    ]);

    // M1: edit/edit.
    run(vec![
        a(Create(s("/a"))),
        a(w("/a", 1)),
        Step::Drain(A),
        Step::Sync(B),
        a(w("/a", 2)),
        b(w("/a", 3)),
        Step::Drain(B),
        Step::Drain(A),
    ]);
    // M3: remote delete against a local edit.
    run(vec![
        a(Create(s("/a"))),
        a(w("/a", 1)),
        Step::Drain(A),
        Step::Sync(B),
        b(w("/a", 2)),
        a(Unlink(s("/a"))),
        Step::Drain(A),
        Step::Drain(B),
    ]);
    // M4: local delete against a remote edit.
    run(vec![
        a(Create(s("/a"))),
        a(w("/a", 1)),
        Step::Drain(A),
        Step::Sync(B),
        a(Unlink(s("/a"))),
        b(w("/a", 2)),
        Step::Drain(B),
        Step::Drain(A),
    ]);
    // M5: rename/rename.
    run(vec![
        a(Create(s("/a"))),
        Step::Drain(A),
        Step::Sync(B),
        a(Rename(s("/a"), s("/b"), false)),
        b(Rename(s("/a"), s("/d"), false)),
        Step::Drain(B),
        Step::Drain(A),
    ]);
    // M6: a folder deleted on one device, a new child on the other.
    run(vec![
        a(Mkdir(s("/d"))),
        a(Create(s("/d/a"))),
        a(Create(s("/d/b"))),
        Step::Drain(A),
        Step::Sync(B),
        b(Create(s("/d/d"))),
        b(w("/d/d", 4)),
        a(Unlink(s("/d/a"))),
        a(Unlink(s("/d/b"))),
        a(Rmdir(s("/d"))),
        Step::Drain(A),
        Step::Drain(B),
    ]);
    // M7: the same file name, the same folder name, an app's edit.
    run(vec![
        a(Create(s("/a"))),
        a(w("/a", 1)),
        b(Create(s("/a"))),
        b(w("/a", 2)),
        Step::Drain(A),
        Step::Drain(B),
    ]);
    run(vec![
        a(Mkdir(s("/d"))),
        a(Create(s("/d/a"))),
        b(Mkdir(s("/d"))),
        b(Create(s("/d/b"))),
        Step::Drain(A),
        Step::Drain(B),
    ]);
    run(vec![
        a(Create(s("/a"))),
        a(w("/a", 1)),
        Step::Drain(A),
        Step::Sync(B),
        Step::App(R::Write(s("/a"), vec![9; 50])),
        a(w("/a", 5)),
        Step::Drain(A),
    ]);
    run(vec![
        a(Create(s("/a"))),
        a(w("/a", 1)),
        Step::Drain(A),
        Step::App(R::Write(s("/a"), vec![9; 50])),
        Step::Sync(A),
        Step::Sync(B),
    ]);
}
