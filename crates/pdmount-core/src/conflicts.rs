//! Conflict naming (DESIGN.md §10.7) and the conflict registry (§10.8).
//!
//! [`conflict_name`] is a pure function from the original name, the device
//! that owns the copied content, a time and a collision attempt to the name
//! of the conflict copy. [`Conflicts`] is the in-memory view of the
//! `conflicts` table, loaded at mount like the access rules.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, Utc};
use pdmount_backend::{LinkId, RevisionId};
use rusqlite::{Connection, params};

use crate::vfs::{Ino, MAX_NAME_BYTES as NAME_MAX};

/// The longest device label kept in a conflict name, in bytes (cut at a
/// character boundary), so the name keeps room for its stem within NAME_MAX.
const DEVICE_MAX_BYTES: usize = 64;

/// The time source for conflict names; tests fix it.
pub type Clock = Arc<dyn Fn() -> SystemTime + Send + Sync>;

/// The wall clock.
pub fn system_clock() -> Clock {
    Arc::new(SystemTime::now)
}

/// Names conflict copies on this device at the clock's time.
#[derive(Clone)]
pub struct Namer {
    pub device: String,
    pub clock: Clock,
}

impl Namer {
    /// [`conflict_name`] for this device, now.
    pub fn name(&self, name: &str, attempt: u32) -> String {
        conflict_name(name, &self.device, (self.clock)(), attempt)
    }
}

impl fmt::Debug for Namer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Namer")
            .field("device", &self.device)
            .finish_non_exhaustive()
    }
}

impl Default for Namer {
    fn default() -> Self {
        Self {
            device: "pdmount".to_string(),
            clock: Arc::new(|| UNIX_EPOCH),
        }
    }
}

/// The name of a conflict copy of `name`: `<stem> (conflict <device> <time> UTC[ <n>])<.ext>`.
///
/// `attempt` is the collision counter: 0 and 1 add nothing, `n >= 2` adds ` n` inside the parentheses.
/// The stem is cut at a char boundary so the result stays within 255 bytes.
pub fn conflict_name(name: &str, device: &str, at: SystemTime, attempt: u32) -> String {
    let at = DateTime::<Utc>::from(at);
    let number = if attempt <= 1 {
        String::new()
    } else {
        format!(" {attempt}")
    };
    let suffix = format!(
        " (conflict {} {} UTC{number})",
        sanitize_device(device),
        at.format("%Y-%m-%d %H-%M-%S")
    );

    let (mut stem, mut ext) = split_extension(name);
    if ext.len() + suffix.len() > NAME_MAX - 1 {
        stem = name;
        ext = "";
    }
    let room = NAME_MAX.saturating_sub(suffix.len() + ext.len());
    let mut cut = 0;
    for (index, c) in stem.char_indices() {
        let end = index + c.len_utf8();
        if end > room {
            break;
        }
        cut = end;
    }
    format!("{}{suffix}{ext}", &stem[..cut])
}

/// Splits at the last `.` that is not the first byte: `(stem, ext including the dot)`.
fn split_extension(name: &str) -> (&str, &str) {
    match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    }
}

/// A device label that is safe inside a name: `/`, parentheses and control characters become `_`,
/// an empty label becomes `remote`, and a long one is cut.
fn sanitize_device(device: &str) -> String {
    let mut clean = String::new();
    for c in device.chars() {
        let c = if matches!(c, '/' | '(' | ')') || c.is_control() {
            '_'
        } else {
            c
        };
        if clean.len() + c.len_utf8() > DEVICE_MAX_BYTES {
            break;
        }
        clean.push(c);
    }
    if clean.is_empty() {
        "remote".to_string()
    } else {
        clean
    }
}

/// What kind of conflict was found (§10.1–§10.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictKind {
    EditEdit,
    CreateCreate,
    NameTaken,
    RemoteDeleteLocalEdit,
    RemoteDeleteLocalRename,
    LocalDeleteRemoteEdit,
    LocalDeleteRemoteChildren,
    RenameRename,
    MoveCycle,
}

impl ConflictKind {
    pub const ALL: [ConflictKind; 9] = [
        Self::EditEdit,
        Self::CreateCreate,
        Self::NameTaken,
        Self::RemoteDeleteLocalEdit,
        Self::RemoteDeleteLocalRename,
        Self::LocalDeleteRemoteEdit,
        Self::LocalDeleteRemoteChildren,
        Self::RenameRename,
        Self::MoveCycle,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::EditEdit => "edit_edit",
            Self::CreateCreate => "create_create",
            Self::NameTaken => "name_taken",
            Self::RemoteDeleteLocalEdit => "remote_delete_local_edit",
            Self::RemoteDeleteLocalRename => "remote_delete_local_rename",
            Self::LocalDeleteRemoteEdit => "local_delete_remote_edit",
            Self::LocalDeleteRemoteChildren => "local_delete_remote_children",
            Self::RenameRename => "rename_rename",
            Self::MoveCycle => "move_cycle",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.as_str() == text)
    }

    /// The state a new conflict of this kind starts in: `Open` when the user
    /// has something to review, `Notice` when the engine settled it.
    pub fn initial_state(self) -> ConflictState {
        match self {
            Self::EditEdit | Self::CreateCreate | Self::NameTaken | Self::LocalDeleteRemoteEdit => {
                ConflictState::Open
            }
            Self::RemoteDeleteLocalEdit
            | Self::RemoteDeleteLocalRename
            | Self::LocalDeleteRemoteChildren
            | Self::RenameRename
            | Self::MoveCycle => ConflictState::Notice,
        }
    }
}

impl fmt::Display for ConflictKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictState {
    Open,
    Notice,
    Resolved,
}

impl ConflictState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Notice => "notice",
            Self::Resolved => "resolved",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        [Self::Open, Self::Notice, Self::Resolved]
            .into_iter()
            .find(|state| state.as_str() == text)
    }
}

/// What the tree or the journal found, before the registry numbers it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictNote {
    pub kind: ConflictKind,
    pub ino: Ino,
    pub other_ino: Option<Ino>,
    pub link: Option<LinkId>,
    pub orig_name: String,
    pub conflict_name: Option<String>,
    pub local_rev: Option<RevisionId>,
    pub remote_rev: Option<RevisionId>,
    pub detail: Option<String>,
}

impl ConflictNote {
    /// A note with only what every conflict has; callers add the rest with
    /// struct update syntax.
    pub fn new(kind: ConflictKind, ino: Ino, orig_name: String) -> Self {
        Self {
            kind,
            ino,
            other_ino: None,
            link: None,
            orig_name,
            conflict_name: None,
            local_rev: None,
            remote_rev: None,
            detail: None,
        }
    }
}

/// A registered conflict (one row of the `conflicts` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub id: i64,
    pub kind: ConflictKind,
    pub ino: Ino,
    pub other_ino: Option<Ino>,
    pub link: Option<LinkId>,
    pub orig_name: String,
    pub conflict_name: Option<String>,
    pub local_rev: Option<RevisionId>,
    pub remote_rev: Option<RevisionId>,
    pub local_device: String,
    pub detail: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    pub state: ConflictState,
    /// Unix seconds when the notification service accepted this conflict's
    /// desktop notification (M6.3); `None`: not shown yet.
    pub notified_at: Option<i64>,
}

/// How `conflicts resolve` settles a conflict (§10.8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    KeepBoth,
    KeepLocal,
    KeepRemote,
}

impl Resolution {
    pub fn parse(text: &str) -> Option<Self> {
        Some(match text {
            "keep_both" | "keep-both" => Self::KeepBoth,
            "keep_local" | "keep-local" => Self::KeepLocal,
            "keep_remote" | "keep-remote" => Self::KeepRemote,
            _ => return None,
        })
    }
}

/// A conflict as `conflicts list|show` report it: the registry row plus the
/// current paths of both copies (`None` when a copy is gone or detached).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictView {
    pub id: i64,
    pub kind: ConflictKind,
    pub state: ConflictState,
    pub path: Option<String>,
    pub other_path: Option<String>,
    pub orig_name: String,
    pub conflict_name: Option<String>,
    pub local_rev: Option<RevisionId>,
    pub remote_rev: Option<RevisionId>,
    pub local_device: String,
    /// Unix seconds.
    pub created_at: i64,
    pub detail: Option<String>,
}

/// A conflict that was just registered, for the notification hook (§10.8).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictNotice {
    pub id: i64,
    pub kind: ConflictKind,
    pub path: Option<String>,
}

/// All conflicts of the profile, by id.
#[derive(Debug, Default)]
pub struct Conflicts {
    by_id: BTreeMap<i64, Conflict>,
    next_id: i64,
}

impl Conflicts {
    pub fn load(conn: &Connection) -> rusqlite::Result<Self> {
        let mut statement = conn.prepare(
            "SELECT id, kind, ino, other_ino, link_id, orig_name, conflict_name, local_rev, remote_rev,
                    local_device, detail, created_at, state, notified_at
             FROM conflicts",
        )?;
        let mut by_id = BTreeMap::new();
        let mut last_id = 0;
        let rows = statement.query_map([], |row| {
            let kind: String = row.get(1)?;
            let state: String = row.get(12)?;
            Ok((
                kind,
                state,
                Conflict {
                    id: row.get(0)?,
                    kind: ConflictKind::EditEdit,
                    ino: row.get::<_, i64>(2)? as Ino,
                    other_ino: row.get::<_, Option<i64>>(3)?.map(|ino| ino as Ino),
                    link: row.get::<_, Option<String>>(4)?.map(LinkId),
                    orig_name: row.get::<_, Option<String>>(5)?.unwrap_or_default(),
                    conflict_name: row.get(6)?,
                    local_rev: row.get::<_, Option<String>>(7)?.map(RevisionId),
                    remote_rev: row.get::<_, Option<String>>(8)?.map(RevisionId),
                    local_device: row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                    detail: row.get(10)?,
                    created_at: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                    state: ConflictState::Open,
                    notified_at: row.get(13)?,
                },
            ))
        })?;
        for row in rows {
            let (kind, state, mut conflict) = row?;
            last_id = last_id.max(conflict.id);
            // A row this version cannot read (a newer or retired kind) is
            // kept out of the registry, but its id stays taken.
            let (Some(kind), Some(state)) =
                (ConflictKind::parse(&kind), ConflictState::parse(&state))
            else {
                tracing::warn!(id = conflict.id, kind, state, "conflict row not understood");
                continue;
            };
            conflict.kind = kind;
            conflict.state = state;
            by_id.insert(conflict.id, conflict);
        }
        Ok(Self {
            by_id,
            next_id: last_id + 1,
        })
    }

    /// Registers a conflict found at `at` (unix seconds) on `device`.
    pub fn insert(&mut self, note: ConflictNote, device: &str, at: i64) -> Conflict {
        let id = self.next_id;
        self.next_id += 1;
        let conflict = Conflict {
            id,
            kind: note.kind,
            ino: note.ino,
            other_ino: note.other_ino,
            link: note.link,
            orig_name: note.orig_name,
            conflict_name: note.conflict_name,
            local_rev: note.local_rev,
            remote_rev: note.remote_rev,
            local_device: device.to_string(),
            detail: note.detail,
            created_at: at,
            state: note.kind.initial_state(),
            notified_at: None,
        };
        self.by_id.insert(id, conflict.clone());
        conflict
    }

    /// Returns the conflict as changed, or `None` for an unknown id.
    pub fn set_state(&mut self, id: i64, state: ConflictState) -> Option<&Conflict> {
        let conflict = self.by_id.get_mut(&id)?;
        conflict.state = state;
        Some(conflict)
    }

    /// Names the other copy once it is known (it may show only after the
    /// conflict was recorded).
    pub fn set_other_ino(&mut self, id: i64, other: Ino) -> Option<&Conflict> {
        let conflict = self.by_id.get_mut(&id)?;
        conflict.other_ino = Some(other);
        Some(conflict)
    }

    pub fn get(&self, id: i64) -> Option<&Conflict> {
        self.by_id.get(&id)
    }

    /// Notes that conflict `id` was shown at `at` (unix seconds); false when
    /// it is unknown or was shown before.
    pub fn mark_notified(&mut self, id: i64, at: i64) -> bool {
        match self.by_id.get_mut(&id) {
            Some(conflict) if conflict.notified_at.is_none() => {
                conflict.notified_at = Some(at);
                true
            }
            _ => false,
        }
    }

    /// The ids of the conflicts that wait for the user (`open`) and whose
    /// notification was not shown yet, oldest first.
    pub fn unnotified_open(&self) -> Vec<i64> {
        self.by_id
            .values()
            .filter(|c| c.state == ConflictState::Open && c.notified_at.is_none())
            .map(|c| c.id)
            .collect()
    }

    /// The conflicts that are not resolved and name `ino` as either inode,
    /// or whose link is `link` (a candidate for the other copy).
    pub fn active_for(&self, ino: Ino, link: Option<&LinkId>) -> Vec<&Conflict> {
        self.by_id
            .values()
            .filter(|c| {
                c.state != ConflictState::Resolved
                    && (c.ino == ino
                        || c.other_ino == Some(ino)
                        || link.is_some() && c.link.as_ref() == link)
            })
            .collect()
    }

    /// Conflicts the user has something to review for (state `open`).
    pub fn count_open(&self) -> usize {
        self.by_id
            .values()
            .filter(|c| c.state == ConflictState::Open)
            .count()
    }

    #[cfg(test)]
    pub fn count_active(&self) -> usize {
        self.by_id
            .values()
            .filter(|c| c.state != ConflictState::Resolved)
            .count()
    }

    pub fn all(&self) -> impl Iterator<Item = &Conflict> {
        self.by_id.values()
    }
}

/// Stores a conflict, updating the row with the same id. `notified_at` is
/// written only with a new row: [`store_notified`] changes it later, and a
/// row written from an older copy must not undo that.
pub fn store_conflict(conn: &Connection, conflict: &Conflict) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO conflicts(id, kind, ino, other_ino, link_id, orig_name, conflict_name,
             local_rev, remote_rev, local_device, detail, created_at, state, notified_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)
         ON CONFLICT(id) DO UPDATE SET
             kind = excluded.kind, ino = excluded.ino, other_ino = excluded.other_ino,
             link_id = excluded.link_id, orig_name = excluded.orig_name,
             conflict_name = excluded.conflict_name, local_rev = excluded.local_rev,
             remote_rev = excluded.remote_rev, local_device = excluded.local_device,
             detail = excluded.detail, created_at = excluded.created_at, state = excluded.state",
        params![
            conflict.id,
            conflict.kind.as_str(),
            conflict.ino as i64,
            conflict.other_ino.map(|ino| ino as i64),
            conflict.link.as_ref().map(|link| link.0.as_str()),
            conflict.orig_name,
            conflict.conflict_name,
            conflict.local_rev.as_ref().map(|rev| rev.0.as_str()),
            conflict.remote_rev.as_ref().map(|rev| rev.0.as_str()),
            conflict.local_device,
            conflict.detail,
            conflict.created_at,
            conflict.state.as_str(),
            conflict.notified_at,
        ],
    )
    .map(drop)
}

/// Records that conflict `id` was shown at `at`; a row shown before keeps
/// its time.
pub fn store_notified(conn: &Connection, id: i64, at: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE conflicts SET notified_at = ?2 WHERE id = ?1 AND notified_at IS NULL",
        params![id, at],
    )
    .map(drop)
}

pub fn store_state(conn: &Connection, id: i64, state: ConflictState) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE conflicts SET state = ?2 WHERE id = ?1",
        params![id, state.as_str()],
    )
    .map(drop)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;

    /// 2026-10-19 14:03:27 UTC.
    fn at() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_792_418_607)
    }

    #[test]
    fn the_namer_uses_its_device_and_clock() {
        let namer = Namer {
            device: "laptop".into(),
            clock: Arc::new(at),
        };
        assert_eq!(
            namer.name("a.txt", 2),
            "a (conflict laptop 2026-10-19 14-03-27 UTC 2).txt"
        );
        assert_eq!(
            Namer::default().name("a", 1),
            "a (conflict pdmount 1970-01-01 00-00-00 UTC)"
        );
    }

    #[test]
    fn conflict_names_follow_the_design_example() {
        assert_eq!(
            conflict_name("report.odt", "laptop", at(), 1),
            "report (conflict laptop 2026-10-19 14-03-27 UTC).odt"
        );
    }

    #[test]
    fn the_extension_is_split_at_the_last_dot() {
        assert_eq!(
            conflict_name("archive.tar.gz", "laptop", at(), 1),
            "archive.tar (conflict laptop 2026-10-19 14-03-27 UTC).gz"
        );
    }

    #[test]
    fn leading_dot_names_keep_the_whole_name() {
        assert_eq!(
            conflict_name(".bashrc", "laptop", at(), 1),
            ".bashrc (conflict laptop 2026-10-19 14-03-27 UTC)"
        );
        assert_eq!(
            conflict_name(".config.json", "laptop", at(), 1),
            ".config (conflict laptop 2026-10-19 14-03-27 UTC).json"
        );
    }

    #[test]
    fn names_without_extension_and_with_a_trailing_dot() {
        assert_eq!(
            conflict_name("Makefile", "laptop", at(), 1),
            "Makefile (conflict laptop 2026-10-19 14-03-27 UTC)"
        );
        assert_eq!(
            conflict_name("foo.", "laptop", at(), 1),
            "foo (conflict laptop 2026-10-19 14-03-27 UTC)."
        );
    }

    #[test]
    fn the_collision_number_goes_inside_the_parentheses() {
        assert_eq!(
            conflict_name("a.txt", "laptop", at(), 0),
            conflict_name("a.txt", "laptop", at(), 1)
        );
        assert_eq!(
            conflict_name("a.txt", "laptop", at(), 2),
            "a (conflict laptop 2026-10-19 14-03-27 UTC 2).txt"
        );
        assert_eq!(
            conflict_name("a.txt", "laptop", at(), 13),
            "a (conflict laptop 2026-10-19 14-03-27 UTC 13).txt"
        );
    }

    #[test]
    fn long_names_are_cut_at_a_char_boundary() {
        let name = format!("{}.txt", "é".repeat(250));
        let out = conflict_name(&name, "laptop", at(), 1);
        let suffix = " (conflict laptop 2026-10-19 14-03-27 UTC)";
        assert!(out.len() <= 255, "{} bytes", out.len());
        assert!(out.ends_with(&format!("{suffix}.txt")));
        // 255 - suffix - ".txt" is odd, so one byte stays unused: whole chars only.
        let stem = out.strip_suffix(&format!("{suffix}.txt")).unwrap();
        assert_eq!(stem.len() % 2, 0);
        assert!(stem.chars().all(|c| c == 'é'));
        assert!(255 - out.len() < 2);
    }

    #[test]
    fn a_huge_extension_moves_into_the_stem() {
        let name = format!("a.{}", "x".repeat(250));
        let out = conflict_name(&name, "laptop", at(), 1);
        assert!(out.len() <= 255, "{} bytes", out.len());
        assert!(out.starts_with("a.xxx"));
        assert!(out.ends_with(" UTC)"));
    }

    #[test]
    fn device_names_are_sanitized() {
        assert_eq!(
            conflict_name("a", "my/dev (1)\n", at(), 1),
            "a (conflict my_dev _1__ 2026-10-19 14-03-27 UTC)"
        );
        assert_eq!(
            conflict_name("a", "", at(), 1),
            "a (conflict remote 2026-10-19 14-03-27 UTC)"
        );
        let out = conflict_name(&"n".repeat(300), &"d".repeat(1000), at(), 1);
        assert!(out.len() <= 255);
        assert!(out.contains(&format!("conflict {} 2026", "d".repeat(64))));
        // Multibyte labels are cut by bytes, so the name still fits.
        let out = conflict_name(&"n".repeat(300), &"😀".repeat(64), at(), 1);
        assert!(out.len() <= 255, "{} bytes", out.len());
        assert!(out.contains(&format!("conflict {} 2026", "😀".repeat(16))));
    }

    #[test]
    fn names_are_deterministic_for_a_fixed_clock() {
        assert_eq!(
            conflict_name("a.txt", "laptop", at(), 1),
            conflict_name("a.txt", "laptop", at(), 1)
        );
        assert_ne!(
            conflict_name("a.txt", "laptop", at(), 1),
            conflict_name("a.txt", "laptop", at() + Duration::from_secs(1), 1)
        );
    }
}
