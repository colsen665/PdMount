//! Mounted tests over the fake backend (DESIGN.md §15.1, "Mounted, CI").
//!
//! The tree served is copied from a local directory, including a bare git
//! repository, so the checks are the read side of R6 and G6: `rsync -a` out
//! twice (the second run transfers nothing), `git clone` and `git fsck`.
//! Skipped when FUSE is not usable (no /dev/fuse or fusermount3).

use bytes::Bytes;
use pdmount_backend::fake::FakeBackend;
use pdmount_backend::{Backend, FileMeta, LinkId, UploadSource};
use pdmount_core::Config;
use pdmount_core::cache::CacheOptions;
use pdmount_core::engine::{Engine, EngineOptions};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;

fn fuse_available() -> bool {
    let device = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .is_ok();
    let fusermount = Command::new("fusermount3")
        .arg("--version")
        .output()
        .is_ok();
    device && fusermount
}

fn run(command: &mut Command) -> String {
    let output = command.output().expect("spawn");
    assert!(
        output.status.success(),
        "{command:?} failed: {}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// Local tree: small files, nested directories, a multi-block file, modes,
/// and a bare git repository with a few commits.
fn make_source(root: &Path) {
    let tree = root.join("tree");
    for (path, size) in [
        ("a.txt", 5usize),
        ("empty", 0),
        ("dir/b.bin", 300_000),
        ("dir/sub/c", 70_000),
    ] {
        let path = tree.join(path);
        std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
        let content: Vec<u8> = (0..size).map(|i| (i * 7 % 251) as u8).collect();
        std::fs::write(&path, content).expect("write");
    }
    std::fs::set_permissions(tree.join("a.txt"), std::fs::Permissions::from_mode(0o600))
        .expect("chmod");
    std::fs::set_permissions(
        tree.join("dir/b.bin"),
        std::fs::Permissions::from_mode(0o755),
    )
    .expect("chmod");

    let work = root.join("work");
    let git = |args: &[&str]| {
        run(Command::new("git")
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t"))
    };
    git(&["init", "-q", "-b", "main", work.to_str().expect("utf8")]);
    for i in 0..5 {
        std::fs::write(
            work.join(format!("f{i}")),
            format!("content {i}\n").repeat(1000),
        )
        .expect("write");
        git(&["-C", work.to_str().expect("utf8"), "add", "-A"]);
        git(&[
            "-C",
            work.to_str().expect("utf8"),
            "commit",
            "-q",
            "-m",
            &format!("c{i}"),
        ]);
    }
    git(&[
        "clone",
        "-q",
        "--bare",
        work.to_str().expect("utf8"),
        root.join("r.git").to_str().expect("utf8"),
    ]);
}

/// Uploads `dir` into `parent` on the fake, keeping mtimes and modes.
fn upload_dir<'a>(
    backend: &'a FakeBackend,
    dir: &'a Path,
    parent: &'a LinkId,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + 'a>> {
    Box::pin(async move {
        for entry in std::fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let meta = entry.metadata().expect("metadata");
            let name = entry.file_name().into_string().expect("utf8");
            let mtime = meta.modified().expect("mtime");
            if meta.is_dir() {
                let link = backend
                    .create_folder(parent, &name, mtime)
                    .await
                    .expect("mkdir");
                upload_dir(backend, &entry.path(), &link).await;
            } else {
                let content = Bytes::from(std::fs::read(entry.path()).expect("read"));
                let file_meta = FileMeta {
                    mtime,
                    mode: Some(meta.permissions().mode() & 0o7777),
                    media_type: "application/octet-stream".into(),
                };
                backend
                    .upload_new(parent, &name, UploadSource::Bytes(content), file_meta)
                    .await
                    .expect("upload");
            }
        }
    })
}

struct Mount {
    engine: Arc<Engine>,
    mounted: pdmount_fuse::Mounted,
}

fn mount(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    state: &Path,
    mountpoint: &Path,
) -> Mount {
    let (engine, invalidations) = runtime
        .block_on(Engine::open(
            Arc::new(backend.clone()),
            EngineOptions {
                state_db: state.join("state.db"),
                cache: CacheOptions {
                    dir: state.join("blocks"),
                    master_key: [9; 32],
                    ram_bytes: 1 << 20,
                    disk_bytes: 64 << 20,
                },
                config: Config::default(),
            },
        ))
        .expect("engine");
    engine.start_background();
    let mounted =
        pdmount_fuse::mount(Arc::clone(&engine), invalidations, mountpoint).expect("mount");
    Mount { engine, mounted }
}

fn unmount(runtime: &tokio::runtime::Runtime, mount: Mount) {
    mount.mounted.unmount().expect("unmount");
    runtime.block_on(mount.engine.shutdown()).expect("shutdown");
}

#[test]
fn read_only_mount_serves_rsync_and_git() {
    if !fuse_available() {
        eprintln!("skipped: FUSE is not available");
        return;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let temp = tempfile::tempdir().expect("tempdir");
    let source = temp.path().join("source");
    make_source(&source);
    let backend = FakeBackend::new();
    // Small blocks, so ordinary files span several of them.
    backend.set_block_size(64 * 1024);
    runtime.block_on(async {
        let root = backend.root().await.expect("root").link_id;
        upload_dir(&backend, &source, &root).await;
    });

    let state = temp.path().join("state");
    let mnt: PathBuf = temp.path().join("mnt");
    std::fs::create_dir_all(&mnt).expect("mkdir");
    let m = mount(&runtime, &backend, &state, &mnt);

    // Content, modes and mtimes match the source.
    run(Command::new("diff")
        .arg("-r")
        .arg(source.join("tree"))
        .arg(mnt.join("tree")));
    let local = std::fs::metadata(source.join("tree/a.txt")).expect("stat");
    let mounted = std::fs::metadata(mnt.join("tree/a.txt")).expect("stat");
    assert_eq!(mounted.mode() & 0o7777, 0o600);
    assert_eq!(mounted.mtime(), local.mtime());
    assert_eq!(mounted.len(), local.len());

    // R6 read side: the second rsync transfers nothing.
    let out = temp.path().join("out");
    run(Command::new("rsync")
        .arg("-a")
        .arg(format!("{}/", mnt.join("tree").display()))
        .arg(&out));
    let second = run(Command::new("rsync")
        .arg("-ai")
        .arg(format!("{}/", mnt.join("tree").display()))
        .arg(&out));
    assert_eq!(second, "", "second rsync run transferred: {second}");
    run(Command::new("diff")
        .arg("-r")
        .arg(source.join("tree"))
        .arg(&out));

    // G6 read side: clone from the mounted bare repository, then fsck.
    let clone = temp.path().join("clone");
    run(Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(mnt.join("r.git"))
        .arg(&clone));
    run(Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["fsck", "--full", "--strict"]));
    let file_clone = temp.path().join("file-clone");
    run(Command::new("git")
        .arg("clone")
        .arg("-q")
        .arg(format!("file://{}", mnt.join("r.git").display()))
        .arg(&file_clone));

    // Writes are refused.
    let error = std::fs::write(mnt.join("new"), b"x").expect_err("read-only");
    assert_eq!(error.raw_os_error(), Some(nix::libc::EROFS));
    assert_eq!(
        std::fs::metadata(mnt.join("missing"))
            .expect_err("missing")
            .kind(),
        std::io::ErrorKind::NotFound
    );

    let metrics = m.engine.metrics();
    assert_eq!(metrics.hot_path_network_calls.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.handler_panics.load(Ordering::Relaxed), 0);
    let ino = std::fs::metadata(mnt.join("tree/dir/b.bin"))
        .expect("stat")
        .ino();
    unmount(&runtime, m);

    // Remount on the same state: same inode numbers, served without listing.
    let m = mount(&runtime, &backend, &state, &mnt);
    assert_eq!(
        std::fs::metadata(mnt.join("tree/dir/b.bin"))
            .expect("stat")
            .ino(),
        ino
    );
    run(Command::new("diff")
        .arg("-r")
        .arg(source.join("tree"))
        .arg(mnt.join("tree")));
    assert_eq!(
        m.engine
            .metrics()
            .hot_path_network_calls
            .load(Ordering::Relaxed),
        0
    );
    unmount(&runtime, m);
}
