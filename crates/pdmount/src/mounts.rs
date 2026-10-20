//! Mount table lookups (DESIGN.md §14.4). The mount table is read from
//! `/proc/self/mountinfo` instead of calling `stat` on the mountpoint, which
//! hangs or fails with ENOTCONN on a dead FUSE mount.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MountEntry {
    pub mount_point: PathBuf,
    pub fs_type: String,
    /// Device minor number: the id under `/sys/fs/fuse/connections/`.
    pub minor: u32,
}

impl MountEntry {
    pub fn is_pdmount(&self) -> bool {
        self.fs_type == "fuse.pdmount"
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
    let fs_type = fields.skip_while(|field| *field != "-").nth(1)?;
    let minor = device.split_once(':')?.1.parse().ok()?;
    Some(MountEntry {
        mount_point: PathBuf::from(mount_point),
        fs_type: fs_type.to_owned(),
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
                minor: 58,
            })
        );
    }

    #[test]
    fn lines_without_optional_fields_parse() {
        let line = "22 1 8:3 / / rw,relatime - btrfs /dev/sdX1 rw";
        let entry = parse_line(line).expect("parse");
        assert_eq!(entry.fs_type, "btrfs");
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
