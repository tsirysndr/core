//! # How we go about receiving a push!
//!
//! `land` will run the pack ingest and the pull-link lookup at the same time,
//! then will merge both into a single post-receive pass.
//! Each ref-update claims its event cursor during ingest
//! and fills the payload in afterward,
//! such that events replay in ref-order even though post-receive was what computed them.
//!
//! Or should I say:
//!
//! ```text
//!   request task                       blocking pool
//!   ---------------------------------  ------------------------------
//!   read the push preamble
//!     |
//!     +---- spawn -------------------> open repo
//!     |                                  |
//!   creates a branch? no -> no link    receive pack
//!     | yes                              |
//!   look up pull link                  seal: 1 cursor per ref update
//!     | repo-DID -> owner, rkey, handle  |
//!     v                                  v
//!   join <---------------------------- updates paired w/ reservations
//!     |
//!   any refs applied? no -> no messages
//!     | yes
//!   owner & rkey for this repo-DID
//!     |
//!   ci flags from push options
//!     |
//!     +---- spawn -------------------> ack lines
//!     |                                  |
//!     |                                post_receive fulfills each
//!     |                                  |            reservation
//!     v                                  v
//!   messages <------------------------ ..and returns its messages
//!     |
//!   note this push for maintenance
//!     |
//!   frame report for the client
//! ```
//!
//! *Figure 1: the tasks of a push.*
//!
//! `EventLog::replay` finishes at the oldest pending cursor,
//! meaning that a single reservation will hide every later-event until it resolves.
//! Every exit from `land` therefore has to either fulfill each one or drop it,
//! and `Reservation`'s `Drop` clears
//! the pending cursor such that replay advances again.
//!
//! ```text
//!   one Reservation
//!   ---------------
//!   reserve -> pending, replay finishes here
//!                |
//!                +-- post_receive fulfills it -----> event is visible
//!                +-- receive errors after seal ----> dropped, logged
//!                +-- receive task panics ----------> dropped, silent
//!                +-- post-receive task panics -----> dropped, logged
//!                +-- caller drops the land future -> dropped, silent
//! ```
//!
//! *Figure 2: every way in which a single reservation can end.*
//!
//! Note that last path in Figure 2 leaves the receive task running
//! with nowhere to return its result,
//! so the reservations inside it drop alongside the discarded value.

use std::cell::RefCell;
use std::sync::Arc;

use knot_atproto::Atproto;
use knot_cob::CobHome;
use knot_events::{EventLog, Reservation};
use knot_git::{Layout, RefUpdate, Repo};
use knot_index::{Index, Resolved};
use knot_maintenance::{MaintenanceHandle, PushBytes};
use knot_messages::{Catalog, PushAckKey, count_refs};
use knot_pack::{
    PackError, PackLimits, PushGuard, ReceiveOutcome, ReceivedPack, frame_report,
    receive_pack_guarded_streamed, receive_preflight,
};
use knot_postreceive::{Actor, Ci, LanguagesPushBudget, OwnerLabel, PullLink, post_receive};
use knot_resource::ResolveSlots;
use knot_runtime::{Clock, HttpTransport};
use knot_types::{
    AccountDid, ActorId, AppviewEndpoint, CiLogsAddr, Handle, KnotHostname, OwnerDid, PushOptions,
    RepoDid,
};

type Applied = Vec<(RefUpdate, Reservation)>;

pub struct Push<'a, H: HttpTransport, C: Clock> {
    pub layout: &'a Layout,
    pub repo_did: &'a RepoDid,
    pub received: ReceivedPack,
    pub limits: PackLimits,
    pub knot_actor: ActorId,
    pub committer: AccountDid,
    pub events: Arc<EventLog<C>>,
    pub index: &'a Index,
    pub atproto: &'a Atproto<H, C>,
    pub resolve_slots: &'a ResolveSlots,
    pub appview: &'a AppviewEndpoint,
    pub maintenance: &'a MaintenanceHandle,
    pub hostname: &'a KnotHostname,
    pub languages_push_budget: LanguagesPushBudget,
    pub ci_logs: Option<CiLogsAddr>,
    pub catalog: Arc<Catalog>,
}

pub async fn land<H: HttpTransport, C: Clock>(push: Push<'_, H, C>) -> Result<Vec<u8>, PackError> {
    let Push {
        layout,
        repo_did,
        received,
        limits,
        knot_actor,
        committer,
        events,
        index,
        atproto,
        resolve_slots,
        appview,
        maintenance,
        hostname,
        languages_push_budget,
        catalog,
        ci_logs,
    } = push;

    let preflight = receive_preflight(received.preamble());
    let body_len = received.len();
    let home = CobHome::from(repo_did);

    let receive = {
        let layout = layout.clone();
        let did = repo_did.clone();
        let events = Arc::clone(&events);
        let catalog = Arc::clone(&catalog);
        tokio::task::spawn_blocking(
            move || -> Result<(Repo, ReceiveOutcome, Applied), PackError> {
                let repo = layout.open(&did)?;
                let guard = PushGuard {
                    cob_authority: knot_actor,
                    home,
                    messages: Arc::clone(&catalog),
                };
                let stash: RefCell<Applied> = RefCell::new(Vec::new());
                let seal = |updates: &[RefUpdate]| {
                    updates.iter().for_each(|update| {
                        stash.borrow_mut().push((update.clone(), events.reserve()))
                    });
                };
                let outcome = match receive_pack_guarded_streamed(
                    &repo,
                    &received,
                    &limits,
                    &guard,
                    &seal,
                    &catalog.reject,
                ) {
                    Ok(outcome) => outcome,
                    Err(error) => {
                        match stash.borrow().len() {
                            0 => {}
                            sealed => tracing::error!(
                                repo = did.as_str(),
                                sealed,
                                "receive failure dropped the events for the sealed ref updates"
                            ),
                        }
                        return Err(error);
                    }
                };
                Ok((repo, outcome, stash.into_inner()))
            },
        )
    };
    let pull = async {
        match preflight.creates_branch {
            true => resolve_pull_link(index, atproto, resolve_slots, appview, repo_did).await,
            false => None,
        }
    };
    let (received, pull) = tokio::join!(receive, pull);
    let (repo, outcome, applied) = match received {
        Ok(Ok(triple)) => triple,
        Ok(Err(error)) => return Err(error),
        Err(_) => return Err(PackError::Pack("receive-pack task panicked".to_string())),
    };

    let messages = match applied.is_empty() {
        true => Vec::new(),
        false => {
            let owner = registry_owner(index, repo_did);
            let ci = ci_from_push_options(&outcome.push_options, ci_logs.clone());
            let push_options = outcome.push_options.clone();
            let did = repo_did.clone();
            let catalog = Arc::clone(&catalog);
            let knot = hostname.clone();
            tokio::task::spawn_blocking(move || {
                let actor = Actor {
                    committer,
                    owner,
                    repo: did,
                };
                let ack = catalog.push.ack.lines(|key| match key {
                    PushAckKey::Knot => knot.as_str().to_string(),
                    PushAckKey::Refs => count_refs(applied.len()),
                });
                ack.into_iter()
                    .chain(post_receive(
                        &repo,
                        &actor,
                        applied,
                        &ci,
                        &push_options,
                        pull.as_ref(),
                        languages_push_budget,
                        &catalog.push,
                    ))
                    .collect()
            })
            .await
            .unwrap_or_else(|error| {
                tracing::error!(repo = repo_did.as_str(), %error, "post-receive task panicked and dropped the ref-update events for this push");
                Vec::new()
            })
        }
    };
    maintenance.note_push(repo_did, PushBytes::new(body_len as u64));
    Ok(frame_report(&outcome.report, &messages, outcome.side_band))
}

fn registry_owner(index: &Index, repo: &RepoDid) -> Option<OwnerDid> {
    let ready = |owner: Resolved<Option<OwnerDid>>| match owner {
        Resolved::Ready(owner) => owner,
        Resolved::Warming => None,
    };
    match index.owner_of(repo) {
        resolved @ Resolved::Ready(_) => ready(resolved),
        Resolved::Warming => {
            if let Err(error) = index.refresh_registry() {
                tracing::warn!(
                    repo = repo.as_str(),
                    %error,
                    "registry refresh during post-receive failed, ref-update event omits the owner"
                );
            }
            ready(index.owner_of(repo))
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PushDirective {
    SkipCi,
    VerboseCi,
}

impl PushDirective {
    fn parse(option: &str) -> Option<Self> {
        match option {
            "skip-ci" | "ci-skip" => Some(Self::SkipCi),
            "verbose-ci" | "ci-verbose" => Some(Self::VerboseCi),
            _ => None,
        }
    }
}

pub fn ci_from_push_options(options: &PushOptions, logs: Option<CiLogsAddr>) -> Ci {
    let directives: Vec<PushDirective> = options
        .as_slice()
        .iter()
        .filter_map(|option| PushDirective::parse(option.as_str()))
        .collect();
    match directives.contains(&PushDirective::SkipCi) {
        true => Ci::Skip,
        false => Ci::Compile {
            logs,
            verbose: directives.contains(&PushDirective::VerboseCi),
        },
    }
}

async fn resolve_pull_link<H: HttpTransport, C: Clock>(
    index: &Index,
    atproto: &Atproto<H, C>,
    resolve_slots: &ResolveSlots,
    appview: &AppviewEndpoint,
    repo_did: &RepoDid,
) -> Option<PullLink> {
    let owner = match index.owner_of(repo_did) {
        Resolved::Ready(Some(owner)) => owner,
        _ => return None,
    };
    let rkey = match index.rkey_of(repo_did) {
        Resolved::Ready(Some(rkey)) => rkey,
        _ => return None,
    };
    Some(PullLink {
        appview: appview.clone(),
        owner: resolve_owner_label(atproto, resolve_slots, &owner).await,
        rkey,
    })
}

async fn resolve_owner_label<H: HttpTransport, C: Clock>(
    atproto: &Atproto<H, C>,
    resolve_slots: &ResolveSlots,
    owner: &OwnerDid,
) -> OwnerLabel {
    let did = AccountDid::from(owner.clone());
    match resolve_handle(atproto, resolve_slots, &did).await {
        Some(handle) => OwnerLabel::Handle(handle),
        None => OwnerLabel::Did(owner.clone()),
    }
}

pub async fn resolve_handle<H: HttpTransport, C: Clock>(
    atproto: &Atproto<H, C>,
    resolve_slots: &ResolveSlots,
    did: &AccountDid,
) -> Option<Handle> {
    let _permit = resolve_slots.try_acquire()?;
    let identity = atproto.resolve_identity(did).await.ok()?;
    identity.primary_handle().cloned()
}
