use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gix::progress::Discard;
use knot_git::Repo;

use crate::fsio;
use crate::{FileCount, MaintError};

const FILE_NAME: &str = "multi-pack-index";

pub(crate) const MIDX_ALLOC_LIMIT_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidxStatus {
    Written(FileCount),
    Removed,
    Absent,
}

pub(crate) fn clear(objects_dir: &Path) -> Result<(), MaintError> {
    let pack_dir = objects_dir.join("pack");
    knot_resource::clear_temps(&pack_dir, FILE_NAME);
    knot_resource::fsync_path(&pack_dir).map_err(Into::into)
}

pub fn write(repo: &Repo) -> Result<MidxStatus, MaintError> {
    let objects_dir = repo.objects_dir();
    let kind = repo.object_format().kind();
    let target = objects_dir.join("pack").join(FILE_NAME);
    let idx_paths = fsio::pack_idx_paths(&objects_dir);
    match FileCount::new(idx_paths.len()) {
        count if count.get() >= 2 => {
            write_atomic(idx_paths, kind, &target)?;
            Ok(MidxStatus::Written(count))
        }
        _ => remove_if_present(&target),
    }
}

fn write_atomic(
    idx_paths: Vec<PathBuf>,
    kind: gix::hash::Kind,
    target: &Path,
) -> Result<(), MaintError> {
    knot_resource::atomic_write(target, knot_resource::FileMode::Inherited, |file| {
        let mut writer = std::io::BufWriter::new(file);
        gix_pack::multi_index::write_from_index_paths(
            idx_paths,
            &mut writer,
            &mut Discard,
            &AtomicBool::new(false),
            gix_pack::multi_index::write::Options { object_hash: kind },
        )
        .map_err(|e| MaintError::Pack(e.to_string()))?;
        std::io::Write::flush(&mut writer).map_err(|e| fsio::io_error(target, e))
    })
}

fn remove_if_present(target: &Path) -> Result<MidxStatus, MaintError> {
    match std::fs::remove_file(target) {
        Ok(()) => {
            if let Some(dir) = target.parent() {
                knot_resource::fsync_path(dir)?;
            }
            Ok(MidxStatus::Removed)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(MidxStatus::Absent),
        Err(error) => Err(fsio::io_error(target, error)),
    }
}

#[cfg(test)]
mod tests {
    use knot_git::{Layout, RefUpdate};
    use knot_types::{BranchName, ObjectFormat, Oid, RefName, RepoDid};

    use super::*;
    use crate::test_support::{commit_on, empty_tree};

    fn midx_file(repo: &knot_git::Repo) -> PathBuf {
        repo.objects_dir().join("pack").join(FILE_NAME)
    }

    fn pack_from(repo: &knot_git::Repo, tip: Oid, kind: gix::hash::Kind) {
        let closure = repo
            .select_pack_objects(knot_git::Wants::new(&[tip]), knot_git::Haves::new(&[]))
            .unwrap();
        let mut bytes = Vec::new();
        knot_pack::write_pack(&repo.objects_dir(), closure, None, &mut bytes, kind).unwrap();
        knot_pack::ingest_pack(
            &repo.objects_dir(),
            &bytes,
            &knot_pack::PackLimits::default(),
            kind,
        )
        .unwrap();
    }

    fn two_pack_repo(format: ObjectFormat) -> (tempfile::TempDir, knot_git::Repo, Oid, Oid) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path())
            .with_object_format(format)
            .with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:limpet").unwrap();
        let repo = layout.create(&did).unwrap();
        let kind = format.kind();
        let first = commit_on(&repo, empty_tree(format), Vec::new(), "scallop");
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/heads/main").unwrap(),
            new: first,
        })
        .unwrap();
        pack_from(&repo, first, kind);
        let second = commit_on(&repo, empty_tree(format), Vec::new(), "whelk");
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/heads/side").unwrap(),
            new: second,
        })
        .unwrap();
        pack_from(&repo, second, kind);
        (dir, layout.open(&did).unwrap(), first, second)
    }

    fn assert_two_pack_midx(format: ObjectFormat) {
        let (_dir, repo, scallop, whelk) = two_pack_repo(format);
        assert_eq!(fsio::pack_idx_paths(&repo.objects_dir()).len(), 2);

        let status = write(&repo).unwrap();
        let count = match status {
            MidxStatus::Written(count) => count,
            other => panic!("expected a written midx, got {other:?}"),
        };
        assert_eq!(count, FileCount::new(2));
        assert!(midx_file(&repo).exists());

        let parsed =
            gix_pack::multi_index::File::at(midx_file(&repo), Some(MIDX_ALLOC_LIMIT_BYTES))
                .unwrap();
        assert_eq!(parsed.num_indices() as usize, 2);
        assert!(parsed.num_objects() >= 4);

        assert!(repo.contains(scallop));
        assert!(repo.contains(whelk));
        assert_eq!(repo.object_format().kind(), format.kind());
    }

    #[test]
    fn writes_a_multi_pack_index_over_two_packs_sha1() {
        assert_two_pack_midx(ObjectFormat::SHA1);
    }

    #[test]
    fn writes_a_multi_pack_index_over_two_packs_sha256() {
        assert_two_pack_midx(ObjectFormat::SHA256);
    }

    #[test]
    fn clear_removes_the_index_and_sidecars_but_keeps_packs() {
        let dir = tempfile::tempdir().unwrap();
        let objects_dir = dir.path();
        let pack_dir = objects_dir.join("pack");
        std::fs::create_dir_all(&pack_dir).unwrap();
        let make = |name: &str| std::fs::write(pack_dir.join(name), b"x").unwrap();
        make(FILE_NAME);
        make("multi-pack-index-abc.bitmap");
        make("multi-pack-index-abc.rev");
        make("pack-scallop.idx");
        make("pack-scallop.pack");

        clear(objects_dir).unwrap();

        assert!(!pack_dir.join(FILE_NAME).exists());
        assert!(!pack_dir.join("multi-pack-index-abc.bitmap").exists());
        assert!(!pack_dir.join("multi-pack-index-abc.rev").exists());
        assert!(
            pack_dir.join("pack-scallop.idx").exists(),
            "real packs are left in place"
        );
        assert!(pack_dir.join("pack-scallop.pack").exists());
    }

    #[test]
    fn lookup_resolves_through_the_midx_once_idx_files_are_gone() {
        let (_dir, repo, scallop, whelk) = two_pack_repo(ObjectFormat::SHA1);
        let objects_dir = repo.objects_dir();
        assert!(matches!(write(&repo).unwrap(), MidxStatus::Written(_)));

        fsio::loose_objects(&objects_dir)
            .iter()
            .for_each(|(_, path)| std::fs::remove_file(path).unwrap());
        fsio::pack_idx_paths(&objects_dir)
            .iter()
            .for_each(|idx| std::fs::remove_file(idx).unwrap());
        assert!(
            midx_file(&repo).exists(),
            "the multi-pack-index is the only index left on disk"
        );

        let via_midx = knot_git::Repo::open(repo.git().git_dir()).unwrap();
        assert!(
            via_midx.contains(scallop) && via_midx.contains(whelk),
            "objects resolve through the multi-pack-index with no per-pack idx present"
        );

        std::fs::remove_file(midx_file(&repo)).unwrap();
        let bare = knot_git::Repo::open(repo.git().git_dir()).unwrap();
        assert!(
            !bare.contains(scallop) && !bare.contains(whelk),
            "with the index gone the packs are unreadable, proving the midx served the lookup"
        );
    }

    #[test]
    fn fewer_than_two_packs_writes_nothing_and_clears_stale() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:conch").unwrap();
        let repo = layout.create(&did).unwrap();

        assert_eq!(write(&repo).unwrap(), MidxStatus::Absent);
        assert!(!midx_file(&repo).exists());

        let target = midx_file(&repo);
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, b"stale").unwrap();
        assert_eq!(write(&repo).unwrap(), MidxStatus::Removed);
        assert!(!midx_file(&repo).exists());
    }
}
