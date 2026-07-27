use std::sync::Arc;

use axum::Json;
use axum::extract::{FromRequestParts, Query, State};
use axum::response::{IntoResponse, Response};
use http::StatusCode;
use http::request::Parts;
use serde::{Deserialize, Serialize};

use knot_cobs::Grant;
use knot_index::Resolved;
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, RepoDid};

use crate::XrpcState;
use crate::error::XrpcError;
use crate::query::{Limit, Offset, Order, Total, ValidatedQuery, next_cursor};
use crate::wire::rfc3339;

pub(crate) const LIST_MEMBERS_ROUTE: &str = "/xrpc/sh.tangled.knot.listMembers";
pub(crate) const LIST_COLLABORATORS_ROUTE: &str = "/xrpc/sh.tangled.repo.listCollaborators";

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;

#[derive(Deserialize)]
pub(crate) struct Paging {
    #[serde(default)]
    limit: Limit<DEFAULT_LIMIT, MAX_LIMIT>,
    #[serde(default)]
    cursor: Offset,
    #[serde(default)]
    order: Order,
}

struct Window {
    offset: Offset,
    limit: Limit<DEFAULT_LIMIT, MAX_LIMIT>,
    descending: bool,
}

impl Paging {
    fn window(self) -> Window {
        Window {
            offset: self.cursor,
            limit: self.limit,
            descending: self.order.descending(),
        }
    }
}

#[derive(Deserialize)]
struct SubjectQuery {
    subject: Option<String>,
}

fn subject_param(parts: &Parts) -> Result<String, XrpcError> {
    Query::<SubjectQuery>::try_from_uri(&parts.uri)
        .map_err(|rejection| XrpcError::invalid_request(rejection.body_text()))?
        .0
        .subject
        .filter(|raw| !raw.is_empty())
        .ok_or_else(|| XrpcError::invalid_request("missing subject parameter"))
}

pub(crate) struct MemberSubject;

impl<S: Send + Sync> FromRequestParts<S> for MemberSubject {
    type Rejection = XrpcError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let subject = subject_param(parts)?;
        AccountDid::new(subject)
            .map(|_| MemberSubject)
            .map_err(|_| {
                XrpcError::named(
                    StatusCode::BAD_REQUEST,
                    "InvalidSubject",
                    "subject must be an account DID",
                )
            })
    }
}

pub(crate) struct CollaboratorRepo(RepoDid);

impl<S: Send + Sync> FromRequestParts<S> for CollaboratorRepo {
    type Rejection = XrpcError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let subject = subject_param(parts)?;
        RepoDid::new(subject).map(CollaboratorRepo).map_err(|_| {
            XrpcError::named(
                StatusCode::BAD_REQUEST,
                "InvalidRepo",
                "subject must be a repo DID",
            )
        })
    }
}

#[derive(Serialize)]
struct ItemWire {
    subject: AccountDid,
    #[serde(rename = "addedBy")]
    added_by: AccountDid,
    #[serde(rename = "createdAt")]
    created_at: String,
}

#[derive(Serialize)]
struct PageWire {
    items: Vec<ItemWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

fn respond(mut entries: Vec<Grant>, window: Window) -> Response {
    entries.sort_by(|a, b| {
        let by_time = a.created_at.cmp(&b.created_at);
        let by_time = match window.descending {
            true => by_time.reverse(),
            false => by_time,
        };
        by_time.then_with(|| a.subject.cmp(&b.subject))
    });
    let total = entries.len();
    let items = entries
        .into_iter()
        .skip(window.offset.get())
        .take(window.limit.get())
        .map(|grant| ItemWire {
            subject: grant.subject,
            added_by: grant.added_by,
            created_at: rfc3339(grant.created_at.get(), 0),
        })
        .collect();
    let cursor = next_cursor(window.offset, window.limit, Total::new(total));
    Json(PageWire { items, cursor }).into_response()
}

fn members_warming() -> XrpcError {
    XrpcError::warming("members projection is still warming")
}

fn collaborators_warming() -> XrpcError {
    XrpcError::warming("collaborators projection is still warming")
}

pub(crate) async fn list_members<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    _subject: MemberSubject,
    ValidatedQuery(paging): ValidatedQuery<Paging>,
) -> Result<Response, XrpcError> {
    let window = paging.window();
    match state.index.member_entries() {
        Resolved::Warming => Err(members_warming()),
        Resolved::Ready(entries) => Ok(respond(entries, window)),
    }
}

pub(crate) async fn list_collaborators<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    CollaboratorRepo(repo): CollaboratorRepo,
    ValidatedQuery(paging): ValidatedQuery<Paging>,
) -> Result<Response, XrpcError> {
    let window = paging.window();
    match state.index.owner_of(&repo) {
        Resolved::Warming => return Err(crate::reads::warming()),
        Resolved::Ready(None) => return Ok(respond(Vec::new(), window)),
        Resolved::Ready(Some(_)) => {}
    }
    let index = Arc::clone(&state.index);
    let target = repo.clone();
    let entries = crate::run_blocking(move || {
        index
            .ensure_collaborators(&target)
            .map_err(|_| collaborators_warming())?;
        match index.collaborator_entries(&target) {
            Resolved::Warming => Err(collaborators_warming()),
            Resolved::Ready(entries) => Ok(entries),
        }
    })
    .await?;
    Ok(respond(entries, window))
}
