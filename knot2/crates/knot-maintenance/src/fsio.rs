use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use knot_types::{Oid, UnixSeconds};

use crate::MaintError;

pub(crate) const MIDX_SIDECAR_PREFIX: &str = "multi-pack-index-";

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
// Every file in a pack-set has only the one stem,
// so maintenance finds siblings by just swapping the file extension.
// Checking the stem once here is more efficient than
// checking it at each place that makes a sibling path.
pub struct PackStem(String);

impl PackStem {
    pub fn of(path: &Path) -> Option<Self> {
        path.file_stem()
            .and_then(|stem| stem.to_str())
            .filter(|stem| stem.starts_with("pack-") || stem.starts_with(MIDX_SIDECAR_PREFIX))
            .map(|stem| Self(stem.to_string()))
    }

    pub(crate) fn midx_sidecar(checksum_hex: &str) -> Self {
        Self(format!("{MIDX_SIDECAR_PREFIX}{checksum_hex}"))
    }

    pub fn file(&self, pack_dir: &Path, extension: &str) -> PathBuf {
        pack_dir.join(format!("{}.{extension}", self.0))
    }
}

pub fn io_error(path: &Path, error: std::io::Error) -> MaintError {
    MaintError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

pub fn loose_objects(objects_dir: &Path) -> Vec<(Oid, PathBuf)> {
    let Ok(shards) = std::fs::read_dir(objects_dir) else {
        return Vec::new();
    };
    shards
        .filter_map(Result::ok)
        .filter(|shard| is_shard_name(&shard.file_name()))
        .flat_map(|shard| loose_in_shard(&shard.path(), &shard.file_name()))
        .collect()
}

fn is_shard_name(name: &std::ffi::OsString) -> bool {
    name.to_str()
        .is_some_and(|text| text.len() == 2 && text.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn loose_in_shard(shard_path: &Path, shard_name: &std::ffi::OsString) -> Vec<(Oid, PathBuf)> {
    let Some(prefix) = shard_name.to_str() else {
        return Vec::new();
    };
    let Ok(entries) = std::fs::read_dir(shard_path) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .filter_map(|entry| {
            let name = entry.file_name();
            let rest = name.to_str()?;
            let oid = Oid::from_hex(&format!("{prefix}{rest}")).ok()?;
            Some((oid, entry.path()))
        })
        .collect()
}

pub fn has_loose_refs(git_dir: &Path) -> bool {
    walkdir::WalkDir::new(git_dir.join("refs"))
        .into_iter()
        .filter_map(Result::ok)
        .any(|entry| entry.file_type().is_file())
}

pub fn pack_idx_paths(objects_dir: &Path) -> Vec<PathBuf> {
    let pack_dir = objects_dir.join("pack");
    let Ok(entries) = std::fs::read_dir(&pack_dir) else {
        return Vec::new();
    };
    entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "idx"))
        .collect()
}

pub fn pack_oids(idx: &Path, kind: gix::hash::Kind) -> Option<Vec<Oid>> {
    let index = gix_pack::index::File::at(idx, kind).ok()?;
    Some(index.iter().map(|entry| Oid::from(entry.oid)).collect())
}

pub fn pack_file(idx: &Path, pack_dir: &Path) -> Option<PathBuf> {
    let stem = idx.file_stem().and_then(|stem| stem.to_str())?;
    Some(pack_dir.join(format!("{stem}.pack")))
}

pub fn remove_pack_files(idx: &Path, pack_dir: &Path) -> bool {
    let Some(stem) = idx.file_stem().and_then(|stem| stem.to_str()) else {
        return false;
    };
    let idx_removed = std::fs::remove_file(idx).is_ok();
    let pack_removed = std::fs::remove_file(pack_dir.join(format!("{stem}.pack"))).is_ok();
    let _ = std::fs::remove_file(pack_dir.join(format!("{stem}.rev")));
    let _ = std::fs::remove_file(pack_dir.join(format!("{stem}.bitmap")));
    let _ = std::fs::remove_file(pack_dir.join(format!("{stem}.mtimes")));
    idx_removed || pack_removed
}

pub fn pack_mtime(idx: &Path, pack_dir: &Path) -> Option<UnixSeconds> {
    let pack = pack_file(idx, pack_dir)?;
    let modified = pack.metadata().ok()?.modified().ok()?;
    let secs = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(UnixSeconds::new(secs as i64))
}

pub fn older_than(path: &Path, grace: Duration) -> bool {
    path.metadata()
        .and_then(|meta| meta.modified())
        .map(|modified| {
            SystemTime::now()
                .duration_since(modified)
                .unwrap_or(Duration::ZERO)
                >= grace
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(path: &Path) {
        std::fs::write(path, b"x").unwrap();
    }

    #[test]
    fn remove_pack_files_clears_idx_and_pack_together() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path();
        let idx = pack_dir.join("pack-scallop.idx");
        touch(&idx);
        touch(&pack_dir.join("pack-scallop.pack"));
        touch(&pack_dir.join("pack-scallop.mtimes"));

        assert!(remove_pack_files(&idx, pack_dir));
        assert!(!idx.exists());
        assert!(!pack_dir.join("pack-scallop.pack").exists());
        assert!(!pack_dir.join("pack-scallop.mtimes").exists());
    }

    #[test]
    fn remove_pack_files_reclaims_an_orphan_idx_with_no_pack() {
        let dir = tempfile::tempdir().unwrap();
        let pack_dir = dir.path();
        let idx = pack_dir.join("pack-whelk.idx");
        touch(&idx);

        assert!(
            remove_pack_files(&idx, pack_dir),
            "a lone idx left by a crash mid-deletion is still reclaimed"
        );
        assert!(
            !idx.exists(),
            "the orphan idx no longer advertises phantom objects"
        );
    }
}
