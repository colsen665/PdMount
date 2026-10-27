//! Inode rows in the store (DESIGN.md §13.2): the `inodes` table holds the
//! local state of every attached node and tombstone, `remote` the last
//! known remote state, keyed by link.

use crate::vfs::{Gens, Ino, NodeRow, RemoteState, ns_to_system_time, system_time_to_ns};
use pdmount_backend::{LinkId, NameHash, NodeKind, RevisionId};
use rusqlite::{Connection, params};

/// Inode numbers stay far below `i64::MAX`; SQLite integers are signed.
pub fn sql_ino(ino: Ino) -> i64 {
    ino as i64
}

pub fn kind_to_int(kind: NodeKind) -> i64 {
    match kind {
        NodeKind::File => 1,
        NodeKind::Folder => 2,
    }
}

pub fn int_to_kind(value: i64) -> NodeKind {
    if value == 2 {
        NodeKind::Folder
    } else {
        NodeKind::File
    }
}

/// Rewrites the rows of a set of nodes: `Some(row)` replaces, `None`
/// deletes. Names and links are cleared first, so renames, swaps and link
/// adoptions within the batch never collide with the unique indexes.
pub fn write_rows(conn: &Connection, rows: &[(Ino, Option<NodeRow>)]) -> rusqlite::Result<()> {
    let mut old_links: Vec<String> = Vec::new();
    {
        let mut select = conn.prepare("SELECT link_id FROM inodes WHERE ino = ?1")?;
        let mut clear = conn.prepare(
            "UPDATE inodes SET parent_ino = NULL, name = NULL, link_id = NULL WHERE ino = ?1",
        )?;
        for (ino, _) in rows {
            let link: Option<Option<String>> = select
                .query_row([sql_ino(*ino)], |row| row.get(0))
                .map(Some)
                .or_else(|error| match error {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    error => Err(error),
                })?;
            if let Some(Some(link)) = link {
                old_links.push(link);
            }
            clear.execute([sql_ino(*ino)])?;
        }
    }
    let mut delete = conn.prepare("DELETE FROM inodes WHERE ino = ?1")?;
    let mut upsert = conn.prepare(
        "INSERT OR REPLACE INTO inodes(ino, kind, parent_ino, name, link_id, mode, mtime_ns, ctime_ns, size,
             listed, local_only, deleted, content_gen, meta_gen, uploaded_content_gen, uploaded_meta_gen,
             deferred_error, atime_ns, remote_newer)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19)",
    )?;
    let mut upsert_remote = conn.prepare(
        "INSERT OR REPLACE INTO remote(link_id, parent_link, name, name_hash, kind, active_rev, size, mtime_ns,
             mode, sha1, trashed, seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 0, strftime('%s','now'))",
    )?;
    for (ino, row) in rows {
        let Some(row) = row else {
            delete.execute([sql_ino(*ino)])?;
            continue;
        };
        let gens = &row.gens;
        upsert.execute(params![
            sql_ino(row.ino),
            kind_to_int(row.kind),
            row.parent.map(sql_ino),
            row.name,
            row.remote.as_ref().map(|r| r.link.0.as_str()),
            row.mode,
            system_time_to_ns(row.mtime),
            system_time_to_ns(row.ctime),
            row.size as i64,
            row.listed,
            row.local_only,
            row.deleted,
            gens.content as i64,
            gens.meta as i64,
            gens.uploaded_content as i64,
            gens.uploaded_meta as i64,
            row.deferred_error,
            row.atime.map_or(0, system_time_to_ns),
            row.remote_newer,
        ])?;
        if let Some(remote) = &row.remote {
            upsert_remote.execute(params![
                remote.link.0,
                remote.parent_link.as_ref().map(|l| l.0.as_str()),
                remote.name,
                remote.name_hash.as_ref().map(|h| h.0.as_str()),
                kind_to_int(row.kind),
                remote.rev.as_ref().map(|r| r.0.as_str()),
                remote.size as i64,
                system_time_to_ns(remote.mtime),
                remote.mode,
                remote.sha1.map(|digest| digest.to_vec()),
            ])?;
        }
    }
    // Remote rows nobody refers to any more.
    let mut orphaned = conn.prepare(
        "DELETE FROM remote WHERE link_id = ?1
           AND NOT EXISTS (SELECT 1 FROM inodes WHERE inodes.link_id = ?1)",
    )?;
    for link in old_links {
        orphaned.execute([link])?;
    }
    Ok(())
}

/// Stored nodes with local changes that are not uploaded yet: new nodes,
/// tombstones, unuploaded content or metadata, moves and renames.
pub fn count_pending(conn: &Connection) -> rusqlite::Result<u64> {
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM inodes i
           LEFT JOIN remote r ON r.link_id = i.link_id
           LEFT JOIN inodes p ON p.ino = i.parent_ino
         WHERE i.ino != 1 AND i.local_only = 0 AND (
               i.deleted = 1
            OR i.link_id IS NULL
            OR i.content_gen != i.uploaded_content_gen
            OR (i.kind = 1 AND i.meta_gen != i.uploaded_meta_gen)
            OR (i.parent_ino IS NOT NULL AND (r.name IS NOT i.name OR r.parent_link IS NOT p.link_id)))",
        [],
        |row| row.get(0),
    )?;
    Ok(count as u64)
}

/// Every stored inode row with its remote state.
pub fn load_rows(conn: &Connection) -> rusqlite::Result<Vec<NodeRow>> {
    let mut statement = conn.prepare(
        "SELECT i.ino, i.kind, i.parent_ino, i.name, i.mode, i.mtime_ns, i.ctime_ns, i.size, i.listed,
                i.local_only, i.deleted, i.content_gen, i.meta_gen, i.uploaded_content_gen,
                i.uploaded_meta_gen, i.deferred_error,
                r.link_id, r.parent_link, r.name, r.name_hash, r.active_rev, r.size, r.mtime_ns, r.mode, r.sha1,
                i.atime_ns, i.remote_newer
         FROM inodes i LEFT JOIN remote r ON r.link_id = i.link_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            let link: Option<String> = row.get(16)?;
            let remote = match link {
                Some(link) => Some(RemoteState {
                    link: LinkId(link),
                    parent_link: row.get::<_, Option<String>>(17)?.map(LinkId),
                    name: row.get::<_, Option<String>>(18)?.unwrap_or_default(),
                    name_hash: row.get::<_, Option<String>>(19)?.map(NameHash),
                    rev: row.get::<_, Option<String>>(20)?.map(RevisionId),
                    size: row.get::<_, Option<i64>>(21)?.unwrap_or(0) as u64,
                    mtime: ns_to_system_time(row.get::<_, Option<i64>>(22)?.unwrap_or(0)),
                    mode: row.get(23)?,
                    sha1: row
                        .get::<_, Option<Vec<u8>>>(24)?
                        .and_then(|digest| <[u8; 20]>::try_from(digest).ok()),
                }),
                None => None,
            };
            Ok(NodeRow {
                ino: row.get::<_, i64>(0)? as Ino,
                kind: int_to_kind(row.get(1)?),
                parent: row.get::<_, Option<i64>>(2)?.map(|p| p as Ino),
                name: row.get(3)?,
                mode: row.get(4)?,
                mtime: ns_to_system_time(row.get(5)?),
                ctime: ns_to_system_time(row.get(6)?),
                // 0: never set, so it follows mtime.
                atime: Some(row.get::<_, i64>(25)?)
                    .filter(|&ns| ns != 0)
                    .map(ns_to_system_time),
                size: row.get::<_, i64>(7)? as u64,
                listed: row.get(8)?,
                local_only: row.get(9)?,
                deleted: row.get(10)?,
                gens: Gens {
                    content: row.get::<_, i64>(11)? as u64,
                    meta: row.get::<_, i64>(12)? as u64,
                    uploaded_content: row.get::<_, i64>(13)? as u64,
                    uploaded_meta: row.get::<_, i64>(14)? as u64,
                },
                deferred_error: row.get(15)?,
                remote_newer: row.get(26)?,
                remote,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use crate::vfs::{Gens, ROOT_INO};
    use std::time::UNIX_EPOCH;

    fn row(ino: Ino, remote_newer: bool) -> NodeRow {
        NodeRow {
            ino,
            kind: NodeKind::File,
            parent: Some(ROOT_INO),
            name: Some(format!("f{ino}")),
            size: 0,
            mtime: UNIX_EPOCH,
            ctime: UNIX_EPOCH,
            atime: None,
            mode: 0o644,
            listed: false,
            local_only: false,
            deleted: false,
            gens: Gens::default(),
            deferred_error: None,
            remote_newer,
            remote: None,
        }
    }

    #[test]
    fn rows_round_trip_remote_newer() {
        let dir = tempfile::tempdir().expect("tempdir");
        let store = Store::open(&dir.path().join("state.db")).expect("open");
        store
            .read(|conn| {
                write_rows(conn, &[(ROOT_INO, Some(row(ROOT_INO, false)))])?;
                write_rows(conn, &[(2, Some(row(2, true))), (3, Some(row(3, false)))])?;
                let rows = load_rows(conn)?;
                let flag = |ino| {
                    rows.iter()
                        .find(|row| row.ino == ino)
                        .map(|row| row.remote_newer)
                };
                assert_eq!((flag(2), flag(3)), (Some(true), Some(false)));
                // A rewrite clears it.
                write_rows(conn, &[(2, Some(row(2, false)))])?;
                let rows = load_rows(conn)?;
                assert!(rows.iter().all(|row| !row.remote_newer));
                Ok(())
            })
            .expect("rows");
    }
}
