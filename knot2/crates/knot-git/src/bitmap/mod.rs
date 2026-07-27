use std::path::{Path, PathBuf};

use knot_types::{ObjectCount, Oid};

use crate::error::GitError;
use crate::objects::{Haves, Wants};
use crate::repo::Repo;

mod bitset;
mod midx;
mod reader;
mod revindex;
mod writer;

use bitset::Bitset;
use revindex::{Order, OrderTable};

knot_types::scalar_newtype! {
    pub(crate) struct BitPosition(u32);
    pub(crate) struct IndexPosition(u32) => ordered;
    pub(crate) struct BitmapEntryOffset(u64);
}

pub fn write_bitmap(repo: &Repo, pack_idx: &Path) -> Result<bool, GitError> {
    writer::write(repo, pack_idx)
}

pub fn write_midx_bitmap(repo: &Repo) -> Result<bool, GitError> {
    midx::write(repo)
}

pub fn reachable_via_bitmap(
    repo: &Repo,
    wants: Wants<'_>,
    haves: Haves<'_>,
) -> Result<Option<Vec<Oid>>, GitError> {
    if let Some((_, found)) = single_pack(repo, wants, haves)? {
        return Ok(Some(found));
    }
    midx::reachable(repo, wants, haves)
}

fn single_pack(
    repo: &Repo,
    wants: Wants<'_>,
    haves: Haves<'_>,
) -> Result<Option<(ObjectCount, Vec<Oid>)>, GitError> {
    let kind = repo.object_format().kind();
    let Some(pack_idx) = bitmapped_pack(&repo.objects_dir()) else {
        return Ok(None);
    };
    let Ok(bytes) = std::fs::read(pack_idx.with_extension("bitmap")) else {
        return Ok(None);
    };
    let index = gix_pack::index::File::at(&pack_idx, kind)
        .map_err(|error| GitError::Backend(format!("open pack index: {error}")))?;
    let rev = OrderTable::from_index(&index);
    let count = rev.len();
    let maps = reader::parse(&bytes, kind, count)?;
    Ok(resolve(repo, &rev, &maps, wants, haves)?
        .map(|reachable| (ObjectCount::new(count), reachable)))
}

pub(super) fn resolve(
    repo: &Repo,
    rev: &impl Order,
    maps: &reader::Bitmaps<'_>,
    wants: Wants<'_>,
    haves: Haves<'_>,
) -> Result<Option<Vec<Oid>>, GitError> {
    let (Some(want), Some(have)) = (
        accumulate(repo, rev, maps, wants.as_slice())?,
        accumulate(repo, rev, maps, haves.as_slice())?,
    ) else {
        return Ok(None);
    };
    let reachable = want
        .difference_indices(&have)
        .map(|bit| rev.oid_at_bit(bit))
        .collect();
    Ok(Some(reachable))
}

pub fn verbatim_clone_pack(
    repo: &Repo,
    wants: Wants<'_>,
) -> Result<Option<std::fs::File>, GitError> {
    let Some(pack_idx) = bitmapped_pack(&repo.objects_dir()) else {
        return Ok(None);
    };
    let Some((num_objects, reachable)) = single_pack(repo, wants, Haves::new(&[]))? else {
        return Ok(None);
    };
    if ObjectCount::new(reachable.len()) != num_objects {
        return Ok(None);
    }
    Ok(std::fs::File::open(pack_idx.with_extension("pack")).ok())
}

fn bitmapped_pack(objects_dir: &Path) -> Option<PathBuf> {
    let pack_dir = objects_dir.join("pack");
    let mut idxs: Vec<PathBuf> = std::fs::read_dir(&pack_dir)
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .collect();
    idxs.sort();
    idxs.into_iter()
        .find(|idx| idx.with_extension("bitmap").exists())
}

fn accumulate(
    repo: &Repo,
    rev: &impl Order,
    maps: &reader::Bitmaps<'_>,
    oids: &[Oid],
) -> Result<Option<Bitset>, GitError> {
    oids.iter()
        .try_fold(Some(Bitset::zeros(rev.len())), |acc, oid| {
            let Some(mut acc) = acc else {
                return Ok(None);
            };
            match contribution(repo, rev, maps, *oid)? {
                Some(part) => {
                    acc.union_with(&part);
                    Ok(Some(acc))
                }
                None => Ok(None),
            }
        })
}

fn contribution(
    repo: &Repo,
    rev: &impl Order,
    maps: &reader::Bitmaps<'_>,
    oid: Oid,
) -> Result<Option<Bitset>, GitError> {
    let Some((commit, tags)) = peel_commit_chain(repo, oid) else {
        return Ok(None);
    };
    let Some(position) = rev.index_of(commit) else {
        return Ok(None);
    };
    let Some(base) = maps.bitmap(position)? else {
        return Ok(None);
    };
    let bits = tags.iter().try_fold(base, |mut bits, tag| {
        let position = rev.index_of(*tag)?;
        bits.set(rev.bit_at_index(position));
        Some(bits)
    });
    Ok(bits)
}

fn peel_commit_chain(repo: &Repo, oid: Oid) -> Option<(Oid, Vec<Oid>)> {
    let object = repo.git().find_object(oid.object_id()).ok()?;
    match object.kind {
        gix::object::Kind::Commit => Some((oid, Vec::new())),
        gix::object::Kind::Tag => {
            let target = object.try_into_tag().ok()?.target_id().ok()?.detach();
            peel_commit_chain(repo, Oid::from(target)).map(|(commit, mut tags)| {
                tags.push(oid);
                (commit, tags)
            })
        }
        _ => None,
    }
}
