use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;
use serde::Deserialize;

use knot_acl::{KnotAcl, can_admin_knot};
use knot_cob::{CobHome, CobStore};
use knot_cobs::{Grant, MembersChange, MembersCob, Removal};
use knot_events::KnotMemberUpdate;
use knot_git::Repo;
use knot_index::Resolved;
use knot_runtime::{Clock, HttpTransport};
use knot_types::AccountDid;

use crate::cob::grant_set_apply;
use crate::error::XrpcError;
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const ADD_ROUTE: &str = "/xrpc/sh.tangled.knot.addMember";
pub(crate) const REMOVE_ROUTE: &str = "/xrpc/sh.tangled.knot.removeMember";

#[derive(Deserialize)]
struct SubjectInput {
    subject: AccountDid,
}

pub(crate) async fn add_member<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_admin_knot(&acl, &actor).is_allowed() {
        return Err(XrpcError::forbidden("only knot admin may add members"));
    }

    let SubjectInput { subject } = decode(&body)?;
    if state.admins.contains(&subject)
        || matches!(state.index.is_member(&subject), Resolved::Ready(true))
    {
        return Ok(ok_empty());
    }

    let now = state.now();
    let event_subject = subject.clone();
    let grant = Grant {
        subject,
        added_by: actor,
        created_at: now,
    };
    let signer = state.secrets.signer(&state.knot_did)?;
    let meta_path = state.meta_path.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let events = Arc::clone(&state.events);
    let home = CobHome::from(&state.knot_did);
    run_blocking(move || {
        let _guard = cob_locks.meta();
        let meta = Repo::open(&meta_path)?;
        let changed = grant_set_apply::<MembersCob>(
            &CobStore::new(&meta),
            &home,
            MembersChange::Add(grant),
            &signer,
            now,
            true,
        )?;
        index.refresh_members()?;
        if changed {
            events.publish(&KnotMemberUpdate::added(event_subject));
        }
        Ok(())
    })
    .await?;

    Ok(ok_empty())
}

pub(crate) async fn remove_member<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_admin_knot(&acl, &actor).is_allowed() {
        return Err(XrpcError::forbidden("only knot admin may remove members"));
    }

    let SubjectInput { subject } = decode(&body)?;
    if matches!(state.index.is_member(&subject), Resolved::Ready(false)) {
        return Ok(ok_empty());
    }

    let now = state.now();
    let event_subject = subject.clone();
    let removal = Removal { subject };
    let signer = state.secrets.signer(&state.knot_did)?;
    let meta_path = state.meta_path.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let events = Arc::clone(&state.events);
    let home = CobHome::from(&state.knot_did);
    run_blocking(move || {
        let _guard = cob_locks.meta();
        let meta = Repo::open(&meta_path)?;
        let changed = grant_set_apply::<MembersCob>(
            &CobStore::new(&meta),
            &home,
            MembersChange::Remove(removal),
            &signer,
            now,
            false,
        )?;
        index.refresh_members()?;
        if changed {
            events.publish(&KnotMemberUpdate::removed(event_subject));
        }
        Ok(())
    })
    .await?;

    Ok(ok_empty())
}
