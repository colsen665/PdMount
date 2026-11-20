//! The Photos tree's rows in its store (`trees/<key>/state.db`): a table of
//! its own beside the store's usual schema, whose block cache rows the
//! tree's [`crate::cache::BlockCache`] uses. Kept so a remount reads neither
//! the timeline nor the names again (D113).
//!
//! `photos` holds one row per photo: the timeline's fields, and the node's
//! name, size and revision once its month was opened; `signatures` (the
//! store's) the failed signatures of those decoded nodes (D132). `meta` holds the
//! volume and root the rows belong to (`photos.volume`, `photos.root`),
//! whether a whole timeline read finished (`photos.timeline`), the event
//! cursor (`photos.cursor`), written after the rows of the page it follows,
//! and the links whose events wait for another try (`photos.pending`, D119),
//! written before the cursor moves past them.

use super::model::{Photo, PhotoMeta};
use crate::errors::Result;
use crate::store::{Store, meta_get, meta_set};
use pdmount_backend::{EventId, LinkId, RevisionId, Unverified};
use rusqlite::Connection;

/// The table's schema; `photos.schema` in `meta` names its version.
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS photos (
    link_id TEXT PRIMARY KEY,
    capture_time INTEGER NOT NULL,
    tags INTEGER NOT NULL,
    main_link TEXT,
    name TEXT,
    size INTEGER,
    revision TEXT
);
";
const SCHEMA_VERSION: &str = "1";
const KEY_SCHEMA: &str = "photos.schema";
const KEY_VOLUME: &str = "photos.volume";
const KEY_ROOT: &str = "photos.root";
const KEY_TIMELINE: &str = "photos.timeline";
const KEY_CURSOR: &str = "photos.cursor";
const KEY_PENDING: &str = "photos.pending";

/// What a tree finds in its store at the open.
#[derive(Debug, Default)]
pub struct Loaded {
    pub photos: Vec<(LinkId, Photo)>,
    /// A whole timeline read finished, and the event stream continues
    /// from `cursor`.
    pub timeline_complete: bool,
    pub cursor: Option<EventId>,
    /// Links whose events could not be applied yet.
    pub pending: Vec<LinkId>,
}

/// Makes the table, and empties it when it belongs to another volume or
/// root (another account's Photos). Returns what is stored.
pub async fn open(store: &Store, volume: &str, root: &LinkId) -> Result<Loaded> {
    let (volume, root) = (volume.to_owned(), root.0.clone());
    store
        .write_durable(move |conn| {
            conn.execute_batch(SCHEMA)?;
            let same = meta_get(conn, KEY_VOLUME)?.as_deref() == Some(volume.as_str())
                && meta_get(conn, KEY_ROOT)?.as_deref() == Some(root.as_str())
                && meta_get(conn, KEY_SCHEMA)?.as_deref() == Some(SCHEMA_VERSION);
            if !same {
                conn.execute("DELETE FROM photos", [])?;
                conn.execute("DELETE FROM signatures", [])?;
                conn.execute(
                    "DELETE FROM meta WHERE key IN (?1, ?2, ?3)",
                    (KEY_TIMELINE, KEY_CURSOR, KEY_PENDING),
                )?;
                meta_set(conn, KEY_VOLUME, &volume)?;
                meta_set(conn, KEY_ROOT, &root)?;
                meta_set(conn, KEY_SCHEMA, SCHEMA_VERSION)?;
            }
            Ok(())
        })
        .await?;
    Ok(store.read(load)?)
}

fn load(conn: &Connection) -> rusqlite::Result<Loaded> {
    let mut select = conn.prepare(
        "SELECT p.link_id, p.capture_time, p.tags, p.main_link, p.name, p.size, p.revision,
                s.parts, s.signer, s.digest
         FROM photos p LEFT JOIN signatures s ON s.link_id = p.link_id",
    )?;
    let photos = select
        .query_map([], |row| {
            let name: Option<String> = row.get(4)?;
            let size: Option<i64> = row.get(5)?;
            let revision: Option<String> = row.get(6)?;
            let meta = match (name, size, revision) {
                (Some(name), Some(size), Some(revision)) => Some(PhotoMeta {
                    name,
                    size: u64::try_from(size).unwrap_or(0),
                    revision: RevisionId::new(revision),
                    unverified: match row.get::<_, Option<u8>>(7)? {
                        Some(parts) => Unverified::from_stored(
                            parts,
                            row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                            row.get::<_, Option<String>>(9)?.unwrap_or_default(),
                        ),
                        None => None,
                    },
                }),
                _ => None,
            };
            Ok((
                LinkId::new(row.get::<_, String>(0)?),
                Photo {
                    capture: row.get(1)?,
                    tags: u32::try_from(row.get::<_, i64>(2)?).unwrap_or(0),
                    main: row.get::<_, Option<String>>(3)?.map(LinkId::new),
                    meta,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(Loaded {
        photos,
        timeline_complete: meta_get(conn, KEY_TIMELINE)?.as_deref() == Some("complete"),
        cursor: meta_get(conn, KEY_CURSOR)?.map(EventId::new),
        pending: meta_get(conn, KEY_PENDING)?
            .map(|links| links.split('\n').map(LinkId::new).collect())
            .unwrap_or_default(),
    })
}

/// Queues the links whose events wait for another try (none: the key goes).
pub fn pending(store: &Store, mut links: Vec<LinkId>) {
    links.sort_unstable();
    store.write(move |conn| {
        if links.is_empty() {
            conn.execute("DELETE FROM meta WHERE key = ?1", [KEY_PENDING])?;
        } else {
            let joined: Vec<&str> = links.iter().map(LinkId::as_str).collect();
            meta_set(conn, KEY_PENDING, &joined.join("\n"))?;
        }
        Ok(())
    });
}

/// Queues the rows of `photos` (written as they are now).
pub fn put(store: &Store, photos: Vec<(LinkId, Photo)>) {
    if photos.is_empty() {
        return;
    }
    store.write(move |conn| {
        let mut upsert = conn.prepare_cached(
            "INSERT INTO photos (link_id, capture_time, tags, main_link, name, size, revision)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(link_id) DO UPDATE SET capture_time = excluded.capture_time,
                 tags = excluded.tags, main_link = excluded.main_link, name = excluded.name,
                 size = excluded.size, revision = excluded.revision",
        )?;
        for (link, photo) in &photos {
            let meta = photo.meta.as_ref();
            upsert.execute((
                &link.0,
                photo.capture,
                i64::from(photo.tags),
                photo.main.as_ref().map(|main| &main.0),
                meta.map(|meta| &meta.name),
                meta.map(|meta| i64::try_from(meta.size).unwrap_or(i64::MAX)),
                meta.map(|meta| &meta.revision.0),
            ))?;
            if let Some(meta) = meta {
                crate::signatures::write(conn, link, meta.unverified.as_ref())?;
            }
        }
        Ok(())
    });
}

/// Queues the removal of `links`.
pub fn remove(store: &Store, links: Vec<LinkId>) {
    if links.is_empty() {
        return;
    }
    store.write(move |conn| {
        let mut delete = conn.prepare_cached("DELETE FROM photos WHERE link_id = ?1")?;
        for link in &links {
            delete.execute([&link.0])?;
            crate::signatures::write(conn, link, None)?;
        }
        Ok(())
    });
}

/// Queues the event cursor; with `complete`, also that a whole timeline
/// read finished.
pub fn cursor(store: &Store, cursor: &EventId, complete: bool) {
    let cursor = cursor.0.clone();
    store.write(move |conn| {
        meta_set(conn, KEY_CURSOR, &cursor)?;
        if complete {
            meta_set(conn, KEY_TIMELINE, "complete")?;
        }
        Ok(())
    });
}

/// Queues that the timeline must be read again (the server asked for a
/// refresh): a restart in between reads it too.
pub fn timeline_stale(store: &Store) {
    store.write(|conn| {
        conn.execute("DELETE FROM meta WHERE key = ?1", [KEY_TIMELINE])?;
        Ok(())
    });
}
