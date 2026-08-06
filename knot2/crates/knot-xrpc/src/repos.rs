use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::HeaderMap;
use serde::{Deserialize, Serialize};

use knot_acl::{KnotAcl, can_admin_knot, can_create_repo, can_delete_repo};
use knot_atproto::{PreparedRepoDid, RecordPresence};
use knot_cob::{CobHome, CobStore};
use knot_cobs::{
    Registration, RegistryChange, Rename, RepoRef, RepoRegistryCob, deregister_repo, register_repo,
};
use knot_git::{GitError, Layout, Repo};
use knot_index::Resolved;
use knot_runtime::{Clock, HttpTransport, Signer};
use knot_types::{
    AccountDid, ActorId, BranchName, OwnerDid, RefName, RepoDid, RepoName, RepoRkey, UnixSeconds,
};

use crate::body::SourceUrl;
use crate::error::XrpcError;
use crate::reservations::ReserveDecision;
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const CREATE_ROUTE: &str = "/xrpc/sh.tangled.repo.create";
pub(crate) const DELETE_ROUTE: &str = "/xrpc/sh.tangled.repo.delete";
pub(crate) const RENAME_ROUTE: &str = "/xrpc/sh.tangled.repo.rename";
pub(crate) const RESERVE_ROUTE: &str = "/xrpc/sh.tangled.repo.reserveKey";

#[derive(Deserialize)]
struct CreateInput {
    rkey: RepoRkey,
    name: RepoName,
    #[serde(rename = "defaultBranch")]
    default_branch: Option<BranchName>,
    #[serde(default, deserialize_with = "crate::body::optional_source_url")]
    source: Option<SourceUrl>,
    #[serde(rename = "repoDid")]
    repo_did: Option<RepoDid>,
}

#[derive(Serialize)]
struct CreateOutput {
    #[serde(rename = "repoDid")]
    repo_did: RepoDid,
    key: ActorId,
    #[serde(rename = "lfsMissing", skip_serializing_if = "Vec::is_empty")]
    lfs_missing: Vec<knot_lfs::LfsOid>,
}

#[derive(Deserialize)]
struct DeleteInput {
    repo: RepoDid,
    #[serde(default)]
    force: bool,
}

#[derive(Deserialize)]
struct RenameInput {
    repo: RepoDid,
    rkey: RepoRkey,
    name: RepoName,
}

#[derive(Deserialize)]
struct ReserveInput {
    #[serde(rename = "repoDid")]
    repo_did: RepoDid,
}

#[derive(Serialize)]
struct ReserveOutput {
    #[serde(rename = "repoDid")]
    repo_did: RepoDid,
    key: ActorId,
}

pub(crate) async fn reserve_key<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_create_repo(&acl, &actor).is_allowed() {
        return Err(XrpcError::forbidden(
            "only knot admin or member may reserve a repository key",
        ));
    }

    let ReserveInput { repo_did } = decode(&body)?;
    if !repo_did.as_str().starts_with("did:web:") {
        return Err(XrpcError::invalid_request(
            "only did:web repo identity needs a reserved key. Omit repoDid on create to mint a did:plc.",
        ));
    }
    if repo_did.as_str() == state.knot_did.as_str() {
        return Err(XrpcError::invalid_request(
            "repoDid mustn't be knot's own identity",
        ));
    }
    match state.index.owner_of(&repo_did) {
        Resolved::Ready(Some(_)) => {
            return Err(XrpcError::conflict(
                "that repo DID is already hosted on this knot",
            ));
        }
        Resolved::Warming => {
            return Err(XrpcError::warming("registry projection is still warming"));
        }
        Resolved::Ready(None) => {}
    }

    let now = state.now();
    state.reservations.prune(now);

    match state.reservations.try_reserve(&repo_did, &actor, now) {
        ReserveDecision::HeldByOther => {
            return Err(XrpcError::conflict(
                "that repo DID is reserved by another account",
            ));
        }
        ReserveDecision::PerActorFull => {
            return Err(XrpcError::rate_limited(
                "you are holding maximum number of reserved repository keys awaiting creation",
            ));
        }
        ReserveDecision::GlobalFull => {
            return Err(XrpcError::rate_limited(
                "knot is holding maximum number of reserved repository keys awaiting creation",
            ));
        }
        ReserveDecision::Fresh | ReserveDecision::Renewed => {}
    }

    let public = match state.secrets.public_key(&state.knot_did) {
        Ok(public) => public,
        Err(error) => {
            state.reservations.release(&repo_did);
            return Err(error.into());
        }
    };
    let key = ActorId::from_secp256k1(public.as_bytes());

    Ok((http::StatusCode::OK, Json(ReserveOutput { repo_did, key })).into_response())
}

pub(crate) async fn create_repo<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_create_repo(&acl, &actor).is_allowed() {
        return Err(XrpcError::forbidden(
            "only knot admin or member may create repositories",
        ));
    }

    let input: CreateInput = decode(&body)?;
    let head = input.default_branch.map(|branch| branch.head_ref());

    let owner = OwnerDid::new(actor.as_str()).expect("account DID is always a valid owner DID");
    match state.index.resolve_repo(&owner, &input.rkey) {
        Resolved::Ready(Some(existing)) => match state.index.rkey_of(&existing) {
            Resolved::Ready(Some(canonical)) if canonical == input.rkey => {
                return Err(XrpcError::conflict(
                    "repository with that record key already exists for this owner",
                ));
            }
            Resolved::Warming => {
                return Err(XrpcError::warming("registry projection is still warming"));
            }
            _ => {}
        },
        Resolved::Warming => {
            return Err(XrpcError::warming("registry projection is still warming"));
        }
        Resolved::Ready(None) => {}
    }

    let fork = match &input.source {
        Some(origin) => Some(crate::forks::ForkSource::resolve(&state, origin).await?),
        None => None,
    };
    let object_format = fork.as_ref().map(|fork| fork.refs.object_format);

    let (repo_did, provisioning) =
        provision_repo_did(&state, &actor, &input.rkey, input.repo_did).await?;
    let now = state.now();
    let knot_signer = state.secrets.signer(&state.knot_did)?;
    let key = ActorId::from_secp256k1(knot_signer.public_key().as_bytes());

    let registration = Registration {
        owner,
        rkey: input.rkey,
        name: input.name,
        repo: repo_did.clone(),
        created_at: now,
    };

    let lfs_store = state.lfs.as_ref().map(|web| Arc::clone(&web.handle.store));
    let layout = state.layout.clone();
    let placed = repo_did.clone();
    let lfs = lfs_store.clone();
    let provisioning = run_blocking(move || {
        let repo = match object_format {
            Some(format) => layout.create_with_format(&placed, format),
            None => layout.create(&placed),
        }
        .map_err(|error| match error {
            GitError::AlreadyExists(_) => XrpcError::conflict("repository already exists on disk"),
            GitError::ReservedDid(_) => {
                XrpcError::invalid_request("repoDid mustn't be knot's own identity")
            }
            other => XrpcError::internal(other.to_string()),
        })?;
        let outcome = stage_repo(&repo, head.as_ref());
        if outcome.is_err() {
            rollback_local(&layout, lfs.as_deref(), &placed);
        }
        outcome.map(|()| provisioning)
    })
    .await?;

    let lfs_missing = match &fork {
        Some(fork) => match populate(&state, &repo_did, fork).await {
            Ok(missing) => missing,
            Err(error) => {
                let layout = state.layout.clone();
                let placed = repo_did.clone();
                let lfs = lfs_store.clone();
                let _ = run_blocking(move || {
                    rollback_local(&layout, lfs.as_deref(), &placed);
                    Ok(())
                })
                .await;
                return Err(error);
            }
        },
        None => Vec::new(),
    };

    let submission = match &provisioning {
        Provisioned::Minted(prepared) => state
            .atproto
            .submit_plc_operation(prepared)
            .await
            .map(|_| ()),
        Provisioned::Reserved => Ok(()),
    };
    if let Err(error) = submission {
        let layout = state.layout.clone();
        let placed = repo_did.clone();
        let lfs = lfs_store.clone();
        let _ = run_blocking(move || {
            rollback_local(&layout, lfs.as_deref(), &placed);
            Ok(())
        })
        .await;
        return Err(error.into());
    }

    let reserved = matches!(provisioning, Provisioned::Reserved);
    let layout = state.layout.clone();
    let cob_locks = Arc::clone(&state.cob_locks);
    let meta_path = state.meta_path.clone();
    let placed = repo_did.clone();
    let lfs = lfs_store.clone();
    let home = CobHome::from(&state.knot_did);
    run_blocking(move || {
        let outcome = {
            let _guard = cob_locks.meta();
            Repo::open(&meta_path)
                .map_err(XrpcError::from)
                .and_then(|meta| {
                    register(
                        &CobStore::new(&meta),
                        &home,
                        registration,
                        &knot_signer,
                        now,
                    )
                })
        };
        if outcome.is_err() {
            rollback_local(&layout, lfs.as_deref(), &placed);
            if !reserved {
                tracing::error!(
                    repo = %placed,
                    "registration failed after did:plc submitted to PLC directory"
                );
            }
        }
        outcome
    })
    .await?;

    if reserved {
        state.reservations.release(&repo_did);
    }

    let index = Arc::clone(&state.index);
    let refreshed = repo_did.clone();
    run_blocking(move || {
        index.refresh_registry().map_err(|error| {
            XrpcError::internal(format!(
                "repo {refreshed} was created and registered but registry projection refresh failed: {error}"
            ))
        })
    })
    .await?;

    Ok((
        http::StatusCode::OK,
        Json(CreateOutput {
            repo_did,
            key,
            lfs_missing,
        }),
    )
        .into_response())
}

async fn populate<H: HttpTransport, C: Clock>(
    state: &Arc<XrpcState<H, C>>,
    repo_did: &RepoDid,
    fork: &crate::forks::ForkSource,
) -> Result<Vec<knot_lfs::LfsOid>, XrpcError> {
    let tips = fork.refs.tips();
    let pack = crate::forks::upstream_pack(
        state,
        &fork.upstream,
        knot_pack::WantOids::new(tips.clone()),
        knot_pack::HaveOids::default(),
    )
    .await?;
    let layout = state.layout.clone();
    let placed = repo_did.clone();
    let origin = fork.origin.clone();
    let refs = fork.refs.clone();
    run_blocking(move || {
        let repo = layout.open(&placed)?;
        crate::forks::populate_fork(&repo, &refs, &pack, &origin)
    })
    .await?;
    crate::lfs::mirror_fork_objects(
        Arc::clone(state),
        fork.upstream.clone(),
        repo_did.clone(),
        knot_pack::WantOids::new(tips),
        knot_pack::HaveOids::default(),
    )
    .await
}

fn rollback_local(layout: &Layout, lfs: Option<&knot_lfs::DiskStore>, repo_did: &RepoDid) {
    if let Some(store) = lfs
        && let Err(error) = store.remove_repo(repo_did)
    {
        tracing::error!(repo = %repo_did, %error, "couldn't roll back lfs prefix");
    }
    if let Err(error) = layout.remove(repo_did) {
        tracing::error!(repo = %repo_did, %error, "couldn't roll back on-disk repo :3");
    }
}

enum Provisioned {
    Minted(PreparedRepoDid),
    Reserved,
}

async fn provision_repo_did<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    actor: &knot_types::AccountDid,
    rkey: &RepoRkey,
    provided: Option<RepoDid>,
) -> Result<(RepoDid, Provisioned), XrpcError> {
    match provided {
        Some(did) if did.as_str().starts_with("did:web:") => {
            match state.index.owner_of(&did) {
                Resolved::Ready(Some(_)) => {
                    return Err(XrpcError::conflict(
                        "that repo DID is already hosted on this knot",
                    ));
                }
                Resolved::Warming => {
                    return Err(XrpcError::warming("registry projection is still warming"));
                }
                Resolved::Ready(None) => {}
            }
            if !state.reservations.holder_is(&did, actor, state.now()) {
                return Err(XrpcError::invalid_request(
                    "reserve this did:web for your own account via sh.tangled.repo.reserveKey before creating it",
                ));
            }
            let knot_public = state.secrets.public_key(&state.knot_did)?;
            state
                .atproto
                .verify_did_web_publishes_key(&did, &knot_public)
                .await
                .map_err(XrpcError::from)?;
            Ok((did, Provisioned::Reserved))
        }
        Some(_) => Err(XrpcError::invalid_request(
            "repoDid must be did:web hosted on your own domain. Omit it to mint a did:plc.",
        )),
        None => {
            let knot_signer = state.secrets.signer(&state.knot_did)?;
            let owner =
                OwnerDid::new(actor.as_str()).expect("account DID is always a valid owner DID");
            let nonce = knot_atproto::MintNonce::mint(&*state.entropy, &owner, rkey);
            let prepared =
                knot_atproto::prepare_repo_did(&knot_signer, &state.knot_service_url, &nonce)
                    .map_err(XrpcError::from)?;
            Ok((prepared.did.clone(), Provisioned::Minted(prepared)))
        }
    }
}

fn stage_repo(repo: &Repo, head: Option<&RefName>) -> Result<(), XrpcError> {
    if let Some(refname) = head {
        repo.set_head(refname)?;
    }
    Ok(())
}

fn register(
    store: &CobStore,
    home: &CobHome,
    registration: Registration,
    signer: &dyn Signer,
    now: UnixSeconds,
) -> Result<(), XrpcError> {
    match store
        .list::<RepoRegistryCob>()
        .map_err(XrpcError::from)?
        .as_slice()
    {
        [] => store
            .create(home, &RegistryChange::Register(registration), signer, now)
            .map(|_| ())
            .map_err(XrpcError::from),
        [object] => register_repo(store, home, *object, registration, signer, now)
            .map(|_| ())
            .map_err(XrpcError::from),
        many => Err(XrpcError::internal(format!(
            "{} repo registry objects share meta-repo",
            many.len()
        ))),
    }
}

pub(crate) async fn delete_repo<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let DeleteInput {
        repo: repo_did,
        force,
    } = decode(&body)?;

    let RepoRef { owner: did, rkey } = match state.index.ownership_of(&repo_did) {
        Resolved::Ready(Some(found)) => found,
        Resolved::Ready(None) => return Ok(ok_empty()),
        Resolved::Warming => {
            return Err(XrpcError::warming("registry projection is still warming"));
        }
    };

    let acl = KnotAcl::new(&state.admins, state.admission, &state.index);
    if !can_delete_repo(&acl, &actor, &repo_did).is_allowed() {
        return Err(XrpcError::forbidden(
            "only repository owner or a knot admin may delete it",
        ));
    }

    if force {
        if !can_admin_knot(&acl, &actor).is_allowed() {
            return Err(XrpcError::forbidden(
                "only knot admin may force a delete past the PDS record check",
            ));
        }
    } else {
        let owner = AccountDid::from(did.clone());
        match state.atproto.repo_record_present(&owner, &rkey).await {
            Ok(RecordPresence::Present) => {
                return Err(XrpcError::conflict(
                    "sh.tangled.repo record still exists on the owner's PDS. Remove it there first or force the delete.",
                ));
            }
            Ok(RecordPresence::Absent) => {}
            Err(error) => {
                tracing::warn!(
                    repo = %repo_did,
                    %error,
                    "proceeding w/ best-effort delete despite unconfirmed owner PDS record :p"
                );
            }
        }
    }

    let now = state.now();
    let knot_signer = state.secrets.signer(&state.knot_did)?;
    let layout = state.layout.clone();
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let meta_path = state.meta_path.clone();
    let target = RepoRef { owner: did, rkey };
    let deleted = repo_did.clone();
    let home = CobHome::from(&state.knot_did);
    let lfs_store = state.lfs.as_ref().map(|lfs| Arc::clone(&lfs.handle.store));
    run_blocking(move || {
        let _repo_guard = cob_locks.repo(&deleted);
        let _meta_guard = cob_locks.meta();
        let meta = Repo::open(&meta_path)?;
        let store = CobStore::new(&meta);
        deregister(&store, &home, target, &deleted, &knot_signer, now)?;
        let removal = layout.remove(&deleted);
        index.refresh_registry().map_err(|error| {
            XrpcError::internal(format!(
                "repo {deleted} was deregistered but registry projection refresh failed: {error}"
            ))
        })?;
        if let Some(store) = &lfs_store
            && let Err(error) = store.remove_repo(&deleted)
        {
            tracing::warn!(
                repo = %deleted,
                %error,
                "lfs prefix removal on delete failed, orphan sweep will reclaim"
            );
        }
        removal.map_err(|error| {
            XrpcError::internal(format!(
                "repo {deleted} was deregistered but its on-disk directory couldn't be removed: {error}"
            ))
        })
    })
    .await?;

    Ok(ok_empty())
}

fn deregister(
    store: &CobStore,
    home: &CobHome,
    target: RepoRef,
    expected: &RepoDid,
    signer: &dyn Signer,
    now: UnixSeconds,
) -> Result<(), XrpcError> {
    match store
        .list::<RepoRegistryCob>()
        .map_err(XrpcError::from)?
        .as_slice()
    {
        [] => Ok(()),
        [object] => deregister_repo(store, home, *object, target, expected.clone(), signer, now)
            .map(|_| ())
            .map_err(XrpcError::from),
        many => Err(XrpcError::internal(format!(
            "{} repo registry objects share meta-repo",
            many.len()
        ))),
    }
}

pub(crate) async fn rename_repo<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let RenameInput { repo, rkey, name } = decode(&body)?;

    let owner = match state.index.owner_of(&repo) {
        Resolved::Ready(Some(owner)) => owner,
        Resolved::Ready(None) => {
            return Err(XrpcError::not_found("no such repository on this knot"));
        }
        Resolved::Warming => {
            return Err(XrpcError::warming("registry projection is still warming"));
        }
    };

    crate::authorize_push(
        &state,
        &actor,
        &repo,
        "only repository owner or a collaborator may rename it",
    )
    .await?;

    let now = state.now();
    let knot_signer = state.secrets.signer(&state.knot_did)?;
    let index = Arc::clone(&state.index);
    let cob_locks = Arc::clone(&state.cob_locks);
    let meta_path = state.meta_path.clone();
    let renamed = repo.clone();
    let home = CobHome::from(&state.knot_did);
    run_blocking(move || {
        {
            let _guard = cob_locks.meta();
            let meta = Repo::open(&meta_path)?;
            let store = CobStore::new(&meta);
            match store
                .list::<RepoRegistryCob>()
                .map_err(XrpcError::from)?
                .as_slice()
            {
                [] => {
                    return Err(XrpcError::not_found(
                        "no repositories are registered on this knot",
                    ));
                }
                [object] => {
                    knot_cobs::rename_repo(
                        &store,
                        &home,
                        *object,
                        Rename {
                            owner,
                            rkey,
                            name,
                            repo: renamed.clone(),
                        },
                        &knot_signer,
                        now,
                    )
                    .map(|_| ())
                    .map_err(XrpcError::from)?;
                }
                many => {
                    return Err(XrpcError::internal(format!(
                        "{} repo registry objects share meta-repo",
                        many.len()
                    )));
                }
            }
        }
        index.refresh_registry().map_err(|error| {
            XrpcError::internal(format!(
                "repo {renamed} was renamed but registry projection refresh failed: {error}"
            ))
        })
    })
    .await?;

    Ok(ok_empty())
}
