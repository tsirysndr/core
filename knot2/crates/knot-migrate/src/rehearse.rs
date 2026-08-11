use std::path::{Path, PathBuf};

use knot_types::scalar_newtype;

use crate::adopt::{self, AdoptError, SourcePolicy, Transfer};
use crate::mapping::AdoptRepo;

#[derive(Debug, thiserror::Error)]
pub enum ScanPathError {
    #[error(
        "the real run will create {path} under {blocked}, which this process can't write: {source}"
    )]
    Uncreatable {
        path: PathBuf,
        blocked: PathBuf,
        source: rustix::io::Errno,
    },
    #[error("the real run will write repos into {path}, which this process can't write: {source}")]
    Unwritable {
        path: PathBuf,
        source: rustix::io::Errno,
    },
    #[error("the real run can't create {path}, which is a symlink to a missing target")]
    Dangling { path: PathBuf },
    #[error("read {path}: {source}")]
    Unreadable {
        path: PathBuf,
        source: std::io::Error,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum RoomError {
    #[error("measure {path}: {source}")]
    Source {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("read the free space on {path}: {source}")]
    Free {
        path: PathBuf,
        source: rustix::io::Errno,
    },
}

scalar_newtype! {
    pub struct Bytes(u64) => ordered;
}

impl std::fmt::Display for Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
        match UNITS.iter().find(|(_, size)| self.get() >= *size) {
            None => write!(f, "{}B", self.get()),
            Some((unit, size)) => write!(f, "{:.1}{unit}", self.get() as f64 / *size as f64),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Occupancy {
    Fresh,
    Occupied,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Fit {
    Short,
    Clear,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Room {
    pub source: Bytes,
    pub free: Bytes,
}

impl Room {
    pub fn fit(self) -> Fit {
        match self.free >= self.source {
            true => Fit::Clear,
            false => Fit::Short,
        }
    }
}

pub struct Inputs<'a> {
    pub source_repos: &'a Path,
    pub adopted: &'a [AdoptRepo],
    pub scan_path: &'a Path,
    pub policy: SourcePolicy,
}

pub struct Rehearsal {
    pub fallback: Option<PathBuf>,
    pub transfer: Result<Transfer, AdoptError>,
    pub scan_path: Result<Occupancy, ScanPathError>,
    pub room: Option<Result<Room, RoomError>>,
}

impl Rehearsal {
    pub fn run(inputs: Inputs<'_>) -> Self {
        let probed = Probed::nearest(inputs.scan_path);
        let transfer = adopt::transfer_mode(inputs.source_repos, probed.existing, inputs.policy);
        Self {
            fallback: probed.fallback().map(Path::to_path_buf),
            room: match transfer {
                Ok(Transfer::Copy) => Some(room(inputs.source_repos, inputs.adopted, probed)),
                Ok(Transfer::Rename) | Err(_) => None,
            },
            transfer,
            scan_path: occupancy(probed),
        }
    }

    pub fn ready(&self) -> bool {
        self.transfer.is_ok()
            && self.scan_path.is_ok()
            && self.room.as_ref().is_none_or(|room| {
                room.as_ref()
                    .is_ok_and(|measured| matches!(measured.fit(), Fit::Clear))
            })
    }
}

#[derive(Debug, Clone, Copy)]
struct Probed<'a> {
    scan_path: &'a Path,
    existing: &'a Path,
}

impl<'a> Probed<'a> {
    fn nearest(scan_path: &'a Path) -> Self {
        Self {
            scan_path,
            existing: scan_path
                .ancestors()
                .find(|candidate| match std::fs::symlink_metadata(candidate) {
                    Ok(_) => true,
                    Err(error) => !adopt::reads_as_absent(&error),
                })
                .unwrap_or_else(|| Path::new(".")),
        }
    }

    fn fallback(self) -> Option<&'a Path> {
        (self.existing != self.scan_path).then_some(self.existing)
    }
}

fn occupancy(probed: Probed<'_>) -> Result<Occupancy, ScanPathError> {
    match (adopt::writable(probed.existing), probed.fallback()) {
        (Err(source), None) if source == rustix::io::Errno::NOENT => Err(ScanPathError::Dangling {
            path: probed.scan_path.to_path_buf(),
        }),
        (Err(source), None) => Err(ScanPathError::Unwritable {
            path: probed.scan_path.to_path_buf(),
            source,
        }),
        (Err(source), Some(blocked)) => Err(ScanPathError::Uncreatable {
            path: probed.scan_path.to_path_buf(),
            blocked: blocked.to_path_buf(),
            source,
        }),
        (Ok(()), Some(_)) => Ok(Occupancy::Fresh),
        (Ok(()), None) => std::fs::read_dir(probed.scan_path)
            .and_then(|mut entries| entries.next().transpose())
            .map(|entry| match entry {
                Some(_) => Occupancy::Occupied,
                None => Occupancy::Fresh,
            })
            .map_err(|source| ScanPathError::Unreadable {
                path: probed.scan_path.to_path_buf(),
                source,
            }),
    }
}

fn room(source_repos: &Path, adopted: &[AdoptRepo], probed: Probed<'_>) -> Result<Room, RoomError> {
    use std::os::unix::fs::MetadataExt;
    let source = adopted.iter().try_fold(0_u64, |total, repo| {
        let path = adopt::source_dir(source_repos, &repo.source_did);
        walkdir::WalkDir::new(&path)
            .into_iter()
            .try_fold(total, |total, entry| {
                entry
                    .and_then(|entry| entry.metadata())
                    .map(|meta| total + meta.blocks() * 512)
            })
            .map_err(|source| RoomError::Source {
                path,
                source: source.into(),
            })
    })?;
    rustix::fs::statvfs(probed.existing)
        .map(|stat| Room {
            source: Bytes::new(source),
            free: Bytes::new(stat.f_bavail * stat.f_frsize),
        })
        .map_err(|source| RoomError::Free {
            path: probed.existing.to_path_buf(),
            source,
        })
}
