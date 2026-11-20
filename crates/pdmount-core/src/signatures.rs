//! Nodes whose signatures do not verify (DESIGN.md §6.3, D132 amended).
//!
//! The adapter reports a node's failed signatures with the node
//! ([`Unverified`]); a tree keeps those of its nodes in its store's
//! `signatures` table, next to their `remote` rows (`rows.rs` for an
//! engine's tree, `photos/rows.rs` for Photos). [`Signatures`] is the
//! tree's view of that table in memory: the digest of each found node and
//! the digest it was accepted for on this device, which the refusals ask on
//! the hot path, and the count of newly found nodes for the notification.
//!
//! An id names a found node to the user: the first 8 hex digits of the
//! BLAKE3 hash of its link, which link ids (random, per volume) make unique
//! across the trees of a profile in practice; an accept of an id that names
//! two nodes is refused.

use crate::config::SignatureMode;
use pdmount_backend::{LinkId, Unverified};
use rusqlite::{Connection, params};
use std::collections::HashMap;

/// A found node as the tree knows it now.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Known {
    digest: String,
    /// The digest an accept was given for.
    accepted: Option<String>,
}

/// One tree's found nodes in memory, and its mode.
pub struct Signatures {
    mode: SignatureMode,
    /// Leaf lock: nothing is taken while it is held.
    known: parking_lot::Mutex<HashMap<LinkId, Known>>,
    /// Newly found nodes, a count per batch, for the notification.
    notices: tokio::sync::broadcast::Sender<u64>,
}

/// A found node as `pdmount signatures list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FoundView {
    pub id: String,
    pub link: String,
    /// Its path in the tree now; `None` when the tree does not place it.
    pub path: Option<String>,
    /// The failed parts: `name`, `key`, `content key`.
    pub parts: Vec<String>,
    /// The claimed signers, `anonymous` for none.
    pub signer: String,
    /// Unix seconds.
    pub first_seen: i64,
    /// An accept on this device covers the failures as they are now.
    pub accepted: bool,
    /// Opening, renaming, moving and deleting it answer EACCES.
    pub refused: bool,
}

/// A row of the `signatures` table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFound {
    pub link: LinkId,
    pub unverified: Unverified,
    pub first_seen: i64,
    pub accepted: Option<String>,
}

/// [`Unverified::covered_by`] for a stored digest.
fn covered(digest: &str, accepted: &str) -> bool {
    let accepted: Vec<&str> = accepted.split_whitespace().collect();
    digest
        .split_whitespace()
        .all(|token| accepted.contains(&token))
}

/// The id a user names a found node by.
pub fn id_of(link: &LinkId) -> String {
    let hash = blake3::hash(link.as_str().as_bytes());
    hash.to_hex()[..8].to_owned()
}

/// Every row of the `signatures` table.
pub fn load(conn: &Connection) -> rusqlite::Result<Vec<StoredFound>> {
    let mut select = conn.prepare(
        "SELECT link_id, parts, signer, digest, first_seen, accepted_digest FROM signatures",
    )?;
    let rows = select
        .query_map([], |row| {
            Ok((
                LinkId(row.get(0)?),
                row.get::<_, u8>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(link, parts, signer, digest, first_seen, accepted)| {
            Some(StoredFound {
                link,
                unverified: Unverified::from_stored(parts, signer, digest)?,
                first_seen,
                accepted,
            })
        })
        .collect())
}

/// Writes the found state of `link`: its row, or none once it verifies.
/// An accept and the first sighting stay while it is found.
pub fn write(
    conn: &Connection,
    link: &LinkId,
    unverified: Option<&Unverified>,
) -> rusqlite::Result<()> {
    match unverified {
        Some(unverified) => {
            conn.prepare_cached(
                "INSERT INTO signatures(link_id, parts, signer, digest, first_seen)
                 VALUES (?1, ?2, ?3, ?4, strftime('%s','now'))
                 ON CONFLICT(link_id) DO UPDATE
                   SET parts = excluded.parts, signer = excluded.signer, digest = excluded.digest",
            )?
            .execute(params![
                link.as_str(),
                unverified.bits(),
                unverified.signer,
                unverified.digest
            ])?;
        }
        None => {
            conn.prepare_cached("DELETE FROM signatures WHERE link_id = ?1")?
                .execute([link.as_str()])?;
        }
    }
    Ok(())
}

/// Records an accept of `link`'s failures as `digest` describes them.
pub fn store_accept(conn: &Connection, link: &LinkId, digest: &str) -> rusqlite::Result<usize> {
    conn.execute(
        "UPDATE signatures SET accepted_digest = ?2, accepted_at = strftime('%s','now')
         WHERE link_id = ?1",
        params![link.as_str(), digest],
    )
}

impl Signatures {
    /// The tree's found nodes as its store has them.
    pub fn new(mode: SignatureMode, stored: Vec<StoredFound>) -> Self {
        let known = stored
            .into_iter()
            .map(|found| {
                (
                    found.link,
                    Known {
                        digest: found.unverified.digest,
                        accepted: found.accepted,
                    },
                )
            })
            .collect();
        Self {
            mode,
            known: parking_lot::Mutex::new(known),
            notices: tokio::sync::broadcast::channel(16).0,
        }
    }

    pub fn mode(&self) -> SignatureMode {
        self.mode
    }

    /// The tree's state of `link` now: found with these failures, or
    /// verified (`None`). Returns whether it was not found before.
    pub fn observe(&self, link: &LinkId, unverified: Option<&Unverified>) -> bool {
        let mut known = self.known.lock();
        match unverified {
            Some(unverified) => match known.get_mut(link) {
                Some(entry) => {
                    entry.digest.clone_from(&unverified.digest);
                    false
                }
                None => {
                    known.insert(
                        link.clone(),
                        Known {
                            digest: unverified.digest.clone(),
                            accepted: None,
                        },
                    );
                    true
                }
            },
            None => {
                known.remove(link);
                false
            }
        }
    }

    /// Drops the nodes `keep` says the tree no longer has.
    pub fn sweep(&self, keep: impl Fn(&LinkId) -> bool) {
        self.known.lock().retain(|link, _| keep(link));
    }

    /// Whether an accept on this device covers `unverified` of `link`.
    pub fn accepted(&self, link: &LinkId, unverified: &Unverified) -> bool {
        self.known
            .lock()
            .get(link)
            .and_then(|known| known.accepted.as_deref())
            .is_some_and(|accepted| unverified.covered_by(accepted))
    }

    /// Whether the tree refuses to open, rename, move or delete the node
    /// `link` with these failures: in `refuse` mode until accepted.
    pub fn refuses(&self, link: &LinkId, unverified: Option<&Unverified>) -> bool {
        self.mode == SignatureMode::Refuse
            && unverified.is_some_and(|unverified| !self.accepted(link, unverified))
    }

    /// Notes an accept of `link`'s failures as `digest` describes them.
    pub fn set_accepted(&self, link: &LinkId, digest: &str) {
        if let Some(known) = self.known.lock().get_mut(link) {
            known.accepted = Some(digest.to_owned());
        }
    }

    /// Newly found nodes, a count per batch.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<u64> {
        self.notices.subscribe()
    }

    /// Announces `count` newly found nodes.
    pub fn announce(&self, count: u64) {
        if count > 0 {
            let _ = self.notices.send(count);
        }
    }

    /// How many nodes are found now.
    pub fn count(&self) -> usize {
        self.known.lock().len()
    }

    /// Found nodes now, and how many of them are refused.
    pub fn counts(&self) -> (usize, usize) {
        let known = self.known.lock();
        let refused = match self.mode {
            SignatureMode::Warn => 0,
            SignatureMode::Refuse => known
                .values()
                .filter(|known| {
                    !known
                        .accepted
                        .as_deref()
                        .is_some_and(|accepted| covered(&known.digest, accepted))
                })
                .count(),
        };
        (known.len(), refused)
    }

    /// The found links whose id is `id`.
    pub fn links_of(&self, id: &str) -> Vec<LinkId> {
        self.known
            .lock()
            .keys()
            .filter(|link| id_of(link) == id)
            .cloned()
            .collect()
    }
}

/// How the found nodes look to the user: `stored` with `path_of` giving
/// each one's path now.
pub fn views(
    signatures: &Signatures,
    stored: Vec<StoredFound>,
    path_of: impl Fn(&LinkId) -> Option<String>,
) -> Vec<FoundView> {
    let mut views: Vec<FoundView> = stored
        .into_iter()
        .map(|found| {
            let accepted = found
                .accepted
                .as_deref()
                .is_some_and(|accepted| found.unverified.covered_by(accepted));
            FoundView {
                id: id_of(&found.link),
                path: path_of(&found.link),
                parts: found
                    .unverified
                    .parts()
                    .iter()
                    .map(|part| part.as_str().to_owned())
                    .collect(),
                signer: found.unverified.signer.clone(),
                first_seen: found.first_seen,
                accepted,
                refused: signatures.mode() == SignatureMode::Refuse && !accepted,
                link: found.link.0,
            }
        })
        .collect();
    views.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.id.cmp(&b.id)));
    views
}

/// What `pdmount signatures check` found in one tree.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CheckCounts {
    /// Folders listed from the server.
    pub folders: u64,
    /// Nodes decoded in those listings.
    pub nodes: u64,
    /// Of those, nodes whose signatures do not verify.
    pub unverified: u64,
    /// Folders whose listing failed.
    pub failed: u64,
}

impl CheckCounts {
    pub fn add(&mut self, other: CheckCounts) {
        self.folders += other.folders;
        self.nodes += other.nodes;
        self.unverified += other.unverified;
        self.failed += other.failed;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pdmount_backend::SignaturePart;

    fn unverified(link: &LinkId, parts: &[SignaturePart]) -> Unverified {
        let failed: Vec<_> = parts.iter().map(|part| (*part, None)).collect();
        Unverified::of(link, "n", &failed).expect("failures")
    }

    #[test]
    fn a_node_is_refused_until_accepted_for_the_same_failures() {
        let link = LinkId::new("l");
        let signatures = Signatures::new(SignatureMode::Refuse, Vec::new());
        let name = unverified(&link, &[SignaturePart::Name]);
        assert!(signatures.observe(&link, Some(&name)), "newly found");
        assert!(!signatures.observe(&link, Some(&name)), "known");
        assert!(signatures.refuses(&link, Some(&name)));
        assert!(!signatures.refuses(&link, None));
        signatures.set_accepted(&link, &name.digest);
        assert!(!signatures.refuses(&link, Some(&name)));
        let both = unverified(&link, &[SignaturePart::Name, SignaturePart::Key]);
        assert!(!signatures.observe(&link, Some(&both)));
        assert!(
            signatures.refuses(&link, Some(&both)),
            "a new part asks again"
        );
        assert_eq!(
            signatures.links_of(&id_of(&link)),
            std::slice::from_ref(&link)
        );
        signatures.observe(&link, None);
        assert_eq!(signatures.count(), 0, "verified: dropped");
        assert!(signatures.observe(&link, Some(&name)), "found again: new");
        assert!(
            signatures.refuses(&link, Some(&name)),
            "the accept went with it"
        );
        signatures.sweep(|_| false);
        assert_eq!(signatures.count(), 0);

        let warn = Signatures::new(SignatureMode::Warn, Vec::new());
        warn.observe(&link, Some(&name));
        assert!(!warn.refuses(&link, Some(&name)));
    }

    #[test]
    fn ids_are_short_and_stable() {
        let id = id_of(&LinkId::new("abc"));
        assert_eq!(id.len(), 8);
        assert_eq!(id, id_of(&LinkId::new("abc")));
        assert_ne!(id, id_of(&LinkId::new("abd")));
    }
}
