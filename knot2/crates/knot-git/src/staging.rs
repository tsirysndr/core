use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::GitError;
use crate::repo::{Repo, init_bare_with_format};

pub const INCOMING_PREFIX: &str = ".knot-incoming.";

fn incoming_path(git_dir: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    git_dir.join(format!("{INCOMING_PREFIX}{}.{nonce}", std::process::id()))
}

pub struct Staging {
    dir: PathBuf,
    repo: Repo,
}

impl Staging {
    pub fn new(live: &Repo) -> Result<Self, GitError> {
        let dir = incoming_path(live.path());
        let _ = std::fs::remove_dir_all(&dir);
        init_bare_with_format(&dir, live.object_format())
            .map_err(|error| GitError::Staging(format!("init: {error}")))?;
        let info = dir.join("objects").join("info");
        std::fs::create_dir_all(&info)
            .map_err(|error| GitError::Staging(format!("objects/info: {error}")))?;
        std::fs::write(
            info.join("alternates"),
            format!("{}\n", live.objects_dir().display()),
        )
        .map_err(|error| GitError::Staging(format!("alternates: {error}")))?;
        match Repo::open(&dir) {
            Ok(repo) => Ok(Self { dir, repo }),
            Err(error) => {
                let _ = std::fs::remove_dir_all(&dir);
                Err(error)
            }
        }
    }

    pub fn repo(&self) -> &Repo {
        &self.repo
    }

    pub fn migrate_into(&self, live: &Repo) -> Result<(), GitError> {
        migrate(
            &StagingObjects(self.repo.objects_dir()),
            &LiveObjects(live.objects_dir()),
        )
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn is_shard(name: &str) -> bool {
    name.len() == 2 && name.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn fsync_object_path(path: &Path) -> Result<(), GitError> {
    let report =
        |error: std::io::Error| GitError::Staging(format!("fsync {}: {error}", path.display()));
    match std::fs::File::open(path) {
        Ok(file) => file.sync_all().map_err(report),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(report(error)),
    }
}

struct StagingObjects(PathBuf);

struct LiveObjects(PathBuf);

fn migrate(from_objects: &StagingObjects, to_objects: &LiveObjects) -> Result<(), GitError> {
    migrate_packs(from_objects, to_objects)?;
    migrate_loose(from_objects, to_objects)
}

fn migrate_packs(from_objects: &StagingObjects, to_objects: &LiveObjects) -> Result<(), GitError> {
    let from_pack = from_objects.0.join("pack");
    let entries = match std::fs::read_dir(&from_pack) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(GitError::Staging(error.to_string())),
    };
    let to_pack = to_objects.0.join("pack");
    std::fs::create_dir_all(&to_pack).map_err(|error| GitError::Staging(error.to_string()))?;
    let names: Vec<std::ffi::OsString> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_type()
                .map(|kind| kind.is_file())
                .unwrap_or(false)
        })
        .map(|entry| entry.file_name())
        .collect();
    let is_idx =
        |name: &std::ffi::OsString| Path::new(name).extension().is_some_and(|ext| ext == "idx");
    let ordered = names
        .iter()
        .filter(|name| !is_idx(name))
        .chain(names.iter().filter(|name| is_idx(name)));
    ordered.into_iter().try_for_each(|name| {
        let dest = to_pack.join(name);
        std::fs::rename(from_pack.join(name), &dest)
            .map_err(|error| GitError::Staging(format!("migrate pack file: {error}")))?;
        fsync_object_path(&dest)
    })?;
    fsync_object_path(&to_pack)
}

fn migrate_loose(from_objects: &StagingObjects, to_objects: &LiveObjects) -> Result<(), GitError> {
    let entries =
        std::fs::read_dir(&from_objects.0).map_err(|error| GitError::Staging(error.to_string()))?;
    entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().map(|kind| kind.is_dir()).unwrap_or(false)
                && entry.file_name().to_str().is_some_and(is_shard)
        })
        .try_for_each(|shard| {
            let dest_shard = to_objects.0.join(shard.file_name());
            std::fs::create_dir_all(&dest_shard)
                .map_err(|error| GitError::Staging(error.to_string()))?;
            std::fs::read_dir(shard.path())
                .map_err(|error| GitError::Staging(error.to_string()))?
                .filter_map(Result::ok)
                .try_for_each(|object| {
                    let dest = dest_shard.join(object.file_name());
                    std::fs::rename(object.path(), &dest).map_err(|error| {
                        GitError::Staging(format!("migrate loose object: {error}"))
                    })?;
                    fsync_object_path(&dest)
                })?;
            fsync_object_path(&dest_shard)
        })?;
    fsync_object_path(&to_objects.0)
}
