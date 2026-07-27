use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use knot_git::{GitError, Layout, Repo};
use knot_types::{ObjectFormat, RepoDid};

use crate::mapping::AdoptRepo;
use crate::source::SourceRepoDid;

#[derive(Debug, thiserror::Error)]
pub enum AdoptError {
    #[error("layout path for {repo}: {source}")]
    Layout { repo: RepoDid, source: GitError },
    #[error("repo {repo} resolves to the reserved knot meta-repo path")]
    ReservesMeta { repo: RepoDid },
    #[error("place {path}: {source}")]
    Place {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("sync {path}: {source}")]
    Sync {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("adopted repo {repo} doesn't open as a git repository: {source}")]
    Unopenable { repo: RepoDid, source: GitError },
    #[error("source directory for {repo} vanished between mapping and adoption")]
    Vanished { repo: RepoDid },
    #[error("consuming the source needs {scan_path} and {target} on one filesystem")]
    CrossDeviceConsume { scan_path: PathBuf, target: PathBuf },
}

#[derive(Debug, PartialEq, Eq)]
pub struct AdoptOutcome {
    pub transfer: Transfer,
    pub adopted: u64,
    pub already_present: u64,
    pub sha1: u64,
    pub sha256: u64,
}

impl AdoptOutcome {
    fn empty(transfer: Transfer) -> Self {
        Self {
            transfer,
            adopted: 0,
            already_present: 0,
            sha1: 0,
            sha256: 0,
        }
    }

    fn merge(self, other: Self) -> Self {
        Self {
            transfer: self.transfer,
            adopted: self.adopted + other.adopted,
            already_present: self.already_present + other.already_present,
            sha1: self.sha1 + other.sha1,
            sha256: self.sha256 + other.sha256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourcePolicy {
    Preserve,
    Consume,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transfer {
    Rename,
    Copy,
}

impl std::fmt::Display for Transfer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Rename => f.write_str("rename"),
            Self::Copy => f.write_str("copy"),
        }
    }
}

enum Placement {
    AlreadyPresent,
    Staged(PathBuf),
    Moved,
}

struct Staged<'repo> {
    did: &'repo RepoDid,
    destination: PathBuf,
    placement: Placement,
}

pub fn source_dir(source_root: &Path, repo_did: &SourceRepoDid) -> PathBuf {
    source_root.join(repo_did.as_str())
}

pub fn source_is_repo(source_root: &Path, repo_did: &SourceRepoDid) -> bool {
    source_dir(source_root, repo_did).join("HEAD").is_file()
}

pub fn adopt_all(
    layout: &Layout,
    source_root: &Path,
    repos: &[AdoptRepo],
    policy: SourcePolicy,
) -> Result<AdoptOutcome, AdoptError> {
    let root = layout.scratch_dir();
    std::fs::create_dir_all(root).map_err(|source| AdoptError::Place {
        path: root.to_path_buf(),
        source,
    })?;
    let transfer = transfer_mode(source_root, root, policy)?;
    let staged = in_lanes(repos, |repo| stage_one(layout, source_root, repo, transfer))?;
    sync_filesystem(root)?;
    staged.iter().try_for_each(commit_one)?;
    sync_filesystem(root)?;
    in_lanes(&staged, |staged| count_one(staged, transfer)).map(|counted| {
        counted
            .into_iter()
            .fold(AdoptOutcome::empty(transfer), AdoptOutcome::merge)
    })
}

fn transfer_mode(
    source_root: &Path,
    target_root: &Path,
    policy: SourcePolicy,
) -> Result<Transfer, AdoptError> {
    use std::os::unix::fs::MetadataExt;
    let device = |path: &Path| {
        std::fs::metadata(path)
            .map(|meta| meta.dev())
            .map_err(|source| AdoptError::Place {
                path: path.to_path_buf(),
                source,
            })
    };
    let one_filesystem = device(source_root)? == device(target_root)?;
    match (policy, one_filesystem) {
        (SourcePolicy::Consume, true) => Ok(Transfer::Rename),
        (SourcePolicy::Consume, false) => Err(AdoptError::CrossDeviceConsume {
            scan_path: source_root.to_path_buf(),
            target: target_root.to_path_buf(),
        }),
        (SourcePolicy::Preserve, _) => Ok(Transfer::Copy),
    }
}

fn in_lanes<'items, T, R, F>(items: &'items [T], work: F) -> Result<Vec<R>, AdoptError>
where
    T: Sync,
    R: Send,
    F: Fn(&'items T) -> Result<R, AdoptError> + Sync,
{
    let cursor = AtomicUsize::new(0);
    std::thread::scope(|scope| {
        (0..lane_count(items.len()))
            .map(|_| {
                scope.spawn(|| {
                    std::iter::from_fn(|| items.get(cursor.fetch_add(1, Ordering::Relaxed)))
                        .map(&work)
                        .collect::<Result<Vec<R>, AdoptError>>()
                })
            })
            .collect::<Vec<_>>()
            .into_iter()
            .map(|lane| lane.join().expect("adoption lane doesn't panic"))
            .collect::<Result<Vec<Vec<R>>, AdoptError>>()
            .map(|lanes| lanes.into_iter().flatten().collect())
    })
}

fn lane_count(items: usize) -> usize {
    std::thread::available_parallelism()
        .map(std::num::NonZeroUsize::get)
        .unwrap_or(1)
        .min(items.max(1))
}

fn stage_one<'repo>(
    layout: &Layout,
    source_root: &Path,
    repo: &'repo AdoptRepo,
    transfer: Transfer,
) -> Result<Staged<'repo>, AdoptError> {
    let destination = layout
        .guarded_path(&repo.did)
        .map_err(|source| match source {
            GitError::ReservedDid(_) => AdoptError::ReservesMeta {
                repo: repo.did.clone(),
            },
            source => AdoptError::Layout {
                repo: repo.did.clone(),
                source,
            },
        })?;
    match destination.exists() {
        true => Ok(Staged {
            did: &repo.did,
            destination,
            placement: Placement::AlreadyPresent,
        }),
        false => {
            let source = source_dir(source_root, &repo.source_did);
            match source.is_dir() {
                false => Err(AdoptError::Vanished {
                    repo: repo.did.clone(),
                }),
                true => place(&source, &destination, transfer).map(|placement| Staged {
                    did: &repo.did,
                    destination,
                    placement,
                }),
            }
        }
    }
}

fn place(source: &Path, destination: &Path, transfer: Transfer) -> Result<Placement, AdoptError> {
    match transfer {
        Transfer::Rename => {
            make_parent(destination)?;
            std::fs::rename(source, destination)
                .map(|()| Placement::Moved)
                .map_err(|error| AdoptError::Place {
                    path: destination.to_path_buf(),
                    source: error,
                })
        }
        Transfer::Copy => stage_tree(source, destination).map(Placement::Staged),
    }
}

fn commit_one(staged: &Staged<'_>) -> Result<(), AdoptError> {
    match &staged.placement {
        Placement::AlreadyPresent | Placement::Moved => Ok(()),
        Placement::Staged(staging) => {
            std::fs::rename(staging, &staged.destination).map_err(|source| AdoptError::Place {
                path: staged.destination.clone(),
                source,
            })
        }
    }
}

fn count_one(staged: &Staged<'_>, transfer: Transfer) -> Result<AdoptOutcome, AdoptError> {
    let opened = Repo::open(&staged.destination).map_err(|source| AdoptError::Unopenable {
        repo: staged.did.clone(),
        source,
    })?;
    let fresh = u64::from(!matches!(staged.placement, Placement::AlreadyPresent));
    let counted = AdoptOutcome {
        adopted: fresh,
        already_present: 1 - fresh,
        ..AdoptOutcome::empty(transfer)
    };
    Ok(match opened.object_format() {
        ObjectFormat::SHA1 => AdoptOutcome { sha1: 1, ..counted },
        _ => AdoptOutcome {
            sha256: 1,
            ..counted
        },
    })
}

fn make_parent(destination: &Path) -> Result<&Path, AdoptError> {
    let parent = destination
        .parent()
        .expect("layout repo paths always have a parent");
    std::fs::create_dir_all(parent)
        .map(|()| parent)
        .map_err(|source| AdoptError::Place {
            path: parent.to_path_buf(),
            source,
        })
}

fn stage_tree(source: &Path, destination: &Path) -> Result<PathBuf, AdoptError> {
    let io = |path: &Path| {
        let path = path.to_path_buf();
        move |source: std::io::Error| AdoptError::Place { path, source }
    };
    let parent = make_parent(destination)?;
    let staging = parent.join(format!(
        ".migrate-staging.{}",
        destination
            .file_name()
            .expect("layout repo paths always have a file name")
            .to_string_lossy()
    ));
    if staging.exists() {
        std::fs::remove_dir_all(&staging).map_err(io(&staging))?;
    }
    place_tree(source, &staging).map(|()| staging)
}

fn place_tree(source: &Path, destination: &Path) -> Result<(), AdoptError> {
    walkdir::WalkDir::new(source)
        .into_iter()
        .try_for_each(|entry| {
            let entry = entry.map_err(|error| AdoptError::Place {
                path: source.to_path_buf(),
                source: error.into(),
            })?;
            let relative = entry
                .path()
                .strip_prefix(source)
                .expect("walkdir yields paths under its root");
            let target = destination.join(relative);
            let io = |source: std::io::Error| AdoptError::Place {
                path: entry.path().to_path_buf(),
                source,
            };
            match entry.file_type() {
                kind if kind.is_dir() => std::fs::create_dir_all(&target).map_err(io),
                #[cfg(unix)]
                kind if kind.is_symlink() => std::fs::read_link(entry.path())
                    .and_then(|link| std::os::unix::fs::symlink(link, &target))
                    .map_err(io),
                _ => std::fs::copy(entry.path(), &target).map(|_| ()).map_err(io),
            }
        })
}

fn sync_filesystem(root: &Path) -> Result<(), AdoptError> {
    std::fs::File::open(root)
        .and_then(|anchor| rustix::fs::syncfs(&anchor).map_err(std::io::Error::from))
        .map_err(|source| AdoptError::Sync {
            path: root.to_path_buf(),
            source,
        })
}
