//! Device identity (DESIGN.md §10.2): a name for conflict copies and a
//! stable random id that becomes the SDK client UID (`pdmount-<id>`), which
//! the server records as `ClientUID` on drafts.

use std::io::Write;
use std::path::Path;

pub use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub id: Uuid,
    pub name: String,
}

impl DeviceIdentity {
    /// SDK client UID derived from the device id.
    pub fn client_uid(&self) -> String {
        format!("pdmount-{}", self.id)
    }
}

/// Reads the device id from `path`, creating it on first use. Creation is
/// atomic (`create_new`), so concurrent first runs agree on one id.
pub fn load_or_create_device_id(path: &Path) -> std::io::Result<uuid::Uuid> {
    match read_device_id(path) {
        Ok(id) => return Ok(id),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        Err(_) => {}
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let id = uuid::Uuid::new_v4();
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => {
            writeln!(file, "{id}")?;
            file.sync_all()?;
            Ok(id)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => read_device_id(path),
        Err(error) => Err(error),
    }
}

fn read_device_id(path: &Path) -> std::io::Result<uuid::Uuid> {
    let text = std::fs::read_to_string(path)?;
    uuid::Uuid::parse_str(text.trim()).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("invalid device id in {}: {error}", path.display()),
        )
    })
}

/// Device name: the configured one, else the host name, else `device`.
pub fn device_name(configured: Option<&str>) -> String {
    configured
        .map(str::to_owned)
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .map(|name| name.trim().to_owned())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "device".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_is_created_once_and_then_reused() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("sub/device-id");
        let first = load_or_create_device_id(&path).expect("create");
        let second = load_or_create_device_id(&path).expect("read");
        assert_eq!(first, second);
        let identity = DeviceIdentity {
            id: first,
            name: "laptop".into(),
        };
        assert_eq!(identity.client_uid(), format!("pdmount-{first}"));
    }

    #[test]
    fn corrupt_device_id_is_an_error_not_a_new_identity() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("device-id");
        std::fs::write(&path, "not-a-uuid").expect("write");
        assert_eq!(
            load_or_create_device_id(&path).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn configured_device_name_wins() {
        assert_eq!(device_name(Some("laptop")), "laptop");
        assert!(!device_name(None).is_empty());
    }
}
