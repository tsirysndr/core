use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::{Mutex, MutexGuard};

use knot_types::RepoDid;

const REPO_SHARDS: usize = 64;

pub struct CobLocks {
    meta: Mutex<()>,
    repos: Vec<Mutex<()>>,
}

impl Default for CobLocks {
    fn default() -> Self {
        Self {
            meta: Mutex::new(()),
            repos: (0..REPO_SHARDS).map(|_| Mutex::new(())).collect(),
        }
    }
}

impl CobLocks {
    pub fn meta(&self) -> MutexGuard<'_, ()> {
        self.meta
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn repo(&self, repo: &RepoDid) -> MutexGuard<'_, ()> {
        let mut hasher = DefaultHasher::new();
        repo.as_str().hash(&mut hasher);
        let shard = (hasher.finish() as usize) % self.repos.len();
        self.repos[shard]
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
