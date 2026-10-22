//! Test helpers: the fake drive as a map of paths, and whole-file reads.

use crate::engine::{Engine, OpenMode};
use crate::vfs::Ino;
use pdmount_backend::fake::FakeBackend;
use pdmount_backend::{Backend, LinkId, NodeKind};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::SystemTime;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FakeFile {
    pub kind: NodeKind,
    pub content: Vec<u8>,
    pub mode: Option<u32>,
    pub mtime: Option<SystemTime>,
}

/// Every node of the fake drive below the root, by path (`/a/b`).
pub async fn fake_tree(backend: &FakeBackend) -> BTreeMap<String, FakeFile> {
    let mut out = BTreeMap::new();
    let root = backend.root().await.expect("root").link_id;
    let mut stack: Vec<(LinkId, String)> = vec![(root, String::new())];
    while let Some((link, path)) = stack.pop() {
        for child in backend.list_children(&link).await.expect("list") {
            let child_path = format!("{path}/{}", child.name);
            let content = match child.kind {
                NodeKind::File => backend
                    .content(&child.link_id)
                    .map(|bytes| bytes.to_vec())
                    .unwrap_or_default(),
                NodeKind::Folder => {
                    stack.push((child.link_id.clone(), child_path.clone()));
                    Vec::new()
                }
            };
            out.insert(
                child_path,
                FakeFile {
                    kind: child.kind,
                    content,
                    mode: child.mode,
                    mtime: child.mtime,
                },
            );
        }
    }
    out
}

/// The whole content of a file through the engine.
pub async fn read_all(engine: &Arc<Engine>, ino: Ino) -> Vec<u8> {
    let (handle, _) = engine.open_file(ino, OpenMode::default()).expect("open");
    let size = engine.getattr(ino).expect("attr").size;
    let mut out = Vec::new();
    while (out.len() as u64) < size {
        let chunk = engine
            .read(handle, out.len() as u64, 1 << 20)
            .await
            .expect("read");
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    engine.release_file(handle);
    out
}
