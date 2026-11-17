//! Mounted tests over the fake backend (DESIGN.md §15.1, "Mounted, CI").
//!
//! The tree served is copied from a local directory, including a bare git
//! repository, so the checks are the read side of R6 and G6: `rsync -a` out
//! twice (the second run transfers nothing), `git clone` and `git fsck`.
//! Skipped when FUSE is not usable (no /dev/fuse or fusermount3).

use pdmount_backend::Backend;
use pdmount_backend::fake::FakeBackend;
use pdmount_core::Config;
use pdmount_core::conflicts::system_clock;
use pdmount_core::device::{DeviceIdentity, Uuid};
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
/// A runtime, a temporary directory and an empty mountpoint in it; `None`
/// (the test is skipped) without FUSE.
fn setup() -> Option<(tokio::runtime::Runtime, tempfile::TempDir, PathBuf)> {
    if !fuse_available() {
        eprintln!("skipped: FUSE is not available");
        return None;
    }
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let temp = tempfile::tempdir().expect("tempdir");
    let mnt = temp.path().join("mnt");
    std::fs::create_dir_all(&mnt).expect("mkdir");
    Some((runtime, temp, mnt))
}

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
    let git = |args: &[&str]| run(git_command().args(args));
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
    mount_with(runtime, backend, state, mountpoint, Config::default())
}

fn mount_with(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    state: &Path,
    mountpoint: &Path,
    config: Config,
) -> Mount {
    mount_options(runtime, backend, state, mountpoint, config, false)
}

fn mount_options(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    state: &Path,
    mountpoint: &Path,
    config: Config,
    read_only: bool,
) -> Mount {
    let mut options = EngineOptions::in_dir(
        state,
        config,
        DeviceIdentity {
            id: Uuid::from_u128(1),
            name: "mounted-test".into(),
        },
        system_clock(),
    );
    options.read_only = read_only;
    let (engine, invalidations) = runtime
        .block_on(Engine::open(Arc::new(backend.clone()), options))
        .expect("engine");
    engine.start_background();
    let mounted =
        pdmount_fuse::mount(Arc::clone(&engine), invalidations, mountpoint).expect("mount");
    Mount { engine, mounted }
}

fn unmount(runtime: &tokio::runtime::Runtime, mount: Mount) {
    let counts = runtime.block_on(mount.engine.drain(std::time::Duration::from_secs(60)));
    assert_eq!(
        counts.pending + counts.parked,
        0,
        "queue left at unmount: {counts:?}"
    );
    mount.mounted.unmount().expect("unmount");
    runtime.block_on(mount.engine.shutdown()).expect("shutdown");
}

#[test]
fn mount_serves_rsync_and_git_reads() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let source = temp.path().join("source");
    make_source(&source);
    let backend = FakeBackend::new();
    // Small blocks, so ordinary files span several of them.
    backend.set_block_size(64 * 1024);
    runtime.block_on(async {
        let root = backend.root().await.expect("root").link_id;
        backend.import_dir(&source, &root).await.expect("import");
    });

    let state = temp.path().join("state");
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

/// git without the user's or the system's configuration.
fn git_command() -> Command {
    let mut command = Command::new("git");
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@t")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@t");
    command
}

fn git(dir: &Path, args: &[&str]) -> String {
    run(git_command().arg("-C").arg(dir).args(args))
}

fn rsync_itemize(args: &[&str], from: &Path, to: &Path) -> String {
    run(Command::new("rsync")
        .args(args)
        .arg(format!("{}/", from.display()))
        .arg(to))
}

/// R1, R2, R3 and G1–G3 in small: rsync into the mount, change and
/// delete, git init and commits inside it; then verify on a remount and
/// on a fresh state that sees only what reached the fake.
#[test]
fn mount_takes_rsync_and_git_writes() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let source = temp.path().join("source");
    make_source(&source);
    let tree = source.join("tree");
    let backend = FakeBackend::new();
    backend.set_block_size(64 * 1024);
    let state = temp.path().join("state");
    let m = mount(&runtime, &backend, &state, &mnt);
    let dst = mnt.join("dst");

    // R1: a full copy, then nothing to do, by mtime and by checksum.
    rsync_itemize(&["-a"], &tree, &dst);
    assert_eq!(rsync_itemize(&["-ani"], &tree, &dst), "");
    assert_eq!(rsync_itemize(&["-ani", "--checksum"], &tree, &dst), "");

    // R2 and R3: change one file, delete another, sync with --delete.
    std::fs::write(tree.join("a.txt"), b"changed").expect("write");
    std::fs::remove_file(tree.join("dir/sub/c")).expect("rm");
    let changed = rsync_itemize(&["-ai", "--delete"], &tree, &dst);
    assert!(
        changed.contains("a.txt")
            && changed
                .lines()
                .any(|l| l.starts_with("*deleting") && l.ends_with("dir/sub/c")),
        "{changed}"
    );
    assert_eq!(
        rsync_itemize(&["-ani", "--delete", "--checksum"], &tree, &dst),
        ""
    );

    // G1-G3: a repository created and committed to inside the mount.
    let repo = mnt.join("repo");
    git(&mnt, &["init", "-q", "-b", "main", "repo"]);
    assert_eq!(git(&repo, &["config", "core.symlinks"]).trim(), "false");
    for i in 0..5 {
        std::fs::write(
            repo.join(format!("f{i}")),
            format!("commit {i}\n").repeat(100),
        )
        .expect("write");
        if i == 3 {
            git(&repo, &["mv", "f0", "renamed"]);
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-q", "-m", &format!("c{i}")]);
    }
    git(&repo, &["fsck", "--full", "--strict"]);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    let head = git(&repo, &["rev-parse", "HEAD"]);

    let metrics = m.engine.metrics();
    assert_eq!(metrics.hot_path_network_calls.load(Ordering::Relaxed), 0);
    assert_eq!(metrics.handler_panics.load(Ordering::Relaxed), 0);
    unmount(&runtime, m);

    // No temporary name ever reached the fake.
    for change in backend.changes() {
        if let pdmount_backend::fake::FakeChange::CreatedFile { name, .. } = change {
            assert!(
                !name.ends_with(".lock") && !name.starts_with(".a.txt."),
                "{name}"
            );
        }
    }

    // Remount: the same state, nothing to transfer.
    let m = mount(&runtime, &backend, &state, &mnt);
    assert_eq!(rsync_itemize(&["-ani", "--checksum"], &tree, &dst), "");
    git(&repo, &["fsck", "--full", "--strict"]);
    unmount(&runtime, m);

    // Fresh: an empty state sees what the fake has. Directory times are
    // local-only (D013), so they are not compared.
    let fresh = temp.path().join("fresh");
    let m = mount(&runtime, &backend, &fresh, &mnt);
    assert_eq!(
        rsync_itemize(&["-ani", "--omit-dir-times"], &tree, &dst),
        ""
    );
    assert_eq!(
        rsync_itemize(&["-ani", "--checksum", "--omit-dir-times"], &tree, &dst),
        ""
    );
    git(&repo, &["fsck", "--full", "--strict"]);
    assert_eq!(git(&repo, &["rev-parse", "HEAD"]), head);
    assert_eq!(git(&repo, &["status", "--porcelain"]), "");
    unmount(&runtime, m);
}

/// `getfattr` output of one attribute, or `None` when it does not exist.
fn xattr(path: &Path, name: &str) -> Option<String> {
    let output = Command::new("getfattr")
        .args(["--only-values", "-n", name])
        .arg(path)
        .output()
        .expect("spawn getfattr");
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

#[test]
fn mount_reports_state_in_xattrs() {
    if Command::new("getfattr").arg("--version").output().is_err() {
        eprintln!("skipped: getfattr is not available");
        return;
    }
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    let m = mount(&runtime, &backend, &temp.path().join("state"), &mnt);
    let file = mnt.join("file.txt");
    std::fs::write(&file, b"content").expect("write");

    let state = xattr(&file, "user.pdmount.state").expect("state");
    assert!(
        ["pending", "uploading", "clean"].contains(&state.as_str()),
        "{state}"
    );
    assert_eq!(xattr(&file, "user.pdmount.access").as_deref(), Some("rw"));
    assert_eq!(xattr(&file, "user.pdmount.conflict"), None);
    assert_eq!(xattr(&file, "user.other"), None);

    let counts = runtime.block_on(m.engine.drain(std::time::Duration::from_secs(60)));
    assert_eq!(counts.pending + counts.parked, 0);
    assert_eq!(xattr(&file, "user.pdmount.state").as_deref(), Some("clean"));
    assert!(
        xattr(&file, "user.pdmount.link_id").is_some_and(|link| !link.is_empty()),
        "an uploaded file has a link"
    );
    assert!(xattr(&file, "user.pdmount.rev").is_some());

    // The list holds our names, with the size protocol (a probe, then data).
    let listed = run(Command::new("getfattr")
        .args(["-d", "-m", "^user.pdmount."])
        .arg(&file));
    for name in [
        "user.pdmount.state",
        "user.pdmount.link_id",
        "user.pdmount.access",
    ] {
        assert!(listed.contains(name), "{listed}");
    }
    // Setting stays unsupported.
    let set = Command::new("setfattr")
        .args(["-n", "user.pdmount.state", "-v", "x"])
        .arg(&file)
        .output();
    if let Ok(set) = set {
        assert!(!set.status.success());
    }
    unmount(&runtime, m);
}

/// Polls `ready` until it holds; panics with `what` after 30 s.
fn wait_until(what: &str, ready: &dyn Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while !ready() {
        assert!(std::time::Instant::now() < deadline, "timed out: {what}");
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// §5.8: requests are timed per operation, deferred replies included, and
/// a reset clears the histograms.
#[test]
fn mount_times_requests() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    let m = mount(&runtime, &backend, &temp.path().join("state"), &mnt);
    let metrics = Arc::clone(m.engine.metrics());
    let count = |op: &str| {
        metrics
            .snapshot_latency()
            .into_iter()
            .find(|(name, _)| *name == op)
            .map_or(0, |(_, summary)| summary.count)
    };

    // A request is timed after its reply has been sent, so a sample can
    // land after the call that caused it has returned: wait for each one.
    std::fs::create_dir(mnt.join("made")).expect("mkdir");
    std::fs::metadata(mnt.join("made")).expect("stat");
    std::fs::write(mnt.join("made/file"), b"content").expect("write");
    assert_eq!(
        std::fs::read(mnt.join("made/file")).expect("read"),
        b"content"
    );
    wait_until("mkdir", &|| count("mkdir") > 0);
    wait_until("lookup or getattr", &|| {
        count("lookup") + count("getattr") > 0
    });
    wait_until("create and write", &|| {
        count("create") > 0 && count("write") > 0
    });
    wait_until("read", &|| count("read_staging") + count("read_ram") > 0);

    // The reset drops what was recorded. Samples still in flight (the
    // flush and release of the closed files, the engine's own background
    // work) may land after it, at moments the test does not control, so
    // the check is on operations that were all timed above: none of their
    // samples may survive it.
    metrics.reset_latency();
    std::fs::create_dir(mnt.join("again")).expect("mkdir");
    wait_until("mkdir after the reset", &|| count("mkdir") > 0);
    assert_eq!(count("mkdir"), 1);
    assert_eq!(count("create") + count("write"), 0);
    unmount(&runtime, m);
}

/// §5.4: an unchanged directory is listed from the kernel's cache, and a
/// change made through the mount shows at the next listing.
#[test]
fn mount_lists_unchanged_directories_from_the_kernel_cache() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    let m = mount(&runtime, &backend, &temp.path().join("state"), &mnt);
    let metrics = Arc::clone(m.engine.metrics());
    let count = |op: &str| {
        metrics
            .snapshot_latency()
            .into_iter()
            .find(|(name, _)| *name == op)
            .map_or(0, |(_, summary)| summary.count)
    };
    let listings = || count("readdir") + count("readdirplus");
    let names = |dir: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .expect("read_dir")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("utf-8")
            })
            .collect();
        names.sort();
        names
    };
    let dir = mnt.join("d");
    std::fs::create_dir(&dir).expect("mkdir");
    std::fs::write(dir.join("a"), b"a").expect("write");
    // A request is timed after its reply has been sent, and the kernel sends
    // `releasedir` after `close` has returned. Waiting for it before each
    // count means every request of the listing before has been timed and the
    // daemon has no other handle of the directory open (D072).
    assert_eq!(names(&dir), ["a"]);
    wait_until("releasedir of the first listing", &|| {
        count("releasedir") >= 1
    });
    let before = listings();
    assert_eq!(names(&dir), ["a"]);
    wait_until("releasedir of the second listing", &|| {
        count("releasedir") >= 2
    });
    assert_eq!(listings(), before, "the second listing came from the cache");

    std::fs::write(dir.join("b"), b"b").expect("write");
    assert_eq!(names(&dir), ["a", "b"]);
    std::fs::remove_file(dir.join("a")).expect("unlink");
    assert_eq!(names(&dir), ["b"]);
    unmount(&runtime, m);
}

/// Set in the child of `mount_keeps_more_pending_files_than_descriptors`.
const FD_LIMIT_CHILD: &str = "PDMOUNT_TEST_FD_LIMIT_CHILD";

/// D063: staged files are opened on demand, so more files can wait for
/// their upload than the open-file limit allows descriptors. Runs itself
/// again under `prlimit --nofile=512`, where one descriptor per staged file
/// ran out (EMFILE) before.
#[test]
fn mount_keeps_more_pending_files_than_descriptors() {
    const FILES: usize = 2000;
    if std::env::var_os(FD_LIMIT_CHILD).is_none() {
        if !fuse_available() {
            eprintln!("skipped: FUSE is not available");
            return;
        }
        if Command::new("prlimit").arg("--version").output().is_err() {
            eprintln!("skipped: prlimit is missing");
            return;
        }
        let output = run(Command::new("prlimit")
            .arg("--nofile=512")
            .arg("--")
            .arg(std::env::current_exe().expect("test binary"))
            .args([
                "mount_keeps_more_pending_files_than_descriptors",
                "--exact",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(FD_LIMIT_CHILD, "1"));
        assert!(
            output.contains("1 passed"),
            "the child ran no test: {output}"
        );
        return;
    }
    let Some((runtime, temp, mnt)) = setup() else {
        panic!("FUSE went away between the parent and the child");
    };
    // Uploads wait, so every file stays staged until the drain.
    let mut config = Config::default();
    let hour = pdmount_core::units::HumanDuration(std::time::Duration::from_secs(3600));
    config.upload.debounce = hour;
    config.upload.max_dirty_age = hour;
    let backend = FakeBackend::new();
    let m = mount_with(&runtime, &backend, &temp.path().join("state"), &mnt, config);
    let dir = mnt.join("many");
    std::fs::create_dir(&dir).expect("mkdir");
    let content = |i: usize| format!("file {i}\n").repeat(i % 7 + 1);
    for i in 0..FILES {
        std::fs::write(dir.join(format!("f{i}")), content(i)).expect("write");
    }
    let gauge = |name: &str| {
        m.engine
            .gauges()
            .into_iter()
            .find(|(gauge, _)| *gauge == name)
            .map(|(_, value)| value)
            .expect("gauge")
    };
    assert!(gauge("queue_pending") >= FILES as u64);
    let capacity = pdmount_core::staging::open_files_capacity() as u64;
    assert!(capacity < 512 / 2, "capacity {capacity}");
    assert!(gauge("staged_open_files") <= capacity);
    for i in 0..FILES {
        assert_eq!(
            std::fs::read_to_string(dir.join(format!("f{i}"))).expect("read"),
            content(i)
        );
    }
    assert!(gauge("staged_open_files") <= capacity);
    unmount(&runtime, m);
}

/// `mount -o ro`: a write answers EROFS, reads work, and a change pending
/// from before uploads.
#[test]
fn a_read_only_mount_answers_erofs_and_uploads_what_is_pending() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    let mut held = Config::default();
    held.upload.debounce = pdmount_core::units::HumanDuration(std::time::Duration::from_secs(3600));
    let m = mount_with(&runtime, &backend, &state, &mnt, held);
    std::fs::write(mnt.join("pending.txt"), b"pending").expect("write");
    assert_ne!(m.engine.queue_counts().pending, 0);
    unmount(&runtime, m);

    let m = mount_options(&runtime, &backend, &state, &mnt, Config::default(), true);
    let denied = std::fs::write(mnt.join("new.txt"), b"x").expect_err("a write");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    let denied = std::fs::OpenOptions::new()
        .write(true)
        .open(mnt.join("pending.txt"))
        .expect_err("an open for writing");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    assert_eq!(
        std::fs::read(mnt.join("pending.txt")).expect("read"),
        b"pending"
    );
    let counts = runtime.block_on(m.engine.drain(std::time::Duration::from_secs(60)));
    assert_eq!((counts.pending, counts.parked), (0, 0));
    let root = runtime.block_on(backend.root()).expect("root").link_id;
    let names: Vec<String> = runtime
        .block_on(backend.list_children(&root))
        .expect("list")
        .into_iter()
        .map(|node| node.name)
        .collect();
    assert!(names.contains(&"pending.txt".to_owned()), "{names:?}");
    unmount(&runtime, m);
}

/// `fsync.remote_paths`: fsync of a matching file returns after the upload,
/// of another one at once, with the upload still to come.
#[test]
fn fsync_of_a_remote_path_waits_for_the_upload() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    let state = temp.path().join("state");
    std::fs::create_dir_all(&state).expect("mkdir");
    let mut config = Config::default();
    config.upload.debounce =
        pdmount_core::units::HumanDuration(std::time::Duration::from_secs(3600));
    config.fsync.remote_paths = vec!["/important/**".to_owned()];
    let m = mount_with(&runtime, &backend, &state, &mnt, config);
    let names = || -> Vec<String> {
        let root = runtime.block_on(backend.root()).expect("root").link_id;
        let important = runtime
            .block_on(backend.list_children(&root))
            .expect("list");
        let mut names: Vec<String> = Vec::new();
        for node in important {
            if node.name == "important" {
                names.extend(
                    runtime
                        .block_on(backend.list_children(&node.link_id))
                        .expect("list")
                        .into_iter()
                        .map(|inner| inner.name),
                );
            }
            names.push(node.name);
        }
        names
    };
    let other = mnt.join("other.txt");
    let file = std::fs::File::create(&other).expect("create");
    use std::io::Write as _;
    (&file).write_all(b"other").expect("write");
    file.sync_all().expect("fsync");
    drop(file);
    assert!(
        !names().contains(&"other.txt".to_owned()),
        "a local fsync does not upload"
    );

    std::fs::create_dir(mnt.join("important")).expect("mkdir");
    let path = mnt.join("important/a.txt");
    let file = std::fs::File::create(&path).expect("create");
    (&file).write_all(b"important").expect("write");
    file.sync_all().expect("fsync");
    assert!(
        names().contains(&"a.txt".to_owned()),
        "uploaded when fsync returned"
    );
    drop(file);
    unmount(&runtime, m);
}

/// The engines of a mount with several trees.
struct Trees {
    engines: Vec<Arc<Engine>>,
    mounted: pdmount_fuse::Mounted,
    _pollers: pdmount_core::poller::Pollers,
}

/// Mounts `trees` (name, volume, root, write gate open) of `backend`, each
/// with its own state under `state`, over one shared connectivity and one
/// set of transfer limits, with one poller per volume (D110).
fn mount_trees(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    state: &Path,
    mountpoint: &Path,
    trees: &[(&str, &str, &pdmount_backend::LinkId, bool)],
) -> Trees {
    mount_trees_on(runtime, backend, state, mountpoint, trees, None)
}

/// [`mount_trees`], with `budget` the disk cap of all the trees together;
/// the first tree keeps half of it, as My Files does (D110 amended).
fn mount_trees_on(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    state: &Path,
    mountpoint: &Path,
    trees: &[(&str, &str, &pdmount_backend::LinkId, bool)],
    budget: Option<&Arc<pdmount_core::cache::DiskBudget>>,
) -> Trees {
    let _entered = runtime.enter();
    let pollers = pdmount_core::poller::Pollers::new();
    let mut health = None;
    let mut limits = None;
    let mut engines = Vec::new();
    let mut mount = Vec::new();
    for (name, volume, root, open) in trees {
        let mut options = EngineOptions::in_dir(
            &state.join(name),
            Config::default(),
            DeviceIdentity {
                id: Uuid::from_u128(1),
                name: "mounted-test".into(),
            },
            system_clock(),
        );
        options.write_gate = *open;
        options.health = health.clone();
        options.limits = limits.clone();
        options.cache.shared = budget.map(|budget| pdmount_core::cache::SharedCap {
            budget: Arc::clone(budget),
            keeps_half: engines.is_empty(),
        });
        let tree = backend.tree(volume, (*root).clone());
        let (engine, invalidations) = runtime
            .block_on(Engine::open(Arc::new(tree), options))
            .expect("engine");
        health.get_or_insert_with(|| Arc::clone(engine.health()));
        limits.get_or_insert_with(|| Arc::clone(engine.limits()));
        engine.start_background_polled();
        let own = *volume == pdmount_backend::fake::MAIN_VOLUME;
        pollers.add(volume, own, Arc::clone(&engine));
        mount.push(pdmount_fuse::MountTree {
            name: (*name).to_owned(),
            engine: Arc::clone(&engine),
            invalidations,
            volume_use: (!own).then(|| {
                pdmount_core::poller::VolumeUse::new(Arc::clone(pollers.cadence()), volume)
            }),
            place: pdmount_fuse::TreePlace::Folder,
        });
        engines.push(engine);
    }
    let mounted = pdmount_fuse::mount_trees(mount, mountpoint).expect("mount");
    Trees {
        engines,
        mounted,
        _pollers: pollers,
    }
}

fn unmount_trees(runtime: &tokio::runtime::Runtime, trees: Trees) {
    for engine in &trees.engines {
        let counts = runtime.block_on(engine.drain(std::time::Duration::from_secs(60)));
        assert_eq!(counts.pending + counts.parked, 0, "queue left: {counts:?}");
    }
    trees.mounted.unmount().expect("unmount");
    for engine in &trees.engines {
        runtime.block_on(engine.shutdown()).expect("shutdown");
    }
}

/// Names on the fake below `root`, recursively, as `a/b`.
fn fake_paths(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    root: &pdmount_backend::LinkId,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![(root.clone(), String::new())];
    while let Some((link, path)) = stack.pop() {
        for child in runtime
            .block_on(backend.list_children(&link))
            .expect("list")
        {
            let child_path = format!("{path}{}", child.name);
            if child.kind == pdmount_backend::NodeKind::Folder {
                stack.push((child.link_id.clone(), format!("{child_path}/")));
            }
            out.push(child_path);
        }
    }
    out.sort();
    out
}

/// M5.10 (D080, D110): two trees on two volumes in one mount, their write
/// gates forced open, each runs the rsync and git write scenarios in its
/// folder of the virtual top level, which is read-only. A rename between
/// them is EXDEV; a closed gate answers EROFS until it opens again; a
/// remount keeps the routed inode numbers; a mount with a single further
/// tree has no router.
#[test]
fn a_mount_with_several_trees_serves_each_from_its_own_engine() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let source = temp.path().join("source");
    make_source(&source);
    let tree = source.join("tree");
    let backend = FakeBackend::new();
    backend.set_block_size(64 * 1024);
    let laptop = backend
        .add_root("Laptop", None, None)
        .expect("a root on the own volume");
    let team = backend
        .add_root(
            "Team",
            Some("owner@example.test"),
            Some(pdmount_backend::fake::FakeRole::Editor),
        )
        .expect("a share on another volume");
    let state = temp.path().join("state");
    let layout = [
        ("Laptop", laptop.volume.as_str(), &laptop.root, true),
        ("Team", team.volume.as_str(), &team.root, true),
    ];
    let m = mount_trees(&runtime, &backend, &state, &mnt, &layout);

    // The top level: one read-only folder per tree.
    let mut top: Vec<String> = std::fs::read_dir(&mnt)
        .expect("read the top level")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    top.sort();
    assert_eq!(top, ["Laptop", "Team"]);
    assert_eq!(std::fs::metadata(&mnt).expect("stat").mode() & 0o777, 0o555);
    let denied = std::fs::create_dir(mnt.join("new")).expect_err("mkdir in the top level");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    let denied = std::fs::rename(mnt.join("Team"), mnt.join("Other")).expect_err("rename");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));

    // The write scenarios in each tree.
    let mut heads = Vec::new();
    for name in ["Laptop", "Team"] {
        let dst = mnt.join(name).join("dst");
        rsync_itemize(&["-a"], &tree, &dst);
        assert_eq!(rsync_itemize(&["-ani", "--checksum"], &tree, &dst), "");
        let repo = mnt.join(name).join("repo");
        git(&mnt.join(name), &["init", "-q", "-b", "main", "repo"]);
        for i in 0..3 {
            std::fs::write(repo.join(format!("f{i}")), format!("{name} {i}\n")).expect("write");
            git(&repo, &["add", "-A"]);
            git(&repo, &["commit", "-q", "-m", &format!("c{i}")]);
        }
        git(&repo, &["fsck", "--full", "--strict"]);
        heads.push(git(&repo, &["rev-parse", "HEAD"]));
    }
    // Inode numbers carry their tree.
    let ino = |path: &str| std::fs::metadata(mnt.join(path)).expect("stat").ino();
    assert_eq!(ino("Laptop") >> 48, 1);
    assert_eq!(ino("Team") >> 48, 2);
    assert_eq!(ino("Team/dst/a.txt") >> 48, 2);
    let team_file = ino("Team/dst/dir/b.bin");

    // Every tree is a file system of its own.
    let denied = std::fs::rename(mnt.join("Laptop/dst/a.txt"), mnt.join("Team/a.txt"))
        .expect_err("a rename between trees");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EXDEV));
    std::fs::rename(mnt.join("Team/dst/a.txt"), mnt.join("Team/moved.txt"))
        .expect("a rename inside a tree");

    // A closed gate: EROFS, reads go on; open again, writes go on.
    m.engines[1].set_write_gate(false);
    let denied = std::fs::write(mnt.join("Team/new.txt"), b"x").expect_err("a write");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    assert_eq!(
        std::fs::metadata(mnt.join("Team/moved.txt"))
            .expect("stat")
            .mode()
            & 0o222,
        0
    );
    assert_eq!(
        std::fs::read(mnt.join("Team/moved.txt"))
            .expect("read")
            .len(),
        5
    );
    std::fs::write(mnt.join("Laptop/new.txt"), b"x").expect("the other tree writes");
    m.engines[1].set_write_gate(true);
    std::fs::write(mnt.join("Team/new.txt"), b"x").expect("a write after the gate opened");

    // A remote change on the share's volume reaches its folder.
    let remote = backend.tree(&team.volume, team.root.clone());
    runtime
        .block_on(remote.upload_new(
            &team.root,
            "remote.txt",
            pdmount_backend::UploadSource::Bytes(bytes::Bytes::from_static(b"from the owner")),
            pdmount_backend::FileMeta {
                mtime: std::time::SystemTime::now(),
                mode: Some(0o644),
                media_type: "text/plain".into(),
                sha1: None,
            },
        ))
        .expect("upload");
    runtime
        .block_on(m.engines[1].sync_events())
        .expect("events");
    wait_until("the remote file shows", &|| {
        mnt.join("Team/remote.txt").exists()
    });
    assert!(!mnt.join("Laptop/remote.txt").exists());
    for engine in &m.engines {
        let metrics = engine.metrics();
        assert_eq!(metrics.hot_path_network_calls.load(Ordering::Relaxed), 0);
        assert_eq!(metrics.handler_panics.load(Ordering::Relaxed), 0);
    }
    unmount_trees(&runtime, m);

    // Each tree's changes reached its own root on the fake.
    let laptop_paths = fake_paths(&runtime, &backend, &laptop.root);
    let team_paths = fake_paths(&runtime, &backend, &team.root);
    assert!(
        laptop_paths.contains(&"dst/a.txt".to_owned()),
        "{laptop_paths:?}"
    );
    assert!(
        laptop_paths.contains(&"new.txt".to_owned()),
        "{laptop_paths:?}"
    );
    assert!(
        team_paths.contains(&"moved.txt".to_owned()),
        "{team_paths:?}"
    );
    assert!(
        team_paths.contains(&"remote.txt".to_owned()),
        "{team_paths:?}"
    );
    assert!(
        !team_paths.contains(&"dst/a.txt".to_owned()),
        "{team_paths:?}"
    );

    // Remount: the same numbers, the same repositories.
    let m = mount_trees(&runtime, &backend, &state, &mnt, &layout);
    assert_eq!(ino("Team/dst/dir/b.bin"), team_file);
    for (name, head) in ["Laptop", "Team"].iter().zip(&heads) {
        let repo = mnt.join(name).join("repo");
        git(&repo, &["fsck", "--full", "--strict"]);
        assert_eq!(&git(&repo, &["rev-parse", "HEAD"]), head);
    }
    unmount_trees(&runtime, m);

    // A mount of one further tree has no router, and its gate is closed.
    let single = [("Team", team.volume.as_str(), &team.root, false)];
    let m = mount_trees(
        &runtime,
        &backend,
        &temp.path().join("single"),
        &mnt,
        &single,
    );
    assert_eq!(std::fs::metadata(&mnt).expect("stat").ino(), 1);
    assert_eq!(
        std::fs::read(mnt.join("remote.txt")).expect("read"),
        b"from the owner"
    );
    let denied = std::fs::write(mnt.join("other.txt"), b"x").expect_err("a write");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    unmount_trees(&runtime, m);
}

/// D110 amended: the trees of a mount share one disk cap. A tree that reads
/// more than the free room makes the first tree (My Files' place) shrink, but
/// never below half of the cap, then evicts its own blocks; every tree's
/// figures add up to the profile's.
#[test]
fn trees_share_one_disk_cap_and_the_first_keeps_half() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    backend.set_block_size(64 * 1024);
    let mine = backend
        .add_root("Mine", None, None)
        .expect("a root on the own volume");
    let team = backend
        .add_root(
            "Team",
            Some("owner@example.test"),
            Some(pdmount_backend::fake::FakeRole::Editor),
        )
        .expect("a share on another volume");
    let upload = |volume: &str, root: &pdmount_backend::LinkId, name: &str, len: usize| {
        let content: Vec<u8> = (0..len).map(|i| (i * 7 % 251) as u8).collect();
        runtime
            .block_on(backend.tree(volume, root.clone()).upload_new(
                root,
                name,
                pdmount_backend::UploadSource::Bytes(bytes::Bytes::from(content.clone())),
                pdmount_backend::FileMeta {
                    mtime: std::time::SystemTime::now(),
                    mode: Some(0o644),
                    media_type: "application/octet-stream".into(),
                    sha1: None,
                },
            ))
            .expect("upload");
        content
    };
    let small = upload(&mine.volume, &mine.root, "small.bin", 3 << 19);
    let large = upload(&team.volume, &team.root, "large.bin", 3 << 20);
    let cap: u64 = 2 << 20;
    let budget = pdmount_core::cache::DiskBudget::new(cap);
    let layout = [
        ("Mine", mine.volume.as_str(), &mine.root, false),
        ("Team", team.volume.as_str(), &team.root, false),
    ];
    let m = mount_trees_on(
        &runtime,
        &backend,
        &temp.path().join("state"),
        &mnt,
        &layout,
        Some(&budget),
    );
    let (first, second) = (&m.engines[0], &m.engines[1]);

    // 1.5 MiB fits under the upper mark: nothing is evicted.
    assert_eq!(
        std::fs::read(mnt.join("Mine/small.bin")).expect("read"),
        small
    );
    let mine_before = first.cache_stats().bytes;
    assert!(mine_before >= small.len() as u64, "{mine_before}");
    assert!(mine_before <= cap / 10 * 9);

    // 3 MiB more: the first tree shrinks to half the cap, the second evicts
    // its own blocks after that.
    assert_eq!(
        std::fs::read(mnt.join("Team/large.bin")).expect("read"),
        large
    );
    let (a, b) = (first.cache_stats(), second.cache_stats());
    assert!(a.bytes < mine_before, "the first tree made room");
    assert!(a.bytes >= cap / 2, "{} below half of {cap}", a.bytes);
    assert!(b.bytes > 0);
    assert_eq!(a.profile_bytes, a.bytes + b.bytes);
    assert_eq!((a.max_bytes, a.caches), (cap, 2));
    assert!(
        a.profile_bytes <= cap / 10 * 9,
        "{} over the mark",
        a.profile_bytes
    );
    assert_eq!(budget.used(), a.profile_bytes);

    // What was evicted reads back from the fake.
    assert_eq!(
        std::fs::read(mnt.join("Mine/small.bin")).expect("read"),
        small
    );
    unmount_trees(&runtime, m);
}

/// Opens a share's tree as a Shared mount does (D112): a `ShareTree` over
/// the share's volume and root, its write gate closed.
fn open_share(
    runtime: &tokio::runtime::Runtime,
    backend: &FakeBackend,
    state: &Path,
    name: &str,
    volume: &str,
    root: &pdmount_backend::LinkId,
    kind: pdmount_backend::NodeKind,
) -> pdmount_fuse::MountTree {
    let mut options = EngineOptions::in_dir(
        &state.join(name),
        Config::default(),
        DeviceIdentity {
            id: Uuid::from_u128(1),
            name: "mounted-test".into(),
        },
        system_clock(),
    );
    options.write_gate = false;
    let tree = pdmount_backend::shared::ShareTree::new(
        Arc::new(backend.tree(volume, root.clone())),
        root.clone(),
        kind,
        Arc::new(|| {}),
    );
    let (engine, invalidations) = runtime
        .block_on(Engine::open(Arc::new(tree), options))
        .expect("engine");
    runtime
        .block_on(engine.ensure_listed(pdmount_core::vfs::ROOT_INO))
        .expect("list the root");
    engine.start_background_polled();
    let place = match kind {
        pdmount_backend::NodeKind::Folder => pdmount_fuse::TreePlace::Folder,
        pdmount_backend::NodeKind::File => {
            pdmount_fuse::TreePlace::File(pdmount_backend::shared::SHARED_FILE_NAME.into())
        }
    };
    pdmount_fuse::MountTree {
        name: name.to_owned(),
        engine,
        invalidations,
        volume_use: None,
        place,
    }
}

/// The names in a folder of the mount, sorted.
fn names_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("read the folder")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .into_string()
                .expect("utf8")
        })
        .collect();
    names.sort();
    names
}

/// M5.12 (D112): a Shared mount's trees come and go while it is mounted.
/// It starts empty; a folder share and a shared file appear (the file in
/// the files folder), read, and refuse writes with EROFS; a rename shows
/// at once; the inode numbers of a tree that stays do not change; a
/// removed (revoked) share disappears, and its open file answers EIO.
#[test]
fn shares_come_and_go_in_a_live_shared_mount() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    let team = backend
        .add_root(
            "Team",
            Some("alice@example.test"),
            Some(pdmount_backend::fake::FakeRole::Viewer),
        )
        .expect("a folder share");
    let content = b"from alice, shared".repeat(1000);
    runtime
        .block_on(
            backend
                .tree(&team.volume, team.root.clone())
                .with_client(pdmount_backend::fake::FakeClient {
                    user: Some("alice@example.test".into()),
                    ..Default::default()
                })
                .upload_new(
                    &team.root,
                    "notes.txt",
                    pdmount_backend::UploadSource::Bytes(bytes::Bytes::from(content.clone())),
                    pdmount_backend::FileMeta {
                        mtime: std::time::SystemTime::now(),
                        mode: Some(0o644),
                        media_type: "text/plain".into(),
                        sha1: None,
                    },
                ),
        )
        .expect("upload");
    let plan = runtime
        .block_on(backend.add_shared_file(
            "plan.txt",
            b"the plan",
            "bob@example.test",
            pdmount_backend::fake::FakeRole::Editor,
        ))
        .expect("a shared file");
    let state = temp.path().join("state");
    let mounted =
        pdmount_fuse::mount_dynamic(Vec::new(), &mnt, &Config::default(), false, "Shared files")
            .expect("mount");
    let set = mounted.tree_set().expect("a tree set");

    // Empty at first: a read-only top level.
    assert!(names_in(&mnt).is_empty());
    assert_eq!(std::fs::metadata(&mnt).expect("stat").mode() & 0o777, 0o555);
    assert!(!mnt.join("Team").exists(), "a negative entry is cached now");

    // The folder share appears.
    let team_slot = set
        .add(open_share(
            &runtime,
            &backend,
            &state,
            "Team",
            &team.volume,
            &team.root,
            pdmount_backend::NodeKind::Folder,
        ))
        .expect("add");
    wait_until("the share shows", &|| mnt.join("Team").exists());
    assert_eq!(names_in(&mnt), ["Team"]);
    assert_eq!(
        std::fs::read(mnt.join("Team/notes.txt")).expect("read"),
        content
    );
    let team_ino = std::fs::metadata(mnt.join("Team")).expect("stat").ino();
    let notes_ino = std::fs::metadata(mnt.join("Team/notes.txt"))
        .expect("stat")
        .ino();
    // Writes: EROFS, in the share and in the top level.
    for denied in [
        std::fs::write(mnt.join("Team/new.txt"), b"x").expect_err("a write"),
        std::fs::write(mnt.join("Team/notes.txt"), b"x").expect_err("a write"),
        std::fs::create_dir(mnt.join("new")).expect_err("mkdir"),
        std::fs::remove_file(mnt.join("Team/notes.txt")).expect_err("unlink"),
    ] {
        assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    }

    // The shared file appears in the files folder.
    let plan_slot = set
        .add(open_share(
            &runtime,
            &backend,
            &state,
            "plan.txt",
            &plan.volume,
            &plan.root,
            pdmount_backend::NodeKind::File,
        ))
        .expect("add");
    wait_until("the files folder shows", &|| {
        mnt.join("Shared files").exists()
    });
    assert_eq!(names_in(&mnt), ["Shared files", "Team"]);
    assert_eq!(names_in(&mnt.join("Shared files")), ["plan.txt"]);
    let plan_path = mnt.join("Shared files/plan.txt");
    assert_eq!(std::fs::read(&plan_path).expect("read"), b"the plan");
    let denied = std::fs::write(&plan_path, b"x").expect_err("a write");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    let denied = std::fs::write(mnt.join("Shared files/new"), b"x").expect_err("a create");
    assert_eq!(denied.raw_os_error(), Some(nix::libc::EROFS));
    // A tree that stays keeps its numbers.
    assert_eq!(
        std::fs::metadata(mnt.join("Team")).expect("stat").ino(),
        team_ino
    );

    // A new name shows at once (a suffix, D107); the old one is gone.
    assert!(set.rename(plan_slot, "plan.txt (bob@example.test)"));
    assert_eq!(
        names_in(&mnt.join("Shared files")),
        ["plan.txt (bob@example.test)"]
    );
    wait_until("the old name is gone", &|| !plan_path.exists());
    assert_eq!(
        std::fs::read(mnt.join("Shared files/plan.txt (bob@example.test)")).expect("read"),
        b"the plan"
    );

    // Revoked: the share leaves the mount. Paths through it answer ENOENT,
    // as for a deleted folder; its open file answers EIO (D112, D114).
    let mut open = std::fs::File::open(mnt.join("Team/notes.txt")).expect("open");
    let mut first = [0; 4];
    std::io::Read::read_exact(&mut open, &mut first).expect("read");
    backend
        .set_own_role(team.share.as_deref().expect("share"), None)
        .expect("revoke");
    let engine = set.remove(team_slot).expect("removed");
    assert!(set.remove(team_slot).is_none(), "removed once");
    assert_eq!(names_in(&mnt), ["Shared files"]);
    // The kernel's entry goes with the invalidation, sent by its thread
    // before those of the inodes the kernel holds; the cached entries below
    // it lead to a tree that answers ENOENT meanwhile, never EIO.
    let not_found = |path: &std::path::Path| {
        std::fs::metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
    };
    wait_until("the share is gone", &|| not_found(&mnt.join("Team")));
    wait_until("paths through the share are gone", &|| {
        not_found(&mnt.join("Team/notes.txt"))
    });
    // The open file's cached pages go with its own invalidation.
    let eio = |result: std::io::Result<usize>| {
        result.is_err_and(|error| error.raw_os_error() == Some(nix::libc::EIO))
    };
    wait_until("the open file answers EIO", &|| {
        eio(std::os::unix::fs::FileExt::read_at(&open, &mut [0; 4], 4))
    });
    let mut rest = Vec::new();
    let error = std::io::Read::read_to_end(&mut open, &mut rest).expect_err("a read");
    assert_eq!(error.raw_os_error(), Some(nix::libc::EIO));
    // The kernel sends `fstat` without the handle: an answer on the name.
    let error = open.metadata().expect_err("fstat of the open file");
    assert_eq!(error.raw_os_error(), Some(nix::libc::ENOENT));
    let error = open.sync_all().expect_err("fsync of the open file");
    assert_eq!(error.raw_os_error(), Some(nix::libc::EIO));
    let error = std::fs::metadata(mnt.join("Team/notes.txt")).expect_err("gone");
    assert_eq!(error.raw_os_error(), Some(nix::libc::ENOENT));
    drop(open);
    runtime.block_on(engine.shutdown()).expect("shutdown");

    // A share of the same name later gets a new slot, so new numbers: the
    // kernel never mixes it up with the one it may still hold.
    let other = backend
        .add_root(
            "Team",
            Some("carol@example.test"),
            Some(pdmount_backend::fake::FakeRole::Viewer),
        )
        .expect("another share");
    let again = set
        .add(open_share(
            &runtime,
            &backend,
            &temp.path().join("again"),
            "Team",
            &other.volume,
            &other.root,
            pdmount_backend::NodeKind::Folder,
        ))
        .expect("add");
    assert!(again > plan_slot);
    // The absence the kernel cached goes with the invalidation.
    wait_until("the new share shows", &|| mnt.join("Team").exists());
    let ino = std::fs::metadata(mnt.join("Team")).expect("stat").ino();
    assert_ne!(ino, team_ino);
    assert_ne!(ino >> 48, notes_ino >> 48);
    for slot in [again, plan_slot] {
        let engine = set.remove(slot).expect("removed");
        runtime.block_on(engine.shutdown()).expect("shutdown");
    }
    // The last file gone: the files folder goes too.
    assert!(names_in(&mnt).is_empty());
    mounted.unmount().expect("unmount");
}

/// D033, D110: the trees of a mount share the session's download permits.
/// Readers in two trees at once never have more than five blocks in flight
/// on the drive together.
#[test]
fn trees_read_in_parallel_within_the_session_download_limit() {
    let Some((runtime, temp, mnt)) = setup() else {
        return;
    };
    let backend = FakeBackend::new();
    backend.set_block_size(64 * 1024);
    let mine = backend
        .add_root("Mine", None, None)
        .expect("a root on the own volume");
    let team = backend
        .add_root(
            "Team",
            Some("owner@example.test"),
            Some(pdmount_backend::fake::FakeRole::Editor),
        )
        .expect("a share on another volume");
    // Four files of 16 blocks per tree: eight opens stay below a scan's ten.
    let mut files = Vec::new();
    for (tree, added) in [("Mine", &mine), ("Team", &team)] {
        for index in 0..4 {
            let content: Vec<u8> = (0..1 << 20)
                .map(|i: usize| ((i + index) * 7 % 251) as u8)
                .collect();
            runtime
                .block_on(backend.tree(&added.volume, added.root.clone()).upload_new(
                    &added.root,
                    &format!("f{index}.bin"),
                    pdmount_backend::UploadSource::Bytes(bytes::Bytes::from(content.clone())),
                    pdmount_backend::FileMeta {
                        mtime: std::time::SystemTime::now(),
                        mode: Some(0o644),
                        media_type: "application/octet-stream".into(),
                        sha1: None,
                    },
                ))
                .expect("upload");
            files.push((format!("{tree}/f{index}.bin"), content));
        }
    }
    let layout = [
        ("Mine", mine.volume.as_str(), &mine.root, false),
        ("Team", team.volume.as_str(), &team.root, false),
    ];
    let m = mount_trees(
        &runtime,
        &backend,
        &temp.path().join("state"),
        &mnt,
        &layout,
    );
    for (tree, _, _, _) in &layout {
        std::fs::read_dir(mnt.join(tree))
            .expect("list")
            .for_each(drop);
    }
    backend.set_latency(std::time::Duration::from_millis(20));
    backend.take_peak_block_fetches();
    std::thread::scope(|scope| {
        for (path, content) in &files {
            let path = mnt.join(path);
            scope.spawn(move || {
                assert_eq!(&std::fs::read(&path).expect("read"), content);
            });
        }
    });
    let peak = backend.take_peak_block_fetches();
    assert!(
        (2..=5).contains(&peak),
        "{peak} blocks in flight at once over two trees"
    );
    unmount_trees(&runtime, m);
}
