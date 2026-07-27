use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime};

use gix::progress::Discard;
use knot_types::{Oid, UnixSeconds};

use crate::fsio::{self, PackStem};
use crate::{FileCount, MaintError, ObjectCount, PruneReport};

const MTIMES_MAGIC: u32 = 0x4d54_4d45;
const MTIMES_VERSION: u32 = 1;

pub fn run(
    objects_dir: &Path,
    kind: gix::hash::Kind,
    reachable: &HashSet<Oid>,
    new_reachable_stem: Option<&PackStem>,
    kept_large: &[PackStem],
    loose: &[(Oid, PathBuf)],
    grace: Duration,
) -> Result<PruneReport, MaintError> {
    let pack_dir = objects_dir.join("pack");
    let idxs = fsio::pack_idx_paths(objects_dir);
    let now = SystemTime::now();
    let now_mtime = PackedMtime::from_system_time(now);

    let kept_pack =
        |stem: &PackStem| -> bool { Some(stem) == new_reachable_stem || kept_large.contains(stem) };

    let kept_pack_oids: HashSet<Oid> = idxs
        .iter()
        .filter(|idx| PackStem::of(idx).is_some_and(|stem| kept_pack(&stem)))
        .filter_map(|idx| fsio::pack_oids(idx, kind))
        .flatten()
        .collect();

    let recorded = read_recorded_mtimes(&idxs, kind);
    let mtime_of = mtime_index(&idxs, &pack_dir, kind, &recorded, loose);

    let unreachable =
        unreachable_candidates(&idxs, kind, loose, reachable, &kept_pack_oids, &kept_pack);
    let mut keep: Vec<Oid> = unreachable
        .into_iter()
        .filter(|oid| !is_expired(mtime_of.get(oid), now, grace))
        .collect();
    keep.sort();

    let cruft_stem = if keep.is_empty() {
        None
    } else {
        let stem = write_cruft_pack(objects_dir, keep.clone(), kind)?;
        if let Some(stem) = &stem {
            write_mtimes(&pack_dir, stem, kind, |oid| {
                mtime_of
                    .get(&oid)
                    .map(PackedMtime::from_unix)
                    .unwrap_or(now_mtime)
            })?;
        }
        stem
    };
    knot_resource::fsync_path(&pack_dir)?;

    let kept_oids: HashSet<Oid> = kept_pack_oids
        .iter()
        .copied()
        .chain(cruft_stem.as_ref().into_iter().flat_map(|stem| {
            fsio::pack_oids(&stem.file(&pack_dir, "idx"), kind).unwrap_or_default()
        }))
        .collect();

    let keep_covered = keep.iter().all(|oid| kept_oids.contains(oid));
    if !closure_is_covered(reachable, &kept_oids) || !keep_covered {
        if let Some(stem) = cruft_stem.as_ref() {
            fsio::remove_pack_files(&stem.file(&pack_dir, "idx"), &pack_dir);
        }
        return Ok(PruneReport::skipped());
    }

    let removed_packs = idxs
        .iter()
        .filter(|idx| {
            PackStem::of(idx)
                .is_some_and(|stem| !kept_pack(&stem) && Some(&stem) != cruft_stem.as_ref())
        })
        .filter(|idx| fsio::remove_pack_files(idx, &pack_dir))
        .count();
    let removed_loose = loose
        .iter()
        .filter(|(oid, _)| !reachable.contains(oid))
        .filter(|(_, path)| std::fs::remove_file(path).is_ok())
        .count();
    knot_resource::fsync_path(&pack_dir)?;
    knot_resource::fsync_path(objects_dir)?;

    Ok(PruneReport {
        removed: FileCount::new(removed_loose),
        removed_packs: FileCount::new(removed_packs),
        crufted: ObjectCount::new(keep.len()),
        ran: true,
    })
}

fn closure_is_covered(reachable: &HashSet<Oid>, kept: &HashSet<Oid>) -> bool {
    reachable.is_subset(kept)
}

fn unreachable_candidates<K: Fn(&PackStem) -> bool>(
    idxs: &[PathBuf],
    kind: gix::hash::Kind,
    loose: &[(Oid, PathBuf)],
    reachable: &HashSet<Oid>,
    kept_pack_oids: &HashSet<Oid>,
    kept_pack: &K,
) -> Vec<Oid> {
    idxs.iter()
        .filter(|idx| PackStem::of(idx).is_some_and(|stem| !kept_pack(&stem)))
        .filter_map(|idx| fsio::pack_oids(idx, kind))
        .flatten()
        .chain(loose.iter().map(|(oid, _)| *oid))
        .filter(|oid| !reachable.contains(oid))
        .filter(|oid| !kept_pack_oids.contains(oid))
        .collect::<HashSet<Oid>>()
        .into_iter()
        .collect()
}

fn mtime_index(
    idxs: &[PathBuf],
    pack_dir: &Path,
    kind: gix::hash::Kind,
    recorded: &HashMap<Oid, UnixSeconds>,
    loose: &[(Oid, PathBuf)],
) -> HashMap<Oid, UnixSeconds> {
    let from_packs = idxs.iter().flat_map(|idx| {
        let is_cruft = idx.with_extension("mtimes").exists();
        let pack_secs = fsio::pack_mtime(idx, pack_dir);
        fsio::pack_oids(idx, kind)
            .unwrap_or_default()
            .into_iter()
            .filter_map(move |oid| {
                let secs = if is_cruft {
                    recorded.get(&oid).copied()
                } else {
                    pack_secs
                };
                secs.map(|secs| (oid, secs))
            })
    });
    let from_loose = loose
        .iter()
        .filter_map(|(oid, path)| loose_mtime(path).map(|secs| (*oid, secs)));
    newest_by_oid(from_packs.chain(from_loose))
}

fn newest_by_oid(pairs: impl Iterator<Item = (Oid, UnixSeconds)>) -> HashMap<Oid, UnixSeconds> {
    pairs.fold(HashMap::new(), |mut acc, (oid, secs)| {
        acc.entry(oid)
            .and_modify(|current| {
                if secs.get() > current.get() {
                    *current = secs;
                }
            })
            .or_insert(secs);
        acc
    })
}

fn read_recorded_mtimes(idxs: &[PathBuf], kind: gix::hash::Kind) -> HashMap<Oid, UnixSeconds> {
    idxs.iter()
        .filter(|idx| idx.with_extension("mtimes").exists())
        .filter_map(|idx| {
            let bytes = std::fs::read(idx.with_extension("mtimes")).ok()?;
            let oids = fsio::pack_oids(idx, kind)?;
            let checksum = gix_pack::data::File::at(idx.with_extension("pack"), kind)
                .ok()?
                .checksum();
            let table = validated_mtimes(&bytes, oids.len(), kind, checksum.as_slice())?;
            Some(
                oids.into_iter()
                    .zip(table)
                    .map(|(oid, mtime)| (oid, mtime.to_unix()))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

fn validated_mtimes(
    bytes: &[u8],
    count: usize,
    kind: gix::hash::Kind,
    pack_checksum: &[u8],
) -> Option<Vec<PackedMtime>> {
    let hash_len = kind.len_in_bytes();
    let header = 12usize;
    let total = header + count * 4 + hash_len * 2;
    if bytes.len() != total
        || bytes[0..4] != MTIMES_MAGIC.to_be_bytes()
        || bytes[4..8] != MTIMES_VERSION.to_be_bytes()
        || bytes[8..12] != hash_id(kind).to_be_bytes()
    {
        return None;
    }
    let table_end = header + count * 4;
    if bytes[table_end..table_end + hash_len] != *pack_checksum {
        return None;
    }
    let mut hasher = gix_hash::hasher(kind);
    hasher.update(&bytes[..total - hash_len]);
    let digest = hasher.try_finalize().ok()?;
    if digest.as_slice() != &bytes[total - hash_len..] {
        return None;
    }
    Some(
        (0..count)
            .map(|index| {
                let offset = header + index * 4;
                PackedMtime(u32::from_be_bytes(
                    bytes[offset..offset + 4].try_into().unwrap(),
                ))
            })
            .collect(),
    )
}

fn is_expired(mtime: Option<&UnixSeconds>, now: SystemTime, grace: Duration) -> bool {
    let Some(mtime) = mtime else {
        return false;
    };
    let when = SystemTime::UNIX_EPOCH + Duration::from_secs(mtime.get().max(0) as u64);
    now.duration_since(when)
        .map(|age| age >= grace)
        .unwrap_or(false)
}

fn loose_mtime(path: &Path) -> Option<UnixSeconds> {
    let modified = path.metadata().ok()?.modified().ok()?;
    Some(PackedMtime::from_system_time(modified).to_unix())
}

#[derive(Debug, Clone, Copy)]
struct PackedMtime(u32);

impl PackedMtime {
    fn from_unix(secs: &UnixSeconds) -> Self {
        Self(secs.get().clamp(0, u32::MAX as i64) as u32)
    }

    fn from_system_time(time: SystemTime) -> Self {
        Self(
            time.duration_since(SystemTime::UNIX_EPOCH)
                .map(|delta| delta.as_secs().min(u32::MAX as u64) as u32)
                .unwrap_or(0),
        )
    }

    fn to_unix(self) -> UnixSeconds {
        UnixSeconds::new(self.0 as i64)
    }

    fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }
}

fn hash_id(kind: gix::hash::Kind) -> u32 {
    match kind {
        gix::hash::Kind::Sha256 => 2,
        _ => 1,
    }
}

fn write_cruft_pack(
    objects_dir: &Path,
    oids: Vec<Oid>,
    kind: gix::hash::Kind,
) -> Result<Option<PackStem>, MaintError> {
    let pack_dir = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(|error| fsio::io_error(&pack_dir, error))?;
    knot_resource::clear_stale(&pack_dir, ".knot-cruft.");
    let staging = pack_dir.join(format!(
        ".knot-cruft.{}.pack",
        knot_resource::staging_nonce()
    ));
    let outcome = stream_pack(objects_dir, oids, kind, &staging)
        .and_then(|()| install_pack(&pack_dir, &staging, kind));
    let _ = std::fs::remove_file(&staging);
    outcome
}

fn stream_pack(
    objects_dir: &Path,
    oids: Vec<Oid>,
    kind: gix::hash::Kind,
    staging: &Path,
) -> Result<(), MaintError> {
    let file = std::fs::File::create(staging).map_err(|error| fsio::io_error(staging, error))?;
    let mut writer = std::io::BufWriter::new(file);
    knot_pack::write_pack(objects_dir, oids, None, &mut writer, kind)
        .map_err(|error| MaintError::Pack(error.to_string()))?;
    writer
        .into_inner()
        .map(|_| ())
        .map_err(|error| fsio::io_error(staging, error.into_error()))
}

fn install_pack(
    pack_dir: &Path,
    staging: &Path,
    kind: gix::hash::Kind,
) -> Result<Option<PackStem>, MaintError> {
    let file = std::fs::File::open(staging).map_err(|error| fsio::io_error(staging, error))?;
    let mut reader = std::io::BufReader::new(file);
    let outcome = gix_pack::Bundle::write_to_directory(
        &mut reader,
        Some(pack_dir),
        &mut Discard,
        &AtomicBool::new(false),
        None::<gix::odb::Handle>,
        gix_pack::bundle::write::Options {
            thread_limit: Some(1),
            iteration_mode: gix_pack::data::input::Mode::Verify,
            index_version: gix_pack::index::Version::default(),
            object_hash: kind,
        },
    )
    .map_err(|error| MaintError::Pack(error.to_string()))?;

    if let Some(keep) = &outcome.keep_path {
        let _ = std::fs::remove_file(keep);
    }
    [&outcome.data_path, &outcome.index_path]
        .into_iter()
        .flatten()
        .try_for_each(|path| knot_resource::fsync_path(path))?;

    Ok(outcome
        .data_path
        .as_ref()
        .and_then(|path| PackStem::of(path)))
}

fn write_mtimes(
    pack_dir: &Path,
    stem: &PackStem,
    kind: gix::hash::Kind,
    mtime_for: impl Fn(Oid) -> PackedMtime,
) -> Result<(), MaintError> {
    let idx = stem.file(pack_dir, "idx");
    let pack = stem.file(pack_dir, "pack");
    let index = gix_pack::index::File::at(&idx, kind)
        .map_err(|error| MaintError::Pack(format!("open cruft index: {error}")))?;
    let checksum = gix_pack::data::File::at(&pack, kind)
        .map_err(|error| MaintError::Pack(format!("open cruft pack: {error}")))?
        .checksum();

    let mut out = Vec::new();
    out.extend_from_slice(&MTIMES_MAGIC.to_be_bytes());
    out.extend_from_slice(&MTIMES_VERSION.to_be_bytes());
    out.extend_from_slice(&hash_id(kind).to_be_bytes());
    index
        .iter()
        .for_each(|entry| out.extend_from_slice(&mtime_for(Oid::from(entry.oid)).to_be_bytes()));
    out.extend_from_slice(checksum.as_slice());
    let mut hasher = gix_hash::hasher(kind);
    hasher.update(&out);
    let digest = hasher
        .try_finalize()
        .map_err(|error| MaintError::Pack(format!("cruft mtimes checksum: {error}")))?;
    out.extend_from_slice(digest.as_slice());

    knot_resource::atomic_write_bytes(
        &stem.file(pack_dir, "mtimes"),
        &out,
        knot_resource::FileMode::Inherited,
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> Oid {
        Oid::from_hex(&format!("{byte:02x}").repeat(20)).unwrap()
    }

    #[test]
    fn closure_covered_when_every_reachable_oid_survives() {
        let reachable: HashSet<Oid> = [oid(1), oid(2)].into_iter().collect();
        let kept: HashSet<Oid> = [oid(1), oid(2), oid(3)].into_iter().collect();
        assert!(closure_is_covered(&reachable, &kept));
    }

    #[test]
    fn closure_uncovered_when_a_reachable_oid_is_missing() {
        let reachable: HashSet<Oid> = [oid(1), oid(2)].into_iter().collect();
        let kept: HashSet<Oid> = [oid(1)].into_iter().collect();
        assert!(!closure_is_covered(&reachable, &kept));
    }

    #[test]
    fn unknown_mtime_is_never_expired() {
        assert!(!is_expired(None, SystemTime::now(), Duration::ZERO));
    }

    #[test]
    fn future_mtime_is_never_expired() {
        let future = PackedMtime::from_system_time(SystemTime::now())
            .to_unix()
            .saturating_add_secs(100_000);
        assert!(!is_expired(
            Some(&future),
            SystemTime::now(),
            Duration::from_secs(1)
        ));
    }

    #[test]
    fn newest_mtime_wins_regardless_of_iteration_order() {
        let shared = oid(7);
        let old = UnixSeconds::new(1_000);
        let fresh = UnixSeconds::new(2_000);
        let forward = newest_by_oid([(shared, old), (shared, fresh)].into_iter());
        let reverse = newest_by_oid([(shared, fresh), (shared, old)].into_iter());
        assert_eq!(forward.get(&shared), Some(&fresh));
        assert_eq!(reverse.get(&shared), Some(&fresh));
    }

    #[test]
    fn old_mtime_past_grace_is_expired() {
        let old = UnixSeconds::new(1_000);
        assert!(is_expired(
            Some(&old),
            SystemTime::now(),
            Duration::from_secs(60)
        ));
    }

    #[test]
    fn corrupt_mtimes_are_rejected_rather_than_trusted() {
        let count = 3usize;
        let hash_len = gix::hash::Kind::Sha1.len_in_bytes();
        let total = 12 + count * 4 + hash_len * 2;
        let zero_checksum = vec![0u8; hash_len];
        let unsigned = vec![0u8; total];
        assert!(
            validated_mtimes(&unsigned, count, gix::hash::Kind::Sha1, &zero_checksum).is_none(),
            "a correctly-sized but unsigned mtimes table isn't trusted"
        );
        let truncated = vec![0u8; total - 1];
        assert!(
            validated_mtimes(&truncated, count, gix::hash::Kind::Sha1, &zero_checksum).is_none()
        );
        let mut wrong_pack = vec![0u8; total];
        wrong_pack[0..4].copy_from_slice(&MTIMES_MAGIC.to_be_bytes());
        wrong_pack[4..8].copy_from_slice(&MTIMES_VERSION.to_be_bytes());
        wrong_pack[8..12].copy_from_slice(&hash_id(gix::hash::Kind::Sha1).to_be_bytes());
        let mismatched = vec![0xabu8; hash_len];
        assert!(
            validated_mtimes(&wrong_pack, count, gix::hash::Kind::Sha1, &mismatched).is_none(),
            "an mtimes table whose pack checksum names a different pack isn't trusted"
        );
    }

    #[test]
    fn run_fail_closes_when_survivors_miss_the_closure() {
        use knot_git::{Layout, RefUpdate};
        use knot_types::{BranchName, RefName, RepoDid};

        use crate::test_support::{commit_on, empty_tree};

        let scan = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:limpet").unwrap();
        let repo = layout.create(&did).unwrap();
        let tip = commit_on(&repo, empty_tree(repo.object_format()), Vec::new(), "a");
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/heads/main").unwrap(),
            new: tip,
        })
        .unwrap();

        let options = crate::Options {
            repack_max_objects: crate::ObjectCount::new(1_000_000),
            geometric_factor: crate::GeometricFactor::full_repack(),
            prune_grace: crate::PruneGrace::from_secs(0),
            reflog_floor: crate::ReflogRetention::from_secs(i64::MAX as u64 / 4),
            commit_graph: false,
            multi_pack_index: false,
            bitmap: false,
        };
        crate::run_repo(&repo, UnixSeconds::new(1_700_000_500), &options).unwrap();

        let objects_dir = repo.objects_dir();
        let kind = repo.object_format().kind();
        let reachable: HashSet<Oid> = repo
            .select_pack_objects(knot_git::Wants::new(&[tip]), knot_git::Haves::new(&[]))
            .unwrap()
            .into_iter()
            .collect();
        assert!(!reachable.is_empty());

        let absent = PackStem::of(Path::new(
            "pack-0000000000000000000000000000000000000000.idx",
        ))
        .unwrap();
        let report = run(
            &objects_dir,
            kind,
            &reachable,
            Some(&absent),
            &[],
            &[],
            Duration::ZERO,
        )
        .unwrap();
        assert!(!report.ran, "an uncovered closure fail-closes the prune");
        let reopened = knot_git::Repo::open(repo.git().git_dir()).unwrap();
        assert!(
            reachable.iter().all(|oid| reopened.contains(*oid)),
            "no reachable object is deleted when survivors don't cover the closure"
        );
    }
}
