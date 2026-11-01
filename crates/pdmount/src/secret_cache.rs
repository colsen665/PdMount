//! The SDK's secret cache, with the account key passphrase out of its
//! eviction (D085).
//!
//! The SDK keeps both in one repository, and the capped repository drops the
//! entries read least recently. The passphrase is written at login and read
//! only when the cached user keys are gone, so it would go first, and only a
//! new login brings it back. Everything else there can be derived again.

use futures::stream::BoxStream;
use proton_drive_sdk::proton_sdk_rs2::cache::CacheRepository;
use std::sync::Arc;

/// `DefaultSecretCache::get_account_passphrase_cache_key` in the SDK.
const PASSPHRASE_PREFIX: &str = "account:passphrase:";

/// Sends passphrases to `keys`, which has no size limit, and the rest to
/// `evicted`.
pub struct SecretCache {
    evicted: Arc<dyn CacheRepository>,
    keys: Arc<dyn CacheRepository>,
}

impl SecretCache {
    pub fn new(evicted: Arc<dyn CacheRepository>, keys: Arc<dyn CacheRepository>) -> Self {
        Self { evicted, keys }
    }

    fn store_of(&self, key: &str) -> &dyn CacheRepository {
        if key.starts_with(PASSPHRASE_PREFIX) {
            self.keys.as_ref()
        } else {
            self.evicted.as_ref()
        }
    }
}

#[async_trait::async_trait]
impl CacheRepository for SecretCache {
    async fn set(&self, key: &str, value: String, tags: Vec<String>) -> anyhow::Result<()> {
        self.store_of(key).set(key, value, tags).await
    }

    async fn remove(&self, key: &str) -> anyhow::Result<()> {
        self.store_of(key).remove(key).await
    }

    /// The SDK stores passphrases without tags.
    async fn remove_by_tag(&self, tag: &str) -> anyhow::Result<()> {
        self.evicted.remove_by_tag(tag).await
    }

    /// Clearing the cache keeps the passphrases: without them the session
    /// is useless until a new login. Logout deletes the files.
    async fn clear(&self) -> anyhow::Result<()> {
        self.evicted.clear().await
    }

    async fn try_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        self.store_of(key).try_get(key).await
    }

    fn get_by_tags(&self, tags: Vec<String>) -> BoxStream<'_, anyhow::Result<(String, String)>> {
        self.evicted.get_by_tags(tags)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proton_drive_sdk::cache::encrypted::EncryptedCacheRepository;
    use proton_drive_sdk::cache::sqlite::SqliteCacheRepository;
    use proton_drive_sdk::proton_sdk_rs2::account::ProtonAccountClient;
    use proton_drive_sdk::proton_sdk_rs2::client::ProtonClientOptions;
    use proton_drive_sdk::proton_sdk_rs2::secret::SessionSecretCache;
    use proton_drive_sdk::proton_sdk_rs2::ser::StoredCredentials;
    use proton_drive_sdk::proton_sdk_rs2::session::ProtonAPISession;
    use proton_drive_sdk::proton_sdk_rs2::{AppVersionConfiguration, PasswordMode};
    use std::path::Path;

    const KEY_ID: &str = "key-1";

    fn sealed(path: &Path, capacity: Option<usize>) -> Arc<dyn CacheRepository> {
        let sqlite = SqliteCacheRepository::open_file(path, capacity).expect("open");
        Arc::new(EncryptedCacheRepository::new(
            Arc::new(sqlite),
            vec![7u8; 32],
        ))
    }

    struct Files {
        _dir: tempfile::TempDir,
        secrets: std::path::PathBuf,
        keys: std::path::PathBuf,
    }

    fn files() -> Files {
        let dir = tempfile::tempdir().expect("tempdir");
        Files {
            secrets: dir.path().join("secrets.db"),
            keys: dir.path().join("keys.db"),
            _dir: dir,
        }
    }

    fn cache(files: &Files, capacity: usize) -> Arc<dyn CacheRepository> {
        let evicted = sealed(&files.secrets, Some(capacity));
        let keys = sealed(&files.keys, None);
        Arc::new(SecretCache::new(evicted, keys))
    }

    /// The SDK's own way in, so a change of its key format fails here.
    fn session_secrets(cache: Arc<dyn CacheRepository>) -> Arc<dyn SessionSecretCache> {
        let session = ProtonAPISession::from_stored_credentials(
            StoredCredentials::new(
                "sid".into(),
                "user@example.test".into(),
                "uid".into(),
                "access".into(),
                "refresh".into(),
                vec!["full".into()],
                false,
                PasswordMode::Single,
            ),
            AppVersionConfiguration::new("pdmount", 0, 0, 0),
            ProtonClientOptions {
                secret_cache_repository: Some(cache),
                ..Default::default()
            },
        );
        ProtonAccountClient::new(&session).cache.session_secrets()
    }

    async fn passphrase_of(cache: Arc<dyn CacheRepository>) -> Option<Vec<u8>> {
        session_secrets(cache)
            .try_get_account_key_passphrase(&KEY_ID.to_owned())
            .await
            .expect("read")
    }

    async fn store_passphrase(cache: Arc<dyn CacheRepository>, passphrase: &[u8]) {
        session_secrets(cache)
            .set_account_key_passphrase(&KEY_ID.to_owned(), passphrase)
            .await
            .expect("store");
    }

    #[tokio::test]
    async fn the_passphrase_outlives_eviction() {
        let files = files();
        let cache = cache(&files, 8);
        store_passphrase(Arc::clone(&cache), b"secret").await;
        cache
            .set("first", "value".to_owned(), vec![])
            .await
            .expect("set");
        for entry in 0..20 {
            cache
                .set(&format!("node:{entry}"), "value".to_owned(), vec![])
                .await
                .expect("set");
        }

        assert_eq!(cache.try_get("first").await.expect("read"), None);
        assert_eq!(
            passphrase_of(Arc::clone(&cache)).await.as_deref(),
            Some(&b"secret"[..])
        );
    }

    #[tokio::test]
    async fn the_cap_alone_would_lose_it() {
        // What the key store prevents: the bug itself, on the SDK's repository.
        let files = files();
        let capped = sealed(&files.secrets, Some(8));
        store_passphrase(Arc::clone(&capped), b"secret").await;
        for entry in 0..20 {
            capped
                .set(&format!("node:{entry}"), "value".to_owned(), vec![])
                .await
                .expect("set");
        }
        assert_eq!(passphrase_of(capped).await, None);
    }

    #[tokio::test]
    async fn opening_twice_keeps_the_passphrase() {
        let files = files();
        store_passphrase(cache(&files, 8), b"secret").await;
        assert_eq!(
            passphrase_of(cache(&files, 8)).await.as_deref(),
            Some(&b"secret"[..])
        );
    }

    #[tokio::test]
    async fn clearing_keeps_the_passphrase_and_removing_it_works() {
        let files = files();
        let cache = cache(&files, 8);
        store_passphrase(Arc::clone(&cache), b"secret").await;
        cache
            .set("node:1", "value".to_owned(), vec!["tag".to_owned()])
            .await
            .expect("set");

        cache.clear().await.expect("clear");
        assert_eq!(cache.try_get("node:1").await.expect("read"), None);
        assert!(passphrase_of(Arc::clone(&cache)).await.is_some());

        cache
            .remove(&format!("{PASSPHRASE_PREFIX}{KEY_ID}"))
            .await
            .expect("remove");
        assert_eq!(passphrase_of(cache).await, None);
    }
}
