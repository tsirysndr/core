use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use bobbin_edge_index::HydrantCursor;
use bobbin_runtime::RuntimeHasher;
use bobbin_types::edges::{Edge, Record};
use bobbin_types::ids::RepoIdent;
use bytes::Bytes;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::nsid::Nsid;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::{AtUri, Cid};
use scc::HashMap as SccMap;
use scc::hash_map::Entry;

pub struct ParkedUpsert {
    pub cursor: HydrantCursor,
    pub source: AtUri<DefaultStr>,
    pub nsid: Nsid<DefaultStr>,
    pub parsed: Record,
    pub bytes: Bytes,
    pub cid: Option<Cid<DefaultStr>>,
    pub edges: Vec<Edge>,
    pub supersedes: Option<RepoIdent>,
}

struct EntryState {
    upsert: ParkedUpsert,
    deps: Vec<RepoIdent>,
}

type EntryHandle = Arc<Mutex<Option<EntryState>>>;

pub struct WarmingBuffer {
    by_source: SccMap<AtUri<DefaultStr>, EntryHandle, RuntimeHasher>,
    by_key: SccMap<RepoIdent, Vec<EntryHandle>, RuntimeHasher>,
    hasher: RuntimeHasher,
    sealed: AtomicBool,
    active_parks: AtomicU64,
    enqueued_total: AtomicU64,
    drained_observe_total: AtomicU64,
    drained_promote_total: AtomicU64,
    evicted_total: AtomicU64,
    rejected_after_seal: AtomicU64,
    distinct_keys_seen: AtomicU64,
    current_entries: AtomicU64,
    max_concurrent_entries: AtomicU64,
    dep_enqueued_total: AtomicU64,
    dep_drained_observe_total: AtomicU64,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WarmingBufferSnapshot {
    pub enqueued_total: u64,
    pub drained_observe_total: u64,
    pub drained_promote_total: u64,
    pub evicted_total: u64,
    pub rejected_after_seal: u64,
    pub distinct_keys_seen: u64,
    pub current_entries: u64,
    pub max_concurrent_entries: u64,
    pub dep_enqueued_total: u64,
    pub dep_drained_observe_total: u64,
}

impl WarmingBuffer {
    pub fn new(hasher: RuntimeHasher) -> Self {
        Self {
            by_source: SccMap::with_hasher(hasher.clone()),
            by_key: SccMap::with_hasher(hasher.clone()),
            hasher,
            sealed: AtomicBool::new(false),
            active_parks: AtomicU64::new(0),
            enqueued_total: AtomicU64::new(0),
            drained_observe_total: AtomicU64::new(0),
            drained_promote_total: AtomicU64::new(0),
            evicted_total: AtomicU64::new(0),
            rejected_after_seal: AtomicU64::new(0),
            distinct_keys_seen: AtomicU64::new(0),
            current_entries: AtomicU64::new(0),
            max_concurrent_entries: AtomicU64::new(0),
            dep_enqueued_total: AtomicU64::new(0),
            dep_drained_observe_total: AtomicU64::new(0),
        }
    }

    pub fn hasher(&self) -> &RuntimeHasher {
        &self.hasher
    }

    pub fn is_sealed(&self) -> bool {
        self.sealed.load(Ordering::Acquire)
    }

    pub async fn try_park(
        &self,
        upsert: ParkedUpsert,
        deps: Vec<RepoIdent>,
    ) -> Result<(), ParkedUpsert> {
        self.active_parks.fetch_add(1, Ordering::AcqRel);
        let outcome = if self.is_sealed() {
            self.rejected_after_seal.fetch_add(1, Ordering::Relaxed);
            Err(upsert)
        } else {
            self.park(upsert, deps).await;
            Ok(())
        };
        self.active_parks.fetch_sub(1, Ordering::Release);
        outcome
    }

    async fn park(&self, upsert: ParkedUpsert, deps: Vec<RepoIdent>) {
        debug_assert!(
            !deps.is_empty(),
            "park requires at least one unresolved dep"
        );
        let dep_count = deps.len() as u64;
        let source = upsert.source.clone();
        let handle: EntryHandle = Arc::new(Mutex::new(Some(EntryState {
            upsert,
            deps: deps.clone(),
        })));

        let prior = match self.by_source.entry_async(source).await {
            Entry::Occupied(mut occ) => Some(occ.insert(handle.clone())),
            Entry::Vacant(vac) => {
                vac.insert_entry(handle.clone());
                None
            }
        };
        if let Some(prior) = prior {
            let prior_deps = self.snapshot_deps(&prior);
            if self.deactivate_handle(&prior) {
                self.evicted_total.fetch_add(1, Ordering::Relaxed);
                self.cleanup_by_key(&prior, &prior_deps).await;
            }
        }

        let _ = futures::future::join_all(deps.into_iter().map(|dep| {
            let h = handle.clone();
            async move { self.register_dep(dep, h).await }
        }))
        .await;

        self.enqueued_total.fetch_add(1, Ordering::Relaxed);
        self.dep_enqueued_total
            .fetch_add(dep_count, Ordering::Relaxed);
        let cur = self.current_entries.fetch_add(1, Ordering::Relaxed) + 1;
        self.max_concurrent_entries
            .fetch_max(cur, Ordering::Relaxed);
    }

    async fn register_dep(&self, dep: RepoIdent, handle: EntryHandle) {
        let was_new = match self.by_key.entry_async(dep).await {
            Entry::Occupied(mut occ) => {
                occ.get_mut().push(handle);
                false
            }
            Entry::Vacant(vac) => {
                vac.insert_entry(vec![handle]);
                true
            }
        };
        if was_new {
            self.distinct_keys_seen.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub async fn evict_source(&self, source: &AtUri<DefaultStr>) -> bool {
        let Some((_, handle)) = self.by_source.remove_async(source).await else {
            return false;
        };
        let deps = self.snapshot_deps(&handle);
        if !self.deactivate_handle(&handle) {
            return false;
        }
        self.evicted_total.fetch_add(1, Ordering::Relaxed);
        self.cleanup_by_key(&handle, &deps).await;
        true
    }

    pub async fn take_observed(
        &self,
        owner: &Did<DefaultStr>,
        rkey: &Rkey<DefaultStr>,
    ) -> Vec<ParkedUpsert> {
        let park_key = RepoIdent::new(owner.clone(), rkey.clone());
        let Some((_, handles)) = self.by_key.remove_async(&park_key).await else {
            return Vec::new();
        };
        let mut drained: Vec<ParkedUpsert> = Vec::new();
        let mut sources_to_remove: Vec<AtUri<DefaultStr>> = Vec::new();
        let mut dep_matches: u64 = 0;
        handles.into_iter().for_each(|handle| {
            let outcome = {
                let mut slot = handle.lock().expect("warming buffer entry mutex poisoned");
                match slot.as_mut() {
                    Some(state) => {
                        let before = state.deps.len();
                        state.deps.retain(|d| d != &park_key);
                        let after = state.deps.len();
                        Some((before > after, after == 0))
                    }
                    None => None,
                }
            };
            let Some((matched, drain_now)) = outcome else {
                return;
            };
            if matched {
                dep_matches += 1;
            }
            if drain_now {
                let mut slot = handle.lock().expect("warming buffer entry mutex poisoned");
                if let Some(state) = slot.take() {
                    sources_to_remove.push(state.upsert.source.clone());
                    drained.push(state.upsert);
                }
            }
        });
        if dep_matches > 0 {
            self.dep_drained_observe_total
                .fetch_add(dep_matches, Ordering::Relaxed);
        }
        let drained_count = drained.len() as u64;
        if drained_count > 0 {
            self.drained_observe_total
                .fetch_add(drained_count, Ordering::Relaxed);
            self.current_entries
                .fetch_sub(drained_count, Ordering::Relaxed);
            let _ = futures::future::join_all(
                sources_to_remove
                    .into_iter()
                    .map(|source| async move { self.by_source.remove_async(&source).await }),
            )
            .await;
        }
        drained
    }

    pub async fn drain_for_promote(&self) -> Vec<(ParkedUpsert, Vec<RepoIdent>)> {
        self.sealed.store(true, Ordering::Release);
        while self.active_parks.load(Ordering::Acquire) > 0 {
            tokio::task::yield_now().await;
        }
        let mut drained: Vec<(ParkedUpsert, Vec<RepoIdent>)> = Vec::new();
        self.by_source
            .retain_async(|_source, handle| {
                let mut slot = handle.lock().expect("warming buffer entry mutex poisoned");
                if let Some(state) = slot.take() {
                    drained.push((state.upsert, state.deps));
                }
                false
            })
            .await;
        let count = drained.len() as u64;
        if count > 0 {
            self.drained_promote_total
                .fetch_add(count, Ordering::Relaxed);
            self.current_entries.fetch_sub(count, Ordering::Relaxed);
        }
        self.by_key.clear_async().await;
        drained
    }

    pub fn snapshot(&self) -> WarmingBufferSnapshot {
        WarmingBufferSnapshot {
            enqueued_total: self.enqueued_total.load(Ordering::Relaxed),
            drained_observe_total: self.drained_observe_total.load(Ordering::Relaxed),
            drained_promote_total: self.drained_promote_total.load(Ordering::Relaxed),
            evicted_total: self.evicted_total.load(Ordering::Relaxed),
            rejected_after_seal: self.rejected_after_seal.load(Ordering::Relaxed),
            distinct_keys_seen: self.distinct_keys_seen.load(Ordering::Relaxed),
            current_entries: self.current_entries.load(Ordering::Relaxed),
            max_concurrent_entries: self.max_concurrent_entries.load(Ordering::Relaxed),
            dep_enqueued_total: self.dep_enqueued_total.load(Ordering::Relaxed),
            dep_drained_observe_total: self.dep_drained_observe_total.load(Ordering::Relaxed),
        }
    }

    fn snapshot_deps(&self, handle: &EntryHandle) -> Vec<RepoIdent> {
        let slot = handle.lock().expect("warming buffer entry mutex poisoned");
        slot.as_ref().map(|s| s.deps.clone()).unwrap_or_default()
    }

    fn deactivate_handle(&self, handle: &EntryHandle) -> bool {
        let mut slot = handle.lock().expect("warming buffer entry mutex poisoned");
        if slot.take().is_some() {
            self.current_entries.fetch_sub(1, Ordering::Relaxed);
            true
        } else {
            false
        }
    }

    async fn cleanup_by_key(&self, handle: &EntryHandle, deps: &[RepoIdent]) {
        let _ = futures::future::join_all(deps.iter().map(|dep| async move {
            let Some(mut occupied) = self.by_key.get_async(dep).await else {
                return;
            };
            occupied.get_mut().retain(|h| !Arc::ptr_eq(h, handle));
            if occupied.get().is_empty() {
                let _ = occupied.remove_entry();
            }
        }))
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_types::ids::nsid_static;

    fn d(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn r(s: &str) -> Rkey<DefaultStr> {
        Rkey::new_owned(s).unwrap()
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn make_upsert(source: &str) -> ParkedUpsert {
        let source_uri = at(source);
        let star_json = br#"{"$type":"sh.tangled.feed.star","createdAt":"2026-05-01T00:00:00Z","subject":{"$type":"sh.tangled.feed.star#repo","did":"did:plc:abalone"}}"#;
        let parsed = Record::from_json_bytes(&nsid_static("sh.tangled.feed.star"), star_json)
            .expect("star fixture parses");
        ParkedUpsert {
            cursor: HydrantCursor::new(1),
            source: source_uri,
            nsid: nsid_static("sh.tangled.feed.star"),
            parsed,
            bytes: Bytes::from_static(star_json),
            cid: None,
            edges: Vec::new(),
            supersedes: None,
        }
    }

    #[tokio::test]
    async fn park_then_observe_drains_to_zero() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let upsert = make_upsert("at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz");
        buf.park(upsert, vec![dep.clone()]).await;
        let drained = buf.take_observed(&dep.owner, &dep.rkey).await;
        assert_eq!(drained.len(), 1);
        let s = buf.snapshot();
        assert_eq!(s.enqueued_total, 1);
        assert_eq!(s.drained_observe_total, 1);
        assert_eq!(s.distinct_keys_seen, 1);
        assert_eq!(s.current_entries, 0);
        assert_eq!(s.max_concurrent_entries, 1);
        assert_eq!(s.dep_enqueued_total, 1);
        assert_eq!(s.dep_drained_observe_total, 1);
    }

    #[tokio::test]
    async fn distinct_keys_tracked_separately() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep_a = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let dep_b = RepoIdent::new(d("did:plc:olaren"), r("abcabcabcabd1"));
        buf.park(
            make_upsert("at://did:plc:starer1/sh.tangled.feed.star/aaaaaaaaaaaaa"),
            vec![dep_a.clone()],
        )
        .await;
        buf.park(
            make_upsert("at://did:plc:starer2/sh.tangled.feed.star/bbbbbbbbbbbbb"),
            vec![dep_b.clone()],
        )
        .await;
        let s = buf.snapshot();
        assert_eq!(s.enqueued_total, 2);
        assert_eq!(s.distinct_keys_seen, 2);
        assert_eq!(s.current_entries, 2);
        assert_eq!(s.max_concurrent_entries, 2);
        assert_eq!(s.dep_enqueued_total, 2);
    }

    #[tokio::test]
    async fn observe_unrelated_key_is_noop() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        buf.park(
            make_upsert("at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz"),
            vec![RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"))],
        )
        .await;
        let drained = buf
            .take_observed(&d("did:plc:olaren"), &r("nopenopenopep"))
            .await;
        assert!(drained.is_empty());
        let s = buf.snapshot();
        assert_eq!(s.current_entries, 1);
        assert_eq!(s.drained_observe_total, 0);
        assert_eq!(s.dep_drained_observe_total, 0);
    }

    #[tokio::test]
    async fn evict_on_replacement_clears_prior_entry() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let source = "at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz";
        buf.park(make_upsert(source), vec![dep.clone()]).await;
        let evicted = buf.evict_source(&at(source)).await;
        assert!(evicted);
        let drained = buf.take_observed(&dep.owner, &dep.rkey).await;
        assert!(
            drained.is_empty(),
            "evicted entry must not surface on observe",
        );
        let s = buf.snapshot();
        assert_eq!(s.current_entries, 0);
        assert_eq!(s.evicted_total, 1);
    }

    #[tokio::test]
    async fn evict_clears_by_key_so_observe_walks_no_handles() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep_a = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let dep_b = RepoIdent::new(d("did:plc:olaren"), r("abcabcabcabd1"));
        let source = "at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz";
        buf.park(make_upsert(source), vec![dep_a.clone(), dep_b.clone()])
            .await;
        assert!(buf.evict_source(&at(source)).await);
        let drained_a = buf.take_observed(&dep_a.owner, &dep_a.rkey).await;
        let drained_b = buf.take_observed(&dep_b.owner, &dep_b.rkey).await;
        assert!(drained_a.is_empty());
        assert!(drained_b.is_empty());
        let s = buf.snapshot();
        assert_eq!(s.dep_drained_observe_total, 0);
    }

    #[tokio::test]
    async fn replacement_park_under_same_source_increments_evicted_total() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let source = "at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz";
        buf.park(make_upsert(source), vec![dep.clone()]).await;
        buf.park(make_upsert(source), vec![dep.clone()]).await;
        let s = buf.snapshot();
        assert_eq!(s.enqueued_total, 2);
        assert_eq!(s.evicted_total, 1);
        assert_eq!(s.current_entries, 1);
    }

    #[tokio::test]
    async fn replacement_park_does_not_double_drain_on_observe() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let source = "at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz";
        buf.park(make_upsert(source), vec![dep.clone()]).await;
        buf.park(make_upsert(source), vec![dep.clone()]).await;
        let drained = buf.take_observed(&dep.owner, &dep.rkey).await;
        assert_eq!(
            drained.len(),
            1,
            "evicted handle must not surface alongside the live one",
        );
        let s = buf.snapshot();
        assert_eq!(s.drained_observe_total, 1);
        assert_eq!(s.current_entries, 0);
    }

    #[tokio::test]
    async fn drain_for_promote_returns_residual() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        buf.park(
            make_upsert("at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz"),
            vec![dep],
        )
        .await;
        let residual = buf.drain_for_promote().await;
        assert_eq!(residual.len(), 1);
        let s = buf.snapshot();
        assert_eq!(s.drained_promote_total, 1);
        assert_eq!(s.current_entries, 0);
    }

    #[tokio::test]
    async fn try_park_after_drain_for_promote_is_rejected() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        let _ = buf.drain_for_promote().await;
        let upsert = make_upsert("at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz");
        let res = buf.try_park(upsert, vec![dep]).await;
        assert!(res.is_err(), "post-seal try_park must reject");
        let s = buf.snapshot();
        assert_eq!(s.rejected_after_seal, 1);
        assert_eq!(s.enqueued_total, 0);
        assert_eq!(s.current_entries, 0);
    }

    #[tokio::test]
    async fn double_observe_after_drain_does_not_double_count() {
        let hasher = RuntimeHasher::default();
        let buf = WarmingBuffer::new(hasher);
        let dep = RepoIdent::new(d("did:plc:nel"), r("abcabcabcabcz"));
        buf.park(
            make_upsert("at://did:plc:starer/sh.tangled.feed.star/zzzzzzzzzzzzz"),
            vec![dep.clone()],
        )
        .await;
        let first = buf.take_observed(&dep.owner, &dep.rkey).await;
        let second = buf.take_observed(&dep.owner, &dep.rkey).await;
        assert_eq!(first.len(), 1);
        assert!(second.is_empty());
        let s = buf.snapshot();
        assert_eq!(s.drained_observe_total, 1);
        assert_eq!(s.dep_drained_observe_total, 1);
    }
}
