//! The Photos tree in memory: years, months and photos, and their inode
//! numbers. Pure bookkeeping, no I/O, so the stages can be tested alone.
//!
//! A photo's month comes from its capture time in the local time zone (as
//! the official clients show it); a related photo (a Live Photo's video)
//! sits in its main photo's month, beside it. A photo shows in its month
//! once its name is known; until then the month holds it unnamed. Names are
//! the server's, left out when they could not be file names; two photos of
//! one month with the same name (the server keeps them unique by name hash
//! in the Photos root, so only a related photo or a bad name can clash) are
//! told apart as `name (2).ext`, in link order.

use crate::vfs::{Ino, ROOT_INO, validate_name};
use pdmount_backend::photos::{TimelineItem, tag};
use pdmount_backend::{LinkId, RevisionId};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

/// A year and a month (1-12).
pub type Month = (i32, u32);

/// What the node of a photo says: its name, size and revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhotoMeta {
    pub name: String,
    pub size: u64,
    pub revision: RevisionId,
}

/// One photo as the tree keeps it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Photo {
    /// Seconds since the epoch, as claimed.
    pub capture: i64,
    /// [`tag`] values as bits.
    pub tags: u32,
    /// The main photo of a related one.
    pub main: Option<LinkId>,
    /// `None` until the month is opened and its names are fetched.
    pub meta: Option<PhotoMeta>,
}

impl Photo {
    pub fn has_tag(&self, tag: u8) -> bool {
        tag < 32 && self.tags & (1 << tag) != 0
    }

    /// A Live Photo or a Motion Photo (tags 3 and 4).
    pub fn is_live(&self) -> bool {
        self.has_tag(tag::LIVE_PHOTO) || self.has_tag(tag::MOTION_PHOTO)
    }
}

/// Tag values as bits; values over 31 are dropped.
pub fn tag_bits(tags: &[u8]) -> u32 {
    tags.iter()
        .filter(|tag| **tag < 32)
        .fold(0, |bits, tag| bits | (1 << tag))
}

/// What an inode number names.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Node {
    Root,
    Year(i32),
    Month(Month),
    Photo(LinkId),
}

/// The names shown in one month.
#[derive(Debug, Default, Clone)]
struct Names {
    by_name: BTreeMap<String, LinkId>,
    by_link: HashMap<LinkId, String>,
}

/// What [`Model::remove`] removed.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Removed {
    pub links: Vec<LinkId>,
    pub months: Vec<Month>,
    /// Month, inode and name of each removed photo that had a name.
    pub shown: Vec<(Month, Ino, String)>,
}

/// How a month's capture time is placed: local time, or UTC in tests.
pub type Placer = fn(i64) -> Month;

/// The month of `capture` in the local time zone.
pub fn local_month(capture: i64) -> Month {
    use chrono::{Datelike, Local, TimeZone};
    match Local.timestamp_opt(capture, 0).earliest() {
        Some(time) => (time.year(), time.month()),
        None => utc_month(capture),
    }
}

/// The month of `capture` in UTC.
pub fn utc_month(capture: i64) -> Month {
    use chrono::{Datelike, TimeZone, Utc};
    match Utc.timestamp_opt(capture, 0).single() {
        Some(time) => (time.year(), time.month()),
        None => (1970, 1),
    }
}

pub struct Model {
    photos: HashMap<LinkId, Photo>,
    /// Main photo to its related ones.
    related: HashMap<LinkId, BTreeSet<LinkId>>,
    /// Where each photo is placed.
    placed: HashMap<LinkId, Month>,
    months: BTreeMap<Month, BTreeSet<LinkId>>,
    names: HashMap<Month, Names>,
    inos: HashMap<Node, Ino>,
    nodes: HashMap<Ino, Node>,
    next_ino: Ino,
    placer: Placer,
    /// Photos without a name whose node the server returns but cannot be
    /// decrypted (D119): not gone, and not waited for, so their month
    /// counts as named; read again when an event names them. Not stored.
    unreadable: HashSet<LinkId>,
}

impl Model {
    pub fn new(placer: Placer) -> Self {
        let mut model = Self {
            photos: HashMap::new(),
            related: HashMap::new(),
            placed: HashMap::new(),
            months: BTreeMap::new(),
            names: HashMap::new(),
            inos: HashMap::new(),
            nodes: HashMap::new(),
            next_ino: ROOT_INO + 1,
            placer,
            unreadable: HashSet::new(),
        };
        model.inos.insert(Node::Root, ROOT_INO);
        model.nodes.insert(ROOT_INO, Node::Root);
        model
    }

    /// The inode number of `node`, given at its first use and kept for the
    /// life of the tree.
    pub fn ino(&mut self, node: Node) -> Ino {
        if let Some(ino) = self.inos.get(&node) {
            return *ino;
        }
        let ino = self.next_ino;
        self.next_ino += 1;
        self.inos.insert(node.clone(), ino);
        self.nodes.insert(ino, node);
        ino
    }

    /// The inode number of `node` if it has one.
    pub fn ino_of(&self, node: &Node) -> Option<Ino> {
        self.inos.get(node).copied()
    }

    /// What `ino` names, if it still exists.
    pub fn node(&self, ino: Ino) -> Option<&Node> {
        let node = self.nodes.get(&ino)?;
        let exists = match node {
            Node::Root => true,
            Node::Year(year) => self.months.keys().any(|(y, _)| y == year),
            Node::Month(month) => self.months.contains_key(month),
            Node::Photo(link) => self.shown_name(link).is_some(),
        };
        exists.then_some(node)
    }

    /// Photos in the tree.
    pub fn count(&self) -> usize {
        self.photos.len()
    }

    /// Photos whose name is known.
    pub fn named(&self) -> usize {
        self.photos.values().filter(|p| p.meta.is_some()).count()
    }

    pub fn photo(&self, link: &LinkId) -> Option<&Photo> {
        self.photos.get(link)
    }

    pub fn links(&self) -> impl Iterator<Item = &LinkId> {
        self.photos.keys()
    }

    /// The month a photo is placed in.
    pub fn month_of(&self, link: &LinkId) -> Option<Month> {
        self.placed.get(link).copied()
    }

    /// Inserts or updates a photo; the months it left and joined change.
    /// Returns them.
    pub fn put(&mut self, link: LinkId, photo: Photo) -> Vec<Month> {
        let mut touched = Vec::new();
        let meta = match self.photos.get(&link) {
            // The name stays: the timeline does not carry it.
            Some(old) if photo.meta.is_none() => old.meta.clone(),
            _ => photo.meta.clone(),
        };
        if let Some(old_main) = self.photos.get(&link).and_then(|p| p.main.clone())
            && Some(&old_main) != photo.main.as_ref()
            && let Some(set) = self.related.get_mut(&old_main)
        {
            set.remove(&link);
        }
        if let Some(main) = &photo.main {
            self.related
                .entry(main.clone())
                .or_default()
                .insert(link.clone());
        }
        self.photos.insert(link.clone(), Photo { meta, ..photo });
        touched.extend(self.place(&link));
        // Its related photos follow it.
        let related: Vec<LinkId> = self
            .related
            .get(&link)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default();
        for other in related {
            touched.extend(self.place(&other));
        }
        touched.sort_unstable();
        touched.dedup();
        touched
    }

    /// A timeline item and its related photos; returns the months that
    /// changed.
    pub fn put_item(&mut self, item: &TimelineItem) -> Vec<Month> {
        let mut touched = self.put(
            item.link.clone(),
            Photo {
                capture: item.capture_time,
                tags: tag_bits(&item.tags),
                main: None,
                meta: None,
            },
        );
        for related in &item.related {
            if related.link == item.link {
                continue;
            }
            // A link the timeline lists as a main photo stays one.
            if self
                .photos
                .get(&related.link)
                .is_some_and(|p| p.main.is_none())
            {
                continue;
            }
            touched.extend(self.put(
                related.link.clone(),
                Photo {
                    capture: related.capture_time,
                    tags: 0,
                    main: Some(item.link.clone()),
                    meta: None,
                },
            ));
        }
        touched.sort_unstable();
        touched.dedup();
        touched
    }

    /// Puts `link` in the month it belongs in; returns the months changed.
    fn place(&mut self, link: &LinkId) -> Vec<Month> {
        let Some(photo) = self.photos.get(link) else {
            return Vec::new();
        };
        let capture = photo
            .main
            .as_ref()
            .and_then(|main| self.photos.get(main))
            .map_or(photo.capture, |main| main.capture);
        // A claimed time beyond four-digit years goes to the epoch's month,
        // so every year folder has a name that looks it up again.
        let month = Some((self.placer)(capture))
            .filter(|(year, _)| (0..=9999).contains(year))
            .unwrap_or((1970, 1));
        let old = self.placed.insert(link.clone(), month);
        let mut touched = vec![month];
        if let Some(old) = old
            && old != month
        {
            self.unplace(link, old);
            touched.push(old);
        }
        self.months.entry(month).or_default().insert(link.clone());
        self.rename(month);
        touched
    }

    fn unplace(&mut self, link: &LinkId, month: Month) {
        if let Some(set) = self.months.get_mut(&month) {
            set.remove(link);
            if set.is_empty() {
                self.months.remove(&month);
                self.names.remove(&month);
                return;
            }
        }
        self.rename(month);
    }

    /// Removes a photo, and the related photos of a main one (they go with
    /// it, as the official clients trash them together).
    pub fn remove(&mut self, link: &LinkId) -> Removed {
        let mut links = vec![link.clone()];
        if let Some(related) = self.related.remove(link) {
            links.extend(related);
        }
        let mut removed = Removed::default();
        for link in links {
            self.unreadable.remove(&link);
            let name = self.shown_name(&link).map(str::to_owned);
            let Some(photo) = self.photos.remove(&link) else {
                continue;
            };
            if let Some(main) = &photo.main
                && let Some(set) = self.related.get_mut(main)
            {
                set.remove(&link);
            }
            if let Some(month) = self.placed.remove(&link) {
                if let (Some(name), Some(ino)) = (name, self.ino_of(&Node::Photo(link.clone()))) {
                    removed.shown.push((month, ino, name));
                }
                self.unplace(&link, month);
                removed.months.push(month);
            }
            removed.links.push(link);
        }
        removed
    }

    /// Sets a photo's name, size and revision; returns its month.
    pub fn set_meta(&mut self, link: &LinkId, meta: Option<PhotoMeta>) -> Option<Month> {
        if meta.is_some() {
            self.unreadable.remove(link);
        }
        let photo = self.photos.get_mut(link)?;
        if photo.meta == meta {
            return None;
        }
        photo.meta = meta;
        let month = *self.placed.get(link)?;
        self.rename(month);
        Some(month)
    }

    /// Marks photos whose node cannot be decrypted (D119): one with a name
    /// keeps it; one without stops being waited for. Returns the months
    /// that now count as named.
    pub fn set_unreadable(&mut self, links: &[LinkId]) -> Vec<Month> {
        let mut months = Vec::new();
        for link in links {
            let Some(photo) = self.photos.get(link) else {
                continue;
            };
            if photo.meta.is_none() && self.unreadable.insert(link.clone()) {
                months.extend(self.placed.get(link).copied());
            }
        }
        months.sort_unstable();
        months.dedup();
        months
    }

    /// Photos whose node could not be decrypted, without a name.
    pub fn unreadable(&self) -> usize {
        self.unreadable.len()
    }

    /// Whether `link`'s node could not be decrypted and it has no name.
    pub fn is_unreadable(&self, link: &LinkId) -> bool {
        self.unreadable.contains(link)
    }

    /// Works out the names shown in `month` again.
    fn rename(&mut self, month: Month) {
        let mut wanted: Vec<(&str, &LinkId)> = self
            .months
            .get(&month)
            .into_iter()
            .flatten()
            .filter_map(|link| {
                let meta = self.photos.get(link)?.meta.as_ref()?;
                let ok = validate_name(meta.name.as_bytes()).is_ok() && !meta.name.contains('\0');
                ok.then_some((meta.name.as_str(), link))
            })
            .collect();
        wanted.sort();
        let mut names = Names::default();
        for (name, link) in wanted {
            let shown = unique_name(name, |candidate| names.by_name.contains_key(candidate));
            names.by_name.insert(shown.clone(), link.clone());
            names.by_link.insert(link.clone(), shown);
        }
        // Inodes for the named photos, so a listing can name them.
        let links: Vec<LinkId> = names.by_link.keys().cloned().collect();
        for link in links {
            self.ino(Node::Photo(link));
        }
        self.names.insert(month, names);
    }

    /// The name a photo shows under; `None` until it has one.
    pub fn shown_name(&self, link: &LinkId) -> Option<&str> {
        let month = self.placed.get(link)?;
        self.names.get(month)?.by_link.get(link).map(String::as_str)
    }

    pub fn years(&self) -> Vec<i32> {
        let mut years: Vec<i32> = self.months.keys().map(|(year, _)| *year).collect();
        years.dedup();
        years
    }

    pub fn months_of(&self, year: i32) -> Vec<u32> {
        self.months
            .range((year, 0)..=(year, 12))
            .map(|((_, month), _)| *month)
            .collect()
    }

    pub fn has_month(&self, month: Month) -> bool {
        self.months.contains_key(&month)
    }

    /// The named photos of `month`, by name.
    pub fn entries(&self, month: Month) -> Vec<(String, LinkId)> {
        self.names
            .get(&month)
            .map(|names| {
                names
                    .by_name
                    .iter()
                    .map(|(name, link)| (name.clone(), link.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The photo named `name` in `month`.
    pub fn find(&self, month: Month, name: &str) -> Option<&LinkId> {
        self.names.get(&month)?.by_name.get(name)
    }

    /// The photos of `month` without a name yet, but for unreadable ones:
    /// main photos first, newest first, then their related ones.
    pub fn unnamed(&self, month: Month) -> Vec<LinkId> {
        let mut links: Vec<(&LinkId, &Photo)> = self
            .months
            .get(&month)
            .into_iter()
            .flatten()
            .filter(|link| !self.unreadable.contains(*link))
            .filter_map(|link| Some((link, self.photos.get(link)?)))
            .filter(|(_, photo)| photo.meta.is_none())
            .collect();
        links.sort_by(|(a_link, a), (b_link, b)| {
            a.main
                .is_some()
                .cmp(&b.main.is_some())
                .then(b.capture.cmp(&a.capture))
                .then(a_link.cmp(b_link))
        });
        links.into_iter().map(|(link, _)| link.clone()).collect()
    }

    /// Whether every photo of `month` has its name, or cannot have one
    /// now (unreadable).
    pub fn complete(&self, month: Month) -> bool {
        self.months.get(&month).is_none_or(|links| {
            links.iter().all(|link| {
                self.unreadable.contains(link)
                    || self.photos.get(link).is_some_and(|p| p.meta.is_some())
            })
        })
    }

    /// Whether some photo of `month` has its name: the month was opened, so
    /// its new photos get their names too.
    pub fn opened(&self, month: Month) -> bool {
        self.months.get(&month).is_some_and(|links| {
            links
                .iter()
                .any(|link| self.photos.get(link).is_some_and(|p| p.meta.is_some()))
        })
    }

    /// Months with names and photos still without: a fetch that stopped.
    pub fn unfinished(&self) -> Vec<Month> {
        self.months
            .keys()
            .copied()
            .filter(|month| self.opened(*month) && !self.complete(*month))
            .collect()
    }

    /// The newest capture time in `month`, or of all photos.
    pub fn newest(&self, within: Option<Month>) -> Option<i64> {
        let links: Box<dyn Iterator<Item = &LinkId>> = match within {
            Some(month) => Box::new(self.months.get(&month).into_iter().flatten()),
            None => Box::new(self.photos.keys()),
        };
        links
            .filter_map(|link| self.photos.get(link).map(|p| p.capture))
            .max()
    }

    /// The newest capture time of a year.
    pub fn newest_in_year(&self, year: i32) -> Option<i64> {
        self.months_of(year)
            .into_iter()
            .filter_map(|month| self.newest(Some((year, month))))
            .max()
    }
}

/// `name`, or `stem (n).ext` with the first `n` from 2 that is free, within
/// 255 bytes: the stem is cut first, then, when the extension alone leaves
/// no room for ` (n)`, the extension's end too.
pub fn unique_name(name: &str, taken: impl Fn(&str) -> bool) -> String {
    if !taken(name) {
        return name.to_owned();
    }
    let (stem, ext) = match name.rfind('.') {
        Some(dot) if dot > 0 => name.split_at(dot),
        _ => (name, ""),
    };
    (2..)
        .map(|n| {
            let marker = format!(" ({n})");
            let room = crate::vfs::MAX_NAME_BYTES.saturating_sub(marker.len());
            let ext = cut_to(ext, room);
            let stem = cut_to(stem, room - ext.len());
            format!("{stem}{marker}{ext}")
        })
        .find(|candidate| !taken(candidate))
        .expect("an unbounded range has a free name")
}

/// The longest start of `text` of at most `bytes` bytes, on a character
/// boundary.
fn cut_to(text: &str, bytes: usize) -> &str {
    let mut cut = text.len().min(bytes);
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// A year's folder name.
pub fn year_name(year: i32) -> String {
    format!("{year:04}")
}

/// A month's folder name: two digits, so they sort.
pub fn month_name(month: u32) -> String {
    format!("{month:02}")
}

/// The year a folder name stands for.
pub fn parse_year(name: &str) -> Option<i32> {
    (name.len() >= 4 && name.bytes().all(|b| b.is_ascii_digit()))
        .then(|| name.parse().ok())
        .flatten()
}

/// The month a folder name stands for.
pub fn parse_month(name: &str) -> Option<u32> {
    (name.len() == 2 && name.bytes().all(|b| b.is_ascii_digit()))
        .then(|| name.parse().ok())
        .flatten()
        .filter(|month| (1..=12).contains(month))
}
