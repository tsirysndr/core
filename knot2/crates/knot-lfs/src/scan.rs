use std::collections::BTreeMap;

use knot_git::{Filter, GitError, Haves, PackBudget, Repo, Wants};

use crate::pointer::{POINTER_MAX_BYTES, parse_pointer};
use crate::{ClaimedSize, LfsOid};

pub fn scan_pointers(
    repo: &Repo,
    wants: Wants<'_>,
    haves: Haves<'_>,
) -> Result<BTreeMap<LfsOid, ClaimedSize>, GitError> {
    let selection = repo.select_pack_objects_filtered(
        wants,
        haves,
        Filter::BlobLimit(POINTER_MAX_BYTES + 1),
        PackBudget::unbounded(),
    )?;
    selection
        .send
        .iter()
        .filter_map(|oid| match repo.blob_size(*oid) {
            Ok(_) => Some(repo.read_blob(*oid)),
            Err(GitError::ObjectType { .. }) => None,
            Err(fault) => Some(Err(fault)),
        })
        .filter_map(|blob| match blob {
            Ok(bytes) => parse_pointer(&bytes).map(|pointer| Ok((pointer.oid, pointer.size))),
            Err(fault) => Some(Err(fault)),
        })
        .collect()
}
