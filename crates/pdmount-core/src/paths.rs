//! Profiles and their on-disk locations (DESIGN.md §13.1).

use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;

/// Name of a profile: one account and one mount. Several profiles can run
/// side by side, e.g. two logins acting as two devices in tests.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Profile(String);

impl Profile {
    pub const DEFAULT: &'static str = "default";
    const MAX_LEN: usize = 64;

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for Profile {
    fn default() -> Self {
        Self(Self::DEFAULT.to_owned())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "invalid profile name {0:?}: use 1-64 characters from a-z, 0-9, '-' and '_', starting with a letter or digit"
)]
pub struct InvalidProfileName(String);

impl FromStr for Profile {
    type Err = InvalidProfileName;

    /// Profile names become file and keyring names, so only a safe subset of
    /// ASCII is accepted.
    fn from_str(name: &str) -> Result<Self, Self::Err> {
        let valid_char = |c: char| c.is_ascii_lowercase() || c.is_ascii_digit() || "-_".contains(c);
        let valid = !name.is_empty()
            && name.len() <= Self::MAX_LEN
            && name.chars().all(valid_char)
            && name.starts_with(|c: char| c.is_ascii_alphanumeric());
        if valid {
            Ok(Self(name.to_owned()))
        } else {
            Err(InvalidProfileName(name.to_owned()))
        }
    }
}

impl fmt::Display for Profile {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// XDG base directories the profile paths are derived from.
#[derive(Debug, Clone)]
pub struct BaseDirs {
    pub config: PathBuf,
    pub data: PathBuf,
    pub cache: PathBuf,
    pub state: PathBuf,
    /// `$XDG_RUNTIME_DIR`; `None` when the session has none.
    pub runtime: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("cannot determine the {0} directory (is $HOME set?)")]
pub struct MissingBaseDir(&'static str);

impl BaseDirs {
    /// Base directories of the current user.
    pub fn for_current_user() -> Result<Self, MissingBaseDir> {
        let state = dirs::state_dir()
            .or_else(|| dirs::home_dir().map(|home| home.join(".local/state")))
            .ok_or(MissingBaseDir("state"))?;
        Ok(Self {
            config: dirs::config_dir().ok_or(MissingBaseDir("config"))?,
            data: dirs::data_dir().ok_or(MissingBaseDir("data"))?,
            cache: dirs::cache_dir().ok_or(MissingBaseDir("cache"))?,
            state,
            runtime: dirs::runtime_dir(),
        })
    }
}

/// Every file and directory belonging to one profile.
#[derive(Debug, Clone)]
pub struct ProfilePaths {
    pub profile: Profile,
    /// `~/.config/pdmount/`
    pub config_dir: PathBuf,
    /// `~/.local/share/pdmount/<profile>/`: state that must survive cache
    /// clean-ups (credentials fallback, SDK secrets, device identity).
    pub data_dir: PathBuf,
    /// `~/.cache/pdmount/<profile>/`: rebuildable data.
    pub cache_dir: PathBuf,
    /// `~/.local/state/pdmount/`
    pub state_dir: PathBuf,
    /// `$XDG_RUNTIME_DIR/pdmount/`
    pub runtime_dir: Option<PathBuf>,
}

const APP_DIR: &str = "pdmount";

/// A SQLite database and its WAL side files.
pub fn sqlite_files(db: &Path) -> [PathBuf; 3] {
    ["", "-wal", "-shm"].map(|suffix| {
        let mut path = db.as_os_str().to_owned();
        path.push(suffix);
        PathBuf::from(path)
    })
}

impl ProfilePaths {
    pub fn new(profile: Profile, base: &BaseDirs) -> Self {
        Self {
            config_dir: base.config.join(APP_DIR),
            data_dir: base.data.join(APP_DIR).join(profile.as_str()),
            cache_dir: base.cache.join(APP_DIR).join(profile.as_str()),
            state_dir: base.state.join(APP_DIR),
            runtime_dir: base.runtime.as_ref().map(|dir| dir.join(APP_DIR)),
            profile,
        }
    }

    pub fn for_current_user(profile: Profile) -> Result<Self, MissingBaseDir> {
        Ok(Self::new(profile, &BaseDirs::for_current_user()?))
    }

    fn named(&self, dir: &Path, suffix: &str) -> PathBuf {
        dir.join(format!("{}{suffix}", self.profile))
    }

    /// `~/.config/pdmount/<profile>.toml`
    pub fn config_file(&self) -> PathBuf {
        self.named(&self.config_dir, ".toml")
    }

    /// `~/.config/pdmount/<profile>.pdignore`
    pub fn local_ignore_file(&self) -> PathBuf {
        self.named(&self.config_dir, ".pdignore")
    }

    /// Session credentials when no keyring is available (mode 0600).
    pub fn credentials_file(&self) -> PathBuf {
        self.data_dir.join("credentials.json")
    }

    /// Cache master key when no keyring is available (mode 0600).
    pub fn cache_key_file(&self) -> PathBuf {
        self.data_dir.join("cache.key")
    }

    /// Device identity (DESIGN.md §10.2).
    pub fn device_id_file(&self) -> PathBuf {
        self.data_dir.join("device-id")
    }

    /// Volume, root folder and account of the drive as last seen online, so
    /// a mount without a connection needs no request (§9.6, §13.1).
    pub fn proton_root_file(&self) -> PathBuf {
        self.data_dir.join("proton-root.json")
    }

    /// Encrypted SDK secret cache. Losing it requires a new login, so it
    /// lives in the data directory.
    pub fn sdk_secret_cache(&self) -> PathBuf {
        self.data_dir.join("sdk-secrets.db")
    }

    /// Encrypted account key passphrases, kept out of the secret cache's
    /// eviction (D085). Losing it requires a new login.
    pub fn sdk_key_store(&self) -> PathBuf {
        self.data_dir.join("sdk-keys.db")
    }

    /// Encrypted SDK entity cache (rebuildable).
    pub fn sdk_entity_cache(&self) -> PathBuf {
        self.cache_dir.join("sdk-entities.db")
    }

    /// SQLite store of the filesystem state (§13.2).
    pub fn state_db(&self) -> PathBuf {
        self.data_dir.join("state.db")
    }

    /// Encrypted block cache (§6.2).
    pub fn blocks_dir(&self) -> PathBuf {
        self.cache_dir.join("blocks")
    }

    /// Staging files of dirty inodes (§7.1).
    pub fn staging_dir(&self) -> PathBuf {
        self.data_dir.join("staging")
    }

    /// `~/.local/state/pdmount/<profile>.log`
    pub fn log_file(&self) -> PathBuf {
        self.named(&self.state_dir, ".log")
    }

    /// `$XDG_RUNTIME_DIR/pdmount/<profile>.sock`, or `control.sock` in the
    /// profile's data directory when the session has no runtime directory.
    pub fn control_socket(&self) -> PathBuf {
        match &self.runtime_dir {
            Some(dir) => self.named(dir, ".sock"),
            None => self.data_dir.join("control.sock"),
        }
    }

    /// Process id of the running daemon, next to the control socket.
    pub fn pid_file(&self) -> PathBuf {
        self.control_socket().with_extension("pid")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BaseDirs {
        BaseDirs {
            config: "/home/u/.config".into(),
            data: "/home/u/.local/share".into(),
            cache: "/home/u/.cache".into(),
            state: "/home/u/.local/state".into(),
            runtime: Some("/run/user/1000".into()),
        }
    }

    #[test]
    fn profile_names_are_restricted_to_safe_characters() {
        for valid in ["default", "work", "laptop-2", "a_b", "0"] {
            assert!(valid.parse::<Profile>().is_ok(), "{valid}");
        }
        for invalid in [
            "",
            "Work",
            "../x",
            "a/b",
            "-x",
            "_x",
            "a b",
            &"x".repeat(65),
        ] {
            assert!(invalid.parse::<Profile>().is_err(), "{invalid}");
        }
        assert_eq!(Profile::default().as_str(), "default");
    }

    #[test]
    fn profile_paths_follow_the_design_layout() {
        let paths = ProfilePaths::new("work".parse().unwrap(), &base());
        assert_eq!(
            paths.config_file(),
            Path::new("/home/u/.config/pdmount/work.toml")
        );
        assert_eq!(
            paths.local_ignore_file(),
            Path::new("/home/u/.config/pdmount/work.pdignore")
        );
        assert_eq!(
            paths.credentials_file(),
            Path::new("/home/u/.local/share/pdmount/work/credentials.json")
        );
        assert_eq!(
            paths.sdk_secret_cache(),
            Path::new("/home/u/.local/share/pdmount/work/sdk-secrets.db")
        );
        assert_eq!(
            paths.sdk_key_store(),
            Path::new("/home/u/.local/share/pdmount/work/sdk-keys.db")
        );
        assert_eq!(
            paths.sdk_entity_cache(),
            Path::new("/home/u/.cache/pdmount/work/sdk-entities.db")
        );
        assert_eq!(
            paths.log_file(),
            Path::new("/home/u/.local/state/pdmount/work.log")
        );
        assert_eq!(
            paths.control_socket(),
            PathBuf::from("/run/user/1000/pdmount/work.sock")
        );
        assert_eq!(
            paths.pid_file(),
            PathBuf::from("/run/user/1000/pdmount/work.pid")
        );
    }

    #[test]
    fn control_socket_falls_back_to_the_data_dir() {
        let base = BaseDirs {
            runtime: None,
            ..base()
        };
        assert_eq!(
            ProfilePaths::new(Profile::default(), &base).control_socket(),
            PathBuf::from("/home/u/.local/share/pdmount/default/control.sock")
        );
    }
}
