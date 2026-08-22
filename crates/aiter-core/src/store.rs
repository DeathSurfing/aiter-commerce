//! Storage abstraction.
//!
//! [`Store`] is a minimal create / get / update trait shared by all backends.
//! [`InMemoryStore`] is a `HashMap`-backed implementation used by the server
//! today; [`SledStore`] is a durable, sled-backed implementation that
//! survives restarts and drops into any [`Store`] caller unchanged (issue #32).

use std::collections::HashMap;
use std::path::Path;

use serde::de::DeserializeOwned;
use serde::Serialize;

/// A key-value store with create / get / update semantics.
pub trait Store {
    type Key: Clone + Eq + std::hash::Hash;
    type Item;
    type Error;

    /// Insert a brand-new item. Fails if the key already exists.
    fn create(&mut self, key: Self::Key, item: Self::Item) -> Result<(), Self::Error>;

    /// Fetch an item by key.
    fn get(&self, key: &Self::Key) -> Option<&Self::Item>;

    /// Replace an existing item. Fails if the key does not exist.
    fn update(&mut self, key: Self::Key, item: Self::Item) -> Result<(), Self::Error>;
}

/// Errors common to in-memory storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StoreError {
    /// `create` with an already-present key.
    AlreadyExists,
    /// `update` with a missing key.
    NotFound,
}

/// `HashMap`-backed [`Store`] implementation.
#[derive(Debug, Default, Clone)]
pub struct InMemoryStore<K, V> {
    map: HashMap<K, V>,
}

impl<K, V> InMemoryStore<K, V> {
    pub fn new() -> Self {
        InMemoryStore {
            map: HashMap::new(),
        }
    }

    /// Number of stored items.
    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

impl<K: Clone + Eq + std::hash::Hash, V> Store for InMemoryStore<K, V> {
    type Key = K;
    type Item = V;
    type Error = StoreError;

    fn create(&mut self, key: K, item: V) -> Result<(), StoreError> {
        if self.map.contains_key(&key) {
            return Err(StoreError::AlreadyExists);
        }
        self.map.insert(key, item);
        Ok(())
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.map.get(key)
    }

    fn update(&mut self, key: K, item: V) -> Result<(), StoreError> {
        if !self.map.contains_key(&key) {
            return Err(StoreError::NotFound);
        }
        self.map.insert(key, item);
        Ok(())
    }
}

/// Errors from the sled-backed store.
///
/// The create/update semantic failures reuse [`StoreError`], shared with
/// [`InMemoryStore`]; storage and serialization failures carry the underlying
/// error instead of being swallowed.
#[derive(Debug)]
pub enum SledStoreError {
    /// `create` with an already-present key, or `update` with a missing key.
    Store(StoreError),
    /// sled storage failure (I/O, corruption, closed database).
    Sled(sled::Error),
    /// key/value (de)serialization failure.
    Serde(serde_json::Error),
}

/// sled-backed durable [`Store`] (issue #32).
///
/// sled is an embedded, pure-Rust key-value store; keys and values are stored
/// `serde_json`-encoded. [`Store::get`] must return `&Item`, so live values
/// also sit in an in-memory write-through cache, while sled stays the source
/// of truth on disk: every mutation is written to `db` first, and the cache
/// is reloaded from `db` on open. Cart/order/consent volumes are small, so
/// loading the whole tree at open is fine.
///
/// ```no_run
/// use aiter_core::store::{SledStore, SledStoreError, Store};
///
/// # fn main() -> Result<(), SledStoreError> {
/// let mut store: SledStore<String, u32> = SledStore::new("/tmp/my-store")?;
/// store.create("cart-1".to_string(), 42)?; // fails if the key exists
/// store.update("cart-1".to_string(), 43)?; // fails if the key is missing
/// assert_eq!(store.get(&"cart-1".to_string()), Some(&43));
/// store.close().map_err(SledStoreError::Sled)?; // flush; reopening keeps the data
/// # Ok(())
/// # }
/// ```
#[derive(Debug)]
pub struct SledStore<K, V> {
    db: sled::Db,
    /// Write-through cache: [`Store::get`] returns `&Item`, so values must
    /// live in memory; every mutation hits `db` first, then this cache.
    cache: HashMap<K, V>,
}

impl<K: DeserializeOwned + Eq + std::hash::Hash, V: DeserializeOwned> SledStore<K, V> {
    /// Open (creating if needed) a durable store at `path`.
    ///
    /// All existing entries are loaded into memory; sled keeps the canonical
    /// copy on disk.
    pub fn new(path: impl AsRef<Path>) -> Result<Self, SledStoreError> {
        let db = sled::open(path).map_err(SledStoreError::Sled)?;
        let mut cache = HashMap::new();
        for entry in db.iter() {
            let (key, value) = entry.map_err(SledStoreError::Sled)?;
            let key = serde_json::from_slice(&key).map_err(SledStoreError::Serde)?;
            let value = serde_json::from_slice(&value).map_err(SledStoreError::Serde)?;
            cache.insert(key, value);
        }
        Ok(SledStore { db, cache })
    }
}

impl<K, V> SledStore<K, V> {
    /// Flush all writes to disk and close the database. The store can be
    /// reopened at the same path with all data intact.
    pub fn close(self) -> Result<(), sled::Error> {
        self.db.flush()?; // the drop that follows closes the database
        Ok(())
    }
}

impl<K, V> Store for SledStore<K, V>
where
    K: Serialize + DeserializeOwned + Clone + Eq + std::hash::Hash,
    V: Serialize + DeserializeOwned,
{
    type Key = K;
    type Item = V;
    type Error = SledStoreError;

    fn create(&mut self, key: K, item: V) -> Result<(), SledStoreError> {
        if self.cache.contains_key(&key) {
            return Err(SledStoreError::Store(StoreError::AlreadyExists));
        }
        let key_bytes = serde_json::to_vec(&key).map_err(SledStoreError::Serde)?;
        let item_bytes = serde_json::to_vec(&item).map_err(SledStoreError::Serde)?;
        self.db
            .insert(key_bytes, item_bytes)
            .map_err(SledStoreError::Sled)?;
        self.cache.insert(key, item);
        Ok(())
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.cache.get(key)
    }

    fn update(&mut self, key: K, item: V) -> Result<(), SledStoreError> {
        if !self.cache.contains_key(&key) {
            return Err(SledStoreError::Store(StoreError::NotFound));
        }
        let key_bytes = serde_json::to_vec(&key).map_err(SledStoreError::Serde)?;
        let item_bytes = serde_json::to_vec(&item).map_err(SledStoreError::Serde)?;
        self.db
            .insert(key_bytes, item_bytes)
            .map_err(SledStoreError::Sled)?;
        self.cache.insert(key, item);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_get_update_cycle() {
        let mut store: InMemoryStore<String, u32> = InMemoryStore::new();
        store.create("a".to_string(), 1).unwrap();
        assert_eq!(store.get(&"a".to_string()), Some(&1));

        store.update("a".to_string(), 2).unwrap();
        assert_eq!(store.get(&"a".to_string()), Some(&2));
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn create_refuses_duplicate_key() {
        let mut store: InMemoryStore<String, u32> = InMemoryStore::new();
        store.create("a".to_string(), 1).unwrap();
        assert_eq!(
            store.create("a".to_string(), 2),
            Err(StoreError::AlreadyExists)
        );
        // original value is untouched
        assert_eq!(store.get(&"a".to_string()), Some(&1));
    }

    #[test]
    fn update_requires_existing_key() {
        let mut store: InMemoryStore<String, u32> = InMemoryStore::new();
        assert_eq!(store.update("a".to_string(), 1), Err(StoreError::NotFound));
        assert_eq!(store.get(&"a".to_string()), None);
    }

    #[test]
    fn stores_arbitrary_key_and_value_types() {
        let mut store: InMemoryStore<u32, String> = InMemoryStore::new();
        store.create(7, "seven".to_string()).unwrap();
        assert_eq!(store.get(&7), Some(&"seven".to_string()));
        assert!(!store.is_empty());
    }

    // --- SledStore ---

    /// Fresh directory under the system temp dir, unique per process/run so
    /// tests never collide with a previous run's data.
    fn unique_temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "aiter-sled-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn sled_store_survives_reopen() {
        let dir = unique_temp_dir();
        {
            let mut store: SledStore<String, u32> = SledStore::new(&dir).unwrap();
            store.create("cart".to_string(), 42).unwrap();
            store.update("cart".to_string(), 99).unwrap();
            store.create("order".to_string(), 7).unwrap();
            assert_eq!(store.get(&"cart".to_string()), Some(&99));
            store.close().unwrap();
        }
        // Reopen at the same path: everything written above must still be there.
        let mut store: SledStore<String, u32> = SledStore::new(&dir).unwrap();
        assert_eq!(store.get(&"cart".to_string()), Some(&99));
        assert_eq!(store.get(&"order".to_string()), Some(&7));
        // create still refuses to re-insert a persisted key
        assert!(
            matches!(
                store.create("cart".to_string(), 1),
                Err(SledStoreError::Store(StoreError::AlreadyExists))
            ),
            "create must refuse a persisted key"
        );
        // update still works on persisted data
        store.update("cart".to_string(), 100).unwrap();
        assert_eq!(store.get(&"cart".to_string()), Some(&100));
        store.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sled_create_refuses_duplicate_key() {
        let dir = unique_temp_dir();
        let mut store: SledStore<String, u32> = SledStore::new(&dir).unwrap();
        store.create("a".to_string(), 1).unwrap();
        assert!(
            matches!(
                store.create("a".to_string(), 2),
                Err(SledStoreError::Store(StoreError::AlreadyExists))
            ),
            "create must refuse a duplicate key"
        );
        // original value is untouched
        assert_eq!(store.get(&"a".to_string()), Some(&1));
        store.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn sled_update_requires_existing_key() {
        let dir = unique_temp_dir();
        let mut store: SledStore<String, u32> = SledStore::new(&dir).unwrap();
        assert!(
            matches!(
                store.update("a".to_string(), 1),
                Err(SledStoreError::Store(StoreError::NotFound))
            ),
            "update must refuse a missing key"
        );
        assert_eq!(store.get(&"a".to_string()), None);
        store.close().unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
