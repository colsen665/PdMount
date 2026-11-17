//! Mount table lookups (DESIGN.md §14.4). The mount table is read from
//! `/proc/self/mountinfo` instead of calling `stat` on the mountpoint, which
//! hangs or fails with ENOTCONN on a dead FUSE mount; [`is_dead`] asks the
//! mount itself, bounded.

use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fs_type: String,
    /// The source field (`fsname`): `pdmount:<profile>` for a PdMount mount
    /// since D127, `pdmount` before.
    pub source: String,
    /// Device minor number: the id under `/sys/fs/fuse/connections/`.
    pub minor: u32,
}

/// How long [`is_dead`] waits for a mount's answer: a live daemon that hangs
/// makes `statfs` wait, and such a mount counts as live.
pub const DEAD_CHECK: Duration = Duration::from_secs(2);

impl MountEntry {
    pub fn is_pdmount(&self) -> bool {
        self.fs_type == "fuse.pdmount"
    }

    /// The profile a PdMount mount names in its source (`pdmount:<profile>`,
    /// D127); `None` for another mount or one of a version before D127.
    pub fn profile(&self) -> Option<&str> {
        self.source
            .strip_prefix("pdmount:")
            .filter(|_| self.is_pdmount())
    }

    pub fn connection_dir(&self) -> PathBuf {
        PathBuf::from(format!("/sys/fs/fuse/connections/{}", self.minor))
    }
}

/// Resolves symlinks in the parent only, so a dead mount is never touched.
pub fn normalize(path: &Path) -> PathBuf {
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => parent
            .canonicalize()
            .map_or_else(|_| path.to_owned(), |parent| parent.join(name)),
        _ => path.to_owned(),
    }
}

/// Whether the FUSE connection of the mount at `mountpoint` is gone: its
/// daemon exited or was killed, so `statfs` answers ENOTCONN (ECONNABORTED
/// after an abort). Only such a mount is stale (D127). A mount that answers,
/// fails otherwise, or does not answer within [`DEAD_CHECK`] (a hung daemon)
/// is live. `statfs` always asks the daemon, where `stat` may answer from the
/// kernel's attribute cache. A call that does not return leaves its thread.
pub fn is_dead(mountpoint: &Path) -> bool {
    let (send, receive) = std::sync::mpsc::channel();
    let target = mountpoint.to_owned();
    let spawned = std::thread::Builder::new()
        .name("pdmount-statfs".into())
        .spawn(move || {
            let _ = send.send(nix::sys::statfs::statfs(&target).map(drop));
        });
    spawned.is_ok()
        && matches!(
            receive.recv_timeout(DEAD_CHECK),
            Ok(Err(
                nix::errno::Errno::ENOTCONN | nix::errno::Errno::ECONNABORTED
            ))
        )
}

/// The mount at exactly `mountpoint`, if any (the last one wins when mounts
/// are stacked).
pub fn find(mountpoint: &Path) -> std::io::Result<Option<MountEntry>> {
    let table = std::fs::read_to_string("/proc/self/mountinfo")?;
    let wanted = normalize(mountpoint);
    Ok(table
        .lines()
        .filter_map(parse_line)
        .rfind(|entry| entry.mount_point == wanted))
}

/// Parses one mountinfo line:
/// `id parent major:minor root mount_point options [optional...] - fstype source super_options`.
fn parse_line(line: &str) -> Option<MountEntry> {
    let mut fields = line.split(' ');
    let _id = fields.next()?;
    let _parent = fields.next()?;
    let device = fields.next()?;
    let _root = fields.next()?;
    let mount_point = unescape(fields.next()?);
    let mut fields = fields.skip_while(|field| *field != "-").skip(1);
    let fs_type = fields.next()?;
    let source = fields.next().map(unescape).unwrap_or_default();
    let minor = device.split_once(':')?.1.parse().ok()?;
    Some(MountEntry {
        mount_point: PathBuf::from(mount_point),
        fs_type: fs_type.to_owned(),
        source,
        minor,
    })
}

/// Undoes the octal escapes (`\040` for a space) used in mountinfo.
fn unescape(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let digits = bytes.get(i + 1..i + 4);
        if bytes[i] == b'\\'
            && let Some(digits) = digits
            && digits.iter().all(|d| (b'0'..=b'7').contains(d))
        {
            let value = digits
                .iter()
                .fold(0u32, |acc, d| acc * 8 + u32::from(d - b'0'));
            out.push(value as u8);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_fuse_lines_with_escaped_paths() {
        let line = "75 30 0:58 / /home/u/Proton\\040Drive rw,nosuid,nodev,relatime shared:40 - fuse.pdmount pdmount ro,user_id=1000";
        assert_eq!(
            parse_line(line),
            Some(MountEntry {
                mount_point: PathBuf::from("/home/u/Proton Drive"),
                fs_type: "fuse.pdmount".into(),
                source: "pdmount".into(),
                minor: 58,
            })
        );
        // A mount of a version before D127 names no profile.
        assert_eq!(parse_line(line).expect("parse").profile(), None);
    }

    /// Since D127 the source names the profile (`fsname=pdmount:<profile>`).
    #[test]
    fn the_source_names_the_profile() {
        let line = "75 30 0:58 / /m rw - fuse.pdmount pdmount:work rw,user_id=1000";
        let entry = parse_line(line).expect("parse");
        assert_eq!(entry.source, "pdmount:work");
        assert_eq!(entry.profile(), Some("work"));
        let other = "75 30 0:58 / /m rw - fuse.sshfs pdmount:work rw";
        assert_eq!(parse_line(other).expect("parse").profile(), None);
    }

    /// A path that is no mount answers `statfs`: not dead. A dead FUSE
    /// mount is checked in `tests/scenarios/svc.sh`.
    #[test]
    fn a_plain_directory_is_not_dead() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_dead(dir.path()));
        assert!(!is_dead(&dir.path().join("missing")));
    }

    #[test]
    fn lines_without_optional_fields_parse() {
        let line = "22 1 8:3 / / rw,relatime - btrfs /dev/sdX1 rw";
        let entry = parse_line(line).expect("parse");
        assert_eq!(entry.fs_type, "btrfs");
        assert_eq!(entry.source, "/dev/sdX1");
        assert!(!entry.is_pdmount());
        assert_eq!(
            entry.connection_dir(),
            PathBuf::from("/sys/fs/fuse/connections/3")
        );
    }

    #[test]
    fn the_root_filesystem_is_found() {
        assert!(find(Path::new("/")).expect("mountinfo").is_some());
    }

    #[test]
    fn unescape_leaves_plain_text_alone() {
        assert_eq!(unescape("a\\040b\\134c"), "a b\\c");
        assert_eq!(unescape("trailing\\"), "trailing\\");
    }
}
