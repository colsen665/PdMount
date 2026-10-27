//! Per-path access rules (DESIGN.md §11, D011, D019).
//!
//! A rule sets an entry and everything below it to `ro` or `rw`; the
//! nearest rule on the way up to the root decides, else `access.default`.
//! Rules are bound to the inode, not the path: they follow renames and
//! moves, local and remote, and become `orphaned` when their entry is
//! deleted. They are local to the device and profile.

use crate::config::AccessMode;
use crate::vfs::{Ino, ROOT_INO, Tree};
use rusqlite::{Connection, params};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleSource {
    Cli,
    Config,
}

impl RuleSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cli => "cli",
            Self::Config => "config",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub ino: Ino,
    pub link: Option<String>,
    /// The path when the rule was set, for display.
    pub path_at_set: String,
    pub mode: AccessMode,
    pub source: RuleSource,
}

/// A rule as `access list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleView {
    pub mode: AccessMode,
    /// Current path; `None` when orphaned.
    pub path: Option<String>,
    pub path_at_set: String,
    pub source: RuleSource,
}

#[derive(Debug, Default)]
pub struct Rules {
    by_ino: HashMap<Ino, Rule>,
    default: AccessMode,
}

pub fn mode_str(mode: AccessMode) -> &'static str {
    match mode {
        AccessMode::Ro => "ro",
        AccessMode::Rw => "rw",
    }
}

/// The inverse of [`mode_str`].
pub fn parse_mode(value: &str) -> Option<AccessMode> {
    [AccessMode::Ro, AccessMode::Rw]
        .into_iter()
        .find(|mode| mode_str(*mode) == value)
}

impl Rules {
    pub fn load(conn: &Connection, default: AccessMode) -> rusqlite::Result<Self> {
        let mut statement =
            conn.prepare("SELECT ino, link_id, path_at_set, mode, source FROM access_rules")?;
        let by_ino = statement
            .query_map([], |row| {
                let ino = row.get::<_, i64>(0)? as Ino;
                Ok((
                    ino,
                    Rule {
                        ino,
                        link: row.get(1)?,
                        path_at_set: row.get(2)?,
                        // The table allows only ro and rw.
                        mode: parse_mode(&row.get::<_, String>(3)?).unwrap_or(AccessMode::Rw),
                        source: if row.get::<_, String>(4)? == "config" {
                            RuleSource::Config
                        } else {
                            RuleSource::Cli
                        },
                    },
                ))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(Self { by_ino, default })
    }

    pub fn is_empty(&self) -> bool {
        self.by_ino.is_empty() && self.default == AccessMode::Rw
    }

    pub fn get(&self, ino: Ino) -> Option<&Rule> {
        self.by_ino.get(&ino)
    }

    pub fn insert(&mut self, rule: Rule) {
        self.by_ino.insert(rule.ino, rule);
    }

    pub fn remove(&mut self, ino: Ino) -> Option<Rule> {
        self.by_ino.remove(&ino)
    }

    /// Effective mode of `ino` and the rule deciding it (`None`: default).
    pub fn effective<'a>(&'a self, tree: &Tree, ino: Ino) -> (AccessMode, Option<&'a Rule>) {
        if self.by_ino.is_empty() {
            return (self.default, None);
        }
        let mut current = ino;
        // Bounded by the tree depth; a detached node uses its last parent.
        for _ in 0..4096 {
            if let Some(rule) = self.by_ino.get(&current)
                && tree
                    .get(current)
                    .is_some_and(|node| node.attached || current == ino)
            {
                return (rule.mode, Some(rule));
            }
            if current == ROOT_INO {
                break;
            }
            match tree.get(current) {
                Some(node) => current = node.parent,
                None => break,
            }
        }
        (self.default, None)
    }

    /// The inode of an orphaned rule set on `path`.
    pub fn orphaned_ino(&self, tree: &Tree, path: &str) -> Option<Ino> {
        self.by_ino
            .values()
            .find(|rule| {
                rule.path_at_set == path && !tree.get(rule.ino).is_some_and(|node| node.attached)
            })
            .map(|rule| rule.ino)
    }

    pub fn is_read_only(&self, tree: &Tree, ino: Ino) -> bool {
        self.effective(tree, ino).0 == AccessMode::Ro
    }

    /// Rules at or below `under` (all with `None`), sorted by path.
    pub fn views(&self, tree: &Tree, under: Option<Ino>) -> Vec<RuleView> {
        let mut views: Vec<RuleView> = self
            .by_ino
            .values()
            .filter_map(|rule| {
                let path = tree
                    .get(rule.ino)
                    .filter(|node| node.attached)
                    .and_then(|_| tree.path(rule.ino));
                if let Some(under) = under
                    && !(path.is_some() && tree.is_within(rule.ino, under))
                {
                    return None;
                }
                Some(RuleView {
                    mode: rule.mode,
                    path,
                    path_at_set: rule.path_at_set.clone(),
                    source: rule.source,
                })
            })
            .collect();
        views.sort_by(|a, b| {
            (a.path.is_none(), &a.path, &a.path_at_set).cmp(&(
                b.path.is_none(),
                &b.path,
                &b.path_at_set,
            ))
        });
        views
    }
}

/// Stores a rule (replacing one on the same inode) and bumps
/// `access_version`.
pub fn store_rule(conn: &Connection, rule: &Rule, orphaned: bool) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO access_rules(ino, link_id, path_at_set, mode, source, status, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'), strftime('%s','now'))
         ON CONFLICT(ino) DO UPDATE SET link_id = excluded.link_id, path_at_set = excluded.path_at_set,
             mode = excluded.mode, source = excluded.source, status = excluded.status,
             updated_at = excluded.updated_at",
        params![
            rule.ino as i64,
            rule.link,
            rule.path_at_set,
            mode_str(rule.mode),
            rule.source.as_str(),
            if orphaned { "orphaned" } else { "active" },
        ],
    )?;
    bump_version(conn)
}

pub fn delete_rule(conn: &Connection, ino: Ino) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM access_rules WHERE ino = ?1", [ino as i64])?;
    bump_version(conn)
}

fn bump_version(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES ('access_version', 1)
         ON CONFLICT(key) DO UPDATE SET value = CAST(value AS INTEGER) + 1",
        [],
    )
    .map(drop)
}
