use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;
use serde::Deserialize;

use knot_acl::{KnotAcl, can_manage_collaborators};
use knot_cob::{CobHome, CobStore};
use knot_cobs::{CollaboratorsChange, CollaboratorsCob, Grant, Removal};
use knot_events::RepoCollaboratorUpdate;
use knot_index::Resolved;
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, RepoDid};

use crate::cob::grant_set_apply;
use crate::error::XrpcError;
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const ADD_ROUTE: &str = "/xrpc/sh.tangled.repo.addCollaborator";
pub(crate) const REMOVE_ROUTE: &str = "/xrpc/sh.tangled.repo.removeCollaborator";

#[derive(Deserialize)]
struct CollaboratorInput {
    repo: RepoDid,
    subject: AccountDid,
}

fn require_owner<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    actor: &AccountDid,
    repo: &RepoDid,
) -> Result<knot_types::OwnerDid, XrpcError> {
    let owner = match state.index.owner_of(repo) {
        Resolved::Ready(Some(owner)) => owner,
        Resolved::Ready(None) => {
            return Err(XrpcError::not_found(
                "repository isn't registered on this knot",
            ));
        }
        Resolved::Warming => {
            return Err(XrpcError::warming("registry projection is still warming"));
        }
    };
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if can_manage_collaborators(&acl, actor, repo).is_allowed() {
        Ok(owner)
    } else {
        Err(XrpcError::forbidden(
            "only repository owner may manage collaborators",
        ))
    }
}

pub(crate) async fn add_collaborator<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let CollaboratorInput { repo, subject } = decode(&body)?;
    let owner = require_owner(&state, &actor, &repo)?;

    crate::fold_collaborators(&state, &repo).await;
    if owner.is(&subject)
        || matches!(
            state.index.is_collaborator(&repo, &subject),
            Resolved::Ready(true)
        )
    {
        return Ok(ok_empty());
    }

    let now = state.now();
    let event_subject = subject.clone();
    let event_repo = repo.clone();
    let grant = Grant {
        subject,
        added_by: actor,
        created_at: now,
    };
    let signer = state.secrets.signer(&state.knot_did).map_err(|error| {
        XrpcError::internal(format!("knot signing key is unavailable: {error}"))
    })?;
    let layout = state.layout.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let events = Arc::clone(&state.events);
    run_blocking(move || {
        let _guard = cob_locks.repo(&repo);
        owner_unmoved(&index, &repo, &owner)?;
        let git = layout.open(&repo)?;
        let changed = grant_set_apply::<CollaboratorsCob>(
            &CobStore::new(&git),
            &CobHome::from(&repo),
            CollaboratorsChange::Add(grant),
            &signer,
            now,
            true,
        )?;
        index.refresh_collaborators(&repo)?;
        if changed {
            events.publish(&RepoCollaboratorUpdate::added(event_subject, event_repo));
        }
        Ok(())
    })
    .await?;

    Ok(ok_empty())
}

fn owner_unmoved(
    index: &knot_index::Index,
    repo: &RepoDid,
    owner: &knot_types::OwnerDid,
) -> Result<(), XrpcError> {
    match index.owner_of(repo) {
        Resolved::Ready(Some(current)) if current == *owner => Ok(()),
        _ => Err(XrpcError::conflict(
            "repository is no longer registered to owner who authorized this request",
        )),
    }
}

pub(crate) async fn remove_collaborator<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let CollaboratorInput { repo, subject } = decode(&body)?;
    let owner = require_owner(&state, &actor, &repo)?;

    crate::fold_collaborators(&state, &repo).await;
    if matches!(
        state.index.is_collaborator(&repo, &subject),
        Resolved::Ready(false)
    ) {
        return Ok(ok_empty());
    }

    let now = state.now();
    let event_subject = subject.clone();
    let event_repo = repo.clone();
    let removal = Removal { subject };
    let signer = state.secrets.signer(&state.knot_did).map_err(|error| {
        XrpcError::internal(format!("knot signing key is unavailable: {error}"))
    })?;
    let layout = state.layout.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let events = Arc::clone(&state.events);
    run_blocking(move || {
        let _guard = cob_locks.repo(&repo);
        owner_unmoved(&index, &repo, &owner)?;
        let git = layout.open(&repo)?;
        let changed = grant_set_apply::<CollaboratorsCob>(
            &CobStore::new(&git),
            &CobHome::from(&repo),
            CollaboratorsChange::Remove(removal),
            &signer,
            now,
            false,
        )?;
        index.refresh_collaborators(&repo)?;
        if changed {
            events.publish(&RepoCollaboratorUpdate::removed(event_subject, event_repo));
        }
        Ok(())
    })
    .await?;

    Ok(ok_empty())
}
