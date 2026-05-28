use std::sync::Arc;

use bobbin_edge_index::{IssueStateKind, PullStatusKind, StateIndex};
use bobbin_types::ids::SubjectRef;
use jacquard_common::DefaultStr;
use jacquard_common::types::did::Did;
use jacquard_common::types::string::AtUri;
use serde::Deserialize;

use crate::AppState;
use crate::{accept_state_source, at_uri_owned_by, source_authority_did};

pub type FilterPredicate = Box<dyn Fn(&AtUri<DefaultStr>) -> bool + Send + Sync + 'static>;

pub trait ListFilter: serde::de::DeserializeOwned + Send + Sync + 'static {
    fn predicate(&self, state: &AppState, subject: &SubjectRef) -> FilterPredicate;
    fn is_identity(&self) -> bool;
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq)]
pub struct NoFilter;

impl ListFilter for NoFilter {
    fn predicate(&self, _: &AppState, _: &SubjectRef) -> FilterPredicate {
        Box::new(|_| true)
    }

    fn is_identity(&self) -> bool {
        true
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum IssueState {
    Open,
    Closed,
}

impl From<IssueState> for IssueStateKind {
    fn from(s: IssueState) -> Self {
        match s {
            IssueState::Open => IssueStateKind::Open,
            IssueState::Closed => IssueStateKind::Closed,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct IssueFilter {
    pub author: Option<Did<DefaultStr>>,
    pub state: Option<IssueState>,
}

impl ListFilter for IssueFilter {
    fn predicate(&self, state: &AppState, subject: &SubjectRef) -> FilterPredicate {
        compose_state_filter::<IssueStateKind>(
            self.author.clone(),
            self.state.map(Into::into),
            state.issue_states.clone(),
            subject.as_did().cloned(),
        )
    }

    fn is_identity(&self) -> bool {
        self.author.is_none() && self.state.is_none()
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum PullStatus {
    Open,
    Closed,
    Merged,
}

impl From<PullStatus> for PullStatusKind {
    fn from(s: PullStatus) -> Self {
        match s {
            PullStatus::Open => PullStatusKind::Open,
            PullStatus::Closed => PullStatusKind::Closed,
            PullStatus::Merged => PullStatusKind::Merged,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct PullFilter {
    pub author: Option<Did<DefaultStr>>,
    pub status: Option<PullStatus>,
}

impl ListFilter for PullFilter {
    fn predicate(&self, state: &AppState, subject: &SubjectRef) -> FilterPredicate {
        compose_state_filter::<PullStatusKind>(
            self.author.clone(),
            self.status.map(Into::into),
            state.pull_statuses.clone(),
            subject.as_did().cloned(),
        )
    }

    fn is_identity(&self) -> bool {
        self.author.is_none() && self.status.is_none()
    }
}

fn compose_state_filter<K>(
    author: Option<Did<DefaultStr>>,
    want: Option<K>,
    index: Arc<StateIndex<K>>,
    repo_owner: Option<Did<DefaultStr>>,
) -> FilterPredicate
where
    K: bobbin_edge_index::StateKind + Default + 'static,
{
    Box::new(move |uri| {
        if let Some(a) = &author
            && !at_uri_owned_by(uri, a)
        {
            return false;
        }
        let Some(want) = want else {
            return true;
        };
        let Some(repo_owner) = repo_owner.as_ref() else {
            return false;
        };
        let entity_author = source_authority_did(uri);
        let accept =
            |src: &AtUri<DefaultStr>| accept_state_source(src, entity_author.as_ref(), repo_owner);
        let effective = index
            .latest_by(uri, accept)
            .map(|(k, _)| k)
            .unwrap_or_default();
        effective == want
    })
}
