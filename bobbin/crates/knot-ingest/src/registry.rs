use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use bobbin_types::knot_acl::KnotHostKey;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::string::AtUri;

#[derive(Default)]
struct Inner {
    hosts: HashSet<KnotHostKey>,
    repos: HashMap<KnotHostKey, HashSet<Did<DefaultStr>>>,
    repo_host: HashMap<Did<DefaultStr>, KnotHostKey>,
    legacy_members: HashMap<AtUri<DefaultStr>, KnotHostKey>,
}

#[derive(Default)]
pub struct KnotRegistry {
    inner: Mutex<Inner>,
}

impl KnotRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_host(&self, host: &KnotHostKey) {
        self.inner.lock().unwrap().hosts.insert(host.clone());
    }

    pub fn observe_repo(&self, host: &KnotHostKey, repo: Did<DefaultStr>) {
        let mut inner = self.inner.lock().unwrap();
        inner.hosts.insert(host.clone());
        inner
            .repos
            .entry(host.clone())
            .or_default()
            .insert(repo.clone());
        inner.repo_host.insert(repo, host.clone());
    }

    pub fn hosts(&self) -> Vec<KnotHostKey> {
        self.inner.lock().unwrap().hosts.iter().cloned().collect()
    }

    pub fn repos(&self, host: &KnotHostKey) -> Vec<Did<DefaultStr>> {
        self.inner
            .lock()
            .unwrap()
            .repos
            .get(host)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn repo_on_host(&self, host: &KnotHostKey, repo: &Did<DefaultStr>) -> bool {
        self.inner
            .lock()
            .unwrap()
            .repos
            .get(host)
            .is_some_and(|set| set.contains(repo))
    }

    pub fn host_of_repo(&self, repo: &Did<DefaultStr>) -> Option<KnotHostKey> {
        self.inner.lock().unwrap().repo_host.get(repo).cloned()
    }

    pub fn note_legacy_member(&self, source: AtUri<DefaultStr>, host: &KnotHostKey) {
        self.inner
            .lock()
            .unwrap()
            .legacy_members
            .insert(source, host.clone());
    }

    pub fn forget_legacy_member(&self, source: &AtUri<DefaultStr>) {
        self.inner.lock().unwrap().legacy_members.remove(source);
    }

    pub fn drain_legacy_members(&self, host: &KnotHostKey) -> Vec<AtUri<DefaultStr>> {
        let mut inner = self.inner.lock().unwrap();
        let matched: Vec<AtUri<DefaultStr>> = inner
            .legacy_members
            .iter()
            .filter(|(_, member_host)| member_host.as_str() == host.as_str())
            .map(|(source, _)| source.clone())
            .collect();
        matched.iter().for_each(|source| {
            inner.legacy_members.remove(source);
        });
        matched
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn did(s: &str) -> Did<DefaultStr> {
        Did::new_owned(s).unwrap()
    }

    fn at(s: &str) -> AtUri<DefaultStr> {
        AtUri::new_owned(s).unwrap()
    }

    fn host(s: &str) -> KnotHostKey {
        KnotHostKey::new(s)
    }

    #[test]
    fn observe_repo_registers_host_and_repo() {
        let registry = KnotRegistry::new();
        registry.observe_repo(&host("oyster.cafe"), did("did:plc:scallop"));
        registry.observe_repo(&host("oyster.cafe"), did("did:plc:limpet"));

        assert_eq!(registry.hosts(), vec![host("oyster.cafe")]);
        let mut repos = registry.repos(&host("oyster.cafe"));
        repos.sort_by(|a, b| a.as_ref().cmp(b.as_ref()));
        assert_eq!(repos, vec![did("did:plc:limpet"), did("did:plc:scallop")]);
    }

    #[test]
    fn observe_repo_dedups() {
        let registry = KnotRegistry::new();
        registry.observe_repo(&host("nel.pet"), did("did:plc:whelk"));
        registry.observe_repo(&host("nel.pet"), did("did:plc:whelk"));
        assert_eq!(registry.repos(&host("nel.pet")), vec![did("did:plc:whelk")]);
    }

    #[test]
    fn observe_host_without_repos() {
        let registry = KnotRegistry::new();
        registry.observe_host(&host("oyster.cafe"));
        assert_eq!(registry.hosts(), vec![host("oyster.cafe")]);
        assert!(registry.repos(&host("oyster.cafe")).is_empty());
    }

    #[test]
    fn host_lookups_are_case_insensitive() {
        let registry = KnotRegistry::new();
        registry.observe_repo(&host("KT.Oyster.Cafe"), did("did:plc:scallop"));
        assert!(registry.repo_on_host(&host("kt.oyster.cafe"), &did("did:plc:scallop")));
        assert_eq!(
            registry.host_of_repo(&did("did:plc:scallop")),
            Some(host("kt.oyster.cafe"))
        );
    }

    #[test]
    fn host_of_repo_resolves_owning_knot() {
        let registry = KnotRegistry::new();
        registry.observe_repo(&host("oyster.cafe"), did("did:plc:scallop"));
        assert_eq!(
            registry.host_of_repo(&did("did:plc:scallop")),
            Some(host("oyster.cafe"))
        );
        assert_eq!(registry.host_of_repo(&did("did:plc:limpet")), None);
    }

    #[test]
    fn drain_legacy_members_returns_only_matching_host() {
        let registry = KnotRegistry::new();
        let here = at("at://did:plc:akshay/sh.tangled.knot.member/r1");
        let elsewhere = at("at://did:plc:akshay/sh.tangled.knot.member/r2");
        registry.note_legacy_member(here.clone(), &host("oyster.cafe"));
        registry.note_legacy_member(elsewhere.clone(), &host("nel.pet"));

        assert_eq!(
            registry.drain_legacy_members(&host("oyster.cafe")),
            vec![here]
        );
        assert!(
            registry
                .drain_legacy_members(&host("oyster.cafe"))
                .is_empty()
        );
        assert_eq!(
            registry.drain_legacy_members(&host("nel.pet")),
            vec![elsewhere]
        );
    }

    #[test]
    fn forget_legacy_member_drops_source() {
        let registry = KnotRegistry::new();
        let source = at("at://did:plc:akshay/sh.tangled.knot.member/r1");
        registry.note_legacy_member(source.clone(), &host("oyster.cafe"));
        registry.forget_legacy_member(&source);
        assert!(
            registry
                .drain_legacy_members(&host("oyster.cafe"))
                .is_empty()
        );
    }
}
