use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;
use serde::Deserialize;

use knot_events::GitRefUpdate;
use knot_git::{GitError, RefUpdate};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{BranchName, RepoDid};

use crate::error::XrpcError;
use crate::reads::require_hosted;
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const SET_DEFAULT_ROUTE: &str = "/xrpc/sh.tangled.repo.setDefaultBranch";
pub(crate) const DELETE_ROUTE: &str = "/xrpc/sh.tangled.repo.deleteBranch";

#[derive(Deserialize)]
struct SetDefaultBranchInput {
    repo: RepoDid,
    #[serde(rename = "defaultBranch")]
    default_branch: BranchName,
}

#[derive(Deserialize)]
struct DeleteBranchInput {
    repo: RepoDid,
    branch: BranchName,
}

const BRANCH_DENIED: &str = "only repository owner or a collaborator may change its branches";

pub(crate) async fn set_default_branch<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let input: SetDefaultBranchInput = decode(&body)?;
    let repo_did = require_hosted(&state, input.repo)?;
    crate::authorize_push(&state, &actor, &repo_did, BRANCH_DENIED).await?;
    let refname = input.default_branch.head_ref();

    let layout = state.layout.clone();
    let target = repo_did.clone();
    let events = Arc::clone(&state.events);
    let reservation = run_blocking(move || {
        let repo = layout.open(&target)?;
        let target_exists = repo.find_ref(&refname)?.is_some();
        let has_branches = !repo.branches()?.is_empty();
        if !target_exists && has_branches {
            return Err(XrpcError::not_found("no such branch to set as default"));
        }
        repo.set_head_sealed(&refname, || events.reserve())
            .map_err(XrpcError::from)
    })
    .await?;

    let owner = crate::current_owner(&state, &repo_did);
    reservation.fulfill(&GitRefUpdate::new(repo_did.into_did(), owner, actor));

    Ok(ok_empty())
}

pub(crate) async fn delete_branch<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let input: DeleteBranchInput = decode(&body)?;
    let repo_did = require_hosted(&state, input.repo)?;
    crate::authorize_push(&state, &actor, &repo_did, BRANCH_DENIED).await?;
    let refname = input.branch.head_ref();

    let layout = state.layout.clone();
    let target = repo_did.clone();
    let deleted_ref = refname.clone();
    let events = Arc::clone(&state.events);
    let (old, reservation, format) = run_blocking(move || {
        let repo = layout.open(&target)?;
        if repo.default_branch().as_ref() == Some(&refname) {
            return Err(XrpcError::invalid_request(
                "default branch cannot be deleted until a different default is set",
            ));
        }
        let old = repo
            .find_ref(&refname)?
            .ok_or_else(|| XrpcError::not_found("no such branch"))?;
        let reservation = repo
            .update_ref_sealed(&RefUpdate::Delete { name: refname, old }, || {
                events.reserve()
            })
            .map_err(|error| match error {
                GitError::AtomicRefs(_) => {
                    XrpcError::conflict("branch changed during deletion, retry")
                }
                other => XrpcError::internal(other.to_string()),
            })?;
        Ok((old, reservation, repo.object_format()))
    })
    .await?;

    let owner = crate::current_owner(&state, &repo_did);
    reservation.fulfill(
        &GitRefUpdate::new(repo_did.into_did(), owner, actor).on_ref(
            deleted_ref,
            knot_types::RefTransition::Delete { old },
            format,
        ),
    );

    Ok(ok_empty())
}
