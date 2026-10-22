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
    pub upload: UploadConfig,
    pub fsync: FsyncConfig,
    pub kernel: KernelConfig,
    pub sync: SyncConfig,
    pub timeouts: TimeoutConfig,
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
            ram_bytes: ByteSize(256 << 20),
            prefetch_small_files: ByteSize(256 << 10),
            prefetch_dir_budget: ByteSize(16 << 20),
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
    /// Globs that use `remote` fsync regardless of `mode`.
    pub remote_paths: Vec<String>,
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
    pub writeback_cache: bool,
}

impl Default for KernelConfig {
    fn default() -> Self {
        Self {
            attr_ttl: HumanDuration(Duration::from_secs(60)),
            entry_ttl: HumanDuration(Duration::from_secs(60)),
            negative_ttl: HumanDuration(Duration::from_secs(10)),
            writeback_cache: false,
        }
    }
}

/// The official clients poll their own volume every 30 s
/// (`cs/client/js/src/internal/events/eventScheduler.ts:1`).
const MIN_POLL_INTERVAL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SyncConfig {
    /// Event polling interval; never below the official clients' 30 s
    /// (D033).
    pub poll_interval: HumanDuration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        Self {
            poll_interval: HumanDuration(MIN_POLL_INTERVAL),
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
            shutdown_grace: HumanDuration(Duration::from_secs(30)),
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
        if self.upload.concurrency == 0 {
            return Err("upload.concurrency must be at least 1".into());
        }
        if self.sync.poll_interval.0 < MIN_POLL_INTERVAL {
            return Err("sync.poll_interval must be at least 30s".into());
        }
        if self.device.name.as_deref().is_some_and(|name| {
            name.trim().is_empty() || name.contains(['/', '(', ')']) || name.len() > 64
        }) {
            return Err("device.name must be 1-64 characters without '/', '(' or ')'".into());
        }
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

    /// Mount point with `~/` expanded against `home`.
    pub fn mountpoint(&self, home: &Path) -> PathBuf {
        let configured = self
            .mountpoint
            .as_deref()
            .unwrap_or(Self::DEFAULT_MOUNTPOINT);
        match configured.strip_prefix("~/") {
            Some(rest) => home.join(rest),
            None if configured == "~" => home.to_owned(),
            None => PathBuf::from(configured),
        }
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
writeback_cache = true

[sync]
poll_interval = "1m"

[timeouts]
metadata = "30s"
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
        assert_eq!(config.cache.ram_bytes, ByteSize(256 << 20));
        assert_eq!(config.upload.concurrency, 2);
        assert_eq!(config.upload.debounce.0, Duration::from_millis(500));
        assert_eq!(
            config.upload.meta_revision,
            MetaRevisionPolicy::MaxSize(ByteSize(64 << 20))
        );
        assert_eq!(config.fsync.mode, FsyncMode::Remote);
        assert!(config.kernel.writeback_cache);
        assert_eq!(config.sync.poll_interval.0, Duration::from_secs(60));
        assert_eq!(config.timeouts.metadata.0, Duration::from_secs(30));
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
        assert!(matches!(
            parse("[sync]\npoll_interval = \"5s\""),
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
    fn serialized_defaults_parse_back() {
        let text = toml::to_string(&Config::default()).expect("serialize");
        assert_eq!(
            toml::from_str::<Config>(&text).expect("parse"),
            Config::default()
        );
    }
}
