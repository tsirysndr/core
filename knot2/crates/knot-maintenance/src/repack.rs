use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

use gix::progress::Discard;
use knot_git::{Haves, Repo, Wants};
use knot_types::Oid;

use crate::fsio::{self, PackStem};
use crate::{FileCount, GeometricFactor, MaintError, ObjectCount, RepackReport, RepackStatus};

type RepackOutcome = (
    RepackReport,
    Option<HashSet<Oid>>,
    Option<PackStem>,
    Vec<PackStem>,
);

pub fn run(
    repo: &Repo,
    objects_dir: &Path,
    kind: gix::hash::Kind,
    roots: Vec<Oid>,
    max_objects: ObjectCount,
    factor: GeometricFactor,
    loose: &[(Oid, PathBuf)],
) -> Result<RepackOutcome, MaintError> {
    let closure = match repo.select_pack_objects(Wants::new(&roots), Haves::new(&[])) {
        Ok(closure) => closure,
        Err(_) => {
            return Ok((
                RepackReport::skipped(RepackStatus::ClosureFailed),
                None,
                None,
                Vec::new(),
            ));
        }
    };
    if closure.is_empty() {
        return Ok((
            RepackReport::skipped(RepackStatus::NothingReachable),
            Some(HashSet::new()),
            None,
            Vec::new(),
        ));
    }
    if closure.len() > max_objects.get() {
        return Ok((
            RepackReport::skipped(RepackStatus::SkippedTooLarge),
            None,
            None,
            Vec::new(),
        ));
    }
    let reachable: HashSet<Oid> = closure.iter().copied().collect();

    let kept_large = if factor == GeometricFactor::full_repack() {
        Vec::new()
    } else {
        kept_large_packs(objects_dir, kind, factor)
    };
    let kept_large_oids: HashSet<Oid> = kept_large
        .iter()
        .filter_map(|idx| fsio::pack_oids(idx, kind))
        .flatten()
        .collect();
    let new_objects: Vec<Oid> = closure
        .into_iter()
        .filter(|oid| !kept_large_oids.contains(oid))
        .collect();

    let new_stem = if new_objects.is_empty() {
        None
    } else {
        build_and_install_pack(objects_dir, new_objects, kind)?
    };

    let removed_loose = loose
        .iter()
        .filter(|(oid, _)| reachable.contains(oid))
        .filter(|(_, path)| std::fs::remove_file(path).is_ok())
        .count();

    let kept_large_stems: Vec<PackStem> = kept_large
        .iter()
        .filter_map(|idx| PackStem::of(idx))
        .collect();

    Ok((
        RepackReport {
            status: RepackStatus::Repacked,
            packed_objects: ObjectCount::new(reachable.len()),
            removed_loose: FileCount::new(removed_loose),
            removed_packs: FileCount::new(0),
        },
        Some(reachable),
        new_stem,
        kept_large_stems,
    ))
}

fn kept_large_packs(
    objects_dir: &Path,
    kind: gix::hash::Kind,
    factor: GeometricFactor,
) -> Vec<PathBuf> {
    let pack_dir = objects_dir.join("pack");
    let mut eligible: Vec<(PathBuf, usize)> = fsio::pack_idx_paths(objects_dir)
        .into_iter()
        .filter(|idx| !is_excluded(idx, &pack_dir))
        .filter_map(|idx| fsio::pack_oids(&idx, kind).map(|oids| (idx, oids.len())))
        .collect();
    eligible.sort_by_key(|(_, count)| *count);
    let weights: Vec<usize> = eligible.iter().map(|(_, count)| *count).collect();
    let split = compute_split(&weights, factor);
    eligible
        .into_iter()
        .skip(split)
        .map(|(idx, _)| idx)
        .collect()
}

fn is_excluded(idx: &Path, pack_dir: &Path) -> bool {
    let Some(stem) = idx.file_stem().and_then(|stem| stem.to_str()) else {
        return true;
    };
    pack_dir.join(format!("{stem}.mtimes")).exists()
}

fn compute_split(weights: &[usize], factor: GeometricFactor) -> usize {
    let n = weights.len();
    if n == 0 {
        return 0;
    }
    let geometric =
        |big: usize, small: usize| (small as u64).saturating_mul(factor.get()) <= big as u64;
    let split1 = (1..n)
        .rev()
        .find(|&i| !geometric(weights[i], weights[i - 1]))
        .map(|i| i + 1)
        .unwrap_or(0);
    let total: u64 = weights[..split1].iter().map(|weight| *weight as u64).sum();
    let extended = (split1..n).try_fold((split1, total), |(split, total), j| {
        match total.checked_mul(factor.get()) {
            Some(threshold) if (weights[j] as u64) < threshold => std::ops::ControlFlow::Continue(
                (split + 1, total.saturating_add(weights[j] as u64)),
            ),
            _ => std::ops::ControlFlow::Break((split, total)),
        }
    });
    match extended {
        std::ops::ControlFlow::Continue((split, _)) => split,
        std::ops::ControlFlow::Break((split, _)) => split,
    }
}

fn build_and_install_pack(
    objects_dir: &Path,
    closure: Vec<Oid>,
    kind: gix::hash::Kind,
) -> Result<Option<PackStem>, MaintError> {
    let pack_dir = objects_dir.join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(|error| fsio::io_error(&pack_dir, error))?;
    knot_resource::clear_stale(&pack_dir, ".knot-repack.");
    let staging = pack_dir.join(format!(
        ".knot-repack.{}.pack",
        knot_resource::staging_nonce()
    ));
    let outcome = write_streaming_pack(objects_dir, closure, kind, &staging)
        .and_then(|()| install_streamed_pack(&pack_dir, &staging, kind));
    let _ = std::fs::remove_file(&staging);
    outcome
}

fn write_streaming_pack(
    objects_dir: &Path,
    closure: Vec<Oid>,
    kind: gix::hash::Kind,
    staging: &Path,
) -> Result<(), MaintError> {
    let file = std::fs::File::create(staging).map_err(|error| fsio::io_error(staging, error))?;
    let mut writer = std::io::BufWriter::new(file);
    knot_pack::write_pack(objects_dir, closure, None, &mut writer, kind)
        .map_err(|error| MaintError::Pack(error.to_string()))?;
    writer
        .into_inner()
        .map(|_| ())
        .map_err(|error| fsio::io_error(staging, error.into_error()))
}

fn install_streamed_pack(
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
    knot_resource::fsync_path(pack_dir)?;

    Ok(outcome
        .data_path
        .as_ref()
        .and_then(|path| PackStem::of(path)))
}

#[cfg(test)]
mod tests {
    use super::{GeometricFactor, compute_split};

    #[test]
    fn uniform_small_packs_all_roll_up() {
        assert_eq!(compute_split(&[1, 1, 1, 1], GeometricFactor::new(2)), 4);
    }

    #[test]
    fn a_clean_geometric_progression_rolls_up_nothing() {
        assert_eq!(compute_split(&[1, 2, 4, 8], GeometricFactor::new(2)), 0);
    }

    #[test]
    fn one_large_pack_with_tiny_additions_keeps_the_large_one() {
        let split = compute_split(&[1, 1, 1, 1000], GeometricFactor::new(2));
        assert_eq!(split, 3);
    }

    #[test]
    fn a_single_pack_is_never_rolled_up() {
        assert_eq!(compute_split(&[42], GeometricFactor::new(2)), 0);
    }

    #[test]
    fn empty_input_rolls_up_nothing() {
        assert_eq!(compute_split(&[], GeometricFactor::new(2)), 0);
    }

    #[test]
    fn a_huge_factor_saturates_without_overflow() {
        assert_eq!(compute_split(&[1, 1, 1], GeometricFactor::full_repack()), 3);
    }
}
