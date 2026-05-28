use std::collections::BTreeMap;
use std::sync::Mutex;

use bobbin_runtime::RuntimeHasher;
use bobbin_types::edges::Record;
use bobbin_types::sh_tangled::repo::issue::state::StateState;
use bobbin_types::sh_tangled::repo::pull::status::StatusStatus;
use jacquard_common::DefaultStr;
use jacquard_common::types::string::AtUri;
use scc::HashMap as SccMap;
use scc::hash_map::Entry;

pub trait StateKind: Copy + Eq + std::fmt::Debug + Send + Sync + 'static {
    fn wire(self) -> &'static str;
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum IssueStateKind {
    #[default]
    Open,
    Closed,
}

impl StateKind for IssueStateKind {
    fn wire(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, Hash, PartialEq)]
pub enum PullStatusKind {
    #[default]
    Open,
    Closed,
    Merged,
}

impl StateKind for PullStatusKind {
    fn wire(self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Closed => "closed",
            Self::Merged => "merged",
        }
    }
}

#[derive(Clone, Debug)]
struct ReverseRef<V: StateKind> {
    entity: AtUri<DefaultStr>,
    sort_micros: u64,
    kind: V,
}

#[derive(Clone, Debug)]
struct SortableSource(AtUri<DefaultStr>);

impl PartialEq for SortableSource {
    fn eq(&self, other: &Self) -> bool {
        self.0.as_ref() == other.0.as_ref()
    }
}

impl Eq for SortableSource {}

impl PartialOrd for SortableSource {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SortableSource {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.as_ref().cmp(other.0.as_ref())
    }
}

type ForwardKey = (u64, SortableSource);

pub struct StateIndex<V: StateKind> {
    forward: SccMap<AtUri<DefaultStr>, BTreeMap<ForwardKey, V>, RuntimeHasher>,
    reverse: SccMap<AtUri<DefaultStr>, ReverseRef<V>, RuntimeHasher>,
    writer: Mutex<()>,
}

impl<V: StateKind> StateIndex<V> {
    pub fn new(hasher: RuntimeHasher) -> Self {
        Self {
            forward: SccMap::with_hasher(hasher.clone()),
            reverse: SccMap::with_hasher(hasher),
            writer: Mutex::new(()),
        }
    }

    pub fn upsert(
        &self,
        source: AtUri<DefaultStr>,
        entity: AtUri<DefaultStr>,
        sort_micros: u64,
        kind: V,
    ) {
        let _w = self
            .writer
            .lock()
            .expect("state-index writer mutex poisoned");
        let rev = self.reverse.entry_sync(source.clone());
        let prior = match &rev {
            Entry::Occupied(occ) => Some((occ.get().entity.clone(), occ.get().sort_micros)),
            Entry::Vacant(_) => None,
        };
        if let Some((prior_entity, prior_micros)) = prior.as_ref()
            && (prior_entity != &entity || *prior_micros != sort_micros)
        {
            self.remove_forward(prior_entity, *prior_micros, &source);
        }
        self.insert_forward(entity.clone(), sort_micros, kind, source);
        match rev {
            Entry::Occupied(mut occ) => {
                let slot = occ.get_mut();
                slot.entity = entity;
                slot.sort_micros = sort_micros;
                slot.kind = kind;
            }
            Entry::Vacant(vac) => {
                vac.insert_entry(ReverseRef {
                    entity,
                    sort_micros,
                    kind,
                });
            }
        }
    }

    pub fn remove_source(&self, source: &AtUri<DefaultStr>) {
        let _w = self
            .writer
            .lock()
            .expect("state-index writer mutex poisoned");
        let Entry::Occupied(occ) = self.reverse.entry_sync(source.clone()) else {
            return;
        };
        let entity = occ.get().entity.clone();
        let sort_micros = occ.get().sort_micros;
        let _ = occ.remove();
        self.remove_forward(&entity, sort_micros, source);
    }

    pub fn remove_entity(&self, entity: &AtUri<DefaultStr>) {
        let _w = self
            .writer
            .lock()
            .expect("state-index writer mutex poisoned");
        let Some((_, set)) = self.forward.remove_sync(entity) else {
            return;
        };
        set.into_iter().for_each(|((_, src), _)| {
            let Entry::Occupied(occ) = self.reverse.entry_sync(src.0.clone()) else {
                return;
            };
            if occ.get().entity == *entity {
                let _ = occ.remove();
            }
        });
    }

    fn insert_forward(
        &self,
        entity: AtUri<DefaultStr>,
        sort_micros: u64,
        kind: V,
        source: AtUri<DefaultStr>,
    ) {
        let mut entry = self.forward.entry_sync(entity).or_default();
        entry
            .get_mut()
            .insert((sort_micros, SortableSource(source)), kind);
    }

    fn remove_forward(
        &self,
        entity: &AtUri<DefaultStr>,
        sort_micros: u64,
        source: &AtUri<DefaultStr>,
    ) {
        let Entry::Occupied(mut entry) = self.forward.entry_sync(entity.clone()) else {
            return;
        };
        entry
            .get_mut()
            .remove(&(sort_micros, SortableSource(source.clone())));
        if entry.get().is_empty() {
            let _ = entry.remove();
        }
    }

    pub fn latest(&self, entity: &AtUri<DefaultStr>) -> Option<(V, u64)> {
        self.latest_by(entity, |_| true)
    }

    pub fn latest_by<F>(&self, entity: &AtUri<DefaultStr>, accept: F) -> Option<(V, u64)>
    where
        F: Fn(&AtUri<DefaultStr>) -> bool,
    {
        self.forward
            .read_sync(entity, |_, set| {
                set.iter()
                    .rev()
                    .find_map(|((m, src), k)| accept(&src.0).then_some((*k, *m)))
            })
            .flatten()
    }

    pub fn entity_count(&self) -> usize {
        self.forward.len()
    }

    pub fn source_count(&self) -> usize {
        self.reverse.len()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyOutcome {
    Applied,
    UnknownVariant,
    NotStateRecord,
    Removed,
}

fn issue_kind_from(s: &StateState<DefaultStr>) -> Option<IssueStateKind> {
    match s {
        StateState::ShTangledRepoIssueStateOpen => Some(IssueStateKind::Open),
        StateState::ShTangledRepoIssueStateClosed => Some(IssueStateKind::Closed),
        StateState::Other(_) => None,
    }
}

fn pull_kind_from(s: &StatusStatus<DefaultStr>) -> Option<PullStatusKind> {
    match s {
        StatusStatus::ShTangledRepoPullStatusOpen => Some(PullStatusKind::Open),
        StatusStatus::ShTangledRepoPullStatusClosed => Some(PullStatusKind::Closed),
        StatusStatus::ShTangledRepoPullStatusMerged => Some(PullStatusKind::Merged),
        StatusStatus::Other(_) => None,
    }
}

pub fn apply_record_state(
    issue_idx: &StateIndex<IssueStateKind>,
    pull_idx: &StateIndex<PullStatusKind>,
    source: &AtUri<DefaultStr>,
    record: &Record,
) -> ApplyOutcome {
    let sort_micros = record.sort_micros_for(source);
    match record {
        Record::IssueState(r) => match issue_kind_from(&r.state) {
            Some(kind) => {
                issue_idx.upsert(source.clone(), r.issue.clone(), sort_micros, kind);
                ApplyOutcome::Applied
            }
            None => ApplyOutcome::UnknownVariant,
        },
        Record::PullStatus(r) => match pull_kind_from(&r.status) {
            Some(kind) => {
                pull_idx.upsert(source.clone(), r.pull.clone(), sort_micros, kind);
                ApplyOutcome::Applied
            }
            None => ApplyOutcome::UnknownVariant,
        },
        _ => ApplyOutcome::NotStateRecord,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn idx() -> StateIndex<IssueStateKind> {
        StateIndex::new(RuntimeHasher::default())
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    #[test]
    fn latest_returns_largest_sort_micros() {
        let s = idx();
        let issue = at("at://did:plc:limpet/sh.tangled.repo.issue/i1");
        s.upsert(
            at("at://did:plc:nel/sh.tangled.repo.issue.state/s1"),
            issue.clone(),
            100,
            IssueStateKind::Open,
        );
        s.upsert(
            at("at://did:plc:nel/sh.tangled.repo.issue.state/s2"),
            issue.clone(),
            200,
            IssueStateKind::Closed,
        );
        assert_eq!(s.latest(&issue), Some((IssueStateKind::Closed, 200)));
    }

    #[test]
    fn remove_source_drops_entry_and_recovers_prior() {
        let s = idx();
        let issue = at("at://did:plc:limpet/sh.tangled.repo.issue/i1");
        let s1 = at("at://did:plc:nel/sh.tangled.repo.issue.state/s1");
        let s2 = at("at://did:plc:nel/sh.tangled.repo.issue.state/s2");
        s.upsert(s1.clone(), issue.clone(), 100, IssueStateKind::Open);
        s.upsert(s2.clone(), issue.clone(), 200, IssueStateKind::Closed);
        s.remove_source(&s2);
        assert_eq!(s.latest(&issue), Some((IssueStateKind::Open, 100)));
        s.remove_source(&s1);
        assert!(s.latest(&issue).is_none());
        assert_eq!(s.entity_count(), 0);
        assert_eq!(s.source_count(), 0);
    }

    #[test]
    fn upsert_same_source_replaces() {
        let s = idx();
        let issue = at("at://did:plc:limpet/sh.tangled.repo.issue/i1");
        let src = at("at://did:plc:nel/sh.tangled.repo.issue.state/s1");
        s.upsert(src.clone(), issue.clone(), 100, IssueStateKind::Open);
        s.upsert(src.clone(), issue.clone(), 100, IssueStateKind::Closed);
        assert_eq!(s.latest(&issue), Some((IssueStateKind::Closed, 100)));
        assert_eq!(s.source_count(), 1);
    }

    #[test]
    fn distinct_sources_with_identical_micros_and_kind_survive_individual_removal() {
        let s = idx();
        let issue = at("at://did:plc:limpet/sh.tangled.repo.issue/i1");
        let s1 = at("at://did:plc:nel/sh.tangled.repo.issue.state/s1");
        let s2 = at("at://did:plc:olaren/sh.tangled.repo.issue.state/s2");
        s.upsert(s1.clone(), issue.clone(), 100, IssueStateKind::Open);
        s.upsert(s2.clone(), issue.clone(), 100, IssueStateKind::Open);
        s.remove_source(&s1);
        assert_eq!(
            s.latest(&issue),
            Some((IssueStateKind::Open, 100)),
            "removing one source must not wipe the other's matching state"
        );
        s.remove_source(&s2);
        assert!(s.latest(&issue).is_none());
    }

    #[test]
    fn unknown_variant_is_reported() {
        use bobbin_types::sh_tangled::repo::issue::state::State as IssueStateRec;
        use jacquard_common::deps::smol_str::SmolStr;
        let issue_idx = idx();
        let pull_idx = StateIndex::<PullStatusKind>::new(RuntimeHasher::default());
        let rec = Record::IssueState(IssueStateRec {
            issue: at("at://did:plc:limpet/sh.tangled.repo.issue/i1"),
            state: StateState::Other(SmolStr::new_static("sh.tangled.repo.issue.state.reopened")),
            extra_data: None,
        });
        let outcome = apply_record_state(
            &issue_idx,
            &pull_idx,
            &at("at://did:plc:nel/sh.tangled.repo.issue.state/s1"),
            &rec,
        );
        assert_eq!(outcome, ApplyOutcome::UnknownVariant);
        assert_eq!(issue_idx.entity_count(), 0);
    }

    #[test]
    fn latest_by_filters_unauthorized_sources() {
        let s = idx();
        let issue = at("at://did:plc:limpet/sh.tangled.repo.issue/i1");
        let owner_state = at("at://did:plc:limpet/sh.tangled.repo.issue.state/legit");
        let attacker_state = at("at://did:plc:nautilus/sh.tangled.repo.issue.state/spoof");
        s.upsert(
            owner_state.clone(),
            issue.clone(),
            100,
            IssueStateKind::Open,
        );
        s.upsert(
            attacker_state.clone(),
            issue.clone(),
            500,
            IssueStateKind::Closed,
        );
        let only_owner = |src: &AtUri<DefaultStr>| src.as_ref().starts_with("at://did:plc:limpet/");
        assert_eq!(
            s.latest_by(&issue, only_owner),
            Some((IssueStateKind::Open, 100)),
            "spoofed attacker state must be ignored",
        );
        assert_eq!(
            s.latest(&issue),
            Some((IssueStateKind::Closed, 500)),
            "unfiltered latest still surfaces the spoof for sanity",
        );
    }

    #[test]
    fn remove_entity_drops_forward_and_reverse() {
        let s = idx();
        let issue = at("at://did:plc:limpet/sh.tangled.repo.issue/i1");
        let s1 = at("at://did:plc:nel/sh.tangled.repo.issue.state/s1");
        let s2 = at("at://did:plc:olaren/sh.tangled.repo.issue.state/s2");
        s.upsert(s1.clone(), issue.clone(), 100, IssueStateKind::Open);
        s.upsert(s2.clone(), issue.clone(), 200, IssueStateKind::Closed);
        s.remove_entity(&issue);
        assert!(s.latest(&issue).is_none());
        assert_eq!(s.entity_count(), 0);
        assert_eq!(
            s.source_count(),
            0,
            "reverse entries pointing at the dead entity must clear too",
        );
    }

    #[test]
    fn remove_entity_does_not_touch_reverse_pointing_elsewhere() {
        let s = idx();
        let dead = at("at://did:plc:limpet/sh.tangled.repo.issue/dead");
        let alive = at("at://did:plc:limpet/sh.tangled.repo.issue/alive");
        let src = at("at://did:plc:nel/sh.tangled.repo.issue.state/s1");
        s.upsert(src.clone(), dead.clone(), 100, IssueStateKind::Open);
        s.upsert(src.clone(), alive.clone(), 200, IssueStateKind::Closed);
        s.remove_entity(&dead);
        assert_eq!(
            s.latest(&alive),
            Some((IssueStateKind::Closed, 200)),
            "removing the dead entity must not touch state for the live one",
        );
        assert_eq!(s.source_count(), 1);
    }

    #[test]
    fn same_micros_distinct_kind_picks_by_source_not_kind() {
        let s = StateIndex::<PullStatusKind>::new(RuntimeHasher::default());
        let pull = at("at://did:plc:limpet/sh.tangled.repo.pull/p1");
        let early = at("at://did:plc:nel/sh.tangled.repo.pull.status/aaa");
        let later = at("at://did:plc:nel/sh.tangled.repo.pull.status/zzz");
        s.upsert(early.clone(), pull.clone(), 1000, PullStatusKind::Merged);
        s.upsert(later.clone(), pull.clone(), 1000, PullStatusKind::Open);
        assert_eq!(
            s.latest(&pull),
            Some((PullStatusKind::Open, 1000)),
            "tied micros must use source ordering as the tiebreak",
        );
    }
}
