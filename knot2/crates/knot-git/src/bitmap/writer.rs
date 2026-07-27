use std::collections::HashSet;
use std::path::Path;

use knot_types::Oid;

use super::revindex::{Order, OrderTable};
use super::{BitPosition, BitmapEntryOffset, IndexPosition};
use crate::error::GitError;
use crate::objects::{Haves, Wants};
use crate::repo::Repo;

const OPT_FULL_DAG: u16 = 0x1;
const OPT_LOOKUP_TABLE: u16 = 0x10;

const CAT_COMMIT: u8 = 0;
const CAT_TREE: u8 = 1;
const CAT_BLOB: u8 = 2;
const CAT_TAG: u8 = 3;

pub(super) struct TypeBits {
    commits: Vec<bool>,
    trees: Vec<bool>,
    blobs: Vec<bool>,
    tags: Vec<bool>,
}

pub(super) struct Selected {
    commit_pos: IndexPosition,
    bits: Vec<bool>,
}

pub(crate) fn write(repo: &Repo, pack_idx: &Path) -> Result<bool, GitError> {
    let kind = repo.object_format().kind();
    let index = gix_pack::index::File::at(pack_idx, kind)
        .map_err(|error| GitError::Backend(format!("open pack index: {error}")))?;
    let rev = OrderTable::from_index(&index);
    if rev.len() == 0 {
        return Ok(false);
    }

    let _boost = knot_resource::saturate();
    let types = type_index_bits(repo, &rev)?;
    let selected = selected_entries(repo, &rev)?;
    if selected.is_empty() {
        return Ok(false);
    }

    let pack_path = pack_idx.with_extension("pack");
    let checksum = gix_pack::data::File::at(&pack_path, kind)
        .map_err(|error| GitError::Backend(format!("open pack data: {error}")))?
        .checksum();

    let bytes = assemble(kind, &checksum, &types, &selected)?;
    install(&pack_idx.with_extension("bitmap"), &bytes)?;
    Ok(true)
}

pub(super) fn type_index_bits<R: Order + Sync>(repo: &Repo, rev: &R) -> Result<TypeBits, GitError> {
    let categories = categories_in_bit_order(repo, rev)?;
    let select = |target: u8| {
        categories
            .iter()
            .map(|value| *value == target)
            .collect::<Vec<bool>>()
    };
    Ok(TypeBits {
        commits: select(CAT_COMMIT),
        trees: select(CAT_TREE),
        blobs: select(CAT_BLOB),
        tags: select(CAT_TAG),
    })
}

fn categories_in_bit_order<R: Order + Sync>(repo: &Repo, rev: &R) -> Result<Vec<u8>, GitError> {
    let path = repo.path().to_owned();
    knot_resource::map_spans(rev.len(), |start, end| {
        let local = Repo::open(&path)?;
        (start..end)
            .map(|bit| object_category(&local, rev.oid_at_bit(BitPosition(bit as u32))))
            .collect::<Result<Vec<u8>, GitError>>()
    })
}

fn object_category(repo: &Repo, oid: Oid) -> Result<u8, GitError> {
    match repo.git().try_find_header(oid.object_id()) {
        Ok(Some(header)) => Ok(match header.kind() {
            gix::object::Kind::Commit => CAT_COMMIT,
            gix::object::Kind::Tree => CAT_TREE,
            gix::object::Kind::Blob => CAT_BLOB,
            gix::object::Kind::Tag => CAT_TAG,
        }),
        Ok(None) => Err(GitError::ObjectNotFound(oid)),
        Err(error) => Err(GitError::Corrupt {
            oid,
            message: error.to_string(),
        }),
    }
}

fn one_selected<R: Order>(
    repo: &Repo,
    rev: &R,
    commit_pos: IndexPosition,
    commit: Oid,
) -> Result<Selected, GitError> {
    let closure = repo.select_pack_objects(Wants::new(&[commit]), Haves::new(&[]))?;
    Ok(Selected {
        commit_pos,
        bits: closure_bits(rev, &closure)?,
    })
}

pub(super) fn selected_entries<R: Order + Sync>(
    repo: &Repo,
    rev: &R,
) -> Result<Vec<Selected>, GitError> {
    let mut seen: HashSet<Oid> = HashSet::new();
    let commits: Vec<(IndexPosition, Oid)> = repo
        .references()?
        .into_iter()
        .filter_map(|record| peel_to_commit(repo, record.target))
        .filter_map(|commit| rev.index_of(commit).map(|position| (position, commit)))
        .filter(|(_, commit)| seen.insert(*commit))
        .collect();

    let path = repo.path().to_owned();
    let mut entries: Vec<Selected> = knot_resource::map_chunks(&commits, |batch| {
        let local = Repo::open(&path)?;
        batch
            .iter()
            .map(|(commit_pos, commit)| one_selected(&local, rev, *commit_pos, *commit))
            .collect::<Result<Vec<_>, GitError>>()
    })?;
    entries.sort_by_key(|entry| entry.commit_pos);
    Ok(entries)
}

fn peel_to_commit(repo: &Repo, oid: Oid) -> Option<Oid> {
    let object = repo.git().find_object(oid.object_id()).ok()?;
    match object.kind {
        gix::object::Kind::Commit => Some(oid),
        gix::object::Kind::Tag => {
            let target = object.try_into_tag().ok()?.target_id().ok()?.detach();
            peel_to_commit(repo, Oid::from(target))
        }
        _ => None,
    }
}

fn closure_bits(rev: &impl Order, closure: &[Oid]) -> Result<Vec<bool>, GitError> {
    closure
        .iter()
        .try_fold(vec![false; rev.len()], |mut bits, oid| {
            let position = rev.index_of(*oid).ok_or_else(|| {
                GitError::Backend(format!(
                    "closure object {oid} is absent from the pack bitmap"
                ))
            })?;
            bits[rev.bit_at_index(position).get() as usize] = true;
            Ok(bits)
        })
}

pub(super) fn assemble(
    kind: gix::hash::Kind,
    checksum: &gix_hash::ObjectId,
    types: &TypeBits,
    selected: &[Selected],
) -> Result<Vec<u8>, GitError> {
    let flags: u16 = OPT_FULL_DAG | OPT_LOOKUP_TABLE;
    let mut out = Vec::new();
    out.extend_from_slice(b"BITM");
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&flags.to_be_bytes());
    out.extend_from_slice(&(selected.len() as u32).to_be_bytes());
    out.extend_from_slice(checksum.as_slice());

    write_ewah(&mut out, &types.commits)?;
    write_ewah(&mut out, &types.trees)?;
    write_ewah(&mut out, &types.blobs)?;
    write_ewah(&mut out, &types.tags)?;

    let offsets: Vec<(IndexPosition, BitmapEntryOffset)> = selected
        .iter()
        .map(|entry| {
            let at = BitmapEntryOffset::new(out.len() as u64);
            out.extend_from_slice(&entry.commit_pos.get().to_be_bytes());
            out.push(0);
            out.push(0);
            write_ewah(&mut out, &entry.bits)?;
            Ok::<_, GitError>((entry.commit_pos, at))
        })
        .collect::<Result<_, _>>()?;

    offsets.iter().for_each(|(commit_pos, offset)| {
        out.extend_from_slice(&commit_pos.get().to_be_bytes());
        out.extend_from_slice(&offset.get().to_be_bytes());
        out.extend_from_slice(&0xffff_ffffu32.to_be_bytes());
    });

    let mut hasher = gix_hash::hasher(kind);
    hasher.update(&out);
    let digest = hasher
        .try_finalize()
        .map_err(|error| GitError::Backend(format!("bitmap checksum: {error}")))?;
    out.extend_from_slice(digest.as_slice());
    Ok(out)
}

fn write_ewah(out: &mut Vec<u8>, bits: &[bool]) -> Result<(), GitError> {
    let vector = gix_bitmap::ewah::Vec::from_bits(bits)
        .ok_or_else(|| GitError::Backend("ewah bit count exceeds u32".to_string()))?;
    vector
        .write_to(out)
        .map_err(|error| GitError::Backend(format!("ewah write: {error}")))
}

pub(super) fn install(path: &Path, bytes: &[u8]) -> Result<(), GitError> {
    knot_resource::atomic_write_bytes(path, bytes, knot_resource::FileMode::Inherited).map_err(
        |error| GitError::Maintenance(format!("write bitmap {}: {}", path.display(), error.source)),
    )
}

#[cfg(test)]
mod tests {
    use super::super::{BitPosition, IndexPosition};
    use super::*;

    struct FakeOrder {
        oids: Vec<Oid>,
    }

    impl Order for FakeOrder {
        fn len(&self) -> usize {
            self.oids.len()
        }

        fn index_of(&self, oid: Oid) -> Option<IndexPosition> {
            self.oids
                .iter()
                .position(|candidate| *candidate == oid)
                .map(|position| IndexPosition::new(position as u32))
        }

        fn bit_at_index(&self, position: IndexPosition) -> BitPosition {
            BitPosition::new(position.get())
        }

        fn oid_at_bit(&self, bit: BitPosition) -> Oid {
            self.oids[bit.get() as usize]
        }
    }

    fn oid(byte: u8) -> Oid {
        Oid::from_hex(&format!("{byte:02x}").repeat(20)).unwrap()
    }

    #[test]
    fn closure_bits_sets_one_bit_per_present_object() {
        let order = FakeOrder {
            oids: vec![oid(1), oid(2), oid(3)],
        };
        let bits = closure_bits(&order, &[oid(1), oid(3)]).unwrap();
        assert_eq!(bits, vec![true, false, true]);
    }

    #[test]
    fn closure_bits_errors_when_an_object_is_outside_the_pack() {
        let order = FakeOrder {
            oids: vec![oid(1), oid(2)],
        };
        assert!(
            closure_bits(&order, &[oid(1), oid(9)]).is_err(),
            "an incomplete closure must fail the bitmap rather than write a partial one"
        );
    }
}
