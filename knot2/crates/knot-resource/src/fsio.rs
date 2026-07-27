use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

const STAGING_INFIX: &str = ".knot-tmp.";

// A staging file belongs to whoever is filling it, and another process filling
// the same target is normal for a knot sharing a volume with a migrate
// or maintenance run. Only reclaim one old enough that no live writer could
// still own it.
const STAGING_REAP_AFTER: Duration = Duration::from_secs(3600);

#[derive(Debug)]
pub struct FsError {
    pub path: PathBuf,
    pub source: io::Error,
}

impl FsError {
    fn at(path: &Path, source: io::Error) -> Self {
        Self {
            path: path.to_path_buf(),
            source,
        }
    }
}

impl std::fmt::Display for FsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.source)
    }
}

impl std::error::Error for FsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileMode {
    Inherited,
    Private,
}

pub fn staging_nonce() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{}.{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

fn staging_prefix(name: &str) -> String {
    format!(".{name}{STAGING_INFIX}")
}

fn pre_hidden_staging_prefix(name: &str) -> String {
    format!("{name}{STAGING_INFIX}")
}

fn remove_matching(
    dir: &Path,
    prefixes: &[&str],
    reclaimable: impl Fn(&std::fs::DirEntry) -> bool,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| prefixes.iter().any(|prefix| name.starts_with(prefix)))
        })
        .filter(|entry| reclaimable(entry))
        .for_each(|entry| {
            let _ = std::fs::remove_file(entry.path());
        });
}

pub fn clear_temps(dir: &Path, prefix: &str) {
    remove_matching(dir, &[prefix], |_| true);
}

pub fn clear_stale(dir: &Path, prefix: &str) {
    let now = SystemTime::now();
    remove_matching(dir, &[prefix], |entry| abandoned(entry, now));
}

fn abandoned(entry: &std::fs::DirEntry, now: SystemTime) -> bool {
    entry
        .metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|modified| now.duration_since(modified).ok())
        .is_some_and(|age| age >= STAGING_REAP_AFTER)
}

pub fn clear_staging(path: &Path) {
    let (Some(parent), Some(name)) = (
        path.parent(),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return;
    };
    let (hidden, pre_hidden) = (staging_prefix(name), pre_hidden_staging_prefix(name));
    let now = SystemTime::now();
    remove_matching(parent, &[&hidden, &pre_hidden], |entry| {
        abandoned(entry, now)
    });
}

pub fn fsync_path(path: &Path) -> Result<(), FsError> {
    match File::open(path) {
        Ok(file) => file.sync_all().map_err(|error| FsError::at(path, error)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(FsError::at(path, error)),
    }
}

fn create(path: &Path, mode: FileMode) -> io::Result<File> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    if mode == FileMode::Private {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

pub fn atomic_write<E, F>(path: &Path, mode: FileMode, fill: F) -> Result<(), E>
where
    F: FnOnce(&mut File) -> Result<(), E>,
    E: From<FsError>,
{
    let (Some(parent), Some(name)) = (
        path.parent(),
        path.file_name().and_then(|name| name.to_str()),
    ) else {
        return Err(FsError::at(path, io::Error::from(io::ErrorKind::InvalidInput)).into());
    };
    clear_staging(path);
    let staging = parent.join(format!("{}{}", staging_prefix(name), staging_nonce()));

    let outcome = create(&staging, mode)
        .map_err(|error| E::from(FsError::at(path, error)))
        .and_then(|mut file| {
            fill(&mut file)?;
            file.sync_all()
                .map_err(|error| E::from(FsError::at(path, error)))
        })
        .and_then(|()| {
            std::fs::rename(&staging, path).map_err(|error| E::from(FsError::at(path, error)))
        });

    match outcome {
        Ok(()) => fsync_path(parent).map_err(Into::into),
        Err(error) => {
            let _ = std::fs::remove_file(&staging);
            Err(error)
        }
    }
}

pub fn atomic_write_bytes(path: &Path, contents: &[u8], mode: FileMode) -> Result<(), FsError> {
    let target = path.to_path_buf();
    atomic_write(path, mode, move |file| {
        file.write_all(contents)
            .map_err(|error| FsError::at(&target, error))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names_in(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_string))
            .collect()
    }

    fn age(path: &Path, by: Duration) {
        let when = SystemTime::now() - by;
        File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(when))
            .unwrap();
    }

    #[test]
    fn a_write_stages_under_a_hidden_name_and_leaves_only_the_target_behind() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("keys.sealed");
        let sibling = dir.path().join("keys.sealed.v2");
        let staging = std::sync::Mutex::new(Vec::new());
        atomic_write::<FsError, _>(&target, FileMode::Private, |file| {
            *staging.lock().unwrap() = names_in(dir.path());
            file.write_all(b"first")
                .map_err(|error| FsError::at(&target, error))
        })
        .unwrap();
        atomic_write_bytes(&target, b"second", FileMode::Private).unwrap();
        atomic_write_bytes(&sibling, b"two", FileMode::Inherited).unwrap();

        let staging = staging.into_inner().unwrap();
        assert_eq!(
            staging
                .iter()
                .filter(|name| name.starts_with("keys.sealed"))
                .count(),
            0,
            "a tool matching on the target's own prefix mustn't find the half-written staging file"
        );
        assert!(
            staging
                .iter()
                .any(|name| name.starts_with(".keys.sealed.knot-tmp.")),
            "saw {staging:?}"
        );
        assert_eq!(std::fs::read(&target).unwrap(), b"second");
        assert_eq!(
            std::fs::read(&sibling).unwrap(),
            b"two",
            "a name that extends another mustn't share its staging path"
        );
        let left = names_in(dir.path());
        assert_eq!(left.len(), 2, "left staging files behind: {left:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&target).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "sealed material stays owner-only");
        }
        assert!(fsync_path(&dir.path().join("never-written")).is_ok());
    }

    #[test]
    fn a_write_that_fails_or_overlaps_another_keeps_what_is_already_stored() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("packed-refs");
        atomic_write_bytes(&target, b"kept", FileMode::Inherited).unwrap();
        let failed: Result<(), FsError> = atomic_write(&target, FileMode::Inherited, |_| {
            Err(FsError::at(&target, io::Error::other("fill failed")))
        });
        assert!(failed.is_err());
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"kept",
            "a failed write mustn't destroy what was already stored"
        );
        assert_eq!(
            names_in(dir.path()).len(),
            1,
            "a failed write removes its staging file"
        );

        let overlapping: Result<(), FsError> = atomic_write(&target, FileMode::Inherited, |file| {
            atomic_write_bytes(
                &target,
                b"the second writer's contents",
                FileMode::Inherited,
            )?;
            file.write_all(b"first writer finishes after")
                .map_err(|error| FsError::at(&target, error))
        });
        assert!(
            overlapping.is_ok(),
            "an overlapping write mustn't delete the staging file this one is filling: \
             {overlapping:?}"
        );
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"first writer finishes after"
        );
    }

    #[test]
    fn a_write_reclaims_aged_staging_files_of_either_naming_and_spares_a_fresh_one() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("packed-refs");
        let crashed = dir.path().join(".packed-refs.knot-tmp.4242.7");
        let pre_hidden = dir.path().join("packed-refs.knot-tmp.4242.8");
        let in_flight = dir.path().join(".packed-refs.knot-tmp.4243.0");
        std::fs::write(&crashed, b"left by a crashed run").unwrap();
        std::fs::write(&pre_hidden, b"left beside the target by an older build").unwrap();
        std::fs::write(&in_flight, b"another process is mid-write").unwrap();
        let aged = STAGING_REAP_AFTER + Duration::from_secs(60);
        age(&crashed, aged);
        age(&pre_hidden, aged);

        atomic_write_bytes(&target, b"fresh", FileMode::Inherited).unwrap();

        assert!(
            !crashed.exists(),
            "the write reclaims a crashed run's staging file"
        );
        assert!(
            !pre_hidden.exists(),
            "moving staging under a dot mustn't strand the temps the previous naming left"
        );
        assert!(
            in_flight.exists(),
            "a second knot on the same volume is a supported deployment \
             whose live staging file this write mustn't sweep out from under its rename"
        );
    }

    #[test]
    fn a_prefix_sweep_spares_a_live_file_only_where_another_writer_could_own_it() {
        let dir = tempfile::tempdir().unwrap();
        let crashed = dir.path().join(".knot-repack.4242.7.pack");
        let in_flight = dir.path().join(".knot-repack.4243.0.pack");
        let installed = dir.path().join("multi-pack-index");
        let bitmap = dir.path().join("multi-pack-index-abc.bitmap");
        std::fs::write(&crashed, b"left by a crashed run").unwrap();
        std::fs::write(&in_flight, b"another process is streaming into this").unwrap();
        std::fs::write(&installed, b"index").unwrap();
        std::fs::write(&bitmap, b"bitmap").unwrap();
        age(&crashed, STAGING_REAP_AFTER + Duration::from_secs(60));

        clear_stale(dir.path(), ".knot-repack.");
        clear_temps(dir.path(), "multi-pack-index");

        assert!(!crashed.exists());
        assert!(
            in_flight.exists(),
            "deleting the staging pack another repack is streaming into fails that run's rename"
        );
        assert!(!installed.exists());
        assert!(
            !bitmap.exists(),
            "removing the index without its bitmap would leave a bitmap describing an index that is gone"
        );
    }
}
