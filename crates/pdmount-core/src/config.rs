//! Profile configuration file `~/.config/pdmount/<profile>.toml`
//! (DESIGN.md Appendix B). Every key is optional; unknown keys are rejected
//! so that typos do not silently fall back to defaults.

use crate::units::{ByteSize, HumanDuration};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Mount point; `~/` is expanded. Default: `~/ProtonDrive`.
    pub mountpoint: Option<String>,
    pub device: DeviceConfig,
    pub auth: AuthConfig,
    pub access: AccessConfig,
    pub cache: CacheConfig,
    /// The bulk-read guard (D081, D111).
    pub scan: ScanConfig,
    pub upload: UploadConfig,
    pub fsync: FsyncConfig,
    pub kernel: KernelConfig,
    pub sync: SyncConfig,
    pub timeouts: TimeoutConfig,
    pub stats: StatsConfig,
    pub photos: PhotosConfig,
    /// Further roots, each on its own mountpoint (M5, D079, D110).
    pub roots: Vec<RootConfig>,
}

/// One further root (`[[roots]]`, D079, D110): a mount of its own next to
/// My Files, served by the same daemon and session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RootConfig {
    pub kind: RootKind,
    /// `~/` is expanded; neither My Files' mountpoint nor another root's,
    /// nor inside one of them.
    pub mountpoint: String,
    /// Like `mount -o ro`, for this mount only.
    #[serde(default)]
    pub read_only: bool,
    /// Leave a pending marker in the root of each writable tree of this
    /// mount when a stop's grace runs out with changes left (D076, D114);
    /// in a tree on another user's volume (a share) it names no device.
    #[serde(default)]
    pub pending_marker: bool,
    /// `kind = "test"` only: the trees of the mount.
    #[serde(default)]
    pub tree: Vec<TestTreeConfig>,
}

/// What a further root maps. Computers is parsed so that a config naming
/// it gets a clear answer, but it is refused (out of M5's scope, D122). Shared
/// (M5.12, D112) mounts one tree per share of the shared-with-me listing;
/// Photos (M5.13, D113) the Photos timeline by year and month, read-only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RootKind {
    Computers,
    Shared,
    Photos,
    /// Trees named in the config, on a fake drive: test builds only
    /// (feature `fake-backend`), checked by the daemon.
    Test,
}

impl RootKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Computers => "computers",
            Self::Shared => "shared",
            Self::Photos => "photos",
            Self::Test => "test",
        }
    }
}

/// A tree of a `test` root: a folder on a fake drive's volume.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestTreeConfig {
    /// The tree's folder in a mount with several trees.
    pub name: String,
    pub volume: String,
    pub root: String,
    /// Opens the write gate (tests only; every further tree's is closed
    /// until M5.14).
    #[serde(default)]
    pub writable: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DeviceConfig {
    /// Device name used in conflict copies. Default: the host name.
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AuthConfig {
    pub browser: BrowserMode,
}

/// Whether the sign-in link is opened in the default browser. The link is
/// always printed as well (DESIGN.md §12.2).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrowserMode {
    /// Open the browser only in an interactive graphical session.
    #[default]
    Auto,
    Always,
    Never,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AccessConfig {
    pub default: AccessMode,
    /// Rules imported at mount time (DESIGN.md §11.5).
    pub rule: Vec<AccessRuleConfig>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AccessMode {
    Ro,
    #[default]
    Rw,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccessRuleConfig {
    pub path: String,
    pub mode: AccessMode,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    pub max_bytes: ByteSize,
    pub ram_bytes: ByteSize,
    pub prefetch_small_files: ByteSize,
    pub prefetch_dir_budget: ByteSize,
}

impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            max_bytes: ByteSize(10 << 30),
            ram_bytes: ByteSize(32 << 20),
            prefetch_small_files: ByteSize(256 << 10),
            prefetch_dir_budget: ByteSize(16 << 20),
        }
    }
}

/// The bulk-read guard (DESIGN.md §6.4, D081, D111): many different files
/// opened in quick succession are a scan, whose first bytes are read below
/// interactive priority, one download at a time, without readahead and in
/// a limited share of the disk cache.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ScanConfig {
    /// Different files opened within `window` that begin a scan; 0 turns
    /// the guard off.
    pub opens: u32,
    pub window: HumanDuration,
    /// Bytes read through a handle opened during a scan before its reads
    /// count as interactive again.
    pub first_bytes: ByteSize,
    /// Spacing of the starts of scan downloads.
    pub interval: HumanDuration,
    /// Percent of `cache.max_bytes` that blocks downloaded by scan reads
    /// may fill; beyond it the oldest of them are evicted first.
    pub cache_share: u8,
}

impl Default for ScanConfig {
    fn default() -> Self {
        Self {
            opens: 10,
            window: HumanDuration(Duration::from_secs(5)),
            first_bytes: ByteSize(8 << 20),
            interval: HumanDuration(Duration::from_millis(100)),
            cache_share: 10,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct UploadConfig {
    pub concurrency: u32,
    pub debounce: HumanDuration,
    pub max_dirty_age: HumanDuration,
    pub meta_revision: MetaRevisionPolicy,
}

impl Default for UploadConfig {
    fn default() -> Self {
        Self {
            concurrency: 4,
            debounce: HumanDuration(Duration::from_secs(2)),
            max_dirty_age: HumanDuration(Duration::from_secs(600)),
            meta_revision: MetaRevisionPolicy::Always,
        }
    }
}

/// When a mode/mtime-only change of a clean file creates a new revision
/// (DESIGN.md §8.5): `always`, `never` or `max_size:<size>`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum MetaRevisionPolicy {
    #[default]
    Always,
    Never,
    MaxSize(ByteSize),
}

impl FromStr for MetaRevisionPolicy {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "always" => Ok(Self::Always),
            "never" => Ok(Self::Never),
            _ => value
                .strip_prefix("max_size:")
                .ok_or_else(|| format!("expected always, never or max_size:<size>, got {value:?}"))?
                .parse()
                .map(Self::MaxSize)
                .map_err(|error| error.to_string()),
        }
    }
}

impl std::fmt::Display for MetaRevisionPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Always => f.write_str("always"),
            Self::Never => f.write_str("never"),
            Self::MaxSize(size) => write!(f, "max_size:{size}"),
        }
    }
}

impl<'de> Deserialize<'de> for MetaRevisionPolicy {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

impl Serialize for MetaRevisionPolicy {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FsyncConfig {
    pub mode: FsyncMode,
    /// Globs that use `remote` fsync regardless of `mode`, matched against
    /// the path from the mount root (`/important/**`).
    pub remote_paths: Vec<String>,
}

impl FsyncConfig {
    /// `remote_paths` compiled; `*` stops at a `/`.
    pub fn remote_matcher(&self) -> Result<globset::GlobSet, globset::Error> {
        let mut set = globset::GlobSetBuilder::new();
        for path in &self.remote_paths {
            set.add(
                globset::GlobBuilder::new(path)
                    .literal_separator(true)
                    .build()?,
            );
        }
        set.build()
    }
}

/// DESIGN.md §5.6, decision D012.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FsyncMode {
    #[default]
    Local,
    Remote,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct KernelConfig {
    pub attr_ttl: HumanDuration,
    pub entry_ttl: HumanDuration,
    pub negative_ttl: HumanDuration,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            attr_ttl: HumanDuration(Duration::from_secs(60)),
            entry_ttl: HumanDuration(Duration::from_secs(60)),
            negative_ttl: HumanDuration(Duration::from_secs(10)),
        }
    }
}

/// The official clients poll their own volume every 30 s
/// (`cs/client/js/src/internal/events/eventScheduler.ts:1`).
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(30);
/// Probes of a lost connection are cheap, one quota request each.
const MIN_PROBE_INTERVAL: Duration = Duration::from_secs(10);
/// A longer window would keep a scan running long after it ended (D111).
const MAX_SCAN_WINDOW: Duration = Duration::from_secs(60);
/// Beyond it a scan read could wait out its patience behind a few others.
const MAX_SCAN_INTERVAL: Duration = Duration::from_secs(10);
/// File uploads at once: below the official SDKs' 5 (JS) and 6 (C#), D033.
const MAX_UPLOAD_CONCURRENCY: u32 = 4;
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Event polling interval; never below the official clients' 30 s
    /// (D033).
    pub poll_interval: HumanDuration,
    /// Time between connectivity probes while offline or signed out (§9.6).
    pub probe_interval: HumanDuration,
    /// Leave `.pdmount/<device id>.pending` in the root when a stop's grace
    /// runs out with changes left (§14.4, D076). Off by default: it adds a
    /// visible folder to the drive. Markers of other devices are always read.
    pub pending_marker: bool,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            poll_interval: HumanDuration(MIN_POLL_INTERVAL),
            probe_interval: HumanDuration(Duration::from_secs(30)),
            pending_marker: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TimeoutConfig {
    pub metadata: HumanDuration,
    pub block: HumanDuration,
    pub offline_read: HumanDuration,
    pub shutdown_grace: HumanDuration,
}

impl Default for TimeoutConfig {
    fn default() -> Self {
        Self {
            metadata: HumanDuration(Duration::from_secs(20)),
            block: HumanDuration(Duration::from_secs(60)),
            offline_read: HumanDuration(Duration::from_secs(2)),
            // D075: long enough for a flush of what is queued at a stop.
            shutdown_grace: HumanDuration(Duration::from_secs(90)),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct StatsConfig {
    /// Prometheus textfile written every 15 s; `~/` is expanded. Empty:
    /// off (DESIGN.md §5.8).
    pub prometheus_textfile: String,
}

/// A Photos root (`[[roots]] kind = "photos"`, D113).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PhotosConfig {
    /// When a month is opened, write Proton's thumbnails of its photos to
    /// the freedesktop thumbnail cache (`$XDG_CACHE_HOME/thumbnails`), so a
    /// file manager need not open the originals (D081, D107, D115).
    pub thumbnail_cache: bool,
}

impl Default for PhotosConfig {
    fn default() -> Self {
        Self {
            thumbnail_cache: true,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("invalid configuration in {path}: {source}")]
    Parse {
        path: PathBuf,
        source: Box<toml::de::Error>,
    },
    #[error("invalid configuration in {path}: {message}")]
    Invalid { path: PathBuf, message: String },
}

impl Config {
    pub const DEFAULT_MOUNTPOINT: &'static str = "~/ProtonDrive";

    /// Loads the file at `path`; a missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(source) => {
                return Err(ConfigError::Read {
                    path: path.to_owned(),
                    source,
                });
            }
        };
        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_owned(),
            source: Box::new(source),
        })?;
        config.validate().map_err(|message| ConfigError::Invalid {
            path: path.to_owned(),
            message,
        })?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        if !(1..=MAX_UPLOAD_CONCURRENCY).contains(&self.upload.concurrency) {
            return Err(format!(
                "upload.concurrency must be 1-{MAX_UPLOAD_CONCURRENCY}: more file uploads at once \
                 than the official SDKs allow puts the service at risk (D033)"
            ));
        }
        if let Err(error) = self.fsync.remote_matcher() {
            return Err(format!("fsync.remote_paths: {error}"));
        }
        if self.cache.ram_bytes.0 < crate::pool::MIN_RAM_BYTES {
            return Err("cache.ram_bytes must be at least 16MiB".into());
        }
        if self.scan.opens == 1 {
            return Err("scan.opens must be 0 (off) or at least 2".into());
        }
        if self.scan.window.0.is_zero() || self.scan.window.0 > MAX_SCAN_WINDOW {
            return Err("scan.window must be more than 0s and at most 60s".into());
        }
        if self.scan.interval.0 > MAX_SCAN_INTERVAL {
            return Err("scan.interval must be at most 10s".into());
        }
        if self.scan.cache_share > 100 {
            return Err("scan.cache_share is a percentage: 0-100".into());
        }
        if self.sync.poll_interval.0 < MIN_POLL_INTERVAL {
            return Err("sync.poll_interval must be at least 30s".into());
        }
        if self.sync.probe_interval.0 < MIN_PROBE_INTERVAL {
            return Err("sync.probe_interval must be at least 10s".into());
        }
        if self.device.name.as_deref().is_some_and(|name| {
            name.trim().is_empty() || name.contains(['/', '(', ')']) || name.len() > 64
        }) {
            return Err("device.name must be 1-64 characters without '/', '(' or ')'".into());
        }
        let textfile = &self.stats.prometheus_textfile;
        if !textfile.is_empty() && !textfile.starts_with('/') && !textfile.starts_with("~/") {
            return Err("stats.prometheus_textfile must be absolute or start with '~/'".into());
        }
        self.validate_roots()?;
        for rule in &self.access.rule {
            if !rule.path.starts_with('/') {
                return Err(format!(
                    "access rule path {:?} must start with '/' (relative to the mount root)",
                    rule.path
                ));
            }
        }
        Ok(())
    }

    fn validate_roots(&self) -> Result<(), String> {
        for (index, root) in self.roots.iter().enumerate() {
            let kind = root.kind.as_str();
            if root.kind == RootKind::Computers {
                return Err(format!(
                    "roots: kind {kind:?} is not available yet; this version mounts My Files, Shared and Photos"
                ));
            }
            // Each share's tree, and the Photos tree, keeps its state under
            // `trees/<key>/`: two such mounts would open it twice.
            if matches!(root.kind, RootKind::Shared | RootKind::Photos)
                && self.roots[..index]
                    .iter()
                    .any(|other| other.kind == root.kind)
            {
                return Err(format!("roots: kind {kind:?} can be configured once"));
            }
            let mountpoint = &root.mountpoint;
            if !mountpoint.starts_with('/') && !mountpoint.starts_with("~/") {
                return Err(format!(
                    "roots: the mountpoint {mountpoint:?} must be absolute or start with '~/'"
                ));
            }
            if self.roots[..index]
                .iter()
                .any(|other| other.mountpoint == root.mountpoint)
            {
                return Err(format!(
                    "roots: the mountpoint {mountpoint:?} is used twice"
                ));
            }
            if root.kind != RootKind::Test && !root.tree.is_empty() {
                return Err(format!(
                    "roots: only kind \"test\" takes trees, not {kind:?}"
                ));
            }
            if root.kind == RootKind::Test && root.tree.is_empty() {
                return Err(
                    "roots: a root of kind \"test\" needs at least one [[roots.tree]]".into(),
                );
            }
            for (at, tree) in root.tree.iter().enumerate() {
                let name = &tree.name;
                if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                    return Err(format!("roots: the tree name {name:?} is not a file name"));
                }
                if root.tree[..at].iter().any(|other| &other.name == name) {
                    return Err(format!("roots: the tree name {name:?} is used twice"));
                }
            }
        }
        Ok(())
    }

    /// The mountpoints of `[[roots]]` with `~/` expanded, in order.
    pub fn root_mountpoints(&self, home: &Path) -> Vec<PathBuf> {
        self.roots
            .iter()
            .map(|root| expand_home(&root.mountpoint, home))
            .collect()
    }

    /// Mount point with `~/` expanded against `home`.
    pub fn mountpoint(&self, home: &Path) -> PathBuf {
        expand_home(
            self.mountpoint
                .as_deref()
                .unwrap_or(Self::DEFAULT_MOUNTPOINT),
            home,
        )
    }

    /// The Prometheus textfile with `~/` expanded; `None` when off.
    pub fn prometheus_textfile(&self, home: &Path) -> Option<PathBuf> {
        let configured = &self.stats.prometheus_textfile;
        (!configured.is_empty()).then(|| expand_home(configured, home))
    }
}

fn expand_home(configured: &str, home: &Path) -> PathBuf {
    match configured.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None if configured == "~" => home.to_owned(),
        None => PathBuf::from(configured),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Config, ConfigError> {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("p.toml");
        std::fs::write(&path, text).expect("write");
        Config::load(&path)
    }

    #[test]
    fn missing_file_gives_defaults() {
        let config = Config::load(Path::new("/nonexistent/pdmount.toml")).expect("defaults");
        assert_eq!(config, Config::default());
        assert_eq!(config.auth.browser, BrowserMode::Auto);
        assert_eq!(config.fsync.mode, FsyncMode::Local);
        assert_eq!(config.access.default, AccessMode::Rw);
        assert_eq!(config.cache.max_bytes, ByteSize(10 << 30));
        assert_eq!(config.upload.debounce.0, Duration::from_secs(2));
        assert!(config.photos.thumbnail_cache, "on by default (D115)");
    }

    #[test]
    fn appendix_b_example_parses() {
        let config = parse(
            r#"
mountpoint = "~/Drive"

[device]
name = "laptop"

[auth]
browser = "never"

[access]
default = "rw"

[[access.rule]]
path = "/Archive"
mode = "ro"

[cache]
max_bytes = "20GiB"

[upload]
concurrency = 2
debounce = "500ms"
meta_revision = "max_size:64MiB"

[fsync]
mode = "remote"
remote_paths = ["/important/**"]

[kernel]
attr_ttl = "30s"

[sync]
poll_interval = "1m"
probe_interval = "15s"
pending_marker = true

[timeouts]
metadata = "30s"

[stats]
prometheus_textfile = "~/metrics/pdmount.prom"

[photos]
thumbnail_cache = false
"#,
        )
        .expect("valid");
        assert_eq!(
            config.mountpoint(Path::new("/home/u")),
            Path::new("/home/u/Drive")
        );
        assert_eq!(config.device.name.as_deref(), Some("laptop"));
        assert_eq!(config.auth.browser, BrowserMode::Never);
        assert_eq!(
            config.access.rule,
            vec![AccessRuleConfig {
                path: "/Archive".into(),
                mode: AccessMode::Ro
            }]
        );
        assert_eq!(config.cache.max_bytes, ByteSize(20 << 30));
        assert_eq!(config.cache.ram_bytes, ByteSize(32 << 20));
        assert_eq!(config.upload.concurrency, 2);
        assert_eq!(config.upload.debounce.0, Duration::from_millis(500));
        assert_eq!(
            config.upload.meta_revision,
            MetaRevisionPolicy::MaxSize(ByteSize(64 << 20))
        );
        assert_eq!(config.fsync.mode, FsyncMode::Remote);
        assert_eq!(config.kernel.attr_ttl.0, Duration::from_secs(30));
        assert_eq!(config.sync.poll_interval.0, Duration::from_secs(60));
        assert_eq!(config.sync.probe_interval.0, Duration::from_secs(15));
        assert!(config.sync.pending_marker);
        assert!(!config.photos.thumbnail_cache);
        assert_eq!(config.timeouts.metadata.0, Duration::from_secs(30));
        assert_eq!(
            config.prometheus_textfile(Path::new("/home/u")),
            Some(PathBuf::from("/home/u/metrics/pdmount.prom"))
        );
    }

    #[test]
    fn scan_keys_have_defaults_and_bounds() {
        let defaults = Config::default().scan;
        assert_eq!(defaults.opens, 10);
        assert_eq!(defaults.window.0, Duration::from_secs(5));
        assert_eq!(defaults.first_bytes, ByteSize(8 << 20));
        assert_eq!(defaults.interval.0, Duration::from_millis(100));
        assert_eq!(defaults.cache_share, 10);
        let set = parse(
            "[scan]\nopens = 0\nwindow = \"2s\"\nfirst_bytes = \"4MiB\"\ninterval = \"500ms\"\ncache_share = 0",
        )
        .expect("valid");
        assert_eq!(set.scan.opens, 0);
        assert_eq!(set.scan.window.0, Duration::from_secs(2));
        assert_eq!(set.scan.first_bytes, ByteSize(4 << 20));
        assert_eq!(set.scan.interval.0, Duration::from_millis(500));
        assert_eq!(set.scan.cache_share, 0);
        for bad in [
            "opens = 1",
            "window = \"0s\"",
            "window = \"2m\"",
            "interval = \"11s\"",
            "cache_share = 101",
        ] {
            assert!(
                matches!(
                    parse(&format!("[scan]\n{bad}")),
                    Err(ConfigError::Invalid { .. })
                ),
                "{bad} was accepted"
            );
        }
    }

    #[test]
    fn prometheus_textfile_is_off_by_default_and_must_be_absolute() {
        let home = Path::new("/home/u");
        assert_eq!(Config::default().prometheus_textfile(home), None);
        let absolute =
            parse("[stats]\nprometheus_textfile = \"/var/lib/node/pd.prom\"").expect("valid");
        assert_eq!(
            absolute.prometheus_textfile(home),
            Some(PathBuf::from("/var/lib/node/pd.prom"))
        );
        assert!(matches!(
            parse("[stats]\nprometheus_textfile = \"pd.prom\""),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse("[stats]\nprometheus = \"/x\""),
            Err(ConfigError::Parse { .. })
        ));
    }

    #[test]
    fn unknown_keys_and_bad_values_are_rejected() {
        assert!(matches!(
            parse("mountpiont = \"x\""),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            parse("[auth]\nbrowser = \"sometimes\""),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            parse("[upload]\ndebounce = \"2\""),
            Err(ConfigError::Parse { .. })
        ));
        assert!(matches!(
            parse("[upload]\nconcurrency = 0"),
            Err(ConfigError::Invalid { .. })
        ));
        // Below the official SDKs' 5 (JS) and 6 (C#) file transfers (D033, D058).
        assert!(
            matches!(parse("[upload]\nconcurrency = 5"), Err(ConfigError::Invalid { message, .. }) if message.contains("D033"))
        );
        assert_eq!(
            parse("[upload]\nconcurrency = 4")
                .expect("4 is allowed")
                .upload
                .concurrency,
            4
        );
        assert!(matches!(
            parse("[cache]\nram_bytes = \"8MiB\""),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse("[sync]\npoll_interval = \"5s\""),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse("[sync]\nprobe_interval = \"5s\""),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse("[device]\nname = \"a/b\""),
            Err(ConfigError::Invalid { .. })
        ));
        assert!(matches!(
            parse("[[access.rule]]\npath = \"Archive\"\nmode = \"ro\""),
            Err(ConfigError::Invalid { .. })
        ));
    }

    #[test]
    fn mountpoint_expansion() {
        let home = Path::new("/home/u");
        assert_eq!(
            Config::default().mountpoint(home),
            Path::new("/home/u/ProtonDrive")
        );
        let absolute = Config {
            mountpoint: Some("/mnt/proton".into()),
            ..Config::default()
        };
        assert_eq!(absolute.mountpoint(home), Path::new("/mnt/proton"));
    }

    #[test]
    fn remote_paths_compile_or_are_a_config_error() {
        let mut config = Config::default();
        config.fsync.remote_paths = vec!["/important/**".into(), "**/*.db".into()];
        let matcher = config.fsync.remote_matcher().expect("globs");
        assert!(matcher.is_match("/important/a/b"));
        assert!(matcher.is_match("/x/y.db"));
        assert!(!matcher.is_match("/other/a"));
        assert!(config.validate().is_ok());
        config.fsync.remote_paths = vec!["/a[".into()];
        let error = config.validate().expect_err("an invalid glob");
        assert!(error.contains("fsync.remote_paths"), "{error}");
    }

    #[test]
    fn serialized_defaults_parse_back() {
        let text = toml::to_string(&Config::default()).expect("serialize");
        assert_eq!(
            toml::from_str::<Config>(&text).expect("parse"),
            Config::default()
        );
    }

    #[test]
    fn further_roots_take_a_kind_and_a_mountpoint() {
        // My Files needs no [[roots]].
        assert!(parse("").expect("defaults").roots.is_empty());
        let config = parse(
            r#"
[[roots]]
kind = "test"
mountpoint = "~/ProtonDrive-Test"
read_only = true

[[roots.tree]]
name = "Team"
volume = "vol-1"
root = "link-2"
writable = true

[[roots.tree]]
name = "Laptop"
volume = "fake"
root = "link-3"
"#,
        )
        .expect("valid");
        assert_eq!(config.roots.len(), 1);
        let root = &config.roots[0];
        assert_eq!(root.kind, RootKind::Test);
        assert!(root.read_only);
        assert_eq!(root.tree.len(), 2);
        assert!(root.tree[0].writable && !root.tree[1].writable);
        assert_eq!(
            config.root_mountpoints(Path::new("/home/u")),
            [PathBuf::from("/home/u/ProtonDrive-Test")]
        );
    }

    #[test]
    fn a_shared_root_takes_a_mountpoint_only() {
        let config = parse("[[roots]]\nkind = \"shared\"\nmountpoint = \"~/ProtonDrive-Shared\"\n")
            .expect("valid");
        assert_eq!(config.roots[0].kind, RootKind::Shared);
        assert!(config.roots[0].tree.is_empty());
        assert!(!config.roots[0].pending_marker, "opt-in");
        let config = parse(
            "[[roots]]\nkind = \"shared\"\nmountpoint = \"~/ProtonDrive-Shared\"\npending_marker = true\n",
        )
        .expect("valid");
        assert!(config.roots[0].pending_marker);
    }

    #[test]
    fn a_photos_root_takes_a_mountpoint_only_and_comes_once() {
        let config =
            parse("[[roots]]\nkind = \"photos\"\nmountpoint = \"~/Photos\"\n").expect("valid");
        assert_eq!(config.roots[0].kind, RootKind::Photos);
        for text in [
            "[[roots]]\nkind = \"photos\"\nmountpoint = \"~/x\"\n[[roots]]\nkind = \"photos\"\nmountpoint = \"~/y\"\n",
            "[[roots]]\nkind = \"photos\"\nmountpoint = \"~/x\"\n[[roots.tree]]\nname = \"a\"\nvolume = \"v\"\nroot = \"r\"\n",
        ] {
            assert!(parse(text).is_err(), "{text}");
        }
    }

    #[test]
    fn roots_that_do_not_exist_yet_are_refused_clearly() {
        let kind = "computers";
        let error = parse(&format!(
            "[[roots]]\nkind = \"{kind}\"\nmountpoint = \"~/x\"\n"
        ))
        .expect_err(kind)
        .to_string();
        assert!(error.contains("not available yet"), "{error}");
        let invalid = [
            "[[roots]]\nkind = \"trash\"\nmountpoint = \"~/x\"\n",
            "[[roots]]\nkind = \"test\"\nmountpoint = \"~/x\"\n",
            "[[roots]]\nkind = \"test\"\nmountpoint = \"x\"\n[[roots.tree]]\nname = \"a\"\nvolume = \"v\"\nroot = \"r\"\n",
            "[[roots]]\nkind = \"test\"\nmountpoint = \"~/x\"\n[[roots.tree]]\nname = \"a/b\"\nvolume = \"v\"\nroot = \"r\"\n",
            "[[roots]]\nkind = \"test\"\nmountpoint = \"~/x\"\n[[roots.tree]]\nname = \"a\"\nvolume = \"v\"\nroot = \"r\"\n[[roots.tree]]\nname = \"a\"\nvolume = \"v\"\nroot = \"s\"\n",
            "[[roots]]\nkind = \"test\"\nmountpoint = \"~/x\"\n[[roots.tree]]\nname = \"a\"\nvolume = \"v\"\nroot = \"r\"\n[[roots]]\nkind = \"test\"\nmountpoint = \"~/x\"\n[[roots.tree]]\nname = \"b\"\nvolume = \"v\"\nroot = \"r\"\n",
            // Shared takes no trees, and comes once.
            "[[roots]]\nkind = \"shared\"\nmountpoint = \"~/x\"\n[[roots.tree]]\nname = \"a\"\nvolume = \"v\"\nroot = \"r\"\n",
            "[[roots]]\nkind = \"shared\"\nmountpoint = \"~/x\"\n[[roots]]\nkind = \"shared\"\nmountpoint = \"~/y\"\n",
        ];
        for text in invalid {
            assert!(parse(text).is_err(), "{text}");
        }
    }
}
