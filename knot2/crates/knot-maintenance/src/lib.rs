use std::collections::HashSet;
use std::path::PathBuf;
use std::time::Duration;

use knot_git::{PackRefsReport, ReflogReport, Repo};
use knot_types::{Oid, UnixSeconds};

mod bitmap;
mod commitgraph;
mod cruft;
mod fsio;
mod midx;
mod prune;
mod repack;
mod scheduler;
#[cfg(test)]
mod test_support;

pub use midx::MidxStatus;
pub use scheduler::{MaintenanceHandle, PushBytes, RepoSource, Scheduler};

pub const MIN_REFLOG_RETENTION_SECS: i64 = 30 * 24 * 60 * 60;

#[derive(Debug, thiserror::Error)]
pub enum MaintError {
    #[error("git: {0}")]
    Git(#[from] knot_git::GitError),
    #[error("pack: {0}")]
    Pack(String),
    #[error("io {path}: {message}")]
    Io { path: PathBuf, message: String },
    #[error("commit-graph: {0}")]
    CommitGraph(String),
}

impl From<knot_resource::FsError> for MaintError {
    fn from(error: knot_resource::FsError) -> Self {
        MaintError::Io {
            path: error.path,
            message: error.source.to_string(),
        }
    }
}

pub use knot_types::ObjectCount;

knot_types::scalar_newtype! {
    pub struct FileCount(usize);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GeometricFactor(u64);

impl GeometricFactor {
    pub const fn new(value: u64) -> Self {
        Self(if value < 2 { 2 } else { value })
    }

    pub const fn full_repack() -> Self {
        Self(u64::MAX)
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub repack_max_objects: ObjectCount,
    pub geometric_factor: GeometricFactor,
    pub prune_grace: PruneGrace,
    pub reflog_floor: ReflogRetention,
    pub commit_graph: bool,
    pub multi_pack_index: bool,
    pub bitmap: bool,
}

impl Options {
    pub fn from_config(config: &knot_config::MaintenanceConfig) -> Self {
        Self {
            repack_max_objects: ObjectCount::new(config.repack_max_objects as usize),
            geometric_factor: GeometricFactor::new(config.repack_geometric_factor),
            prune_grace: PruneGrace::from_secs(config.prune_grace_secs),
            reflog_floor: ReflogRetention::from_secs(config.reflog_expire_secs),
            commit_graph: config.commit_graph,
            multi_pack_index: config.multi_pack_index,
            bitmap: config.bitmap,
        }
    }
}

const LFS_GRACE_MIN: Duration = Duration::from_secs(86_400);

#[derive(Debug, Clone, Copy)]
pub struct GcGrace(Duration);

impl GcGrace {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ReflogRetention(Duration);

impl ReflogRetention {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct PruneGrace(Duration);

impl PruneGrace {
    pub const fn from_secs(secs: u64) -> Self {
        Self(Duration::from_secs(secs))
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LfsGrace(Duration);

impl LfsGrace {
    pub const fn get(self) -> Duration {
        self.0
    }
}

#[derive(Debug, Clone, Copy)]
pub struct SweepInterval(Duration);

impl SweepInterval {
    pub const fn new(interval: Duration) -> Self {
        Self(interval)
    }

    pub const fn get(self) -> Duration {
        self.0
    }
}

pub fn lfs_grace(gc_grace: GcGrace, reflog_retention: ReflogRetention) -> LfsGrace {
    let ceiling = reflog_retention
        .0
        .max(Duration::from_secs(MIN_REFLOG_RETENTION_SECS as u64))
        .max(LFS_GRACE_MIN);
    LfsGrace(gc_grace.0.clamp(LFS_GRACE_MIN, ceiling))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepackStatus {
    Repacked,
    Clean,
    SkippedTooLarge,
    ClosureFailed,
    NothingReachable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RepackReport {
    pub status: RepackStatus,
    pub packed_objects: ObjectCount,
    pub removed_loose: FileCount,
    pub removed_packs: FileCount,
}

impl RepackReport {
    fn skipped(status: RepackStatus) -> Self {
        Self {
            status,
            packed_objects: ObjectCount::new(0),
            removed_loose: FileCount::new(0),
            removed_packs: FileCount::new(0),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PruneReport {
    pub removed: FileCount,
    pub removed_packs: FileCount,
    pub crufted: ObjectCount,
    pub ran: bool,
}

impl PruneReport {
    fn skipped() -> Self {
        Self {
            removed: FileCount::new(0),
            removed_packs: FileCount::new(0),
            crufted: ObjectCount::new(0),
            ran: false,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    pub packed_refs: PackRefsReport,
    pub reflog: ReflogReport,
    pub commit_graph: bool,
    pub repack: RepackReport,
    pub prune: PruneReport,
    pub multi_pack_index: MidxStatus,
    pub bitmap: bool,
}

impl Report {
    fn noop() -> Self {
        Self {
            packed_refs: PackRefsReport { packed: 0 },
            reflog: ReflogReport {
                files: 0,
                dropped: 0,
            },
            commit_graph: false,
            repack: RepackReport::skipped(RepackStatus::Clean),
            prune: PruneReport::skipped(),
            multi_pack_index: MidxStatus::Absent,
            bitmap: false,
        }
    }
}

pub fn run_repo(
    repo: &Repo,
    now_seconds: UnixSeconds,
    opts: &Options,
) -> Result<Report, MaintError> {
    let objects_dir = repo.objects_dir();
    let kind = repo.object_format().kind();
    let loose = fsio::loose_objects(&objects_dir);
    let pack_count = fsio::pack_idx_paths(&objects_dir).len();
    let loose_refs = fsio::has_loose_refs(repo.git().git_dir());

    let graph_pending = opts.commit_graph && pack_count >= 1 && !commitgraph::exists(repo);
    let bitmap_pending = opts.bitmap && pack_count == 1 && !bitmap::exists(&objects_dir);
    if !opts.commit_graph {
        commitgraph::remove(repo)?;
    }
    if loose.is_empty() && pack_count <= 1 && !loose_refs && !graph_pending && !bitmap_pending {
        return Ok(Report::noop());
    }

    let packed_refs = if loose_refs {
        repo.pack_refs()?
    } else {
        PackRefsReport { packed: 0 }
    };
    let floor_secs = (opts.reflog_floor.get().as_secs() as i64).max(MIN_REFLOG_RETENTION_SECS);
    let reflog = repo.expire_reflogs(now_seconds.saturating_sub_secs(floor_secs))?;

    let commit_graph = if opts.commit_graph {
        commitgraph::write(repo)?
    } else {
        false
    };

    let retention_floor = now_seconds.saturating_sub_secs(floor_secs);
    let (repack, reachable, roots, new_stem, kept_large) = if loose.is_empty() && pack_count <= 1 {
        (
            RepackReport::skipped(RepackStatus::Clean),
            None,
            HashSet::new(),
            None,
            Vec::new(),
        )
    } else {
        let roots = collect_roots(repo, retention_floor)?;
        let (report, reachable, new_stem, kept_large) = repack::run(
            repo,
            &objects_dir,
            kind,
            roots.iter().copied().collect(),
            opts.repack_max_objects,
            opts.geometric_factor,
            &loose,
        )?;
        (report, reachable, roots, new_stem, kept_large)
    };

    let prune = match &reachable {
        Some(set) => repo.with_ref_lock(|| {
            let current = collect_roots(repo, retention_floor)?;
            if current != roots {
                return Ok(PruneReport::skipped());
            }
            if repack.status == RepackStatus::Repacked {
                midx::clear(&objects_dir)?;
                cruft::run(
                    &objects_dir,
                    kind,
                    set,
                    new_stem.as_ref(),
                    &kept_large,
                    &loose,
                    opts.prune_grace.get(),
                )
            } else {
                prune::run(&objects_dir, set, &loose, opts.prune_grace.get())
            }
        })?,
        None => PruneReport::skipped(),
    };

    let multi_pack_index = if opts.multi_pack_index {
        midx::write(repo)?
    } else {
        MidxStatus::Absent
    };

    let bitmap = if opts.bitmap {
        bitmap::refresh(repo, &objects_dir)?
    } else {
        false
    };

    Ok(Report {
        packed_refs,
        reflog,
        commit_graph,
        repack,
        prune,
        multi_pack_index,
        bitmap,
    })
}

fn collect_roots(repo: &Repo, retention_floor: UnixSeconds) -> Result<HashSet<Oid>, MaintError> {
    let mut roots: HashSet<Oid> = repo
        .references()?
        .into_iter()
        .map(|record| record.target)
        .collect();
    repo.reflog_updates_since(retention_floor)
        .into_iter()
        .for_each(|update| {
            roots.insert(update.new);
            if let Some(old) = update.old {
                roots.insert(old);
            }
        });
    Ok(roots
        .into_iter()
        .filter(|oid| repo.contains(*oid))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{GcGrace, LFS_GRACE_MIN, MIN_REFLOG_RETENTION_SECS, ReflogRetention, lfs_grace};

    #[test]
    fn the_default_grace_is_not_clamped_by_the_coupling() {
        let fourteen_days = 14 * 86_400;
        let ninety_days = 90 * 86_400;
        assert_eq!(
            lfs_grace(
                GcGrace::from_secs(fourteen_days),
                ReflogRetention::from_secs(ninety_days)
            )
            .get()
            .as_secs(),
            fourteen_days,
            "the 14-day default is within the window and never clamped"
        );
    }

    #[test]
    fn a_small_grace_is_clamped_to_the_hard_minimum() {
        assert_eq!(
            lfs_grace(
                GcGrace::from_secs(0),
                ReflogRetention::from_secs(90 * 86_400)
            )
            .get(),
            LFS_GRACE_MIN
        );
        assert_eq!(
            lfs_grace(
                GcGrace::from_secs(60),
                ReflogRetention::from_secs(90 * 86_400)
            )
            .get(),
            LFS_GRACE_MIN
        );
    }

    #[test]
    fn grace_never_exceeds_the_reflog_retention() {
        let short_reflog = MIN_REFLOG_RETENTION_SECS as u64;
        assert_eq!(
            lfs_grace(
                GcGrace::from_secs(u64::MAX),
                ReflogRetention::from_secs(short_reflog)
            )
            .get()
            .as_secs(),
            short_reflog,
            "a grace above the reflog retention is clamped to it"
        );
    }
}
