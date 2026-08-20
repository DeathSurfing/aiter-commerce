//! Storage abstraction.
//!
//! [`Store`] is a minimal create / get / update trait so persistence can be
//! swapped in later (a real database, files, etc.). [`InMemoryStore`] is a
//! `HashMap`-backed implementation used today and for tests.

use std::collections::HashMap;

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
}
