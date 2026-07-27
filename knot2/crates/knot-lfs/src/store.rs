use std::collections::{HashMap, HashSet};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, SystemTime};

use knot_types::RepoDid;
use sha2::{Digest, Sha256};

use crate::types::RepoPrefix;
use crate::{ClaimedSize, FreeSpaceFloor, LfsError, LfsOid, LfsSize, LfsStorePath, ObjectRelPath};

pub trait LfsStore: Send + Sync {
    fn put(
        &self,
        repo: &RepoDid,
        oid: &LfsOid,
        size: ClaimedSize,
        body: &mut dyn Read,
    ) -> Result<(), LfsError>;

    fn read(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Box<dyn Read + Send>, LfsError>;

    fn probe(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError>;

    fn touch(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredObject {
    pub oid: LfsOid,
    pub size: LfsSize,
    pub mtime: SystemTime,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reclaimed {
    Swept(LfsSize),
    Spared,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OrphanSweep {
    pub prefixes: usize,
    pub objects: usize,
    pub bytes: LfsSize,
}

pub(crate) fn expired(now: SystemTime, mtime: SystemTime, grace: Duration) -> bool {
    now.duration_since(mtime)
        .map(|age| age >= grace)
        .unwrap_or(false)
}

const COPY_CHUNK: usize = 64 * 1024;

fn io_at(op: &'static str, path: &Path) -> impl FnOnce(std::io::Error) -> LfsError {
    let path = path.to_path_buf();
    move |source| LfsError::Io { op, path, source }
}

pub(crate) fn for_each_chunk(
    body: &mut dyn Read,
    mut step: impl FnMut(&[u8]) -> Result<(), LfsError>,
) -> Result<(), LfsError> {
    let mut buffer = vec![0u8; COPY_CHUNK];
    std::iter::from_fn(|| match body.read(&mut buffer) {
        Ok(0) => None,
        Ok(count) => Some(step(&buffer[..count])),
        Err(source) if source.kind() == std::io::ErrorKind::Interrupted => Some(Ok(())),
        Err(source) => Some(Err(LfsError::BodyRead { source })),
    })
    .try_for_each(std::convert::identity)
}

fn write_verified(
    declared: &LfsOid,
    size: ClaimedSize,
    body: &mut dyn Read,
    mut sink: impl FnMut(&[u8]) -> Result<(), LfsError>,
) -> Result<(), LfsError> {
    let mut hasher = Sha256::new();
    let mut received: u64 = 0;
    for_each_chunk(body, |chunk| {
        received += chunk.len() as u64;
        if received > size.get() {
            return Err(LfsError::SizeMismatch {
                declared: size,
                received: LfsSize::new(received),
            });
        }
        hasher.update(chunk);
        sink(chunk)
    })?;
    if received != size.get() {
        return Err(LfsError::SizeMismatch {
            declared: size,
            received: LfsSize::new(received),
        });
    }
    let computed = LfsOid::from_digest(hasher.finalize().into());
    match computed == *declared {
        true => Ok(()),
        false => Err(LfsError::HashMismatch {
            declared: declared.clone(),
            computed,
        }),
    }
}

fn fsync_dir(path: &Path) -> Result<(), LfsError> {
    std::fs::File::open(path)
        .and_then(|dir| dir.sync_all())
        .map_err(io_at("sync dir", path))
}

fn fsync_chain(root: &Path, leaf: &Path) -> Result<(), LfsError> {
    leaf.ancestors()
        .take_while(|dir| dir.starts_with(root))
        .try_for_each(fsync_dir)
}

const INCOMING_DIR: &str = ".incoming";
const OID_LOCK_STRIPES: usize = 64;

fn set_mtime_now(path: &Path) -> Result<(), LfsError> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .and_then(|file| file.set_modified(SystemTime::now()))
        .map_err(io_at("touch mtime", path))
}

fn subdirs(path: &Path) -> Result<Vec<PathBuf>, LfsError> {
    match std::fs::read_dir(path) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(io_at("read dir", path))
            })
            .filter(|entry| entry.as_ref().map(|path| path.is_dir()).unwrap_or(true))
            .collect(),
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(source) => Err(io_at("read dir", path)(source)),
    }
}

fn stored_object(path: &Path) -> Result<Option<StoredObject>, LfsError> {
    let Some(oid) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| LfsOid::new(name).ok())
    else {
        return Ok(None);
    };
    let meta = std::fs::metadata(path).map_err(io_at("stat", path))?;
    let mtime = meta.modified().map_err(io_at("read mtime", path))?;
    Ok(Some(StoredObject {
        oid,
        size: LfsSize::new(meta.len()),
        mtime,
    }))
}

fn is_object_file(path: &Path) -> bool {
    path.is_file()
        && path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| LfsOid::new(name).is_ok())
}

fn is_shard_nibble(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.len() == 2
                && name
                    .bytes()
                    .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        })
}

fn is_object_leaf(dir: &Path) -> bool {
    is_shard_nibble(dir) && dir.parent().is_some_and(is_shard_nibble)
}

fn discover_prefixes(dir: &Path) -> Result<Vec<PathBuf>, LfsError> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries
            .map(|entry| {
                entry
                    .map(|entry| entry.path())
                    .map_err(io_at("read dir", dir))
            })
            .collect::<Result<Vec<_>, _>>()?,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => return Err(io_at("read dir", dir)(source)),
    };
    if is_object_leaf(dir) && entries.iter().any(|path| is_object_file(path)) {
        return Ok(dir
            .parent()
            .and_then(|shard| shard.parent())
            .map(Path::to_path_buf)
            .into_iter()
            .collect());
    }
    entries
        .iter()
        .filter(|path| path.is_dir())
        .map(|sub| discover_prefixes(sub))
        .collect::<Result<Vec<_>, _>>()
        .map(|nested| nested.into_iter().flatten().collect())
}

fn enumerate_prefix(prefix: &Path) -> Result<Vec<StoredObject>, LfsError> {
    subdirs(prefix)?
        .iter()
        .map(|shard| subdirs(shard))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .map(|nibble| match std::fs::read_dir(&nibble) {
            Ok(entries) => entries
                .map(|entry| {
                    entry
                        .map(|entry| entry.path())
                        .map_err(io_at("read dir", &nibble))
                })
                .collect::<Result<Vec<_>, _>>(),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(io_at("read dir", &nibble)(source)),
        })
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .filter_map(|file| stored_object(&file).transpose())
        .collect()
}

pub struct DiskStore {
    root: LfsStorePath,
    locks: Box<[Mutex<()>]>,
}

impl DiskStore {
    pub fn open(root: LfsStorePath) -> Result<Self, LfsError> {
        let incoming = root.as_path().join(INCOMING_DIR);
        std::fs::create_dir_all(&incoming).map_err(io_at("create dir", &incoming))?;
        std::fs::read_dir(&incoming)
            .map_err(io_at("read dir", &incoming))?
            .try_for_each(|entry| {
                let path = entry.map_err(io_at("read dir", &incoming))?.path();
                match std::fs::remove_file(&path) {
                    Ok(()) => Ok(()),
                    Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(source) => Err(io_at("remove abandoned upload", &path)(source)),
                }
            })?;
        let locks = std::iter::repeat_with(|| Mutex::new(()))
            .take(OID_LOCK_STRIPES)
            .collect();
        Ok(Self { root, locks })
    }

    fn object_path(&self, repo: &RepoDid, oid: &LfsOid) -> Result<PathBuf, LfsError> {
        Ok(self.root.object_path(&ObjectRelPath::new(repo, oid)?))
    }

    fn oid_lock(&self, oid: &LfsOid) -> &Mutex<()> {
        let stripe = u8::from_str_radix(&oid.as_str()[0..2], 16).unwrap_or(0) as usize;
        &self.locks[stripe % OID_LOCK_STRIPES]
    }
}

impl LfsStore for DiskStore {
    fn put(
        &self,
        repo: &RepoDid,
        oid: &LfsOid,
        size: ClaimedSize,
        body: &mut dyn Read,
    ) -> Result<(), LfsError> {
        let target = self.object_path(repo, oid)?;
        let incoming = self.root.as_path().join(INCOMING_DIR);
        let mut temp = tempfile::Builder::new()
            .prefix("put-")
            .tempfile_in(&incoming)
            .map_err(io_at("create temp under", &incoming))?;
        let temp_path = temp.path().to_path_buf();
        write_verified(oid, size, body, |chunk| {
            temp.as_file_mut()
                .write_all(chunk)
                .map_err(io_at("write", &temp_path))
        })?;
        temp.as_file()
            .sync_all()
            .map_err(io_at("sync", &temp_path))?;
        let parent = target
            .parent()
            .expect("object path always has a shard parent");
        let _guard = self
            .oid_lock(oid)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::fs::create_dir_all(parent).map_err(io_at("create dir", parent))?;
        temp.persist(&target).map_err(|fault| LfsError::Io {
            op: "rename into",
            path: target.clone(),
            source: fault.error,
        })?;
        fsync_chain(self.root.as_path(), parent)
    }

    fn read(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Box<dyn Read + Send>, LfsError> {
        let path = self.object_path(repo, oid)?;
        match std::fs::File::open(&path) {
            Ok(file) => Ok(Box::new(file)),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Err(LfsError::NotFound { oid: oid.clone() })
            }
            Err(source) => Err(io_at("open", &path)(source)),
        }
    }

    fn probe(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError> {
        Ok(self.object_file(repo, oid)?.map(|(size, _)| size))
    }

    fn touch(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError> {
        let path = self.object_path(repo, oid)?;
        let _guard = self
            .oid_lock(oid)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match std::fs::metadata(&path) {
            Ok(meta) => {
                set_mtime_now(&path)?;
                Ok(Some(LfsSize::new(meta.len())))
            }
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_at("stat", &path)(source)),
        }
    }
}

impl DiskStore {
    pub fn object_file(
        &self,
        repo: &RepoDid,
        oid: &LfsOid,
    ) -> Result<Option<(LfsSize, PathBuf)>, LfsError> {
        let path = self.object_path(repo, oid)?;
        match std::fs::metadata(&path) {
            Ok(meta) => Ok(Some((LfsSize::new(meta.len()), path))),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(io_at("stat", &path)(source)),
        }
    }

    pub fn probe_ready(&self) -> Result<(), LfsError> {
        let incoming = self.root.as_path().join(INCOMING_DIR);
        tempfile::tempfile_in(&incoming)
            .map(|_| ())
            .map_err(io_at("probe writability under", &incoming))
    }

    pub fn remove_repo(&self, repo: &RepoDid) -> Result<(), LfsError> {
        let prefix = self.root.as_path().join(RepoPrefix::new(repo)?.as_path());
        match std::fs::remove_dir_all(&prefix) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(source) => Err(io_at("remove repo prefix", &prefix)(source)),
        }
    }

    pub fn enumerate(&self, repo: &RepoDid) -> Result<Vec<StoredObject>, LfsError> {
        let prefix = self.root.as_path().join(RepoPrefix::new(repo)?.as_path());
        enumerate_prefix(&prefix)
    }

    pub fn collect_expired(
        &self,
        repo: &RepoDid,
        oid: &LfsOid,
        grace: Duration,
        now: SystemTime,
    ) -> Result<Reclaimed, LfsError> {
        let path = self.object_path(repo, oid)?;
        let _guard = self
            .oid_lock(oid)
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let meta = match std::fs::metadata(&path) {
            Ok(meta) => meta,
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Reclaimed::Spared);
            }
            Err(source) => return Err(io_at("stat", &path)(source)),
        };
        let mtime = meta.modified().map_err(io_at("read mtime", &path))?;
        if !expired(now, mtime, grace) {
            return Ok(Reclaimed::Spared);
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(Reclaimed::Swept(LfsSize::new(meta.len()))),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => Ok(Reclaimed::Spared),
            Err(source) => Err(io_at("remove object", &path)(source)),
        }
    }

    pub fn sweep_orphans(
        &self,
        hosted: &HashSet<RepoDid>,
        grace: Duration,
        now: SystemTime,
    ) -> Result<OrphanSweep, LfsError> {
        let root = self.root.as_path();
        let expected: HashSet<PathBuf> = hosted
            .iter()
            .filter_map(|repo| RepoPrefix::new(repo).ok())
            .map(|prefix| prefix.as_path().to_path_buf())
            .collect();
        let orphans: HashSet<PathBuf> = discover_prefixes(root)?
            .into_iter()
            .filter(|prefix| {
                prefix
                    .strip_prefix(root)
                    .ok()
                    .filter(|rel| matches!(rel.components().count(), 2 | 3))
                    .map(|rel| !expected.contains(rel))
                    .unwrap_or(false)
            })
            .collect();
        orphans
            .iter()
            .map(|prefix| self.reclaim_orphan(prefix, grace, now))
            .try_fold(OrphanSweep::default(), |acc, outcome| {
                let outcome = outcome?;
                Ok(OrphanSweep {
                    prefixes: acc.prefixes + outcome.prefixes,
                    objects: acc.objects + outcome.objects,
                    bytes: acc.bytes.saturating_add(outcome.bytes),
                })
            })
    }

    fn reclaim_orphan(
        &self,
        prefix: &Path,
        grace: Duration,
        now: SystemTime,
    ) -> Result<OrphanSweep, LfsError> {
        let objects = enumerate_prefix(prefix)?;
        let live = objects
            .iter()
            .any(|object| !expired(now, object.mtime, grace));
        if live {
            return Ok(OrphanSweep::default());
        }
        match std::fs::remove_dir_all(prefix) {
            Ok(()) => Ok(OrphanSweep {
                prefixes: 1,
                objects: objects.len(),
                bytes: objects
                    .iter()
                    .map(|object| object.size)
                    .fold(LfsSize::new(0), LfsSize::saturating_add),
            }),
            Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
                Ok(OrphanSweep::default())
            }
            Err(source) => Err(io_at("remove orphan prefix", prefix)(source)),
        }
    }
}

#[derive(Clone)]
pub struct LfsHandle {
    pub store: std::sync::Arc<DiskStore>,
    pub admission: std::sync::Arc<crate::StoreAdmission>,
}

impl LfsHandle {
    pub fn open(
        root: LfsStorePath,
        max_object: LfsSize,
        free_space_floor: FreeSpaceFloor,
    ) -> Result<Self, LfsError> {
        let admission = crate::StoreAdmission::new(root.clone(), max_object, free_space_floor);
        Ok(Self {
            store: std::sync::Arc::new(DiskStore::open(root)?),
            admission: std::sync::Arc::new(admission),
        })
    }
}

#[derive(Default)]
pub struct MemoryStore {
    objects: Mutex<HashMap<ObjectRelPath, Vec<u8>>>,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, HashMap<ObjectRelPath, Vec<u8>>> {
        self.objects.lock().expect("lfs memory store lock poisoned")
    }
}

impl LfsStore for MemoryStore {
    fn put(
        &self,
        repo: &RepoDid,
        oid: &LfsOid,
        size: ClaimedSize,
        body: &mut dyn Read,
    ) -> Result<(), LfsError> {
        let rel = ObjectRelPath::new(repo, oid)?;
        let mut bytes = Vec::new();
        write_verified(oid, size, body, |chunk| {
            bytes.extend_from_slice(chunk);
            Ok(())
        })?;
        self.locked().insert(rel, bytes);
        Ok(())
    }

    fn read(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Box<dyn Read + Send>, LfsError> {
        let rel = ObjectRelPath::new(repo, oid)?;
        self.locked()
            .get(&rel)
            .cloned()
            .map(|bytes| Box::new(std::io::Cursor::new(bytes)) as Box<dyn Read + Send>)
            .ok_or_else(|| LfsError::NotFound { oid: oid.clone() })
    }

    fn probe(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError> {
        let rel = ObjectRelPath::new(repo, oid)?;
        Ok(self
            .locked()
            .get(&rel)
            .map(|bytes| LfsSize::new(bytes.len() as u64)))
    }

    fn touch(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError> {
        self.probe(repo, oid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONTH: Duration = Duration::from_secs(30 * 86_400);
    const GRACE: Duration = Duration::from_secs(14 * 86_400);

    fn oid_of(bytes: &[u8]) -> LfsOid {
        LfsOid::from_digest(Sha256::digest(bytes).into())
    }

    fn disk() -> (DiskStore, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let store = DiskStore::open(LfsStorePath::new(dir.path())).unwrap();
        (store, dir)
    }

    fn seed(store: &DiskStore, repo: &RepoDid, body: &[u8]) -> (LfsOid, LfsSize) {
        let oid = oid_of(body);
        let bytes = body.len() as u64;
        store
            .put(repo, &oid, ClaimedSize::new(bytes), &mut &body[..])
            .unwrap();
        (oid, LfsSize::new(bytes))
    }

    fn backdate(store: &DiskStore, repo: &RepoDid, oid: &LfsOid, past: Duration) {
        let path = store.object_file(repo, oid).unwrap().unwrap().1;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - past)
            .unwrap();
    }

    fn read_back(store: &dyn LfsStore, repo: &RepoDid, oid: &LfsOid) -> Vec<u8> {
        let mut out = Vec::new();
        store
            .read(repo, oid)
            .unwrap()
            .read_to_end(&mut out)
            .unwrap();
        out
    }

    fn store_contract(store: &dyn LfsStore) {
        let repo = RepoDid::new("did:plc:squid").unwrap();
        let body: &[u8] = b"lfs media bytes for the round trip";
        let oid = oid_of(body);
        let size = LfsSize::new(body.len() as u64);

        assert_eq!(store.probe(&repo, &oid).unwrap(), None);
        assert!(matches!(
            store.read(&repo, &oid),
            Err(LfsError::NotFound { .. })
        ));

        let claim = ClaimedSize::new(size.get());
        store.put(&repo, &oid, claim, &mut &body[..]).unwrap();
        store.put(&repo, &oid, claim, &mut &body[..]).unwrap();
        assert_eq!(
            store.probe(&repo, &oid).unwrap(),
            Some(size),
            "a re-put of identical bytes is idempotent"
        );

        let other = RepoDid::new("did:plc:limpet").unwrap();
        assert_eq!(store.probe(&other, &oid).unwrap(), None);
        assert!(matches!(
            store.read(&other, &oid),
            Err(LfsError::NotFound { .. })
        ));
    }

    #[test]
    fn every_store_honors_the_contract() {
        store_contract(&MemoryStore::new());
        let (store, _dir) = disk();
        store_contract(&store);
    }

    #[test]
    fn a_body_longer_than_its_declared_size_errors_before_the_end() {
        let store = MemoryStore::new();
        let repo = RepoDid::new("did:plc:squid").unwrap();
        let mut endless = std::io::repeat(0x5a);
        assert!(matches!(
            store.put(
                &repo,
                &oid_of(b"whatever"),
                ClaimedSize::new(8),
                &mut endless
            ),
            Err(LfsError::SizeMismatch { .. })
        ));
    }

    #[test]
    fn disk_writes_are_sharded_and_the_boot_sweep_clears_only_temp_files() {
        let (store, dir) = disk();
        let repo = RepoDid::new("did:plc:squid").unwrap();
        let (oid, size) = seed(&store, &repo, b"sharded placement");
        let sharded = dir
            .path()
            .join("plc/sq/uid")
            .join(&oid.as_str()[0..2])
            .join(&oid.as_str()[2..4])
            .join(oid.as_str());
        assert_eq!(std::fs::read(&sharded).unwrap(), b"sharded placement");

        let method = RepoDid::new("did:incoming:squid").unwrap();
        seed(
            &store,
            &method,
            b"a method named incoming mustn't alias the temp dir",
        );

        let tampered = oid_of(b"a different object");
        assert!(matches!(
            store.put(&repo, &tampered, ClaimedSize::new(4), &mut &b"nope"[..]),
            Err(LfsError::HashMismatch { .. })
        ));
        assert_eq!(store.probe(&repo, &tampered).unwrap(), None);
        let incoming = dir.path().join(INCOMING_DIR);
        assert!(
            std::fs::read_dir(&incoming).unwrap().next().is_none(),
            "a failed put leaves no files in the incoming dir"
        );

        std::fs::write(incoming.join("put-torn4321"), b"partial bytes from a crash").unwrap();
        let store = DiskStore::open(LfsStorePath::new(dir.path())).unwrap();
        assert!(
            std::fs::read_dir(&incoming).unwrap().next().is_none(),
            "the boot sweep clears abandoned uploads"
        );
        assert_eq!(store.probe(&repo, &oid).unwrap(), Some(size));
        assert_eq!(read_back(&store, &repo, &oid), b"sharded placement");
    }

    #[test]
    fn remove_repo_reclaims_the_prefix_and_spares_shard_neighbors() {
        let (store, _dir) = disk();
        let doomed = RepoDid::new("did:plc:squid").unwrap();
        let neighbor = RepoDid::new("did:plc:squirrel").unwrap();
        let (oid, size) = seed(&store, &doomed, b"prefix removal");
        seed(&store, &neighbor, b"prefix removal");
        store.remove_repo(&doomed).unwrap();
        assert_eq!(store.probe(&doomed, &oid).unwrap(), None);
        assert_eq!(store.probe(&neighbor, &oid).unwrap(), Some(size));
        store.remove_repo(&doomed).unwrap();
    }

    #[test]
    fn collect_touch_and_enumerate_govern_the_sweep_per_object() {
        let (store, _dir) = disk();
        let repo = RepoDid::new("did:plc:squid").unwrap();
        assert!(
            store
                .enumerate(&RepoDid::new("did:plc:limpet").unwrap())
                .unwrap()
                .is_empty(),
            "a missing prefix enumerates to nothing"
        );

        let (stale, stale_size) = seed(&store, &repo, b"long unreferenced");
        let (fresh, _) = seed(&store, &repo, b"still within grace");
        let (vouched, vouched_size) = seed(&store, &repo, b"vouched for moments before the sweep");
        backdate(&store, &repo, &stale, MONTH);
        backdate(&store, &repo, &vouched, MONTH);
        let now = SystemTime::now();

        let listed: HashSet<LfsOid> = store
            .enumerate(&repo)
            .unwrap()
            .into_iter()
            .map(|object| object.oid)
            .collect();
        assert_eq!(
            listed,
            HashSet::from([stale.clone(), fresh.clone(), vouched.clone()])
        );

        assert_eq!(
            store.collect_expired(&repo, &fresh, GRACE, now).unwrap(),
            Reclaimed::Spared,
            "a fresh object is inside its grace window"
        );
        assert_eq!(
            store.collect_expired(&repo, &stale, GRACE, now).unwrap(),
            Reclaimed::Swept(stale_size)
        );
        assert_eq!(store.probe(&repo, &stale).unwrap(), None);
        assert_eq!(
            store.collect_expired(&repo, &stale, GRACE, now).unwrap(),
            Reclaimed::Spared,
            "collecting an already-gone object is a no-op"
        );

        assert_eq!(store.touch(&repo, &vouched).unwrap(), Some(vouched_size));
        assert_eq!(
            store
                .collect_expired(&repo, &vouched, GRACE, SystemTime::now())
                .unwrap(),
            Reclaimed::Spared,
            "the touch bumped the mtime inside the grace window"
        );
        assert_eq!(store.probe(&repo, &vouched).unwrap(), Some(vouched_size));
    }

    #[test]
    fn the_orphan_sweep_reclaims_unregistered_prefixes_and_spares_every_other_class() {
        let (store, dir) = disk();
        let hosted = RepoDid::new("did:plc:squid").unwrap();
        let orphan = RepoDid::new("did:plc:limpet").unwrap();
        let fresh = RepoDid::new("did:plc:cuttle").unwrap();
        let short_hosted = RepoDid::new("did:web:ab").unwrap();
        let short_orphan = RepoDid::new("did:web:cd").unwrap();
        assert_eq!(
            RepoPrefix::new(&short_hosted)
                .unwrap()
                .as_path()
                .components()
                .count(),
            2,
            "a short method-specific-id shards to a two-component prefix"
        );

        let (kept, kept_size) = seed(&store, &hosted, b"belongs to a live repo");
        let (doomed, doomed_size) = seed(&store, &orphan, b"repo was deleted");
        let (spared, spared_size) = seed(&store, &fresh, b"deleted repo, but only just");
        let (short_kept, short_kept_size) = seed(&store, &short_hosted, b"live short-did object");
        let (short_doomed, short_doomed_size) =
            seed(&store, &short_orphan, b"orphaned short-did media");
        [
            (&hosted, &kept),
            (&orphan, &doomed),
            (&short_hosted, &short_kept),
            (&short_orphan, &short_doomed),
        ]
        .iter()
        .for_each(|(did, oid)| backdate(&store, did, oid, MONTH));

        std::fs::write(
            dir.path().join(doomed.as_str()),
            b"stray at the wrong depth",
        )
        .unwrap();

        let registry = HashSet::from([hosted.clone(), short_hosted.clone()]);
        let sweep = store
            .sweep_orphans(&registry, GRACE, SystemTime::now())
            .unwrap();

        assert_eq!(sweep.prefixes, 2, "both past-grace orphans are reclaimed");
        assert_eq!(sweep.objects, 2, "one object under each reclaimed prefix");
        assert_eq!(sweep.bytes, doomed_size.saturating_add(short_doomed_size));
        assert_eq!(store.probe(&orphan, &doomed).unwrap(), None);
        assert_eq!(store.probe(&short_orphan, &short_doomed).unwrap(), None);
        assert_eq!(
            store.probe(&hosted, &kept).unwrap(),
            Some(kept_size),
            "a hosted prefix is never an orphan"
        );
        assert_eq!(
            store.probe(&short_hosted, &short_kept).unwrap(),
            Some(short_kept_size),
            "a hosted repo shallower than the oid shards survives too"
        );
        assert_eq!(
            store.probe(&fresh, &spared).unwrap(),
            Some(spared_size),
            "a fresh orphan is held by the grace window"
        );
    }
}
