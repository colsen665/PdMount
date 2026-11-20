//! `cache wipe` in the Photos tree (M6.0.10): the tree's own block cache,
//! all of it or the blocks of the photos at or below a path of the tree.

use super::model::{parse_month, parse_year};
use super::*;
use crate::cache::Evicted;

impl PhotosTree {
    /// Evicts every block of the tree's cache that may go, or with a path
    /// (`/2024`, `/2024/06`, `/2024/06/<name>`) only the blocks of the
    /// photos at or below it: a year or month folder wipes every named
    /// photo below it. Never pinned or protected blocks. Asks the server
    /// nothing.
    pub async fn cache_wipe(&self, path: Option<&str>) -> Result<Evicted> {
        let links = match path {
            None => None,
            Some(path) => Some(self.links_at(path)?),
        };
        let cache = Arc::clone(&self.cache);
        tokio::task::spawn_blocking(move || cache.wipe(links.as_ref()))
            .await
            .map_err(|error| CoreError::Internal(format!("cache wipe: {error}")))
    }

    /// The links of the photos at or below `path`; an empty path is the
    /// whole tree.
    fn links_at(&self, path: &str) -> Result<HashSet<LinkId>> {
        let mut parts = Vec::new();
        for part in path
            .split('/')
            .filter(|part| !part.is_empty() && *part != ".")
        {
            if part == ".." {
                return Err(CoreError::InvalidArgument);
            }
            parts.push(part);
        }
        let model = self.model.read();
        let mut links = HashSet::new();
        let Some(first) = parts.first() else {
            links.extend(model.links().cloned());
            return Ok(links);
        };
        let year = parse_year(first)
            .filter(|year| model.years().contains(year))
            .ok_or(CoreError::NotFound)?;
        let months: Vec<u32> = match parts.get(1) {
            None => model.months_of(year),
            Some(name) => vec![
                parse_month(name)
                    .filter(|month| model.has_month((year, *month)))
                    .ok_or(CoreError::NotFound)?,
            ],
        };
        for month in months {
            match parts.get(2) {
                None => {
                    links.extend(
                        model
                            .entries((year, month))
                            .into_iter()
                            .map(|(_, link)| link),
                    );
                }
                Some(name) => {
                    if parts.len() > 3 {
                        return Err(CoreError::NotADirectory);
                    }
                    links.insert(
                        model
                            .find((year, month), name)
                            .cloned()
                            .ok_or(CoreError::NotFound)?,
                    );
                }
            }
        }
        Ok(links)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use pdmount_backend::UploadSource;
    use pdmount_backend::fake::FakeBackend;

    const MARCH_2023: i64 = 1_678_881_600;
    const JULY_2023: i64 = 1_689_422_400;
    const MAY_2021: i64 = 1_621_080_000;

    async fn photo(fake: &FakeBackend, name: &str, capture: i64) {
        let content = Bytes::from(format!("content of {name}").into_bytes());
        fake.add_photo(
            name,
            UploadSource::Bytes(content),
            capture,
            Vec::new(),
            None,
        )
        .await
        .expect("add photo");
    }

    /// Reads a photo of `year`/`month` through the tree, into its cache.
    async fn read(tree: &Arc<PhotosTree>, year: &str, month: &str, name: &str) {
        let mut ino = ROOT_INO;
        for part in [year, month, name] {
            ino = match tree.lookup(ino, part.as_bytes()).expect("lookup") {
                Lookup::Found(attr) => attr.ino,
                Lookup::Negative => panic!("{part} not found"),
            };
        }
        let (handle, _) = tree.open(ino).expect("open");
        tree.read(handle, 0, 100).await.expect("read");
        tree.release(handle);
    }

    async fn named(tree: &Arc<PhotosTree>, month: Month) {
        tree.want(month);
        while let Some((_, batch)) = tree.next_batch() {
            tree.fetch_nodes(&batch).await.expect("names");
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_path_wipes_the_blocks_of_the_photos_below_it() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = FakeBackend::new();
        for (name, capture) in [
            ("A.jpg", MARCH_2023),
            ("B.jpg", MARCH_2023 + 60),
            ("C.jpg", JULY_2023),
            ("D.jpg", MAY_2021),
        ] {
            photo(&fake, name, capture).await;
        }
        let tree_backend = fake.photos_tree().expect("photos tree");
        let volume = fake.photos_volume_now().expect("volume");
        let options = PhotosOptions {
            volume: volume.volume,
            root: volume.root,
            state_db: dir.path().join("state.db"),
            cache: CacheOptions {
                dir: dir.path().join("blocks"),
                master_key: Some([7; 32]),
                ram_bytes: 16 << 20,
                disk_bytes: 64 << 20,
                shared: None,
                downloads: None,
            },
            config: Config::default(),
            health: None,
            placer: utc_month,
            thumbnails: None,
            scan: None,
            limits: None,
        };
        let (tree, _invalidations) =
            PhotosTree::open(Arc::new(tree_backend), Arc::new(fake.clone()), options)
                .await
                .expect("open");
        tree.gate.set_interval(Duration::ZERO);
        tree.read_timeline().await.expect("timeline");
        for month in [(2023, 3), (2023, 7), (2021, 5)] {
            named(&tree, month).await;
        }
        let fill = || async {
            for (year, month, name) in [
                ("2023", "03", "A.jpg"),
                ("2023", "03", "B.jpg"),
                ("2023", "07", "C.jpg"),
                ("2021", "05", "D.jpg"),
            ] {
                read(&tree, year, month, name).await;
            }
            tree.cache_usage().1
        };
        assert_eq!(fill().await, 4);
        // A photo.
        let freed = tree.cache_wipe(Some("/2023/03/A.jpg")).await.expect("wipe");
        assert_eq!(freed.blocks, 1);
        assert_eq!(tree.cache_usage().1, 3);
        // A month, then a year.
        assert_eq!(fill().await, 4);
        let freed = tree.cache_wipe(Some("/2023/03")).await.expect("wipe");
        assert_eq!(freed.blocks, 2);
        assert_eq!(fill().await, 4);
        let freed = tree.cache_wipe(Some("/2023")).await.expect("wipe");
        assert_eq!(freed.blocks, 3);
        assert_eq!(tree.cache_usage().1, 1, "2021 stays");
        // The whole tree: with no path and with the root.
        assert_eq!(fill().await, 4);
        assert_eq!(tree.cache_wipe(Some("/")).await.expect("wipe").blocks, 4);
        assert_eq!(fill().await, 4);
        assert_eq!(tree.cache_wipe(None).await.expect("wipe").blocks, 4);
        assert_eq!(tree.cache_usage().1, 0);
        // What is not in the tree.
        for path in ["/1999", "/2023/12", "/2023/03/nope.jpg"] {
            assert!(
                matches!(tree.cache_wipe(Some(path)).await, Err(CoreError::NotFound)),
                "{path}"
            );
        }
        assert!(matches!(
            tree.cache_wipe(Some("/2023/03/A.jpg/x")).await,
            Err(CoreError::NotADirectory)
        ));
        assert!(matches!(
            tree.cache_wipe(Some("/2023/../2021")).await,
            Err(CoreError::InvalidArgument)
        ));
    }
}
