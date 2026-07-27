use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::State;
use axum::response::Response;
use http::HeaderMap;
use serde::Deserialize;

use knot_acl::{KnotAcl, can_admin_knot};
use knot_cob::{CobHome, CobStore};
use knot_cobs::{BlocklistChange, BlocklistCob, Grant, Removal};
use knot_git::Repo;
use knot_index::Resolved;
use knot_runtime::{Clock, HttpTransport};
use knot_types::AccountDid;

use crate::cob::grant_set_apply;
use crate::error::XrpcError;
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const BAN_ROUTE: &str = "/xrpc/sh.tangled.knot.ban";
pub(crate) const UNBAN_ROUTE: &str = "/xrpc/sh.tangled.knot.unban";

#[derive(Deserialize)]
struct SubjectInput {
    subject: AccountDid,
}

pub(crate) async fn ban<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_admin_knot(&acl, &actor).is_allowed() {
        return Err(XrpcError::forbidden("only knot admin may ban accounts"));
    }

    let SubjectInput { subject } = decode(&body)?;
    if state.admins.contains(&subject) {
        return Err(XrpcError::forbidden("admin cannot be banned"));
    }
    if matches!(state.index.is_blocked(&subject), Resolved::Ready(true)) {
        return Ok(ok_empty());
    }

    let now = state.now();
    let grant = Grant {
        subject,
        added_by: actor,
        created_at: now,
    };
    let signer = state.secrets.signer(&state.knot_did)?;
    let meta_path = state.meta_path.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let home = CobHome::from(&state.knot_did);
    run_blocking(move || {
        let _guard = cob_locks.meta();
        let meta = Repo::open(&meta_path)?;
        grant_set_apply::<BlocklistCob>(
            &CobStore::new(&meta),
            &home,
            BlocklistChange::Add(grant),
            &signer,
            now,
            true,
        )?;
        index.refresh_blocklist().map_err(XrpcError::from)
    })
    .await?;

    Ok(ok_empty())
}

pub(crate) async fn unban<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_admin_knot(&acl, &actor).is_allowed() {
        return Err(XrpcError::forbidden("only knot admin may unban accounts"));
    }

    let SubjectInput { subject } = decode(&body)?;
    if matches!(state.index.is_blocked(&subject), Resolved::Ready(false)) {
        return Ok(ok_empty());
    }

    let now = state.now();
    let removal = Removal { subject };
    let signer = state.secrets.signer(&state.knot_did)?;
    let meta_path = state.meta_path.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let home = CobHome::from(&state.knot_did);
    run_blocking(move || {
        let _guard = cob_locks.meta();
        let meta = Repo::open(&meta_path)?;
        grant_set_apply::<BlocklistCob>(
            &CobStore::new(&meta),
            &home,
            BlocklistChange::Remove(removal),
            &signer,
            now,
            false,
        )?;
        index.refresh_blocklist().map_err(XrpcError::from)
    })
    .await?;

    Ok(ok_empty())
}
