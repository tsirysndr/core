use crate::{RepoIdResolver, Resolution};
use bobbin_types::search::SearchableRecord;
use bobbin_types::sh_tangled::repo::artifact::Artifact;
use jacquard_common::DefaultStr;
use jacquard_common::IntoStatic;
use jacquard_common::types::did::Did;
use jacquard_common::types::ident::AtIdentifier;
use jacquard_common::types::recordkey::Rkey;
use jacquard_common::types::string::AtUri;

pub(crate) const REPO_COLLECTION: &str = "sh.tangled.repo";

pub trait NormalizeRepoRefs: Sized {
    fn normalize(
        self,
        resolver: &RepoIdResolver,
    ) -> impl std::future::Future<Output = Option<Self>> + Send;
}

pub(crate) fn is_repo_at_uri(uri: &AtUri<DefaultStr>) -> bool {
    uri.collection()
        .map(|c| c.as_ref() == REPO_COLLECTION)
        .unwrap_or(false)
}

pub(crate) async fn resolve_repo_uri(
    resolver: &RepoIdResolver,
    uri: &AtUri<DefaultStr>,
) -> Option<Did<DefaultStr>> {
    if !is_repo_at_uri(uri) {
        return None;
    }
    let owner = match uri.authority() {
        AtIdentifier::Did(d) => d.clone().into_static(),
        AtIdentifier::Handle(_) => return None,
    };
    let rkey: Rkey<DefaultStr> = uri.rkey()?.clone().into_static();
    match resolver.resolve(&owner, &rkey).await {
        Resolution::Mapped(did) => Some(did),
        Resolution::NoRepoDid | Resolution::Unresolvable => None,
    }
}

async fn fill_repo_did(
    resolver: &RepoIdResolver,
    uri_field: &mut Option<AtUri<DefaultStr>>,
    did_field: &mut Option<Did<DefaultStr>>,
) -> bool {
    if did_field.is_some() {
        *uri_field = None;
        return true;
    }
    let Some(uri) = uri_field.as_ref() else {
        return false;
    };
    let Some(did) = resolve_repo_uri(resolver, uri).await else {
        return false;
    };
    *did_field = Some(did);
    *uri_field = None;
    true
}

impl NormalizeRepoRefs for Artifact<DefaultStr> {
    async fn normalize(mut self, resolver: &RepoIdResolver) -> Option<Self> {
        if !fill_repo_did(resolver, &mut self.repo, &mut self.repo_did).await {
            return None;
        }
        Some(self)
    }
}

impl NormalizeRepoRefs for bobbin_types::sh_tangled::pipeline::Pipeline<DefaultStr> {
    async fn normalize(mut self, resolver: &RepoIdResolver) -> Option<Self> {
        let trig = &mut self.trigger_metadata.repo;
        if trig.repo_did.is_some() {
            trig.repo = None;
            return Some(self);
        }
        let raw = trig.repo.as_deref()?;
        let parsed = AtUri::<DefaultStr>::new_owned(raw).ok()?;
        let did = resolve_repo_uri(resolver, &parsed).await?;
        trig.repo_did = Some(did);
        trig.repo = None;
        Some(self)
    }
}

impl NormalizeRepoRefs for SearchableRecord {
    async fn normalize(self, _resolver: &RepoIdResolver) -> Option<Self> {
        Some(self)
    }
}

macro_rules! identity_normalize {
    ($($t:ty),+ $(,)?) => {
        $(
            impl NormalizeRepoRefs for $t {
                async fn normalize(self, _resolver: &RepoIdResolver) -> Option<Self> {
                    Some(self)
                }
            }
        )+
    };
}

use bobbin_types::sh_tangled::actor::profile::Profile;
use bobbin_types::sh_tangled::feed::comment::Comment as FeedComment;
use bobbin_types::sh_tangled::feed::reaction::Reaction;
use bobbin_types::sh_tangled::feed::star::Star;
use bobbin_types::sh_tangled::git::ref_update::RefUpdate;
use bobbin_types::sh_tangled::graph::follow::Follow;
use bobbin_types::sh_tangled::graph::vouch::Vouch;
use bobbin_types::sh_tangled::knot::Knot;
use bobbin_types::sh_tangled::knot::member::Member as KnotMember;
use bobbin_types::sh_tangled::label::definition::Definition as LabelDefinition;
use bobbin_types::sh_tangled::label::op::Op as LabelOp;
use bobbin_types::sh_tangled::pipeline::status::Status as PipelineStatus;
use bobbin_types::sh_tangled::public_key::PublicKey;
use bobbin_types::sh_tangled::repo::Repo;
use bobbin_types::sh_tangled::repo::collaborator::Collaborator;
use bobbin_types::sh_tangled::repo::issue::Issue;
use bobbin_types::sh_tangled::repo::issue::state::State as IssueState;
use bobbin_types::sh_tangled::repo::pull::Pull;
use bobbin_types::sh_tangled::repo::pull::status::Status as PullStatus;
use bobbin_types::sh_tangled::spindle::Spindle;
use bobbin_types::sh_tangled::spindle::member::Member as SpindleMember;
use bobbin_types::sh_tangled::string::TangledString;

identity_normalize!(
    Profile<DefaultStr>,
    FeedComment<DefaultStr>,
    Reaction<DefaultStr>,
    Star<DefaultStr>,
    RefUpdate<DefaultStr>,
    Follow<DefaultStr>,
    Vouch<DefaultStr>,
    Knot<DefaultStr>,
    KnotMember<DefaultStr>,
    LabelDefinition<DefaultStr>,
    LabelOp<DefaultStr>,
    PipelineStatus<DefaultStr>,
    PublicKey<DefaultStr>,
    Repo<DefaultStr>,
    Collaborator<DefaultStr>,
    Issue<DefaultStr>,
    IssueState<DefaultStr>,
    Pull<DefaultStr>,
    PullStatus<DefaultStr>,
    Spindle<DefaultStr>,
    SpindleMember<DefaultStr>,
    TangledString<DefaultStr>,
);
