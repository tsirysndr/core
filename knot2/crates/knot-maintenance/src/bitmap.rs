use std::path::Path;

use knot_git::Repo;

use crate::MaintError;
use crate::fsio::{self, MIDX_SIDECAR_PREFIX, PackStem};

pub fn exists(objects_dir: &Path) -> bool {
    fsio::pack_idx_paths(objects_dir)
        .iter()
        .any(|idx| idx.with_extension("bitmap").exists())
}

pub fn refresh(repo: &Repo, objects_dir: &Path) -> Result<bool, MaintError> {
    let idxs = fsio::pack_idx_paths(objects_dir);
    match idxs.as_slice() {
        [only] => {
            let wrote = knot_git::write_bitmap(repo, only)
                .map_err(|error| MaintError::Pack(error.to_string()))?;
            let stem = PackStem::of(only);
            prune_sidecars(objects_dir, stem.as_ref());
            Ok(wrote)
        }
        [] => {
            prune_sidecars(objects_dir, None);
            Ok(false)
        }
        _ => {
            let wrote = knot_git::write_midx_bitmap(repo)
                .map_err(|error| MaintError::Pack(error.to_string()))?;
            let keep = current_midx_stem(objects_dir);
            prune_sidecars(objects_dir, keep.as_ref());
            Ok(wrote)
        }
    }
}

fn current_midx_stem(objects_dir: &Path) -> Option<PackStem> {
    let path = objects_dir.join("pack").join("multi-pack-index");
    let file =
        gix_pack::multi_index::File::at(path, Some(crate::midx::MIDX_ALLOC_LIMIT_BYTES)).ok()?;
    Some(PackStem::midx_sidecar(
        &file.checksum().to_hex().to_string(),
    ))
}

fn prune_sidecars(objects_dir: &Path, keep_stem: Option<&PackStem>) {
    let pack_dir = objects_dir.join("pack");
    let Ok(entries) = std::fs::read_dir(&pack_dir) else {
        return;
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| is_bitmap_sidecar(path))
        .filter(|path| PackStem::of(path).as_ref() != keep_stem)
        .for_each(|path| {
            let _ = std::fs::remove_file(path);
        });
}

fn is_bitmap_sidecar(path: &Path) -> bool {
    match path.extension().and_then(|ext| ext.to_str()) {
        Some("bitmap") => true,
        Some("rev") => path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(MIDX_SIDECAR_PREFIX)),
        _ => false,
    }
}
