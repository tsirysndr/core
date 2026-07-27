use std::collections::BTreeMap;

use gix::bstr::{BStr, ByteSlice as _};
use gix::date::Time;
use knot_git::Repo;
use knot_runtime::{Signature, Signer};
use knot_types::{ActorId, ChangeId, CobId, Oid, RefName, TypeName, UnixSeconds};

use crate::change::{Change, CobHome, Payload};
use crate::error::CobError;
use crate::graph::ChangeGraph;
use crate::object::HistoryModel;

const COBS_PREFIX: &str = "refs/cobs/";
const CHECKPOINTS_PREFIX: &str = "refs/cob-checkpoints/";
const TYPE_HEADER: &str = "cob-type";
const SIG_HEADER: &str = "cob-sig";
const AUTHOR_HEADER: &str = "cob-author";
const PAYLOAD_BLOB: &str = "payload";
pub(crate) const MAX_GRAPH_CHANGES: usize = 100_000;
const REBUILD_CHANGE_BYTES: u64 = 2560;
const REBUILD_FOLD_DIVISOR: u64 = 4;
// two bazillion
const REBUILD_UNMEASURED_CHANGES: usize = 2_000_000;

pub(crate) fn rebuild_change_limit_for(available: Option<knot_resource::AvailableBytes>) -> usize {
    let derived = match available {
        Some(available) => {
            usize::try_from(available.get() / REBUILD_FOLD_DIVISOR / REBUILD_CHANGE_BYTES)
                .unwrap_or(usize::MAX)
        }
        None => REBUILD_UNMEASURED_CHANGES,
    };
    derived.max(MAX_GRAPH_CHANGES)
}

pub(crate) fn rebuild_graph_limit() -> usize {
    rebuild_change_limit_for(knot_resource::available_bytes())
}

pub(crate) fn cob_ref_name(type_name: &TypeName, object: CobId) -> Result<RefName, CobError> {
    let raw = format!(
        "{COBS_PREFIX}{}/{}",
        type_name.as_str(),
        object.oid().to_hex()
    );
    RefName::new(raw.as_str()).map_err(|_| CobError::RefName(raw))
}

pub(crate) fn checkpoint_ref_name(
    type_name: &TypeName,
    object: CobId,
) -> Result<RefName, CobError> {
    let raw = format!(
        "{CHECKPOINTS_PREFIX}{}/{}",
        type_name.as_str(),
        object.oid().to_hex()
    );
    RefName::new(raw.as_str()).map_err(|_| CobError::RefName(raw))
}

pub fn parse_cob_ref(refname: &str) -> Option<(TypeName, CobId)> {
    let (nsid, oid) = refname.strip_prefix(COBS_PREFIX)?.rsplit_once('/')?;
    let type_name = TypeName::new(nsid).ok()?;
    let object = Oid::from_hex(oid).ok().map(CobId::new)?;
    Some((type_name, object))
}

pub(crate) fn resolve_tip(
    repo: &Repo,
    type_name: &TypeName,
    object: CobId,
) -> Result<Option<Oid>, CobError> {
    let name = cob_ref_name(type_name, object)?;
    Ok(repo.find_ref(&name)?)
}

pub(crate) fn list_objects(repo: &Repo, type_name: &TypeName) -> Result<Vec<CobId>, CobError> {
    let prefix = format!("{COBS_PREFIX}{}/", type_name.as_str());
    Ok(repo
        .references()?
        .into_iter()
        .filter_map(|record| {
            let rest = record.name.as_str().strip_prefix(&prefix)?;
            Oid::from_hex(rest).ok().map(CobId::new)
        })
        .collect())
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_change(
    home: &CobHome,
    repo: &Repo,
    type_name: &TypeName,
    payload: &[u8],
    parents: &[ChangeId],
    object: Option<CobId>,
    signer: &dyn Signer,
    timestamp: UnixSeconds,
) -> Result<ChangeId, CobError> {
    let git = repo.git();
    let payload_oid = git
        .write_blob(payload)
        .map_err(|error| CobError::Write(error.to_string()))?
        .detach();
    let revision = git
        .write_object(build_tree(payload_oid))
        .map_err(|error| CobError::Write(error.to_string()))?
        .detach();
    let author = ActorId::from_secp256k1(signer.public_key().as_bytes());
    let revision_oid = Oid::from(revision);
    let binding = crate::change::object_binding(parents, object);
    let signing = crate::change::signing_bytes(
        home,
        revision_oid,
        parents,
        type_name,
        &author,
        timestamp,
        binding,
    );
    let signature = signer.sign(&signing);
    if !crate::change::verify_signature(
        home,
        revision_oid,
        parents,
        type_name,
        &author,
        timestamp,
        object,
        signature.as_bytes(),
    ) {
        return Err(CobError::SelfCheck(type_name.clone()));
    }
    let commit = gix::objs::Commit {
        tree: revision,
        parents: parents
            .iter()
            .map(|parent| parent.oid().object_id())
            .collect(),
        author: knot_identity(timestamp),
        committer: knot_identity(timestamp),
        encoding: None,
        message: Vec::new().into(),
        extra_headers: vec![
            (TYPE_HEADER.into(), type_name.as_str().into()),
            (AUTHOR_HEADER.into(), author.as_str().into()),
            (
                SIG_HEADER.into(),
                knot_types::lowercase_hex(signature.as_bytes()).into(),
            ),
        ],
    };
    let id = git
        .write_object(commit)
        .map_err(|error| CobError::Write(error.to_string()))?
        .detach();
    Ok(ChangeId::new(Oid::from(id)))
}

pub(crate) fn load_graph(
    repo: &Repo,
    type_name: &TypeName,
    object: CobId,
    history: HistoryModel,
    limit: usize,
) -> Result<(ChangeGraph, ChangeId), CobError> {
    let tip = resolve_tip(repo, type_name, object)?.ok_or(CobError::NoSuchObject(object))?;
    let changes = collect(repo, ChangeId::new(tip), object, limit, None)?;
    check_full_shape(&changes, object, history)?;
    Ok((ChangeGraph::new(object, changes), ChangeId::new(tip)))
}

pub(crate) fn check_full_shape(
    changes: &BTreeMap<ChangeId, Change>,
    object: CobId,
    history: HistoryModel,
) -> Result<(), CobError> {
    let root_id = ChangeId::new(object.oid());
    let root = changes.get(&root_id).ok_or(CobError::DetachedTip(object))?;
    if !root.parents.is_empty() {
        return Err(CobError::RootNotGenesis(object));
    }
    if let Some(stray) = changes
        .values()
        .find(|change| change.id != root_id && change.parents.is_empty())
    {
        return Err(CobError::MultipleRoots {
            object,
            stray: stray.id,
        });
    }
    check_no_forbidden_merge(changes, object, history)
}

pub(crate) fn check_delta_shape(
    changes: &BTreeMap<ChangeId, Change>,
    object: CobId,
    since: ChangeId,
    history: HistoryModel,
) -> Result<(), CobError> {
    check_no_forbidden_merge(changes, object, history)?;
    let descends = changes
        .values()
        .any(|change| change.parents.contains(&since));
    if !changes.is_empty() && !descends {
        return Err(CobError::DivergedTip { object, since });
    }
    Ok(())
}

fn check_no_forbidden_merge(
    changes: &BTreeMap<ChangeId, Change>,
    object: CobId,
    history: HistoryModel,
) -> Result<(), CobError> {
    let forbidden_merge = (history == HistoryModel::Linear)
        .then(|| changes.values().find(|change| change.parents.len() > 1))
        .flatten();
    match forbidden_merge {
        Some(merge) => Err(CobError::ForkedHistory {
            object,
            change: merge.id,
        }),
        None => Ok(()),
    }
}

pub(crate) fn collect(
    repo: &Repo,
    tip: ChangeId,
    object: CobId,
    limit: usize,
    stop: Option<ChangeId>,
) -> Result<BTreeMap<ChangeId, Change>, CobError> {
    let mut frontier = vec![tip];
    let mut seen: BTreeMap<ChangeId, Change> = BTreeMap::new();
    let mut overflowed = false;
    let walk = {
        let mut step = || -> Option<Result<(), CobError>> {
            let head = frontier.pop()?;
            if Some(head) == stop || seen.contains_key(&head) {
                return Some(Ok(()));
            }
            if seen.len() >= limit {
                overflowed = true;
                return None;
            }
            match read_change(repo, head) {
                Ok(change) => {
                    frontier.extend(change.parents.iter().copied());
                    seen.insert(head, change);
                    Some(Ok(()))
                }
                Err(error) => Some(Err(error)),
            }
        };
        std::iter::from_fn(&mut step).try_for_each(|outcome| outcome)
    };
    walk?;
    if overflowed {
        return Err(CobError::HistoryTooLong(object));
    }
    Ok(seen)
}

pub(crate) fn read_change(repo: &Repo, id: ChangeId) -> Result<Change, CobError> {
    let oid = id.oid();
    let malformed = |reason: String| CobError::MalformedChange { oid, reason };
    #[cfg(feature = "instrument")]
    crate::instrument::record_read();
    let data = repo
        .git()
        .find_object(oid.object_id())
        .map_err(|error| malformed(error.to_string()))?
        .detach()
        .data;
    let commit = gix::objs::CommitRef::from_bytes(&data, repo.git().object_hash())
        .map_err(|error| malformed(error.to_string()))?;
    let revision = Oid::from(commit.tree());
    let parents = commit
        .parents()
        .map(|parent| ChangeId::new(Oid::from(parent)))
        .collect();
    let timestamp = UnixSeconds::new(
        commit
            .time()
            .map_err(|error| malformed(error.to_string()))?
            .seconds,
    );
    let type_raw = commit
        .extra_headers()
        .find(TYPE_HEADER)
        .ok_or_else(|| malformed("missing cob-type header".into()))?;
    let type_name = TypeName::new(
        type_raw
            .to_str()
            .map_err(|error| malformed(error.to_string()))?,
    )
    .map_err(|error| malformed(error.to_string()))?;
    let author_raw = commit
        .extra_headers()
        .find(AUTHOR_HEADER)
        .ok_or_else(|| malformed("missing cob-author header".into()))?;
    let author = ActorId::new(
        author_raw
            .to_str()
            .map_err(|error| malformed(error.to_string()))?,
    )
    .map_err(|error| malformed(error.to_string()))?;
    let signature = commit
        .extra_headers()
        .find(SIG_HEADER)
        .ok_or_else(|| malformed("missing cob-sig header".into()))
        .and_then(|raw| {
            knot_types::decode_hex(raw).ok_or_else(|| malformed("cob-sig isn't valid hex".into()))
        })?;
    let payload = read_payload(repo, revision)?;
    Ok(Change {
        id,
        revision,
        parents,
        type_name,
        author,
        signature: Signature::from_bytes(signature),
        payload: Payload::new(payload),
        timestamp,
    })
}

fn read_payload(repo: &Repo, revision: Oid) -> Result<Vec<u8>, CobError> {
    let malformed = |reason: String| CobError::MalformedChange {
        oid: revision,
        reason,
    };
    #[cfg(feature = "instrument")]
    crate::instrument::record_read();
    let data = repo
        .git()
        .find_object(revision.object_id())
        .map_err(|error| malformed(error.to_string()))?
        .detach()
        .data;
    let tree = gix::objs::TreeRef::from_bytes(&data, repo.git().object_hash())
        .map_err(|error| malformed(error.to_string()))?;
    let payload_oid =
        entry_oid(&tree, PAYLOAD_BLOB).ok_or_else(|| malformed("missing payload blob".into()))?;
    let payload = repo
        .git()
        .find_object(payload_oid)
        .map_err(|error| malformed(error.to_string()))?
        .detach()
        .data;
    Ok(payload)
}

fn entry_oid(tree: &gix::objs::TreeRef<'_>, name: &str) -> Option<gix::ObjectId> {
    tree.entries
        .iter()
        .find(|entry| entry.filename == BStr::new(name))
        .map(|entry| entry.oid.to_owned())
}

fn build_tree(payload_oid: gix::ObjectId) -> gix::objs::Tree {
    gix::objs::Tree {
        entries: vec![blob_entry(PAYLOAD_BLOB, payload_oid)],
    }
}

fn blob_entry(name: &str, oid: gix::ObjectId) -> gix::objs::tree::Entry {
    gix::objs::tree::Entry {
        mode: gix::objs::tree::EntryKind::Blob.into(),
        filename: name.into(),
        oid,
    }
}

fn knot_identity(time: UnixSeconds) -> gix::actor::Signature {
    gix::actor::Signature {
        name: "knot".into(),
        email: "noreply@knot".into(),
        time: Time::new(time.get(), 0),
    }
}
