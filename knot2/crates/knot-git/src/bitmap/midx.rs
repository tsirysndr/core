use std::path::{Path, PathBuf};

use knot_types::Oid;

use crate::objects::{Haves, Wants};

use super::reader;
use super::revindex::{Order, OrderTable};
use super::writer;
use crate::error::GitError;
use crate::repo::Repo;

// hashtag easter egg
const RIDX_SIGNATURE: u32 = 0x5249_4458;
const MIDX_ALLOC_LIMIT_BYTES: usize = 16 * 1024 * 1024;

fn midx_path(objects_dir: &Path) -> PathBuf {
    objects_dir.join("pack").join("multi-pack-index")
}

fn sidecar(objects_dir: &Path, checksum: &gix_hash::ObjectId, ext: &str) -> PathBuf {
    objects_dir
        .join("pack")
        .join(format!("multi-pack-index-{}.{ext}", checksum.to_hex()))
}

pub(super) fn write(repo: &Repo) -> Result<bool, GitError> {
    let kind = repo.object_format().kind();
    let objects_dir = repo.objects_dir();
    let file = match gix_pack::multi_index::File::at(
        midx_path(&objects_dir),
        Some(MIDX_ALLOC_LIMIT_BYTES),
    ) {
        Ok(file) => file,
        Err(_) => return Ok(false),
    };
    let order = OrderTable::from_file(&file);
    if order.len() == 0 {
        return Ok(false);
    }

    let _boost = knot_resource::saturate();
    let types = writer::type_index_bits(repo, &order)?;
    let selected = writer::selected_entries(repo, &order)?;
    if selected.is_empty() {
        return Ok(false);
    }

    let checksum = file.checksum();
    let bytes = writer::assemble(kind, &checksum, &types, &selected)?;

    write_rev(
        &sidecar(&objects_dir, &checksum, "rev"),
        &order,
        kind,
        &checksum,
    )?;
    writer::install(&sidecar(&objects_dir, &checksum, "bitmap"), &bytes)?;
    Ok(true)
}

fn write_rev(
    path: &Path,
    order: &OrderTable,
    kind: gix::hash::Kind,
    checksum: &gix_hash::ObjectId,
) -> Result<(), GitError> {
    let hash_id: u32 = match kind {
        gix::hash::Kind::Sha256 => 2,
        _ => 1,
    };
    let mut out = Vec::new();
    out.extend_from_slice(&RIDX_SIGNATURE.to_be_bytes());
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&hash_id.to_be_bytes());
    order
        .index_positions_in_bit_order()
        .iter()
        .for_each(|position| out.extend_from_slice(&position.get().to_be_bytes()));
    out.extend_from_slice(checksum.as_slice());
    let mut hasher = gix_hash::hasher(kind);
    hasher.update(&out);
    let digest = hasher
        .try_finalize()
        .map_err(|error| GitError::Backend(format!("midx revindex checksum: {error}")))?;
    out.extend_from_slice(digest.as_slice());
    writer::install(path, &out)
}

pub(super) fn reachable(
    repo: &Repo,
    wants: Wants<'_>,
    haves: Haves<'_>,
) -> Result<Option<Vec<Oid>>, GitError> {
    let kind = repo.object_format().kind();
    let objects_dir = repo.objects_dir();
    let file = match gix_pack::multi_index::File::at(
        midx_path(&objects_dir),
        Some(MIDX_ALLOC_LIMIT_BYTES),
    ) {
        Ok(file) => file,
        Err(_) => return Ok(None),
    };
    let Ok(bytes) = std::fs::read(sidecar(&objects_dir, &file.checksum(), "bitmap")) else {
        return Ok(None);
    };
    let order = OrderTable::from_file(&file);
    let maps = reader::parse(&bytes, kind, order.len())?;
    super::resolve(repo, &order, &maps, wants, haves)
}
