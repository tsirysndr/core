use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use bobbin_edge_index::EdgeStore;
use bobbin_types::ids::{EdgeKey, SubjectRef, nsid_static};
use bobbin_types::knot_acl::{self, KnotHostKey};
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use serde::Deserialize;

use crate::registry::KnotRegistry;

const REPO_COLLABORATOR_KIND: &str = "sh.tangled.repo.collaborator";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct Cursor(pub i64);

impl Cursor {
    fn micros(self) -> u64 {
        self.0.max(0) as u64 / 1000
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AclOp {
    Add,
    Remove,
}

#[derive(Clone, Eq, Hash, PartialEq)]
enum DedupKey {
    Member(Did<DefaultStr>),
    Collaborator(Did<DefaultStr>, Did<DefaultStr>),
}

struct SeenState {
    cursor: Cursor,
    present: bool,
}

pub struct Roster {
    store: Arc<EdgeStore>,
    knot: Did<DefaultStr>,
    registry: Arc<KnotRegistry>,
    host: KnotHostKey,
    seen: HashMap<DedupKey, SeenState>,
}

impl Roster {
    pub fn new(
        store: Arc<EdgeStore>,
        knot: Did<DefaultStr>,
        registry: Arc<KnotRegistry>,
        host: KnotHostKey,
    ) -> Self {
        Self {
            store,
            knot,
            registry,
            host,
            seen: HashMap::new(),
        }
    }

    pub fn apply_member(&mut self, op: AclOp, subject: Did<DefaultStr>, cursor: Cursor) {
        if !self.advance(
            DedupKey::Member(subject.clone()),
            cursor,
            matches!(op, AclOp::Add),
        ) {
            return;
        }
        match op {
            AclOp::Add => {
                if let Some((source, edges)) =
                    knot_acl::member_upsert(&self.knot, &subject, cursor.micros())
                {
                    self.store.upsert_source(&source, edges);
                }
            }
            AclOp::Remove => {
                if let Some(source) = knot_acl::member_source(&self.knot, &subject) {
                    self.store.remove_source(&source);
                }
            }
        }
    }

    pub fn apply_collaborator(
        &mut self,
        op: AclOp,
        repo: Did<DefaultStr>,
        subject: Did<DefaultStr>,
        cursor: Cursor,
    ) {
        if !self.registry.repo_on_host(&self.host, &repo) {
            return;
        }
        if !self.advance(
            DedupKey::Collaborator(repo.clone(), subject.clone()),
            cursor,
            matches!(op, AclOp::Add),
        ) {
            return;
        }
        match op {
            AclOp::Add => {
                if let Some((source, edges)) =
                    knot_acl::collaborator_upsert(&repo, &subject, cursor.micros())
                {
                    self.store.upsert_source(&source, edges);
                }
            }
            AclOp::Remove => {
                if let Some(source) = knot_acl::collaborator_source(&repo, &subject) {
                    self.store.remove_source(&source);
                }
            }
        }
    }

    pub fn max_cursor(&self) -> Cursor {
        self.seen
            .values()
            .map(|state| state.cursor)
            .max()
            .unwrap_or(Cursor(0))
    }

    pub fn reap_members(&mut self, present: &HashSet<Did<DefaultStr>>, horizon: Cursor) {
        let stale: Vec<Did<DefaultStr>> = self
            .seen
            .iter()
            .filter_map(|(key, state)| match key {
                DedupKey::Member(subject)
                    if state.present && state.cursor <= horizon && !present.contains(subject) =>
                {
                    Some(subject.clone())
                }
                _ => None,
            })
            .collect();
        stale
            .into_iter()
            .for_each(|subject| self.retire_member(subject));
    }

    pub fn reap_collaborators(
        &mut self,
        repo: &Did<DefaultStr>,
        present: &HashSet<Did<DefaultStr>>,
        horizon: Cursor,
    ) {
        let stale: Vec<Did<DefaultStr>> = self
            .seen
            .iter()
            .filter_map(|(key, state)| match key {
                DedupKey::Collaborator(edge_repo, subject)
                    if edge_repo == repo
                        && state.present
                        && state.cursor <= horizon
                        && !present.contains(subject) =>
                {
                    Some(subject.clone())
                }
                _ => None,
            })
            .collect();
        stale
            .into_iter()
            .for_each(|subject| self.retire_collaborator(repo.clone(), subject));
    }

    pub fn purge_legacy(&self) {
        self.registry
            .drain_legacy_members(&self.host)
            .iter()
            .for_each(|source| self.store.remove_source(source));

        self.registry
            .repos(&self.host)
            .into_iter()
            .for_each(|repo| {
                let key = EdgeKey::new(nsid_static(REPO_COLLABORATOR_KIND), SubjectRef::Did(repo));
                self.store
                    .sources_for(&key)
                    .into_iter()
                    .filter(|source| knot_acl::decode_knot_owned_source(source).is_none())
                    .for_each(|source| self.store.remove_source(&source));
            });
    }

    fn retire_member(&mut self, subject: Did<DefaultStr>) {
        if let Some(source) = knot_acl::member_source(&self.knot, &subject) {
            self.store.remove_source(&source);
        }
        if let Some(state) = self.seen.get_mut(&DedupKey::Member(subject)) {
            state.present = false;
        }
    }

    fn retire_collaborator(&mut self, repo: Did<DefaultStr>, subject: Did<DefaultStr>) {
        if let Some(source) = knot_acl::collaborator_source(&repo, &subject) {
            self.store.remove_source(&source);
        }
        if let Some(state) = self.seen.get_mut(&DedupKey::Collaborator(repo, subject)) {
            state.present = false;
        }
    }

    fn advance(&mut self, key: DedupKey, cursor: Cursor, present: bool) -> bool {
        match self.seen.get(&key) {
            Some(state) if cursor <= state.cursor => false,
            _ => {
                self.seen.insert(key, SeenState { cursor, present });
                true
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bobbin_runtime::RuntimeHasher;
    use bobbin_types::edges::Edge;
    use bobbin_types::ids::{EdgeKey, SubjectRef, nsid_static};
    use jacquard_common::types::string::AtUri;

    fn store() -> Arc<EdgeStore> {
        Arc::new(EdgeStore::new(RuntimeHasher::default()))
    }

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn host() -> KnotHostKey {
        KnotHostKey::new("oyster.cafe")
    }

    fn add_legacy_edge(
        store: &EdgeStore,
        kind: &'static str,
        subject: &Did<DefaultStr>,
        source: &AtUri<DefaultStr>,
    ) {
        store.upsert_source(
            source,
            vec![Edge {
                kind: nsid_static(kind),
                subject: SubjectRef::Did(subject.clone()),
                source: source.clone(),
                sort_micros: 0,
            }],
        );
    }

    fn knot() -> Did<DefaultStr> {
        knot_acl::host_to_knot_did("oyster.cafe").unwrap()
    }

    fn member_count(store: &EdgeStore, subject: &Did<DefaultStr>) -> u64 {
        store.count(&EdgeKey::new(
            nsid_static("sh.tangled.knot.member"),
            SubjectRef::Did(subject.clone()),
        ))
    }

    fn collaborator_count(store: &EdgeStore, repo: &Did<DefaultStr>) -> u64 {
        store.count(&EdgeKey::new(
            nsid_static("sh.tangled.repo.collaborator"),
            SubjectRef::Did(repo.clone()),
        ))
    }

    fn empty_registry() -> Arc<KnotRegistry> {
        Arc::new(KnotRegistry::new())
    }

    fn member_roster(store: Arc<EdgeStore>) -> Roster {
        Roster::new(store, knot(), empty_registry(), host())
    }

    fn subjects(items: &[&Did<DefaultStr>]) -> HashSet<Did<DefaultStr>> {
        items.iter().map(|d| (*d).clone()).collect()
    }

    #[test]
    fn member_add_then_remove() {
        let store = store();
        let mut roster = member_roster(store.clone());
        let m = did("did:plc:boltless");
        roster.apply_member(AclOp::Add, m.clone(), Cursor(1_000_000));
        assert_eq!(member_count(&store, &m), 1);
        roster.apply_member(AclOp::Remove, m.clone(), Cursor(2_000_000));
        assert_eq!(member_count(&store, &m), 0);
    }

    #[test]
    fn stale_add_cannot_resurrect_removed_member() {
        let store = store();
        let mut roster = member_roster(store.clone());
        let m = did("did:plc:boltless");
        roster.apply_member(AclOp::Remove, m.clone(), Cursor(5));
        roster.apply_member(AclOp::Add, m.clone(), Cursor(1));
        assert_eq!(member_count(&store, &m), 0);
    }

    #[test]
    fn duplicate_cursor_is_idempotent() {
        let store = store();
        let mut roster = member_roster(store.clone());
        let m = did("did:plc:akshay");
        roster.apply_member(AclOp::Add, m.clone(), Cursor(10));
        roster.apply_member(AclOp::Add, m.clone(), Cursor(10));
        assert_eq!(member_count(&store, &m), 1);
    }

    #[test]
    fn collaborator_keyed_on_repo_when_hosted() {
        let store = store();
        let repo = did("did:plc:scallop");
        let registry = empty_registry();
        registry.observe_repo(&host(), repo.clone());
        let mut roster = Roster::new(store.clone(), knot(), registry, host());
        let subject = did("did:plc:olaren");
        roster.apply_collaborator(AclOp::Add, repo.clone(), subject.clone(), Cursor(7));
        assert_eq!(collaborator_count(&store, &repo), 1);
        roster.apply_collaborator(AclOp::Remove, repo.clone(), subject.clone(), Cursor(8));
        assert_eq!(collaborator_count(&store, &repo), 0);
    }

    #[test]
    fn reap_removes_departed_member() {
        let store = store();
        let mut roster = member_roster(store.clone());
        let stayed = did("did:plc:akshay");
        let left = did("did:plc:boltless");
        roster.apply_member(AclOp::Add, stayed.clone(), Cursor(10));
        roster.apply_member(AclOp::Add, left.clone(), Cursor(20));

        let horizon = roster.max_cursor();
        roster.reap_members(&subjects(&[&stayed]), horizon);

        assert_eq!(member_count(&store, &stayed), 1);
        assert_eq!(
            member_count(&store, &left),
            0,
            "a member absent from the authoritative snapshot is reaped"
        );
    }

    #[test]
    fn reap_skips_member_added_after_horizon() {
        let store = store();
        let mut roster = member_roster(store.clone());
        let m = did("did:plc:boltless");
        let horizon = roster.max_cursor();
        roster.apply_member(AclOp::Add, m.clone(), Cursor(100));

        roster.reap_members(&subjects(&[]), horizon);

        assert_eq!(
            member_count(&store, &m),
            1,
            "a member added after the snapshot horizon must survive the reap"
        );
    }

    #[test]
    fn reap_removes_departed_collaborator() {
        let store = store();
        let repo = did("did:plc:scallop");
        let registry = empty_registry();
        registry.observe_repo(&host(), repo.clone());
        let mut roster = Roster::new(store.clone(), knot(), registry, host());
        let left = did("did:plc:olaren");
        roster.apply_collaborator(AclOp::Add, repo.clone(), left.clone(), Cursor(7));

        let horizon = roster.max_cursor();
        roster.reap_collaborators(&repo, &subjects(&[]), horizon);

        assert_eq!(collaborator_count(&store, &repo), 0);
    }

    #[test]
    fn purge_legacy_strips_pds_collaborator_keeps_knot_owned() {
        let store = store();
        let repo = did("did:plc:scallop");
        let registry = empty_registry();
        registry.observe_repo(&host(), repo.clone());
        let mut roster = Roster::new(store.clone(), knot(), registry, host());

        roster.apply_collaborator(AclOp::Add, repo.clone(), did("did:plc:olaren"), Cursor(7));
        add_legacy_edge(
            &store,
            "sh.tangled.repo.collaborator",
            &repo,
            &at("at://did:plc:akshay/sh.tangled.repo.collaborator/r1"),
        );
        assert_eq!(collaborator_count(&store, &repo), 2);

        roster.purge_legacy();
        assert_eq!(
            collaborator_count(&store, &repo),
            1,
            "only the knot-owned collaborator survives the purge"
        );
    }

    #[test]
    fn purge_legacy_removes_indexed_member_edges() {
        let store = store();
        let registry = empty_registry();
        let roster = Roster::new(store.clone(), knot(), registry.clone(), host());
        let member = did("did:plc:boltless");
        let source = at("at://did:plc:akshay/sh.tangled.knot.member/r1");

        add_legacy_edge(&store, "sh.tangled.knot.member", &member, &source);
        registry.note_legacy_member(source.clone(), &host());
        assert_eq!(member_count(&store, &member), 1);

        roster.purge_legacy();
        assert_eq!(member_count(&store, &member), 0);
    }

    #[test]
    fn collaborator_for_unhosted_repo_is_dropped() {
        let store = store();
        let repo = did("did:plc:scallop");
        let mut roster = Roster::new(store.clone(), knot(), empty_registry(), host());
        roster.apply_collaborator(AclOp::Add, repo.clone(), did("did:plc:olaren"), Cursor(7));
        assert_eq!(
            collaborator_count(&store, &repo),
            0,
            "a knot cannot assert collaborators on a repo it does not host"
        );
    }
}
