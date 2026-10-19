//! `pdmount status` (DESIGN.md §14.2). M0 reports the profile, session,
//! account, quota and device; mount state is added with the daemon in M1.

use crate::credentials::Storage;
use crate::session::{self, Resumed};
use pdmount_backend::proton::ProtonBackend;
use pdmount_backend::{Backend, StorageInfo};
use pdmount_core::device::{self, DeviceIdentity};
use pdmount_core::units::ByteSize;
use pdmount_core::{Config, ProfilePaths};
use proton_drive_sdk::client::ProtonDriveClient;
use std::fmt;

pub struct StatusReport {
    pub profile: String,
    pub session: SessionState,
    pub device: DeviceIdentity,
    pub mountpoint: std::path::PathBuf,
}

pub enum SessionState {
    NotLoggedIn,
    NeedsLogin(String),
    Active {
        username: String,
        storage: Storage,
        quota: StorageInfo,
    },
}

impl SessionState {
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active { .. })
    }
}

fn percent(used: u64, max: u64) -> f64 {
    if max == 0 {
        0.0
    } else {
        used as f64 * 100.0 / max as f64
    }
}

impl fmt::Display for StatusReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "Profile:    {}", self.profile)?;
        match &self.session {
            SessionState::NotLoggedIn => {
                writeln!(f, "Session:    not logged in (run `pdmount login`)")?;
            }
            SessionState::NeedsLogin(reason) => {
                writeln!(f, "Session:    expired, run `pdmount login` ({reason})")?;
            }
            SessionState::Active {
                username,
                storage,
                quota,
            } => {
                writeln!(f, "Account:    {username}")?;
                writeln!(f, "Session:    active (stored in {storage})")?;
                writeln!(
                    f,
                    "Storage:    {} used of {} ({:.1} %)",
                    ByteSize(quota.used).human(),
                    ByteSize(quota.max).human(),
                    percent(quota.used, quota.max)
                )?;
            }
        }
        writeln!(f, "Device:     {} ({})", self.device.name, self.device.id)?;
        writeln!(f, "Mountpoint: {}", self.mountpoint.display())?;
        write!(f, "Mount:      not available yet (milestone M1)")
    }
}

pub async fn status(
    paths: &ProfilePaths,
    config: &Config,
    home: &std::path::Path,
) -> anyhow::Result<StatusReport> {
    let device = DeviceIdentity {
        id: device::load_or_create_device_id(&paths.device_id_file())?,
        name: device::device_name(config.device.name.as_deref()),
    };
    let session = match session::resume(paths).await? {
        Resumed::NotLoggedIn => SessionState::NotLoggedIn,
        Resumed::NeedsLogin(error) => SessionState::NeedsLogin(format!("{error:#}")),
        Resumed::Active(logged_in) => {
            let username = logged_in.session.username.clone();
            let client = ProtonDriveClient::new(&logged_in.session, Some(device.client_uid()))?;
            let backend = ProtonBackend::new(client, username.clone()).await?;
            SessionState::Active {
                username: backend.account().await?.username,
                storage: logged_in.storage,
                quota: backend.storage_info().await?,
            }
        }
    };
    Ok(StatusReport {
        profile: paths.profile.to_string(),
        session,
        device,
        mountpoint: config.mountpoint(home),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(session: SessionState) -> String {
        StatusReport {
            profile: "default".into(),
            session,
            device: DeviceIdentity {
                id: pdmount_core::device::Uuid::nil(),
                name: "laptop".into(),
            },
            mountpoint: "/home/u/ProtonDrive".into(),
        }
        .to_string()
    }

    #[test]
    fn active_report_shows_account_and_quota() {
        let text = report(SessionState::Active {
            username: "user@proton.me".into(),
            storage: Storage::Keyring,
            quota: StorageInfo {
                used: 3 << 30,
                max: 500 << 30,
            },
        });
        assert!(text.contains("Account:    user@proton.me"));
        assert!(text.contains("Storage:    3.0 GiB used of 500.0 GiB (0.6 %)"));
        assert!(text.contains("stored in keyring"));
        assert!(text.contains("Device:     laptop"));
    }

    #[test]
    fn logged_out_report_tells_how_to_log_in() {
        assert!(report(SessionState::NotLoggedIn).contains("run `pdmount login`"));
        assert!(report(SessionState::NeedsLogin("expired".into())).contains("expired"));
    }

    #[test]
    fn percent_handles_zero_quota() {
        assert_eq!(percent(1, 0), 0.0);
        assert_eq!(percent(1, 4), 25.0);
    }
}
