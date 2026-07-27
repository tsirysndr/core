use std::collections::BTreeSet;

use knot_index::{Index, Resolved};
use knot_types::{AccountDid, AdmissionPolicy, OwnerDid, RepoDid};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use]
pub enum Decision {
    Allow,
    Deny,
}

impl Decision {
    pub fn is_allowed(self) -> bool {
        matches!(self, Decision::Allow)
    }

    fn allow_if(granted: bool) -> Self {
        if granted {
            Decision::Allow
        } else {
            Decision::Deny
        }
    }
}

pub trait Acl {
    fn is_admin(&self, who: &AccountDid) -> bool;
    fn admission(&self) -> AdmissionPolicy;
    fn is_member(&self, who: &AccountDid) -> Resolved<bool>;
    fn is_blocked(&self, who: &AccountDid) -> Resolved<bool>;
    fn is_collaborator(&self, repo: &RepoDid, who: &AccountDid) -> Resolved<bool>;
    fn repo_owner(&self, repo: &RepoDid) -> Resolved<Option<OwnerDid>>;
}

fn confirmed(resolved: Resolved<bool>) -> bool {
    matches!(resolved, Resolved::Ready(true))
}

fn owns_repo(acl: &impl Acl, who: &AccountDid, repo: &RepoDid) -> bool {
    confirmed(
        acl.repo_owner(repo)
            .map(|owner| owner.is_some_and(|owner| owner.is(who))),
    )
}

fn not_blocked(acl: &impl Acl, who: &AccountDid) -> bool {
    acl.is_admin(who) || matches!(acl.is_blocked(who), Resolved::Ready(false))
}

pub fn can_admin_knot(acl: &impl Acl, who: &AccountDid) -> Decision {
    Decision::allow_if(acl.is_admin(who))
}

pub fn can_create_repo(acl: &impl Acl, who: &AccountDid) -> Decision {
    Decision::allow_if(
        acl.is_admin(who)
            || (not_blocked(acl, who)
                && match acl.admission() {
                    AdmissionPolicy::Open => true,
                    AdmissionPolicy::Closed => confirmed(acl.is_member(who)),
                }),
    )
}

pub fn can_push(acl: &impl Acl, who: &AccountDid, repo: &RepoDid) -> Decision {
    Decision::allow_if(
        not_blocked(acl, who)
            && (owns_repo(acl, who, repo) || confirmed(acl.is_collaborator(repo, who))),
    )
}

pub fn can_manage_collaborators(acl: &impl Acl, who: &AccountDid, repo: &RepoDid) -> Decision {
    Decision::allow_if(not_blocked(acl, who) && owns_repo(acl, who, repo))
}

pub fn can_delete_repo(acl: &impl Acl, who: &AccountDid, repo: &RepoDid) -> Decision {
    Decision::allow_if(acl.is_admin(who) || owns_repo(acl, who, repo))
}

pub struct KnotAcl<'a> {
    admins: &'a BTreeSet<AccountDid>,
    policy: AdmissionPolicy,
    index: &'a Index,
}

impl<'a> KnotAcl<'a> {
    pub fn new(
        admins: &'a BTreeSet<AccountDid>,
        policy: AdmissionPolicy,
        index: &'a Index,
    ) -> Self {
        Self {
            admins,
            policy,
            index,
        }
    }
}

impl Acl for KnotAcl<'_> {
    fn is_admin(&self, who: &AccountDid) -> bool {
        self.admins.contains(who)
    }

    fn admission(&self) -> AdmissionPolicy {
        self.policy
    }

    fn is_member(&self, who: &AccountDid) -> Resolved<bool> {
        self.index.is_member(who)
    }

    fn is_blocked(&self, who: &AccountDid) -> Resolved<bool> {
        self.index.is_blocked(who)
    }

    fn is_collaborator(&self, repo: &RepoDid, who: &AccountDid) -> Resolved<bool> {
        self.index.is_collaborator(repo, who)
    }

    fn repo_owner(&self, repo: &RepoDid) -> Resolved<Option<OwnerDid>> {
        self.index.owner_of(repo)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn acc(suffix: &str) -> AccountDid {
        AccountDid::new(format!("did:plc:{suffix}")).unwrap()
    }

    fn owner(suffix: &str) -> OwnerDid {
        OwnerDid::new(format!("did:plc:{suffix}")).unwrap()
    }

    fn repo(suffix: &str) -> RepoDid {
        RepoDid::new(format!("did:plc:{suffix}")).unwrap()
    }

    struct Fake {
        admins: BTreeSet<AccountDid>,
        admission: AdmissionPolicy,
        member: Resolved<bool>,
        blocked: Resolved<bool>,
        collaborator: Resolved<bool>,
        owner: Resolved<Option<OwnerDid>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                admins: BTreeSet::new(),
                admission: AdmissionPolicy::Closed,
                member: Resolved::Warming,
                blocked: Resolved::Ready(false),
                collaborator: Resolved::Warming,
                owner: Resolved::Warming,
            }
        }

        fn admin(mut self, who: &str) -> Self {
            self.admins.insert(acc(who));
            self
        }

        fn open(mut self) -> Self {
            self.admission = AdmissionPolicy::Open;
            self
        }

        fn member(mut self, resolved: Resolved<bool>) -> Self {
            self.member = resolved;
            self
        }

        fn blocked(mut self, resolved: Resolved<bool>) -> Self {
            self.blocked = resolved;
            self
        }

        fn collaborator(mut self, resolved: Resolved<bool>) -> Self {
            self.collaborator = resolved;
            self
        }

        fn owner(mut self, resolved: Resolved<Option<OwnerDid>>) -> Self {
            self.owner = resolved;
            self
        }
    }

    impl Acl for Fake {
        fn is_admin(&self, who: &AccountDid) -> bool {
            self.admins.contains(who)
        }

        fn admission(&self) -> AdmissionPolicy {
            self.admission
        }

        fn is_member(&self, _who: &AccountDid) -> Resolved<bool> {
            self.member.clone()
        }

        fn is_blocked(&self, _who: &AccountDid) -> Resolved<bool> {
            self.blocked.clone()
        }

        fn is_collaborator(&self, _repo: &RepoDid, _who: &AccountDid) -> Resolved<bool> {
            self.collaborator.clone()
        }

        fn repo_owner(&self, _repo: &RepoDid) -> Resolved<Option<OwnerDid>> {
            self.owner.clone()
        }
    }

    #[test]
    fn an_admin_administers_and_creates_but_does_not_push_arbitrary_repos() {
        let acl = Fake::new()
            .admin("nel")
            .owner(Resolved::Ready(Some(owner("olaren"))))
            .collaborator(Resolved::Ready(false));
        assert_eq!(can_admin_knot(&acl, &acc("nel")), Decision::Allow);
        assert_eq!(can_create_repo(&acl, &acc("nel")), Decision::Allow);
        assert_eq!(
            can_push(&acl, &acc("nel"), &repo("squid")),
            Decision::Deny,
            "knot admin has no push on repo it neither owns nor collaborates on"
        );
    }

    #[test]
    fn decisions() {
        type Case = (&'static str, Fake, fn(&Fake) -> Decision, Decision);
        let cases: Vec<Case> = vec![
            (
                "a_member_creates_repos",
                Fake::new().member(Resolved::Ready(true)),
                |acl| can_create_repo(acl, &acc("olaren")),
                Decision::Allow,
            ),
            (
                "a_member_cannot_administer_the_knot",
                Fake::new().member(Resolved::Ready(true)),
                |acl| can_admin_knot(acl, &acc("olaren")),
                Decision::Deny,
            ),
            (
                "an_open_knot_admits_a_non_member",
                Fake::new().open().member(Resolved::Ready(false)),
                |acl| can_create_repo(acl, &acc("teq")),
                Decision::Allow,
            ),
            (
                "an_open_knot_does_not_widen_push",
                Fake::new()
                    .open()
                    .owner(Resolved::Ready(Some(owner("nel"))))
                    .collaborator(Resolved::Ready(false)),
                |acl| can_push(acl, &acc("teq"), &repo("squid")),
                Decision::Deny,
            ),
            (
                "a_blocked_account_cannot_create",
                Fake::new()
                    .open()
                    .member(Resolved::Ready(true))
                    .blocked(Resolved::Ready(true)),
                |acl| can_create_repo(acl, &acc("squid")),
                Decision::Deny,
            ),
            (
                "an_admin_is_immune_to_the_blocklist",
                Fake::new()
                    .open()
                    .admin("nel")
                    .blocked(Resolved::Ready(true)),
                |acl| can_create_repo(acl, &acc("nel")),
                Decision::Allow,
            ),
            (
                "the_repo_owner_pushes",
                Fake::new()
                    .owner(Resolved::Ready(Some(owner("nel"))))
                    .collaborator(Resolved::Ready(false)),
                |acl| can_push(acl, &acc("nel"), &repo("squid")),
                Decision::Allow,
            ),
            (
                "a_collaborator_pushes_without_owning",
                Fake::new()
                    .owner(Resolved::Ready(Some(owner("nel"))))
                    .collaborator(Resolved::Ready(true)),
                |acl| can_push(acl, &acc("olaren"), &repo("squid")),
                Decision::Allow,
            ),
            (
                "push_allows_on_a_confirmed_collaborator_while_the_registry_warms",
                Fake::new()
                    .owner(Resolved::Warming)
                    .collaborator(Resolved::Ready(true)),
                |acl| can_push(acl, &acc("olaren"), &repo("squid")),
                Decision::Allow,
            ),
            (
                "push_allows_a_confirmed_owner_while_collaborators_warm",
                Fake::new()
                    .owner(Resolved::Ready(Some(owner("nel"))))
                    .collaborator(Resolved::Warming),
                |acl| can_push(acl, &acc("nel"), &repo("squid")),
                Decision::Allow,
            ),
            (
                "push_denies_when_ownership_is_warming_and_not_a_collaborator",
                Fake::new()
                    .owner(Resolved::Warming)
                    .collaborator(Resolved::Ready(false)),
                |acl| can_push(acl, &acc("nel"), &repo("squid")),
                Decision::Deny,
            ),
            (
                "push_denies_an_unregistered_repo",
                Fake::new()
                    .owner(Resolved::Ready(None))
                    .collaborator(Resolved::Ready(false)),
                |acl| can_push(acl, &acc("nel"), &repo("squid")),
                Decision::Deny,
            ),
            (
                "push_matches_a_did_web_owner_across_authority_case",
                Fake::new()
                    .owner(Resolved::Ready(Some(
                        OwnerDid::new("did:web:OYSTER.cafe").unwrap(),
                    )))
                    .collaborator(Resolved::Ready(false)),
                |acl| {
                    can_push(
                        acl,
                        &AccountDid::new("did:web:oyster.cafe").unwrap(),
                        &repo("squid"),
                    )
                },
                Decision::Allow,
            ),
            (
                "push_denies_a_did_plc_owner_whose_case_differs",
                Fake::new()
                    .owner(Resolved::Ready(Some(OwnerDid::new("did:plc:ABC").unwrap())))
                    .collaborator(Resolved::Ready(false)),
                |acl| {
                    can_push(
                        acl,
                        &AccountDid::new("did:plc:abc").unwrap(),
                        &repo("squid"),
                    )
                },
                Decision::Deny,
            ),
            (
                "manage_collaborators_fails_closed_while_ownership_is_warming",
                Fake::new().owner(Resolved::Warming),
                |acl| can_manage_collaborators(acl, &acc("olaren"), &repo("squid")),
                Decision::Deny,
            ),
            (
                "an_admin_is_authorized_before_the_projection_warms",
                Fake::new().admin("nel"),
                |acl| can_admin_knot(acl, &acc("nel")),
                Decision::Allow,
            ),
            (
                "an_admin_creates_before_the_projection_warms",
                Fake::new().admin("nel"),
                |acl| can_create_repo(acl, &acc("nel")),
                Decision::Allow,
            ),
            (
                "an_admin_deletes_a_repo_before_the_registry_warms",
                Fake::new().admin("nel").owner(Resolved::Warming),
                |acl| can_delete_repo(acl, &acc("nel"), &repo("squid")),
                Decision::Allow,
            ),
        ];
        cases.iter().for_each(|(label, acl, eval, expected)| {
            assert_eq!(eval(acl), *expected, "{label}");
        });
    }

    #[test]
    fn a_blocked_owner_cannot_push_or_invite() {
        let acl = Fake::new()
            .owner(Resolved::Ready(Some(owner("squid"))))
            .collaborator(Resolved::Ready(false))
            .blocked(Resolved::Ready(true));
        assert_eq!(
            can_push(&acl, &acc("squid"), &repo("anemone")),
            Decision::Deny,
            "ban overrides ownership on write path"
        );
        assert_eq!(
            can_manage_collaborators(&acl, &acc("squid"), &repo("anemone")),
            Decision::Deny
        );
    }

    #[test]
    fn a_warming_blocklist_fails_create_and_push_closed() {
        let acl = Fake::new()
            .open()
            .blocked(Resolved::Warming)
            .owner(Resolved::Ready(Some(owner("squid"))));
        assert_eq!(
            can_create_repo(&acl, &acc("squid")),
            Decision::Deny,
            "unresolved blocklist must not admit, ban could be hiding in it"
        );
        assert_eq!(
            can_push(&acl, &acc("squid"), &repo("anemone")),
            Decision::Deny
        );
    }

    #[test]
    fn a_stranger_is_denied_everything() {
        let acl = Fake::new()
            .member(Resolved::Ready(false))
            .collaborator(Resolved::Ready(false))
            .owner(Resolved::Ready(Some(owner("nel"))));
        assert_eq!(can_admin_knot(&acl, &acc("teq")), Decision::Deny);
        assert_eq!(can_create_repo(&acl, &acc("teq")), Decision::Deny);
        assert_eq!(can_push(&acl, &acc("teq"), &repo("squid")), Decision::Deny);
    }

    #[test]
    fn a_fully_warming_index_denies_every_index_backed_decision() {
        let acl = Fake::new();
        assert_eq!(can_create_repo(&acl, &acc("olaren")), Decision::Deny);
        assert_eq!(can_push(&acl, &acc("nel"), &repo("squid")), Decision::Deny);
    }

    #[test]
    fn is_allowed_reports_the_verdict() {
        assert!(Decision::Allow.is_allowed());
        assert!(!Decision::Deny.is_allowed());
    }

    #[test]
    fn only_the_repo_owner_manages_collaborators() {
        let acl = Fake::new()
            .admin("nel")
            .owner(Resolved::Ready(Some(owner("olaren"))))
            .collaborator(Resolved::Ready(true));
        assert_eq!(
            can_manage_collaborators(&acl, &acc("olaren"), &repo("squid")),
            Decision::Allow,
            "repo owner manages its own collaborators"
        );
        assert_eq!(
            can_manage_collaborators(&acl, &acc("lyna"), &repo("squid")),
            Decision::Deny,
            "collaborator cannot manage collaborator set"
        );
        assert_eq!(
            can_manage_collaborators(&acl, &acc("nel"), &repo("squid")),
            Decision::Deny,
            "knot admin has no collaborator-invite right on repo it does not own"
        );
    }

    #[test]
    fn repo_deletion_is_the_owner_or_a_knot_admin() {
        let acl = Fake::new()
            .admin("nel")
            .owner(Resolved::Ready(Some(owner("olaren"))))
            .collaborator(Resolved::Ready(true));
        assert_eq!(
            can_delete_repo(&acl, &acc("olaren"), &repo("squid")),
            Decision::Allow,
            "repo owner deletes its own repo"
        );
        assert_eq!(
            can_delete_repo(&acl, &acc("nel"), &repo("squid")),
            Decision::Allow,
            "knot admin deletes any repo"
        );
        assert_eq!(
            can_delete_repo(&acl, &acc("lyna"), &repo("squid")),
            Decision::Deny,
            "collaborator cannot delete the repo"
        );
    }

    mod integration {
        use super::*;
        use knot_cob::{ChangePayload, CobHome, CobStore};
        use knot_cobs::{CollaboratorsChange, Grant, MembersChange, Registration, RegistryChange};
        use knot_git::{Layout, Repo};
        use knot_runtime::{K256Signer, SeededEntropy};
        use knot_types::{KnotId, RepoName, RepoRkey, UnixSeconds};
        use std::path::PathBuf;

        fn knot_home() -> CobHome {
            CobHome::from(&KnotId::new("did:web:knot.nel.pet").unwrap())
        }

        fn grant(subject: &str, at: i64) -> Grant {
            Grant {
                subject: acc(subject),
                added_by: acc("nel"),
                created_at: UnixSeconds::new(at),
            }
        }

        fn registration(
            owner_id: &str,
            key: &str,
            repo_did: &knot_types::RepoDid,
            at: i64,
        ) -> Registration {
            Registration {
                owner: owner(owner_id),
                rkey: RepoRkey::new(key).unwrap(),
                name: RepoName::new(key).unwrap(),
                repo: repo_did.clone(),
                created_at: UnixSeconds::new(at),
            }
        }

        fn world() -> (tempfile::TempDir, PathBuf, Layout, K256Signer) {
            let dir = tempfile::tempdir().unwrap();
            let meta_path = dir.path().join("meta");
            Repo::create(&meta_path).unwrap();
            let layout = Layout::new(dir.path().join("repos"));
            let signer = K256Signer::generate(&SeededEntropy::new(1));
            (dir, meta_path, layout, signer)
        }

        fn seed<P: ChangePayload>(
            store: &CobStore,
            home: &CobHome,
            change: &P,
            signer: &K256Signer,
            at: UnixSeconds,
        ) {
            store.create(home, change, signer, at).unwrap();
        }

        #[test]
        fn the_enforcer_decides_over_a_real_rebuilt_index() {
            let (_dir, meta_path, layout, signer) = world();
            let at = UnixSeconds::new;

            let meta = Repo::open(&meta_path).unwrap();
            let store = CobStore::new(&meta);
            let squid = repo("squid");
            seed(
                &store,
                &knot_home(),
                &MembersChange::Add(grant("olaren", 1)),
                &signer,
                at(1),
            );
            seed(
                &store,
                &knot_home(),
                &RegistryChange::Register(registration("nel", "anemone", &squid, 1)),
                &signer,
                at(1),
            );
            let git = layout.create(&squid).unwrap();
            seed(
                &CobStore::new(&git),
                &CobHome::from(&squid),
                &CollaboratorsChange::Add(grant("lyna", 1)),
                &signer,
                at(1),
            );

            let index = Index::new(&meta_path, layout.clone());
            index.rebuild().unwrap();
            index.ensure_collaborators(&squid).unwrap();
            let admins = BTreeSet::from([acc("nel")]);
            let acl = KnotAcl::new(&admins, AdmissionPolicy::Closed, &index);

            assert_eq!(can_admin_knot(&acl, &acc("nel")), Decision::Allow);
            assert_eq!(can_create_repo(&acl, &acc("nel")), Decision::Allow);
            assert_eq!(
                can_push(&acl, &acc("nel"), &squid),
                Decision::Allow,
                "nel owns squid in the registry"
            );

            assert_eq!(can_admin_knot(&acl, &acc("olaren")), Decision::Deny);
            assert_eq!(can_create_repo(&acl, &acc("olaren")), Decision::Allow);
            assert_eq!(
                can_push(&acl, &acc("olaren"), &squid),
                Decision::Deny,
                "member who is neither owner nor collaborator cannot push"
            );

            assert_eq!(
                can_push(&acl, &acc("lyna"), &squid),
                Decision::Allow,
                "lyna collaborates on squid"
            );
            assert_eq!(can_create_repo(&acl, &acc("lyna")), Decision::Deny);

            assert_eq!(can_push(&acl, &acc("teq"), &squid), Decision::Deny);
            assert_eq!(can_create_repo(&acl, &acc("teq")), Decision::Deny);

            let cold = Index::new(&meta_path, layout);
            let cold_acl = KnotAcl::new(&admins, AdmissionPolicy::Closed, &cold);
            assert_eq!(
                can_admin_knot(&cold_acl, &acc("nel")),
                Decision::Allow,
                "admin is config, answered before any rebuild"
            );
            assert_eq!(
                can_push(&cold_acl, &acc("nel"), &squid),
                Decision::Deny,
                "before rebuild owner lookup is warming, so push fails closed"
            );
            assert_eq!(can_create_repo(&cold_acl, &acc("olaren")), Decision::Deny);
        }

        #[test]
        fn a_repo_re_registered_under_a_second_owner_grants_push_only_to_the_later_owner() {
            let (_dir, meta_path, layout, signer) = world();
            let at = UnixSeconds::new;

            let squid = repo("squid");
            layout.create(&squid).unwrap();

            let meta = Repo::open(&meta_path).unwrap();
            let store = CobStore::new(&meta);
            let created = store
                .create(
                    &knot_home(),
                    &RegistryChange::Register(registration("nel", "anemone", &squid, 1)),
                    &signer,
                    at(1),
                )
                .unwrap();
            store
                .update(
                    &knot_home(),
                    created.object,
                    &RegistryChange::Register(registration("olaren", "fork", &squid, 2)),
                    &signer,
                    at(2),
                )
                .unwrap();

            let index = Index::new(&meta_path, layout);
            index.rebuild().unwrap();
            let admins = BTreeSet::new();
            let acl = KnotAcl::new(&admins, AdmissionPolicy::Closed, &index);

            assert_eq!(
                can_push(&acl, &acc("nel"), &squid),
                Decision::Deny,
                "re-register moves repo wholesale, so displaced owner loses push"
            );
            assert_eq!(
                can_push(&acl, &acc("olaren"), &squid),
                Decision::Allow,
                "linear causal order gives later registrant deterministic ownership"
            );
        }

        #[test]
        fn a_collaborator_on_an_unregistered_repo_cannot_push_after_a_real_rebuild() {
            let (_dir, meta_path, layout, signer) = world();
            let at = UnixSeconds::new;

            let squid = repo("squid");
            let git = layout.create(&squid).unwrap();
            seed(
                &CobStore::new(&git),
                &CobHome::from(&squid),
                &CollaboratorsChange::Add(grant("lyna", 1)),
                &signer,
                at(1),
            );

            let index = Index::new(&meta_path, layout);
            index.rebuild().unwrap();
            let admins = BTreeSet::new();
            let acl = KnotAcl::new(&admins, AdmissionPolicy::Closed, &index);
            assert_eq!(
                can_push(&acl, &acc("lyna"), &squid),
                Decision::Deny,
                "rebuild folds collaborators only for registered repos, so collaborator COB on unregistered repo never warms and grants no push"
            );
        }
    }
}
