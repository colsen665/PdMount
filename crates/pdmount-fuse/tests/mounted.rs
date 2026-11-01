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

    std::fs::create_dir(mnt.join("made")).expect("mkdir");
    std::fs::metadata(mnt.join("made")).expect("stat");
    std::fs::write(mnt.join("made/file"), b"content").expect("write");
    assert_eq!(
        std::fs::read(mnt.join("made/file")).expect("read"),
        b"content"
    );
    assert!(count("mkdir") > 0);
    assert!(count("lookup") + count("getattr") > 0);
    assert!(count("create") > 0 && count("write") > 0);
    assert!(count("read_staging") + count("read_ram") > 0);

    metrics.reset_latency();
    assert!(metrics.snapshot_latency().is_empty());
    std::fs::create_dir(mnt.join("again")).expect("mkdir");
    assert_eq!(count("mkdir"), 1);
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
    let listings = || {
        metrics
            .snapshot_latency()
            .into_iter()
            .filter(|(name, _)| matches!(*name, "readdir" | "readdirplus"))
            .map(|(_, summary)| summary.count)
            .sum::<u64>()
    };
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
    assert_eq!(names(&dir), ["a"]);
    let before = listings();
    assert_eq!(names(&dir), ["a"]);
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
