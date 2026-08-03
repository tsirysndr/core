use std::net::IpAddr;
use std::sync::Arc;

use knot_atproto::ClaimedKeys;
use knot_index::{Coverage, Resolved};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AccountDid, OfferedKey, OwnerRef};

use crate::SshState;

#[derive(Clone)]
pub(crate) enum Credential {
    Identified(AccountDid),
    Offered(OfferedKey),
}

pub(crate) struct Asserted {
    claim: OwnerRef,
    outcome: Claimed,
}

enum Claimed {
    Publishes {
        did: AccountDid,
        keys: Vec<OfferedKey>,
    },
    Unreadable,
}

pub(crate) enum Verdict {
    Identified(AccountDid),
    Offered,
    Refused,
}

pub(crate) async fn verify<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    claim: Option<OwnerRef>,
    key: &OfferedKey,
    peer: Option<IpAddr>,
    asserted: &mut Option<Asserted>,
) -> Verdict {
    match claim {
        Some(claim) => match against_claim(state, claim, key, peer, asserted).await {
            Some(verdict) => verdict,
            None => against_key_set(state, key, peer),
        },
        None => against_key_set(state, key, peer),
    }
}

fn against_key_set<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    key: &OfferedKey,
    peer: Option<IpAddr>,
) -> Verdict {
    let now = state.atproto.now().seconds();
    match (
        state.index.owner_of_key(key, now),
        state.index.keys().coverage(),
    ) {
        (Resolved::Ready(Some(_)), _) => Verdict::Offered,
        (_, Coverage::Warming) => Verdict::Offered,
        (_, Coverage::Ready) => match state.index.keys().any_unheld() {
            true => Verdict::Offered,
            false => {
                if miss_worth_a_reread(state, peer) {
                    state.index.keys().note_miss();
                }
                Verdict::Refused
            }
        },
    }
}

fn miss_worth_a_reread<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    peer: Option<IpAddr>,
) -> bool {
    peer.is_none_or(|peer| state.miss_pace.reserve_now(&peer, state.atproto.now()))
}

async fn against_claim<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    claim: OwnerRef,
    key: &OfferedKey,
    peer: Option<IpAddr>,
    asserted: &mut Option<Asserted>,
) -> Option<Verdict> {
    let known = match asserted.take().filter(|known| known.claim == claim) {
        Some(known) => known,
        None => Asserted {
            outcome: resolve_claim(state, &claim, peer).await,
            claim,
        },
    };
    let verdict = match &known.outcome {
        Claimed::Unreadable => None,
        Claimed::Publishes { did, keys } if keys.contains(key) => {
            Some(Verdict::Identified(did.clone()))
        }
        Claimed::Publishes { did, keys } => {
            tracing::debug!(
                ?peer,
                did = did.as_str(),
                published = keys.len(),
                "ssh auth refused a key the asserted account doesn't publish"
            );
            Some(Verdict::Refused)
        }
    };
    *asserted = Some(known);
    verdict
}

async fn resolve_claim<H: HttpTransport, C: Clock>(
    state: &Arc<SshState<H, C>>,
    claim: &OwnerRef,
    peer: Option<IpAddr>,
) -> Claimed {
    let Ok(_peer_guard) = state.lookup_peers.admit(peer, state.atproto.now()) else {
        tracing::debug!(?peer, "ssh auth couldn't check a claim, peer budget spent");
        return Claimed::Unreadable;
    };
    let did = match claim {
        OwnerRef::Did(did) => AccountDid::from(did.clone()),
        OwnerRef::Handle(handle) => match state.atproto.resolve_handle_to_did(handle).await {
            Ok(did) => did,
            Err(error) => {
                tracing::warn!(
                    ?peer,
                    handle = handle.as_str(),
                    %error,
                    "ssh auth couldn't resolve the handle in the login name"
                );
                return Claimed::Unreadable;
            }
        },
    };
    match state.atproto.claimed_pubkeys(&did).await {
        ClaimedKeys::Published(keys) => Claimed::Publishes { did, keys },
        ClaimedKeys::Unread(error) => {
            tracing::warn!(
                ?peer,
                did = did.as_str(),
                %error,
                "ssh auth couldn't read the asserted account's published keys"
            );
            Claimed::Unreadable
        }
    }
}
