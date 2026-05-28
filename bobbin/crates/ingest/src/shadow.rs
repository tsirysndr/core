use std::sync::atomic::{AtomicU64, Ordering};

use bobbin_runtime::RuntimeHasher;
use bobbin_types::ids::RepoIdent;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::recordkey::Rkey;
use scc::HashMap as SccMap;

pub struct WarmingShadowBuffer {
    pending: SccMap<RepoIdent, u64, RuntimeHasher>,
    enqueued_total: AtomicU64,
    drained_via_observe_total: AtomicU64,
    max_concurrent: AtomicU64,
    current_concurrent: AtomicU64,
    distinct_keys_seen: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WarmingShadowSnapshot {
    pub enqueued_total: u64,
    pub drained_via_observe_total: u64,
    pub max_concurrent: u64,
    pub residual: u64,
    pub distinct_keys_seen: u64,
}

impl WarmingShadowBuffer {
    pub fn new(hasher: RuntimeHasher) -> Self {
        Self {
            pending: SccMap::with_hasher(hasher),
            enqueued_total: AtomicU64::new(0),
            drained_via_observe_total: AtomicU64::new(0),
            max_concurrent: AtomicU64::new(0),
            current_concurrent: AtomicU64::new(0),
            distinct_keys_seen: AtomicU64::new(0),
        }
    }

    pub async fn note_unresolved(&self, owner: Did<DefaultStr>, rkey: Rkey<DefaultStr>) {
        let key = RepoIdent::new(owner, rkey);
        let mut entry = self.pending.entry_async(key).await.or_insert_with(|| {
            self.distinct_keys_seen.fetch_add(1, Ordering::Relaxed);
            0
        });
        *entry.get_mut() += 1;
        self.enqueued_total.fetch_add(1, Ordering::Relaxed);
        let cur = self.current_concurrent.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_concurrent.fetch_max(cur, Ordering::Relaxed);
    }

    pub async fn note_observed(&self, owner: &Did<DefaultStr>, rkey: &Rkey<DefaultStr>) {
        let key = RepoIdent::new(owner.clone(), rkey.clone());
        let Some(mut entry) = self.pending.get_async(&key).await else {
            return;
        };
        let count = std::mem::replace(entry.get_mut(), 0);
        if count > 0 {
            self.drained_via_observe_total
                .fetch_add(count, Ordering::Relaxed);
            self.current_concurrent.fetch_sub(count, Ordering::Relaxed);
        }
    }

    pub fn snapshot(&self) -> WarmingShadowSnapshot {
        WarmingShadowSnapshot {
            enqueued_total: self.enqueued_total.load(Ordering::Relaxed),
            drained_via_observe_total: self.drained_via_observe_total.load(Ordering::Relaxed),
            max_concurrent: self.max_concurrent.load(Ordering::Relaxed),
            residual: self.current_concurrent.load(Ordering::Relaxed),
            distinct_keys_seen: self.distinct_keys_seen.load(Ordering::Relaxed),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jacquard_common::types::did::Did;
    use jacquard_common::types::recordkey::Rkey;

    fn d(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn r(s: &str) -> Rkey<DefaultStr> {
        Rkey::new_owned(s).unwrap()
    }

    #[tokio::test]
    async fn enqueue_then_observe_drains_to_zero() {
        let shadow = WarmingShadowBuffer::new(RuntimeHasher::default());
        shadow
            .note_unresolved(d("did:plc:nel"), r("abcabcabcabcz"))
            .await;
        shadow
            .note_unresolved(d("did:plc:nel"), r("abcabcabcabcz"))
            .await;
        shadow
            .note_observed(&d("did:plc:nel"), &r("abcabcabcabcz"))
            .await;
        let s = shadow.snapshot();
        assert_eq!(s.enqueued_total, 2);
        assert_eq!(s.drained_via_observe_total, 2);
        assert_eq!(s.max_concurrent, 2);
        assert_eq!(s.residual, 0);
        assert_eq!(s.distinct_keys_seen, 1);
    }

    #[tokio::test]
    async fn distinct_keys_tracked_separately() {
        let shadow = WarmingShadowBuffer::new(RuntimeHasher::default());
        shadow
            .note_unresolved(d("did:plc:nel"), r("abcabcabcabcz"))
            .await;
        shadow
            .note_unresolved(d("did:plc:olaren"), r("abcabcabcabd1"))
            .await;
        let s = shadow.snapshot();
        assert_eq!(s.enqueued_total, 2);
        assert_eq!(s.distinct_keys_seen, 2);
        assert_eq!(s.max_concurrent, 2);
        assert_eq!(s.residual, 2);
    }

    #[tokio::test]
    async fn observe_without_prior_enqueue_is_a_noop() {
        let shadow = WarmingShadowBuffer::new(RuntimeHasher::default());
        shadow
            .note_observed(&d("did:plc:nel"), &r("abcabcabcabcz"))
            .await;
        let s = shadow.snapshot();
        assert_eq!(s.enqueued_total, 0);
        assert_eq!(s.drained_via_observe_total, 0);
        assert_eq!(s.distinct_keys_seen, 0);
    }

    #[tokio::test]
    async fn second_observe_after_drain_does_not_double_count() {
        let shadow = WarmingShadowBuffer::new(RuntimeHasher::default());
        shadow
            .note_unresolved(d("did:plc:nel"), r("abcabcabcabcz"))
            .await;
        shadow
            .note_observed(&d("did:plc:nel"), &r("abcabcabcabcz"))
            .await;
        shadow
            .note_observed(&d("did:plc:nel"), &r("abcabcabcabcz"))
            .await;
        let s = shadow.snapshot();
        assert_eq!(s.drained_via_observe_total, 1);
        assert_eq!(s.residual, 0);
    }

    #[tokio::test]
    async fn residual_reflects_unobserved_keys() {
        let shadow = WarmingShadowBuffer::new(RuntimeHasher::default());
        shadow
            .note_unresolved(d("did:plc:nel"), r("abcabcabcabcz"))
            .await;
        shadow
            .note_unresolved(d("did:plc:olaren"), r("abcabcabcabd1"))
            .await;
        shadow
            .note_observed(&d("did:plc:nel"), &r("abcabcabcabcz"))
            .await;
        let s = shadow.snapshot();
        assert_eq!(s.enqueued_total, 2);
        assert_eq!(s.drained_via_observe_total, 1);
        assert_eq!(s.residual, 1);
    }
}
