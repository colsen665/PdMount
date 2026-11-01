//! `pdmount` command-line interface (DESIGN.md §14).

mod access;
mod auth;
mod autostart;
mod cache;
mod conflicts;
mod control;
mod credentials;
mod daemon;
mod doctor;
mod mounts;
mod notify;
mod pins;
#[cfg(test)]
mod probe;
mod queue;
mod secret_cache;
mod session;
mod stats;
mod status;
mod trash;

use clap::{Args, Parser, Subcommand};
use pdmount_core::config::BrowserMode;
use pdmount_core::engine::queue::QueueTarget;
use pdmount_core::{Config, Profile, ProfilePaths};
use std::process::ExitCode;

/// Mount Proton Drive "My Files" as a filesystem.
#[derive(Debug, Parser)]
#[command(name = "pdmount", version, about)]
struct Cli {
    /// Profile to use: one account and one mount.
    #[arg(long, global = true, default_value = Profile::DEFAULT)]
    profile: Profile,

    /// Store secrets in 0600 files when the keyring is unavailable.
    #[arg(long, global = true, conflicts_with = "force_file_secrets")]
    allow_file_secrets: bool,

    /// Store secrets in 0600 files only; never use the keyring.
    #[arg(long, global = true)]
    force_file_secrets: bool,

    /// More log output (-v info, -vv debug); RUST_LOG overrides it.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in with Proton in the browser and store the session.
    Login(BrowserArgs),
    /// End the session and remove stored secrets.
    Logout,
    /// Show session, storage, device, mount, connection, queue and conflicts.
    Status,
    /// Mount My Files and run in the background.
    Mount(MountArgs),
    /// Upload what is pending, then unmount.
    Unmount(UnmountArgs),
    /// Show the daemon's counters, gauges and latencies.
    Stats(StatsArgs),
    /// Delete the local state and block cache; the login is kept.
    Reset(ResetArgs),
    /// Check FUSE, config, keyring, login, disk space, clock and autostart.
    Doctor,
    /// Read-only and read-write rules per path (while mounted).
    #[command(subcommand)]
    Access(AccessCommand),
    /// Conflicts between this device and others (while mounted).
    #[command(subcommand)]
    Conflicts(ConflictsCommand),
    /// Keep a file or folder offline (while mounted).
    Pin(PinArgs),
    /// Stop keeping a path offline (while mounted).
    Unpin {
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: std::path::PathBuf,
    },
    /// Mount this profile at desktop login; without an argument, show the state.
    Autostart {
        #[command(subcommand)]
        command: Option<AutostartCommand>,
    },
    /// Upload what is pending now and wait for it (while mounted).
    Sync {
        /// Give up after this many seconds.
        #[arg(long, default_value_t = 600)]
        timeout: u64,
        /// Only what is at or below this path. Absolute inside the
        /// mountpoint, or relative to the mount root.
        path: Option<std::path::PathBuf>,
    },
    /// Inspect and control the upload queue (while mounted).
    #[command(subcommand)]
    Queue(QueueCommand),
    /// The block cache (while mounted).
    #[command(subcommand)]
    Cache(CacheCommand),
    /// The account's trash (Proton only, while not mounted).
    #[command(subcommand)]
    Trash(TrashCommand),
    /// Test builds: manage a persisted fake drive (DESIGN.md §15.1).
    #[cfg(feature = "fake-backend")]
    #[command(subcommand)]
    Fake(FakeCommand),
}

#[cfg(feature = "fake-backend")]
#[derive(Debug, Subcommand)]
enum FakeCommand {
    /// Copy a local directory tree into the root of the fake drive.
    Seed {
        /// Local directory to copy.
        #[arg(long)]
        from: std::path::PathBuf,
        /// The fake drive's directory (created if missing).
        dir: std::path::PathBuf,
    },
    /// Print the changes the fake drive keeps, one per line, numbered.
    Changes {
        /// Only changes numbered above this.
        #[arg(long, default_value_t = 0)]
        after: u64,
        /// The fake drive's directory.
        dir: std::path::PathBuf,
    },
    /// Rename or move a node as another client would (`/a/b` paths).
    Rename {
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        dir: std::path::PathBuf,
    },
    /// Move a node to the trash as another client would.
    Trash {
        #[arg(long)]
        path: String,
        dir: std::path::PathBuf,
    },
    /// Upload a local file as a new revision of a file, as another client
    /// would (its mode is kept).
    Write {
        #[arg(long)]
        path: String,
        #[arg(long)]
        from: std::path::PathBuf,
        /// Act as an official app: no mode and no SHA-1 in the revision.
        #[arg(long)]
        app: bool,
        dir: std::path::PathBuf,
    },
    /// Create a new file from a local file, as another client would.
    Create {
        /// The new file's path (`/a/b`); its folder must exist.
        #[arg(long)]
        path: String,
        #[arg(long)]
        from: std::path::PathBuf,
        /// Act as an official app: no mode and no SHA-1 in the revision.
        #[arg(long)]
        app: bool,
        dir: std::path::PathBuf,
    },
    /// Create a folder, as another client would.
    Mkdir {
        #[arg(long)]
        path: String,
        dir: std::path::PathBuf,
    },
    /// Make calls of running daemons on this fake fail, or stop doing so.
    #[command(subcommand)]
    Fault(FaultCommand),
    /// Simulate a network for the daemons on this fake: a round trip per
    /// call, one shared pipe per direction. Options left out are 0 or
    /// unlimited; without options, print the profile.
    Profile(ProfileArgs),
}

#[cfg(feature = "fake-backend")]
#[derive(Debug, clap::Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct ProfileArgs {
    #[command(subcommand)]
    command: Option<ProfileCommand>,
    /// Round-trip time in milliseconds.
    #[arg(long)]
    rtt: Option<u64>,
    /// Upload bandwidth in bytes per second.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    up: Option<u64>,
    /// Download bandwidth in bytes per second.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
    down: Option<u64>,
    /// The fake drive's directory.
    #[arg(required = true)]
    dir: Option<std::path::PathBuf>,
}

#[cfg(feature = "fake-backend")]
#[derive(Debug, Subcommand)]
enum ProfileCommand {
    /// Remove the profile: calls cost nothing again.
    Clear { dir: std::path::PathBuf },
}

#[cfg(feature = "fake-backend")]
#[derive(Debug, Subcommand)]
enum FaultCommand {
    /// Calls fail as if the network were down.
    Offline(FaultArgs),
    /// Calls fail as if the session had expired.
    Unauthorized(FaultArgs),
    /// Remove the rules (of one client, or all).
    Clear {
        /// Client UID (`pdmount-<device id>`, see `pdmount status`).
        #[arg(long)]
        client: Option<String>,
        dir: std::path::PathBuf,
    },
}

#[cfg(feature = "fake-backend")]
#[derive(Debug, clap::Args)]
struct FaultArgs {
    /// Stop after this many seconds (default: until cleared).
    #[arg(long = "for")]
    seconds: Option<u64>,
    /// Only for this client UID.
    #[arg(long)]
    client: Option<String>,
    dir: std::path::PathBuf,
}

/// The client a `pdmount fake` write acts as.
#[cfg(feature = "fake-backend")]
fn fake_client(app: bool) -> pdmount_backend::fake::FakeClient {
    if app {
        pdmount_backend::fake::FakeClient {
            uid: Some("web".into()),
            claims_sha1: false,
            claims_mode: false,
        }
    } else {
        pdmount_backend::fake::FakeClient::default()
    }
}

/// The link of `/a/b` on a fake drive.
#[cfg(feature = "fake-backend")]
async fn fake_resolve(
    fake: &pdmount_backend::fake::FakeBackend,
    path: &str,
) -> anyhow::Result<pdmount_backend::LinkId> {
    use pdmount_backend::Backend;
    let mut link = fake.root().await?.link_id;
    for part in path.split('/').filter(|p| !p.is_empty()) {
        link = fake
            .list_children(&link)
            .await?
            .into_iter()
            .find(|child| child.name == part)
            .map(|child| child.link_id)
            .ok_or_else(|| anyhow::anyhow!("{path} not found on the fake drive"))?;
    }
    Ok(link)
}

#[derive(Debug, Subcommand)]
enum AccessCommand {
    /// Set `ro` or `rw` on a path and everything below it.
    Set {
        /// `ro` or `rw`.
        #[arg(value_parser = access::parse_mode)]
        mode: pdmount_core::config::AccessMode,
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: std::path::PathBuf,
    },
    /// Remove the rule on a path (also an orphaned one).
    Unset {
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: std::path::PathBuf,
    },
    /// Show all rules, or those at or below a path.
    List {
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: Option<std::path::PathBuf>,
    },
    /// Show the mode of a path and the rule that decides it.
    Effective {
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: std::path::PathBuf,
    },
}

#[derive(Debug, Subcommand)]
enum ConflictsCommand {
    /// Show the unresolved conflicts, or those at or below a path.
    List {
        /// Include resolved conflicts.
        #[arg(long)]
        all: bool,
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: Option<std::path::PathBuf>,
    },
    /// Show one conflict, by id or by the path of either copy.
    Show {
        /// A conflict id, or a path (write `./2024` for a file named 2024).
        target: String,
    },
    /// Settle a conflict: keep both copies, or keep one of them.
    Resolve {
        /// `keep-both`, `keep-local` or `keep-remote`.
        #[arg(value_parser = parse_resolution)]
        choice: pdmount_core::conflicts::Resolution,
        /// A conflict id, or the path of either copy.
        target: String,
    },
}

fn parse_resolution(value: &str) -> Result<pdmount_core::conflicts::Resolution, String> {
    pdmount_core::conflicts::Resolution::parse(value)
        .filter(|_| value.contains('-'))
        .ok_or_else(|| format!("expected keep-both, keep-local or keep-remote, got {value:?}"))
}

#[derive(Debug, Args)]
#[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
struct PinArgs {
    #[command(subcommand)]
    command: Option<PinCommand>,
    /// Absolute inside the mountpoint, or relative to the mount root
    /// (write `./list` for a file named list).
    #[arg(required = true)]
    path: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
enum PinCommand {
    /// Show the pinned paths: files, bytes fetched and state.
    List,
}

#[derive(Debug, Subcommand)]
enum AutostartCommand {
    /// Write the XDG autostart entry for this profile.
    Enable,
    /// Remove the entry.
    Disable,
}

/// Exactly one of `--id`, `--all` or a path.
#[derive(Debug, Args)]
#[group(required = true, multiple = false)]
struct QueueTargetArgs {
    /// The id `queue list` shows (the inode number).
    #[arg(long)]
    id: Option<u64>,
    /// Every entry.
    #[arg(long)]
    all: bool,
    /// Its entry or, for a folder, every entry under it. Absolute inside the
    /// mountpoint, or relative to the mount root.
    path: Option<std::path::PathBuf>,
}

#[derive(Debug, Subcommand)]
enum QueueCommand {
    /// Show what is pending: id, kind, state, attempts, age and path.
    List {
        /// Only what is at or below this path.
        path: Option<std::path::PathBuf>,
    },
    /// Show entries with their last error, base revision and dependencies.
    Show {
        /// The id `queue list` shows (the inode number).
        #[arg(long, conflicts_with = "path")]
        id: Option<u64>,
        /// Its entry or, for a folder, every entry under it. Absolute inside the
        /// mountpoint, or relative to the mount root.
        #[arg(required_unless_present = "id")]
        path: Option<std::path::PathBuf>,
    },
    /// Upload now: clears parks and back-offs.
    Retry(QueueTargetArgs),
    /// Hold entries until `queue retry` or the next mount.
    Park(QueueTargetArgs),
    /// Discard local changes: back to the remote version, or removed if
    /// never uploaded. Without --yes it only prints what it would discard.
    Drop {
        #[command(flatten)]
        target: QueueTargetArgs,
        /// Really discard the changes; this cannot be undone.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum CacheCommand {
    /// Show the disk cache against its cap, pinned bytes and the RAM pool.
    Stats,
    /// Evict the least recently used blocks; pinned blocks and the bases of
    /// changed files stay.
    Gc {
        /// Bytes to go down to, for example 2GiB (default: 80 % of `cache.max_bytes`).
        #[arg(long)]
        target: Option<pdmount_core::units::ByteSize>,
    },
    /// Evict every block that may go, or only those of files at or below a path.
    Wipe {
        /// Absolute inside the mountpoint, or relative to the mount root.
        path: Option<std::path::PathBuf>,
    },
}

#[derive(Debug, Subcommand)]
enum TrashCommand {
    /// Delete everything in the trash for good, account-wide.
    Empty {
        /// Required: the deletion cannot be undone.
        #[arg(long, required = true)]
        yes: bool,
    },
}

#[derive(Debug, Args)]
struct BrowserArgs {
    /// Open the sign-in link in the default browser.
    #[arg(long, conflicts_with = "no_browser")]
    browser: bool,
    /// Only print the sign-in link.
    #[arg(long)]
    no_browser: bool,
}

#[derive(Debug, Args)]
struct MountArgs {
    /// Stay in the foreground (used by the background launcher and services).
    #[arg(long)]
    foreground: bool,
    /// Storage backend: `proton`, or `fake:<dir>` in test builds.
    #[arg(long, default_value = "proton", value_parser = daemon::BackendChoice::parse,
          hide = !cfg!(feature = "fake-backend"))]
    backend: daemon::BackendChoice,
    /// Fail instead of signing in when the profile has no session.
    #[arg(long)]
    no_login: bool,
    #[command(flatten)]
    browser: BrowserArgs,
    /// Mount option: `ro` answers EROFS to every write; pending work still uploads.
    #[arg(short = 'o', value_name = "OPTION", value_parser = ["ro"])]
    option: Option<String>,
    /// Mountpoint; default: `mountpoint` from the config (~/ProtonDrive).
    mountpoint: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
struct UnmountArgs {
    /// Wait up to 10 minutes for pending uploads first (the default).
    #[arg(long, conflicts_with = "force")]
    wait: bool,
    /// Detach the mount even if it is busy or the daemon hangs, and stop the daemon.
    #[arg(long)]
    force: bool,
    /// Mountpoint; default: `mountpoint` from the config.
    mountpoint: Option<std::path::PathBuf>,
}

#[derive(Debug, Args)]
struct StatsArgs {
    /// Print the reply as JSON.
    #[arg(long)]
    json: bool,
    /// Clear the latency histograms after reading them.
    #[arg(long)]
    reset: bool,
}

#[derive(Debug, Args)]
struct ResetArgs {
    /// Required: the login and device identity are kept.
    #[arg(long, required = true)]
    keep_login: bool,
}

impl BrowserArgs {
    /// Command-line choice, falling back to `auth.browser` in the config.
    fn mode(&self, config: &Config) -> BrowserMode {
        match (self.browser, self.no_browser) {
            (true, _) => BrowserMode::Always,
            (_, true) => BrowserMode::Never,
            _ => config.auth.browser,
        }
    }
}

fn init_logging(verbose: u8) {
    use tracing_subscriber::Layer;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;
    let rust_log = std::env::var("RUST_LOG").ok();
    tracing_subscriber::registry()
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(std::io::stderr)
                // The daemon's stderr is its log file: no color codes there.
                .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
                .with_filter(log_filter(verbose, rust_log.as_deref())),
        )
        .init();
}

/// The SDK whose log records may carry secrets (DESIGN.md §12.4).
const SDK_SECRET_TARGET: &str = "proton_sdk_rs2";

/// The log filter: `RUST_LOG` when it parses, else the level from `-v`.
/// Records of `proton_sdk_rs2` above WARN never pass, whatever the
/// directives say: its debug logs printed key material before P16, and a
/// more specific directive (`proton_sdk_rs2::account=debug`, a span
/// directive) would win over a crate-wide `proton_sdk_rs2=warn` alone.
fn log_filter<S>(
    verbose: u8,
    rust_log: Option<&str>,
) -> impl tracing_subscriber::layer::Filter<S> + use<S>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a>,
{
    use tracing_subscriber::filter::{EnvFilter, FilterExt, filter_fn};
    let level = match verbose {
        0 => "warn",
        1 => "info",
        _ => "debug",
    };
    let sdk = format!("{SDK_SECRET_TARGET}=warn");
    // pgp warns on every re-encoded key whose length differs from the
    // original; it is harmless noise, so only RUST_LOG shows it.
    let env = rust_log
        .and_then(|directives| EnvFilter::try_new(directives).ok())
        .unwrap_or_else(|| EnvFilter::new(format!("{level},pgp::packet::packet_sum=error")))
        .add_directive(sdk.parse().expect("valid"));
    env.and(filter_fn(|meta| {
        *meta.level() <= tracing::Level::WARN || !meta.target().starts_with(SDK_SECRET_TARGET)
    }))
}

#[cfg(feature = "fake-backend")]
async fn fake_command(command: FakeCommand) -> anyhow::Result<ExitCode> {
    use pdmount_backend::Backend;
    use pdmount_backend::fake::{FakeBackend, FakeChange};
    match command {
        FakeCommand::Seed { from, dir } => {
            let fake = FakeBackend::open(&dir)?;
            let root = fake.root().await?.link_id;
            fake.import_dir(&from, &root).await?;
            println!(
                "Copied {} into the fake drive {}.",
                from.display(),
                dir.display()
            );
        }
        FakeCommand::Rename { from, to, dir } => {
            let fake = FakeBackend::open(&dir)?;
            let node = fake_resolve(&fake, &from).await?;
            let (parent, name) = to.rsplit_once('/').unwrap_or(("", to.as_str()));
            let parent = fake_resolve(&fake, parent).await?;
            fake.rename(&node, Some(&parent), name, None).await?;
        }
        FakeCommand::Trash { path, dir } => {
            let fake = FakeBackend::open(&dir)?;
            let node = fake_resolve(&fake, &path).await?;
            for result in fake.trash(&[node]).await? {
                result?;
            }
        }
        FakeCommand::Write {
            path,
            from,
            app,
            dir,
        } => {
            let fake = FakeBackend::open(&dir)?.with_client(fake_client(app));
            let link = fake_resolve(&fake, &path).await?;
            let node = fake
                .get_nodes(std::slice::from_ref(&link))
                .await?
                .pop()
                .flatten()
                .ok_or_else(|| anyhow::anyhow!("{path} not found"))?;
            let base = node
                .active_revision
                .ok_or_else(|| anyhow::anyhow!("{path} is not a file"))?;
            let data = anyhow::Context::with_context(std::fs::read(&from), || {
                format!("cannot read {}", from.display())
            })?;
            let meta = pdmount_backend::FileMeta {
                mtime: std::time::SystemTime::now(),
                mode: node.mode,
                media_type: "application/octet-stream".into(),
                sha1: None,
            };
            let source = pdmount_backend::UploadSource::Bytes(data.into());
            fake.upload_revision(&link, &base, source, meta).await?;
        }
        FakeCommand::Create {
            path,
            from,
            app,
            dir,
        } => {
            let fake = FakeBackend::open(&dir)?.with_client(fake_client(app));
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let parent = fake_resolve(&fake, parent).await?;
            let data = anyhow::Context::with_context(std::fs::read(&from), || {
                format!("cannot read {}", from.display())
            })?;
            let meta = pdmount_backend::FileMeta {
                mtime: std::time::SystemTime::now(),
                mode: Some(0o644),
                media_type: "application/octet-stream".into(),
                sha1: None,
            };
            let source = pdmount_backend::UploadSource::Bytes(data.into());
            fake.upload_new(&parent, name, source, meta).await?;
        }
        FakeCommand::Mkdir { path, dir } => {
            let fake = FakeBackend::open(&dir)?;
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            let parent = fake_resolve(&fake, parent).await?;
            fake.create_folder(&parent, name, std::time::SystemTime::now())
                .await?;
        }
        FakeCommand::Fault(command) => fake_fault(command)?,
        FakeCommand::Profile(args) => fake_profile(args)?,
        FakeCommand::Changes { after, dir } => {
            for (seq, change) in FakeBackend::open(&dir)?.changes_since(after) {
                match change {
                    FakeChange::CreatedFile { link, name } => {
                        println!("{seq} created-file {link} {name}")
                    }
                    FakeChange::CreatedFolder { link, name } => {
                        println!("{seq} created-folder {link} {name}")
                    }
                    FakeChange::NewRevision { link, revision } => {
                        println!("{seq} revision {link} {revision}")
                    }
                    FakeChange::Renamed { link, name } => println!("{seq} renamed {link} {name}"),
                    FakeChange::Trashed { link } => println!("{seq} trashed {link}"),
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

#[cfg(feature = "fake-backend")]
fn fake_fault(command: FaultCommand) -> anyhow::Result<()> {
    use pdmount_backend::fake::{FaultKind, FaultRule, add_fault, clear_faults};
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let rule = |kind, args: FaultArgs| {
        let rule = FaultRule {
            kind,
            until_ms: args.seconds.map(|seconds| now_ms + seconds * 1000),
            client: args.client,
        };
        add_fault(&args.dir, rule)
    };
    match command {
        FaultCommand::Offline(args) => rule(FaultKind::Offline, args)?,
        FaultCommand::Unauthorized(args) => rule(FaultKind::Unauthorized, args)?,
        FaultCommand::Clear { client, dir } => clear_faults(&dir, client.as_deref())?,
    }
    Ok(())
}

#[cfg(feature = "fake-backend")]
fn fake_profile(args: ProfileArgs) -> anyhow::Result<()> {
    use pdmount_backend::fake::{NetProfile, clear_profile, read_profile, write_profile};
    if let Some(ProfileCommand::Clear { dir }) = args.command {
        return Ok(clear_profile(&dir)?);
    }
    let dir = args.dir.expect("clap requires the directory");
    if args.rtt.is_some() || args.up.is_some() || args.down.is_some() {
        let profile = NetProfile {
            rtt: std::time::Duration::from_millis(args.rtt.unwrap_or(0)),
            up_bytes_per_sec: args.up,
            down_bytes_per_sec: args.down,
        };
        return Ok(write_profile(&dir, profile)?);
    }
    let profile = read_profile(&dir);
    let rate = |rate: Option<u64>| rate.map_or("unlimited".to_owned(), |r| format!("{r} B/s"));
    println!(
        "rtt {} ms, up {}, down {}",
        profile.rtt.as_millis(),
        rate(profile.up_bytes_per_sec),
        rate(profile.down_bytes_per_sec)
    );
    Ok(())
}

fn home_dir() -> anyhow::Result<std::path::PathBuf> {
    std::env::var_os("HOME")
        .map(Into::into)
        .ok_or_else(|| anyhow::anyhow!("$HOME is not set"))
}

async fn run(cli: Cli) -> anyhow::Result<ExitCode> {
    credentials::set_mode(match (cli.allow_file_secrets, cli.force_file_secrets) {
        (_, true) => credentials::SecretsMode::ForceFile,
        (true, _) => credentials::SecretsMode::AllowFile,
        _ => credentials::SecretsMode::Keyring,
    });
    let paths = ProfilePaths::for_current_user(cli.profile.clone())?;
    match cli.command {
        Command::Login(browser) => {
            let config = Config::load(&paths.config_file())?;
            if let Some(code) = daemon::login_via_daemon(&paths, browser.mode(&config)).await? {
                return Ok(code);
            }
            let logged_in = session::login(&paths, browser.mode(&config)).await?;
            println!(
                "Logged in as {} (profile {}, session stored in {}).",
                logged_in.session.username, paths.profile, logged_in.storage
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Logout => {
            match session::logout(&paths).await? {
                session::LogoutOutcome::WasNotLoggedIn => {
                    println!("Profile {} was not logged in.", paths.profile);
                }
                session::LogoutOutcome::LoggedOut { revoked: true } => {
                    println!("Logged out; the session was ended on the server.");
                }
                session::LogoutOutcome::LoggedOut { revoked: false } => {
                    println!(
                        "Logged out locally; the session could not be ended on the server \
                         (it expires on its own)."
                    );
                }
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Status => {
            let config = Config::load(&paths.config_file())?;
            let report = status::status(&paths, &config, &home_dir()?).await?;
            println!("{report}");
            Ok(if report.session.is_active() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            })
        }
        Command::Mount(args) => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::mountpoint(&config, &home_dir()?, args.mountpoint);
            let read_only = args.option.is_some();
            if args.foreground {
                let textfile = config.prometheus_textfile(&home_dir()?);
                daemon::run_foreground(
                    &paths,
                    &config,
                    &mountpoint,
                    &args.backend,
                    textfile,
                    read_only,
                )
                .await
            } else {
                let login = (!args.no_login).then_some(args.browser.mode(&config));
                daemon::mount_background(
                    &paths,
                    &mountpoint,
                    &args.backend,
                    cli.verbose,
                    login,
                    read_only,
                )
                .await
            }
        }
        Command::Unmount(args) => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint =
                daemon::existing_mountpoint(&paths, &config, &home_dir()?, args.mountpoint);
            daemon::unmount(&paths, &mountpoint, args.force).await
        }
        Command::Stats(args) => stats::run(&paths, args.json, args.reset).await,
        Command::Reset(_) => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            daemon::reset(&paths, &mountpoint).await
        }
        #[cfg(feature = "fake-backend")]
        Command::Fake(command) => fake_command(command).await,
        Command::Trash(TrashCommand::Empty { .. }) => {
            let config = Config::load(&paths.config_file())?;
            trash::empty(&paths, &config).await
        }
        Command::Access(command) => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            let relative = |path: &std::path::Path| access::mount_relative(path, &mountpoint);
            match command {
                AccessCommand::Set { mode, path } => {
                    access::set(&paths, relative(&path)?, mode).await
                }
                AccessCommand::Unset { path } => access::unset(&paths, relative(&path)?).await,
                AccessCommand::List { path } => {
                    let path = path.map(|p| relative(&p)).transpose()?;
                    access::list(&paths, path).await
                }
                AccessCommand::Effective { path } => {
                    access::effective(&paths, relative(&path)?).await
                }
            }
        }
        Command::Conflicts(command) => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            match command {
                ConflictsCommand::List { all, path } => {
                    let path = path
                        .map(|p| access::mount_relative(&p, &mountpoint))
                        .transpose()?;
                    conflicts::list(&paths, path, all).await
                }
                ConflictsCommand::Show { target } => {
                    conflicts::show(&paths, conflicts::target(&target, &mountpoint)?).await
                }
                ConflictsCommand::Resolve { choice, target } => {
                    conflicts::resolve(&paths, conflicts::target(&target, &mountpoint)?, choice)
                        .await
                }
            }
        }
        Command::Pin(args) => {
            let Some(path) = args.path else {
                return pins::list(&paths).await;
            };
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            pins::set(&paths, access::mount_relative(&path, &mountpoint)?).await
        }
        Command::Unpin { path } => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            pins::unset(&paths, access::mount_relative(&path, &mountpoint)?).await
        }
        Command::Autostart { command } => match command {
            Some(AutostartCommand::Enable) => autostart::enable(&paths),
            Some(AutostartCommand::Disable) => autostart::disable(&paths),
            None => Ok(autostart::show(&paths)),
        },
        Command::Sync { timeout, path } => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            let path = path
                .map(|p| access::mount_relative(&p, &mountpoint))
                .transpose()?;
            queue::sync(&paths, path, timeout).await
        }
        Command::Queue(command) => {
            let config = Config::load(&paths.config_file())?;
            let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
            let by_path = |path: std::path::PathBuf| {
                access::mount_relative(&path, &mountpoint).map(QueueTarget::Path)
            };
            let target = |args: QueueTargetArgs| match (args.id, args.all, args.path) {
                (Some(id), _, _) => Ok(QueueTarget::Id(id)),
                (None, true, _) => Ok(QueueTarget::All),
                (None, false, Some(path)) => by_path(path),
                (None, false, None) => unreachable!("clap requires one"),
            };
            match command {
                QueueCommand::List { path } => {
                    let target = path.map_or(Ok(QueueTarget::All), by_path)?;
                    queue::list(&paths, target).await
                }
                QueueCommand::Show { id, path } => {
                    let target = match (id, path) {
                        (Some(id), _) => QueueTarget::Id(id),
                        (None, Some(path)) => by_path(path)?,
                        (None, None) => unreachable!("clap requires one"),
                    };
                    queue::show(&paths, target).await
                }
                QueueCommand::Retry(args) => queue::retry(&paths, target(args)?).await,
                QueueCommand::Park(args) => queue::park(&paths, target(args)?).await,
                QueueCommand::Drop { target: args, yes } => {
                    queue::drop(&paths, target(args)?, yes).await
                }
            }
        }
        Command::Cache(command) => match command {
            CacheCommand::Stats => cache::stats(&paths).await,
            CacheCommand::Gc { target } => cache::gc(&paths, target).await,
            CacheCommand::Wipe { path } => {
                let config = Config::load(&paths.config_file())?;
                let mountpoint = daemon::existing_mountpoint(&paths, &config, &home_dir()?, None);
                let path = path
                    .map(|p| access::mount_relative(&p, &mountpoint))
                    .transpose()?;
                cache::wipe(&paths, path).await
            }
        },
        Command::Doctor => {
            let checks = doctor::run(&paths).await;
            for check in &checks {
                println!("{check}");
            }
            let failed = checks.iter().any(|c| c.level == doctor::Level::Fail);
            Ok(if failed {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
    }
}

/// The SDK parses ML-DSA keys recursively; worker threads need a larger
/// stack than tokio's default (pdcli uses the same size).
pub(crate) const WORKER_STACK_SIZE: usize = 16 << 20;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging(cli.verbose);
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(WORKER_STACK_SIZE)
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("error: cannot start the async runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    // Run on a worker thread so the large stack applies.
    let result = runtime.block_on(async { tokio::spawn(run(cli)).await });
    match result {
        Ok(Ok(code)) => code,
        Ok(Err(error)) => {
            eprintln!("error: {error:#}");
            ExitCode::FAILURE
        }
        Err(join_error) => {
            eprintln!("error: {join_error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use clap::CommandFactory;

    /// A log writer that keeps everything in memory.
    #[derive(Clone, Default)]
    pub(crate) struct Captured(std::sync::Arc<parking_lot::Mutex<Vec<u8>>>);

    impl std::io::Write for Captured {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Captured {
        pub(crate) fn text(&self) -> String {
            String::from_utf8_lossy(&self.0.lock()).into_owned()
        }
    }

    #[test]
    fn sdk_debug_records_never_reach_the_log() {
        use tracing_subscriber::Layer;
        use tracing_subscriber::layer::SubscriberExt;
        // The SDK logs through `log`; the bridge is what the daemon uses.
        let _ = tracing_log::LogTracer::builder()
            .with_max_level(log::LevelFilter::Trace)
            .init();
        let directives = [
            None,
            Some("trace"),
            Some("proton_sdk_rs2=trace"),
            Some("proton_sdk_rs2::account=trace,proton_sdk_rs2::api::cache=debug"),
            Some("[sdk]=trace"),
        ];
        for rust_log in directives {
            let captured = Captured::default();
            let writer = captured.clone();
            let subscriber = tracing_subscriber::registry().with(
                tracing_subscriber::fmt::layer()
                    .with_writer(move || writer.clone())
                    .with_ansi(false)
                    .with_filter(log_filter(2, rust_log)),
            );
            tracing::subscriber::with_default(subscriber, || {
                let _span = tracing::info_span!("sdk").entered();
                log::debug!(target: "proton_sdk_rs2::account", "Passphrase: hunter2");
                log::trace!(target: "proton_sdk_rs2::api::cache", "raw: address-key");
                tracing::debug!(target: "proton_sdk_rs2::secret", "serialized-passphrase");
                log::warn!(target: "proton_sdk_rs2::auth", "refresh token rejected");
                tracing::debug!("pdmount debug record");
            });
            let text = captured.text();
            for secret in ["hunter2", "address-key", "serialized-passphrase"] {
                assert!(
                    !text.contains(secret),
                    "{rust_log:?} let {secret} through: {text}"
                );
            }
            assert!(
                text.contains("refresh token rejected"),
                "{rust_log:?}: {text}"
            );
            if rust_log.is_none_or(|d| d == "trace") {
                assert!(
                    text.contains("pdmount debug record"),
                    "{rust_log:?}: {text}"
                );
            }
        }
    }

    #[test]
    fn cli_definition_is_consistent() {
        Cli::command().debug_assert();
    }

    #[test]
    fn profile_option_goes_before_or_after_the_subcommand() {
        let before =
            Cli::try_parse_from(["pdmount", "--profile", "work", "status"]).expect("parse");
        let after = Cli::try_parse_from(["pdmount", "status", "--profile", "work"]).expect("parse");
        assert_eq!(before.profile.as_str(), "work");
        assert_eq!(after.profile.as_str(), "work");
        assert!(Cli::try_parse_from(["pdmount", "--profile", "../x", "status"]).is_err());
        assert_eq!(
            Cli::try_parse_from(["pdmount", "doctor"])
                .expect("parse")
                .profile,
            Profile::default()
        );
    }

    #[test]
    fn trash_empty_requires_yes() {
        assert!(Cli::try_parse_from(["pdmount", "trash", "empty"]).is_err());
        assert!(matches!(
            Cli::try_parse_from(["pdmount", "trash", "empty", "--yes"])
                .expect("parse")
                .command,
            Command::Trash(TrashCommand::Empty { yes: true })
        ));
    }

    #[test]
    fn mount_browser_flags_override_the_config() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).expect("parse").command {
            Command::Mount(args) => args.browser,
            other => panic!("unexpected {other:?}"),
        };
        let config = Config::default();
        assert_eq!(
            parse(&["pdmount", "mount"]).mode(&config),
            BrowserMode::Auto
        );
        assert_eq!(
            parse(&["pdmount", "mount", "--browser"]).mode(&config),
            BrowserMode::Always
        );
        assert_eq!(
            parse(&["pdmount", "mount", "--no-browser"]).mode(&config),
            BrowserMode::Never
        );
    }

    #[test]
    fn login_browser_flags_override_the_config() {
        let parse = |args: &[&str]| match Cli::try_parse_from(args).expect("parse").command {
            Command::Login(browser) => browser,
            other => panic!("unexpected {other:?}"),
        };
        let config = Config::default();
        assert_eq!(
            parse(&["pdmount", "login"]).mode(&config),
            BrowserMode::Auto
        );
        assert_eq!(
            parse(&["pdmount", "login", "--browser"]).mode(&config),
            BrowserMode::Always
        );
        assert_eq!(
            parse(&["pdmount", "login", "--no-browser"]).mode(&config),
            BrowserMode::Never
        );
        assert!(Cli::try_parse_from(["pdmount", "login", "--browser", "--no-browser"]).is_err());
        let mut never = Config::default();
        never.auth.browser = BrowserMode::Never;
        assert_eq!(
            parse(&["pdmount", "login"]).mode(&never),
            BrowserMode::Never
        );
    }
}
