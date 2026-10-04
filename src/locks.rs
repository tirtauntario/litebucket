//! Keyed coordination guards whose registry holds only keys currently in use.
//!
//! Lock order (documented in ADR 0002): upload finalization guard, then the
//! destination per-key commit guard, then a short metadata writer transaction.
//! No guard is ever awaited from inside a database transaction, and at most
//! one key guard is held at a time.

use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

pub struct KeyedLocks<K: Eq + Hash + Clone> {
    map: Arc<Mutex<HashMap<K, Arc<AsyncMutex<()>>>>>,
}

impl<K: Eq + Hash + Clone> Default for KeyedLocks<K> {
    fn default() -> Self {
        Self {
            map: Arc::new(Mutex::new(HashMap::new())),
        }
    }
}

impl<K: Eq + Hash + Clone> KeyedLocks<K> {
    pub async fn lock(&self, key: K) -> KeyGuard<K> {
        let cell = {
            let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            m.entry(key.clone()).or_default().clone()
        };
        let guard = cell.lock_owned().await;
        KeyGuard {
            guard: Some(guard),
            key: Some(key),
            map: self.map.clone(),
        }
    }

    /// Number of keys with a live guard or waiter (bounded by active work).
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct KeyGuard<K: Eq + Hash + Clone> {
    guard: Option<OwnedMutexGuard<()>>,
    key: Option<K>,
    map: Arc<Mutex<HashMap<K, Arc<AsyncMutex<()>>>>>,
}

impl<K: Eq + Hash + Clone> Drop for KeyGuard<K> {
    fn drop(&mut self) {
        drop(self.guard.take());
        if let Some(key) = self.key.take() {
            let mut m = self.map.lock().unwrap_or_else(|e| e.into_inner());
            // Only the registry's own reference remains: nobody holds or waits.
            if m.get(&key).is_some_and(|c| Arc::strong_count(c) == 1) {
                m.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test(flavor = "multi_thread")]
    async fn mutual_exclusion_and_cleanup() {
        let locks = Arc::new(KeyedLocks::<u32>::default());
        let inside = Arc::new(AtomicUsize::new(0));
        let mut tasks = Vec::new();
        for _ in 0..32 {
            let (locks, inside) = (locks.clone(), inside.clone());
            tasks.push(tokio::spawn(async move {
                let _g = locks.lock(7).await;
                assert_eq!(inside.fetch_add(1, Ordering::SeqCst), 0);
                tokio::task::yield_now().await;
                inside.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(locks.is_empty(), "registry must not retain released keys");
        let _a = locks.lock(1).await;
        let _b = locks.lock(2).await;
        assert_eq!(locks.len(), 2);
    }
}
