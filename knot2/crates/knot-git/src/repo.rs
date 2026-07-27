use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target};
use knot_cache::{Cache, Moka, Weight};
use knot_types::{
    BranchName, KnotId, ObjectFormat, Oid, RefName, RefTransition, RepoDid, UnixSeconds,
};

use crate::error::GitError;
use crate::objects::{Haves, PackBudget, Walked, Wants};

const RESERVED_PREFIX: &str = "refs/cobs/";
const CHECKPOINT_PREFIX: &str = "refs/cob-checkpoints/";
const HIDDEN_PREFIX: &str = "refs/hidden/";
const REFLOG_COMMITTER_NAME: &str = "knot";
const REFLOG_COMMITTER_EMAIL: &str = "noreply@knot";
const HEADS_PREFIX: &str = "refs/heads/";
const TAGS_PREFIX: &str = "refs/tags/";
const MAX_SYMREF_DEPTH: usize = 5;
const ADVERT_BYTES_PER_REF: u64 = 128;

fn tuned(mut git: gix::Repository) -> gix::Repository {
    git.object_cache_size_if_unset(knot_resource::object_cache_bytes());
    pin_reflog_identity(&mut git);
    git
}

fn assembled(git: gix::Repository, path: PathBuf) -> Repo {
    Repo {
        git: tuned(git),
        path,
        commit_graph: OnceLock::new(),
    }
}

fn pin_reflog_identity(git: &mut gix::Repository) {
    use gix::config::tree::{Committer, Core};
    let mut config = git.config_snapshot_mut();
    let pinned = config.set_value(&Core::LOG_ALL_REF_UPDATES, "true").is_ok()
        && config
            .set_value(&Committer::NAME, REFLOG_COMMITTER_NAME)
            .is_ok()
        && config
            .set_value(&Committer::EMAIL, REFLOG_COMMITTER_EMAIL)
            .is_ok();
    if pinned {
        let _ = config.commit();
    }
}

knot_types::scalar_newtype! {
    struct RefEpoch(u64);
    struct RefGeneration(u64);
}

struct RefState {
    lock: Mutex<()>,
    generation: AtomicU64,
    epoch: RefEpoch,
}

type RefRegistry = Mutex<HashMap<PathBuf, Arc<RefState>>>;

fn ref_registry() -> &'static RefRegistry {
    static REGISTRY: OnceLock<RefRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn next_epoch() -> RefEpoch {
    static EPOCH: AtomicU64 = AtomicU64::new(0);
    RefEpoch::new(EPOCH.fetch_add(1, Ordering::Relaxed))
}

fn ref_state(git_dir: &Path) -> Arc<RefState> {
    let mut states = ref_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Arc::clone(states.entry(git_dir.to_path_buf()).or_insert_with(|| {
        Arc::new(RefState {
            lock: Mutex::new(()),
            generation: AtomicU64::new(0),
            epoch: next_epoch(),
        })
    }))
}

fn forget_ref_state(git_dir: &Path) {
    ref_registry()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .remove(git_dir);
}

type AdvertCache = Moka<(RefEpoch, RefGeneration), Arc<Vec<RefRecord>>>;

fn advert_cache() -> &'static Arc<AdvertCache> {
    static CACHE: OnceLock<Arc<AdvertCache>> = OnceLock::new();
    CACHE.get_or_init(|| {
        let cache = Arc::new(Moka::by_weight(
            Weight::new(knot_resource::advert_cache_bytes()),
            |refs: &Arc<Vec<RefRecord>>| {
                Weight::new(
                    (refs.len() as u64)
                        .max(1)
                        .saturating_mul(ADVERT_BYTES_PER_REF),
                )
            },
        ));
        knot_cache::register(&cache);
        cache
    })
}

fn safe_component(part: &str) -> bool {
    !matches!(part, "." | "..") && !part.contains(['/', '\\', '\0'])
}

pub fn repo_shard(did: &RepoDid) -> Result<PathBuf, GitError> {
    shard_components(did.as_str())
}

pub fn knot_shard(knot: &KnotId) -> Result<PathBuf, GitError> {
    shard_components(knot.as_str())
}

fn shard_components(did: &str) -> Result<PathBuf, GitError> {
    let mut parts = did.splitn(3, ':');
    parts.next();
    let method = parts.next().unwrap_or("did");
    let msid = parts.next().unwrap_or_default();
    let split = msid
        .char_indices()
        .nth(2)
        .map(|(index, _)| index)
        .unwrap_or(msid.len());
    let (shard, remainder) = msid.split_at(split);
    if [method, shard, remainder]
        .iter()
        .any(|part| !safe_component(part))
    {
        return Err(GitError::UnsafeRepoDid(did.to_string()));
    }
    Ok(PathBuf::from(method).join(shard).join(remainder))
}

#[derive(Debug, Clone)]
pub struct Layout {
    scan_path: PathBuf,
    head: RefName,
    reserved: Option<PathBuf>,
    object_format: ObjectFormat,
}

fn default_head() -> RefName {
    RefName::new(format!("{HEADS_PREFIX}main")).expect("refs/heads/main is valid ref name")
}

impl Layout {
    pub fn new(scan_path: impl Into<PathBuf>) -> Self {
        Self {
            scan_path: scan_path.into(),
            head: default_head(),
            reserved: None,
            object_format: ObjectFormat::default(),
        }
    }

    pub fn with_default_branch(mut self, branch: BranchName) -> Self {
        self.head = branch.head_ref();
        self
    }

    pub fn with_object_format(mut self, object_format: ObjectFormat) -> Self {
        self.object_format = object_format;
        self
    }

    pub fn reserving_meta(mut self, knot: &KnotId) -> Result<Self, GitError> {
        let reserved = self.meta_path(knot)?;
        self.reserved = Some(reserved);
        Ok(self)
    }

    pub fn repo_path(&self, did: &RepoDid) -> Result<PathBuf, GitError> {
        Ok(self.scan_path.join(shard_components(did.as_str())?))
    }

    pub fn scratch_dir(&self) -> &Path {
        &self.scan_path
    }

    pub fn meta_path(&self, knot: &KnotId) -> Result<PathBuf, GitError> {
        Ok(self.scan_path.join(shard_components(knot.as_str())?))
    }

    pub fn guarded_path(&self, did: &RepoDid) -> Result<PathBuf, GitError> {
        let path = self.repo_path(did)?;
        match &self.reserved {
            Some(reserved) if *reserved == path => {
                Err(GitError::ReservedDid(did.as_str().to_string()))
            }
            _ => Ok(path),
        }
    }

    pub fn open(&self, did: &RepoDid) -> Result<Repo, GitError> {
        Repo::open(self.guarded_path(did)?)
    }

    pub fn create(&self, did: &RepoDid) -> Result<Repo, GitError> {
        self.init_repo(self.guarded_path(did)?)
    }

    pub fn remove(&self, did: &RepoDid) -> Result<(), GitError> {
        let path = self.guarded_path(did)?;
        if let Ok(repo) = Repo::open(&path) {
            forget_ref_state(repo.git.git_dir());
        }
        match std::fs::remove_dir_all(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(GitError::Remove {
                path,
                message: error.to_string(),
            }),
        }
    }

    pub fn bootstrap_meta(&self, knot: &KnotId) -> Result<Repo, GitError> {
        init_bare_idempotent(self.meta_path(knot)?)
    }

    fn init_repo(&self, path: PathBuf) -> Result<Repo, GitError> {
        let repo = Repo::create_with_format(path, self.object_format)?;
        repo.set_head(&self.head)?;
        Ok(repo)
    }
}

pub(crate) fn init_bare_with_format(
    path: &Path,
    format: ObjectFormat,
) -> Result<gix::Repository, String> {
    let object_hash = (format != ObjectFormat::SHA1).then(|| format.kind());
    gix::ThreadSafeRepository::init_opts(
        path,
        gix::create::Kind::Bare,
        gix::create::Options {
            object_hash,
            ..Default::default()
        },
        gix::open::Options::default(),
    )
    .map(Into::into)
    .map_err(|error| error.to_string())
}

fn staging_path(parent: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nonce = COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!(".knot-staging.{}.{}", std::process::id(), nonce))
}

fn init_bare_idempotent(path: PathBuf) -> Result<Repo, GitError> {
    if let Ok(git) = gix::open(&path) {
        return Ok(assembled(git, path));
    }
    let parent = path.parent().ok_or_else(|| GitError::Create {
        path: path.clone(),
        message: "meta path has no parent directory".to_string(),
    })?;
    std::fs::create_dir_all(parent).map_err(|error| GitError::Create {
        path: path.clone(),
        message: error.to_string(),
    })?;
    let staging = staging_path(parent);
    let _ = std::fs::remove_dir_all(&staging);
    gix::init_bare(&staging).map_err(|error| GitError::Create {
        path: staging.clone(),
        message: error.to_string(),
    })?;
    match std::fs::rename(&staging, &path) {
        Ok(()) => Repo::open(path),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&staging);
            Repo::open(path)
        }
    }
}

pub struct Repo {
    git: gix::Repository,
    path: PathBuf,
    commit_graph: OnceLock<Option<gix::commitgraph::Graph>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefRecord {
    pub name: RefName,
    pub target: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackHash(String);

impl PackHash {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        (matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
            .then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackfileUrl(String);

impl PackfileUrl {
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        let authority = value
            .strip_prefix("https://")
            .or_else(|| value.strip_prefix("http://"))
            .filter(|rest| !rest.is_empty() && !rest.starts_with('/'));
        (authority.is_some() && !value.chars().any(|c| c.is_whitespace() || c.is_control()))
            .then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackfileUri {
    pub oid: Oid,
    pub pack_hash: PackHash,
    pub uri: PackfileUrl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeadRef {
    pub name: RefName,
    pub target: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogUpdate {
    pub name: RefName,
    pub old: Option<Oid>,
    pub new: Oid,
    pub seconds: UnixSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefUpdate {
    Create { name: RefName, new: Oid },
    Update { name: RefName, old: Oid, new: Oid },
    Delete { name: RefName, old: Oid },
}

impl RefUpdate {
    pub fn name(&self) -> &RefName {
        match self {
            RefUpdate::Create { name, .. }
            | RefUpdate::Update { name, .. }
            | RefUpdate::Delete { name, .. } => name,
        }
    }

    pub fn transition(&self) -> RefTransition {
        match self {
            RefUpdate::Create { new, .. } => RefTransition::Create { new: *new },
            RefUpdate::Update { old, new, .. } => RefTransition::Advance {
                old: *old,
                new: *new,
            },
            RefUpdate::Delete { old, .. } => RefTransition::Delete { old: *old },
        }
    }
}

pub fn is_reserved(name: &RefName) -> bool {
    screens_reserved(name.as_str())
}

pub fn screens_reserved(raw: &str) -> bool {
    raw.starts_with(RESERVED_PREFIX) || raw.starts_with(CHECKPOINT_PREFIX)
}

fn is_hidden(name: &RefName) -> bool {
    name.as_str().starts_with(HIDDEN_PREFIX)
}

pub fn is_branch(name: &RefName) -> bool {
    name.as_str().starts_with(HEADS_PREFIX)
}

pub fn is_public_ref(name: &RefName) -> bool {
    !is_reserved(name) && !is_hidden(name)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdvertScope {
    Upload,
    Receive,
}

impl AdvertScope {
    fn config_key(self) -> &'static str {
        match self {
            AdvertScope::Upload => "uploadpack.hideRefs",
            AdvertScope::Receive => "receive.hideRefs",
        }
    }
}

fn ref_hidden_by(name: &RefName, patterns: &[String]) -> bool {
    patterns.iter().any(|pattern| {
        name.as_str() == pattern || name.as_str().starts_with(&format!("{pattern}/"))
    })
}

pub(crate) fn fsync_if_present(path: &Path) -> Result<(), GitError> {
    knot_resource::fsync_path(path).map_err(|error| GitError::Fsync {
        path: error.path,
        message: error.source.to_string(),
    })
}

impl Repo {
    pub fn open(path: impl Into<PathBuf>) -> Result<Repo, GitError> {
        let path = path.into();
        let git = gix::open(&path).map_err(|error| GitError::Open {
            path: path.clone(),
            message: error.to_string(),
        })?;
        Ok(assembled(git, path))
    }

    pub fn create(path: impl Into<PathBuf>) -> Result<Repo, GitError> {
        Self::create_with_format(path, ObjectFormat::default())
    }

    pub fn create_with_format(
        path: impl Into<PathBuf>,
        format: ObjectFormat,
    ) -> Result<Repo, GitError> {
        let path = path.into();
        if path.exists() {
            return Err(GitError::AlreadyExists(path));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| GitError::Create {
                path: path.clone(),
                message: error.to_string(),
            })?;
        }
        let git = init_bare_with_format(&path, format).map_err(|message| GitError::Create {
            path: path.clone(),
            message,
        })?;
        Ok(assembled(git, path))
    }

    pub fn git(&self) -> &gix::Repository {
        &self.git
    }

    pub fn object_format(&self) -> ObjectFormat {
        ObjectFormat::from_kind(self.git.object_hash())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn objects_dir(&self) -> PathBuf {
        self.git.git_dir().join("objects")
    }

    pub fn references(&self) -> Result<Vec<RefRecord>, GitError> {
        self.git
            .references()
            .map_err(|error| GitError::Backend(error.to_string()))?
            .all()
            .map_err(|error| GitError::Backend(error.to_string()))?
            .filter_map(|reference| {
                let reference = match reference {
                    Ok(reference) => reference,
                    Err(error) => return Some(Err(GitError::Backend(error.to_string()))),
                };
                let raw = reference.name().as_bstr().to_string();
                let target = Oid::from(self.direct_target(&reference, MAX_SYMREF_DEPTH)?);
                let name = RefName::new(raw).ok()?;
                Some(Ok(RefRecord { name, target }))
            })
            .collect()
    }

    pub fn reflog_updates_since(&self, since_seconds: UnixSeconds) -> Vec<ReflogUpdate> {
        let Ok(references) = self.git.references() else {
            return Vec::new();
        };
        let Ok(all) = references.all() else {
            return Vec::new();
        };
        all.filter_map(Result::ok)
            .filter(|reference| {
                let name = reference.name().as_bstr().to_string();
                name.starts_with(HEADS_PREFIX) || name.starts_with(TAGS_PREFIX)
            })
            .flat_map(|reference| self.ref_reflog_since(&reference, since_seconds))
            .collect()
    }

    fn ref_reflog_since(
        &self,
        reference: &gix::Reference<'_>,
        since_seconds: UnixSeconds,
    ) -> Vec<ReflogUpdate> {
        let Ok(name) = RefName::new(reference.name().as_bstr().to_string()) else {
            return Vec::new();
        };
        let mut platform = reference.log_iter();
        let Ok(Some(reverse)) = platform.rev() else {
            return Vec::new();
        };
        reverse
            .filter_map(Result::ok)
            .take_while(|line| UnixSeconds::new(line.signature.time.seconds) >= since_seconds)
            .filter_map(|line| {
                (!line.new_oid.is_null()).then(|| ReflogUpdate {
                    name: name.clone(),
                    old: Some(line.previous_oid)
                        .filter(|previous| !previous.is_null())
                        .map(Oid::from),
                    new: Oid::from(line.new_oid),
                    seconds: UnixSeconds::new(line.signature.time.seconds),
                })
            })
            .collect()
    }

    pub fn find_ref(&self, name: &RefName) -> Result<Option<Oid>, GitError> {
        match self
            .git
            .try_find_reference(name.as_str())
            .map_err(|error| GitError::Backend(error.to_string()))?
        {
            Some(reference) => Ok(self
                .direct_target(&reference, MAX_SYMREF_DEPTH)
                .map(Oid::from)),
            None => Ok(None),
        }
    }

    fn direct_target(&self, reference: &gix::Reference<'_>, depth: usize) -> Option<gix::ObjectId> {
        match (depth, reference.follow()) {
            (_, None) => reference.try_id().map(|id| id.detach()),
            (0, Some(_)) => None,
            (_, Some(Ok(next))) => self.direct_target(&next, depth - 1),
            (_, Some(Err(_))) => None,
        }
    }

    pub(crate) fn commit_graph(&self) -> Option<&gix::commitgraph::Graph> {
        self.commit_graph
            .get_or_init(|| self.git.commit_graph().ok())
            .as_ref()
    }

    pub fn with_ref_lock<R>(&self, body: impl FnOnce() -> R) -> R {
        let state = ref_state(self.git.git_dir());
        let _guard = state
            .lock
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        body()
    }

    fn locked_value<R>(&self, body: impl FnOnce() -> R) -> R {
        self.with_ref_lock(|| {
            let outcome = body();
            let state = ref_state(self.git.git_dir());
            let previous = RefGeneration::new(state.generation.fetch_add(1, Ordering::SeqCst));
            advert_cache().invalidate(&(state.epoch, previous));
            outcome
        })
    }

    pub(crate) fn locked<R>(
        &self,
        body: impl FnOnce() -> Result<R, GitError>,
    ) -> Result<R, GitError> {
        self.locked_value(body)
    }

    pub fn with_ref_txn<R>(&self, body: impl FnOnce(&RefTxn<'_>) -> R) -> R {
        self.locked_value(|| body(&RefTxn { repo: self }))
    }

    pub fn set_head(&self, target: &RefName) -> Result<(), GitError> {
        self.locked(|| self.set_head_locked(target))
    }

    pub fn set_head_sealed<R>(
        &self,
        target: &RefName,
        seal: impl FnOnce() -> R,
    ) -> Result<R, GitError> {
        self.locked(|| {
            self.set_head_locked(target)?;
            Ok(seal())
        })
    }

    fn set_head_locked(&self, target: &RefName) -> Result<(), GitError> {
        let raw = target.as_str();
        let target_name = FullName::try_from(raw).map_err(|error| GitError::Reference {
            name: raw.to_string(),
            message: error.to_string(),
        })?;
        let edit = RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "knot set HEAD".into(),
                },
                expected: PreviousValue::Any,
                new: Target::Symbolic(target_name),
            },
            name: FullName::try_from("HEAD").map_err(|error| GitError::Reference {
                name: "HEAD".to_string(),
                message: error.to_string(),
            })?,
            deref: false,
        };
        self.git
            .edit_reference(edit)
            .map_err(|error| GitError::Reference {
                name: "HEAD".to_string(),
                message: error.to_string(),
            })?;
        let git_dir = self.git.git_dir();
        fsync_if_present(&git_dir.join("HEAD"))?;
        fsync_if_present(git_dir)
    }

    fn persist_refs<'a>(
        &self,
        mut names: impl Iterator<Item = &'a RefName>,
    ) -> Result<(), GitError> {
        let git_dir = self.git.git_dir();
        let mut dirs = BTreeSet::from([git_dir.to_path_buf()]);
        names.try_for_each(|name| -> Result<(), GitError> {
            let ref_path = git_dir.join(name.as_str());
            fsync_if_present(&ref_path)?;
            std::iter::successors(ref_path.parent(), |path| path.parent())
                .take_while(|path| path.starts_with(git_dir))
                .for_each(|path| {
                    dirs.insert(path.to_path_buf());
                });
            Ok(())
        })?;
        fsync_if_present(&git_dir.join("packed-refs"))?;
        dirs.iter().try_for_each(|dir| fsync_if_present(dir))
    }

    pub fn origin_url(&self) -> Option<String> {
        self.git
            .config_snapshot()
            .string("remote.origin.url")
            .map(|value| value.to_string())
    }

    pub fn set_origin_url(&self, url: &str) -> Result<(), GitError> {
        let path = self.git.git_dir().join("config");
        let report = |message: String| GitError::Config {
            path: path.clone(),
            message,
        };
        let mut file =
            gix::config::File::from_path_no_includes(path.clone(), gix::config::Source::Local)
                .map_err(|error| report(error.to_string()))?;
        file.set_raw_value_by(
            "remote",
            Some(gix::bstr::BStr::new("origin")),
            "url",
            gix::bstr::BStr::new(url),
        )
        .map_err(|error| report(error.to_string()))?;
        knot_resource::atomic_write(&path, knot_resource::FileMode::Inherited, |out| {
            file.write_to(out)
                .map_err(|error| report(error.to_string()))
        })?;
        fsync_if_present(self.git.git_dir())
    }

    pub fn branches(&self) -> Result<Vec<RefRecord>, GitError> {
        self.references().map(|records| {
            records
                .into_iter()
                .filter(|record| record.name.as_str().starts_with(HEADS_PREFIX))
                .collect()
        })
    }

    pub fn tags(&self) -> Result<Vec<RefRecord>, GitError> {
        self.references().map(|records| {
            records
                .into_iter()
                .filter(|record| record.name.as_str().starts_with(TAGS_PREFIX))
                .collect()
        })
    }

    pub fn advertised_refs(&self) -> Result<Arc<Vec<RefRecord>>, GitError> {
        let state = ref_state(self.git.git_dir());
        let key = (
            state.epoch,
            RefGeneration::new(state.generation.load(Ordering::SeqCst)),
        );
        advert_cache()
            .get_or_try_insert_with(key, || self.public_refs().map(Arc::new))
            .map_err(|error: Arc<GitError>| GitError::Backend(error.to_string()))
    }

    fn public_refs(&self) -> Result<Vec<RefRecord>, GitError> {
        self.references().map(|records| {
            records
                .into_iter()
                .filter(|record| is_public_ref(&record.name))
                .collect()
        })
    }

    pub fn advertised_refs_for(&self, scope: AdvertScope) -> Result<Vec<RefRecord>, GitError> {
        let patterns = self.hidden_ref_patterns(scope);
        let base = self.advertised_refs()?;
        if patterns.is_empty() {
            return Ok(base.to_vec());
        }
        Ok(base
            .iter()
            .filter(|record| !ref_hidden_by(&record.name, &patterns))
            .cloned()
            .collect())
    }

    pub fn blob_packfile_uris(&self) -> Vec<PackfileUri> {
        let snapshot = self.git.config_snapshot();
        snapshot
            .strings("uploadpack.blobPackfileUri")
            .into_iter()
            .flatten()
            .filter_map(|value| {
                let text = value.to_string();
                let mut parts = text.split_whitespace();
                let oid = Oid::from_hex(parts.next()?).ok()?;
                let pack_hash = PackHash::new(parts.next()?)?;
                let uri = PackfileUrl::new(parts.next()?)?;
                Some(PackfileUri {
                    oid,
                    pack_hash,
                    uri,
                })
            })
            .collect()
    }

    fn hidden_ref_patterns(&self, scope: AdvertScope) -> Vec<String> {
        let snapshot = self.git.config_snapshot();
        ["transfer.hideRefs", scope.config_key()]
            .into_iter()
            .filter_map(|key| snapshot.strings(key))
            .flatten()
            .map(|value| value.to_string())
            .collect()
    }

    pub fn head(&self) -> Option<HeadRef> {
        let target = self.git.head_id().ok()?.detach();
        let raw = self.git.head_name().ok()??.as_bstr().to_string();
        let name = RefName::new(raw).ok()?;
        Some(HeadRef {
            name,
            target: Oid::from(target),
        })
    }

    pub fn default_branch(&self) -> Option<RefName> {
        let raw = self.git.head_name().ok()??.as_bstr().to_string();
        RefName::new(raw).ok()
    }

    pub fn contains(&self, oid: Oid) -> bool {
        self.git.has_object(oid.object_id())
    }

    pub fn is_shallow(&self) -> bool {
        self.git.is_shallow()
    }

    pub(crate) fn shallow_grafts(&self) -> Result<HashSet<gix::ObjectId>, GitError> {
        Ok(self
            .git
            .shallow_commits()
            .map_err(|error| GitError::Decode(format!("shallow file: {error}")))?
            .map(|commits| commits.iter().copied().collect())
            .unwrap_or_default())
    }

    pub fn rev_walk(&self, wants: Wants, haves: Haves) -> Result<Vec<Oid>, GitError> {
        let mut walked = Walked::new(PackBudget::unbounded());
        self.rev_walk_each(wants, haves, &mut walked)
    }

    pub(crate) fn rev_walk_each(
        &self,
        wants: Wants<'_>,
        haves: Haves<'_>,
        walked: &mut Walked,
    ) -> Result<Vec<Oid>, GitError> {
        let present: Vec<gix::ObjectId> = haves
            .as_slice()
            .iter()
            .copied()
            .filter(|oid| self.contains(*oid))
            .map(Oid::object_id)
            .collect();
        let mut probe = *walked;
        let collected = self
            .git
            .rev_walk(wants.as_slice().iter().copied().map(Oid::object_id))
            .with_hidden(present.iter().copied())
            .all()
            .ok()
            .and_then(|walk| {
                walk.map(|info| {
                    probe.tick()?;
                    info.map(|info| Oid::from(info.id))
                        .map_err(|error| GitError::RevWalk(error.to_string()))
                })
                .collect::<Result<Vec<Oid>, _>>()
                .ok()
            });
        match collected {
            Some(commits) => {
                *walked = probe;
                Ok(commits)
            }
            None => self.rev_walk_lenient(wants.as_slice(), &present, walked),
        }
    }

    fn rev_walk_lenient(
        &self,
        wants: &[Oid],
        hidden_tips: &[gix::ObjectId],
        walked: &mut Walked,
    ) -> Result<Vec<Oid>, GitError> {
        let mut hidden: HashSet<gix::ObjectId> = HashSet::new();
        let mut stack = hidden_tips.to_vec();
        while let Some(oid) = stack.pop() {
            if hidden.insert(oid)
                && let Ok((_, parents)) = self.commit_tree_and_parents(Oid::from(oid))
            {
                stack.extend(parents);
            }
        }
        let mut visited: HashSet<gix::ObjectId> = HashSet::new();
        let mut commits = Vec::new();
        let mut stack: Vec<gix::ObjectId> = wants.iter().copied().map(Oid::object_id).collect();
        while let Some(oid) = stack.pop() {
            if hidden.contains(&oid) || !visited.insert(oid) {
                continue;
            }
            walked.tick()?;
            let (_, parents) = self.commit_tree_and_parents(Oid::from(oid))?;
            commits.push(Oid::from(oid));
            stack.extend(parents);
        }
        Ok(commits)
    }

    fn ref_edit(update: &RefUpdate, via_head: bool) -> Result<RefEdit, GitError> {
        let edited = if via_head {
            "HEAD"
        } else {
            update.name().as_str()
        };
        let name = FullName::try_from(edited).map_err(|error| GitError::Reference {
            name: edited.to_string(),
            message: error.to_string(),
        })?;
        let log = LogChange {
            mode: RefLog::AndReference,
            force_create_reflog: true,
            message: "knot ref update".into(),
        };
        let change = match update {
            RefUpdate::Create { new, .. } => Change::Update {
                log,
                expected: PreviousValue::MustNotExist,
                new: Target::Object(new.object_id()),
            },
            RefUpdate::Update { old, new, .. } => Change::Update {
                log,
                expected: PreviousValue::MustExistAndMatch(Target::Object(old.object_id())),
                new: Target::Object(new.object_id()),
            },
            RefUpdate::Delete { old, .. } => Change::Delete {
                expected: PreviousValue::MustExistAndMatch(Target::Object(old.object_id())),
                log: RefLog::AndReference,
            },
        };
        Ok(RefEdit {
            change,
            name,
            deref: via_head,
        })
    }

    fn updates_head_branch(&self, update: &RefUpdate) -> bool {
        !matches!(update, RefUpdate::Delete { .. })
            && self
                .default_branch()
                .is_some_and(|head| head.as_str() == update.name().as_str())
    }

    pub fn update_ref(&self, update: &RefUpdate) -> Result<(), GitError> {
        self.locked(|| self.update_ref_locked(update))
    }

    pub fn update_ref_sealed<R>(
        &self,
        update: &RefUpdate,
        seal: impl FnOnce() -> R,
    ) -> Result<R, GitError> {
        self.locked(|| {
            self.update_ref_locked(update)?;
            Ok(seal())
        })
    }

    fn update_ref_locked(&self, update: &RefUpdate) -> Result<(), GitError> {
        let raw = update.name().as_str().to_string();
        let via_head = self.updates_head_branch(update);
        self.git
            .edit_reference(Self::ref_edit(update, via_head)?)
            .map_err(|error| GitError::Reference {
                name: raw,
                message: error.to_string(),
            })?;
        self.persist_refs(std::iter::once(update.name()))
    }

    pub fn update_refs(&self, updates: &[RefUpdate]) -> Result<(), GitError> {
        self.locked(|| self.update_refs_locked(updates))
    }

    pub fn update_refs_sealed<R>(
        &self,
        updates: &[RefUpdate],
        seal: impl FnOnce() -> R,
    ) -> Result<R, GitError> {
        self.locked(|| {
            self.update_refs_locked(updates)?;
            Ok(seal())
        })
    }

    fn reject_df_conflicts(&self, updates: &[RefUpdate]) -> Result<(), GitError> {
        let creates: Vec<&str> = updates
            .iter()
            .filter_map(|update| match update {
                RefUpdate::Create { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        if creates.is_empty() {
            return Ok(());
        }
        let deletes: HashSet<&str> = updates
            .iter()
            .filter_map(|update| match update {
                RefUpdate::Delete { name, .. } => Some(name.as_str()),
                _ => None,
            })
            .collect();
        let names: Vec<String> = self
            .references()?
            .iter()
            .map(|record| record.name.as_str().to_string())
            .filter(|name| !deletes.contains(name.as_str()))
            .chain(creates.iter().copied().map(str::to_string))
            .collect();
        let name_set: HashSet<&str> = names.iter().map(String::as_str).collect();
        names
            .iter()
            .find_map(|name| {
                name.match_indices('/')
                    .map(|(at, _)| &name[..at])
                    .find(|ancestor| name_set.contains(ancestor))
                    .map(|ancestor| (ancestor.to_string(), name.clone()))
            })
            .map_or(Ok(()), |(directory, leaf)| {
                Err(GitError::AtomicRefs(format!(
                    "d/f conflict: {directory} blocks {leaf}"
                )))
            })
    }

    fn update_refs_locked(&self, updates: &[RefUpdate]) -> Result<(), GitError> {
        self.reject_df_conflicts(updates)?;
        let edits = updates
            .iter()
            .map(|update| {
                let via_head = self.updates_head_branch(update);
                Self::ref_edit(update, via_head)
            })
            .collect::<Result<Vec<_>, _>>()?;
        self.git
            .edit_references(edits)
            .map_err(|error| GitError::AtomicRefs(error.to_string()))?;
        self.persist_refs(updates.iter().map(RefUpdate::name))
    }
}

pub struct RefTxn<'a> {
    repo: &'a Repo,
}

impl RefTxn<'_> {
    pub fn update_ref(&self, update: &RefUpdate) -> Result<(), GitError> {
        self.repo.update_ref_locked(update)
    }

    pub fn update_refs(&self, updates: &[RefUpdate]) -> Result<(), GitError> {
        self.repo.update_refs_locked(updates)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";

    fn oid(hex: &str) -> Oid {
        Oid::from_hex(hex).unwrap()
    }

    fn head_ref() -> RefName {
        RefName::new("refs/heads/main").unwrap()
    }

    fn repo() -> (tempfile::TempDir, Layout, RepoDid) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let did = RepoDid::new("did:plc:squid").unwrap();
        (dir, layout, did)
    }

    #[test]
    fn layout_paths_shard_and_stay_within_scan() {
        let layout = Layout::new("/srv/git");
        let cases: &[(&str, &str)] = &[
            ("did:plc:squid", "plc/sq/uid"),
            ("did:web:oyster.cafe", "web/oy/ster.cafe"),
            ("did:web:nel.pet", "web/ne/l.pet"),
        ];
        cases.iter().for_each(|&(raw, suffix)| {
            let path = layout.repo_path(&RepoDid::new(raw).unwrap()).unwrap();
            assert!(path.ends_with(suffix), "{path:?} missing shard {suffix}");
            assert!(path.starts_with("/srv/git"), "{path:?} escaped scan path");
        });
    }

    #[test]
    fn dot_only_did_cannot_escape_scan_path() {
        let layout = Layout::new("/srv/git/scan");
        ["did:plc:....", "did:web:....", "did:plc:...", "did:plc:.."]
            .into_iter()
            .map(|raw| RepoDid::new(raw).unwrap())
            .for_each(|did| {
                assert!(
                    matches!(layout.repo_path(&did), Err(GitError::UnsafeRepoDid(_))),
                    "dot-only method-specific-id must be refused, never resolved to path"
                );
                assert!(matches!(layout.open(&did), Err(GitError::UnsafeRepoDid(_))));
                assert!(matches!(
                    layout.create(&did),
                    Err(GitError::UnsafeRepoDid(_))
                ));
            });

        let real = RepoDid::new("did:web:oyster.cafe").unwrap();
        assert!(
            layout.repo_path(&real).is_ok(),
            "legitimate did:web with dots in its domain must still resolve"
        );
    }

    #[test]
    fn meta_repo_path_is_sharded_and_never_collides() {
        let layout = Layout::new("/srv/git");
        let knot = KnotId::new("did:web:oyster.cafe").unwrap();
        let meta = layout.meta_path(&knot).unwrap();
        assert!(meta.ends_with("web/oy/ster.cafe"));
        ["did:plc:squid", "did:web:nel.pet"]
            .into_iter()
            .map(|raw| RepoDid::new(raw).unwrap())
            .for_each(|did| {
                assert_ne!(layout.repo_path(&did).unwrap(), meta);
            });
    }

    #[test]
    fn bootstrap_meta_creates_then_opens_idempotently() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let knot = KnotId::new("did:web:oyster.cafe").unwrap();

        let created = layout.bootstrap_meta(&knot).unwrap();
        assert!(created.references().unwrap().is_empty());
        assert_eq!(created.path(), layout.meta_path(&knot).unwrap());

        let reopened = layout.bootstrap_meta(&knot).unwrap();
        assert_eq!(reopened.path(), layout.meta_path(&knot).unwrap());
    }

    #[test]
    fn concurrent_bootstrap_meta_converges_for_every_caller() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let knot = KnotId::new("did:web:oyster.cafe").unwrap();
        let meta = layout.meta_path(&knot).unwrap();

        let paths = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let layout = layout.clone();
                    let knot = knot.clone();
                    scope.spawn(move || {
                        layout
                            .bootstrap_meta(&knot)
                            .map(|repo| repo.path().to_path_buf())
                    })
                })
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert!(
            paths
                .iter()
                .all(|outcome| matches!(outcome, Ok(path) if path == &meta)),
            "every racing bootstrap must converge on one meta repo, not fail: {paths:?}"
        );
        let reopened = layout.bootstrap_meta(&knot).unwrap();
        assert!(reopened.references().unwrap().is_empty());
    }

    #[test]
    fn reserving_meta_refuses_the_knot_did_for_open_and_create() {
        let dir = tempfile::tempdir().unwrap();
        let knot = KnotId::new("did:web:oyster.cafe").unwrap();
        let layout = Layout::new(dir.path()).reserving_meta(&knot).unwrap();
        layout.bootstrap_meta(&knot).unwrap();

        let knot_as_repo = RepoDid::new("did:web:oyster.cafe").unwrap();
        assert!(matches!(
            layout.open(&knot_as_repo),
            Err(GitError::ReservedDid(_))
        ));
        assert!(matches!(
            layout.create(&knot_as_repo),
            Err(GitError::ReservedDid(_))
        ));

        let ordinary = RepoDid::new("did:plc:squid").unwrap();
        assert!(layout.create(&ordinary).is_ok());
        assert!(layout.open(&ordinary).is_ok());
    }

    #[test]
    fn creating_a_bare_repo_is_sha1_and_rejects_a_second_create() {
        let (_dir, layout, did) = repo();

        let repo = layout.create(&did).unwrap();
        assert!(repo.references().unwrap().is_empty());
        assert!(repo.head().is_none());
        assert_eq!(repo.object_format(), ObjectFormat::SHA1);

        assert!(layout.open(&did).unwrap().references().unwrap().is_empty());
        assert!(matches!(
            layout.create(&did),
            Err(GitError::AlreadyExists(_))
        ));
    }

    #[test]
    fn with_ref_txn_holds_the_ref_lock_across_its_whole_body() {
        use std::sync::Mutex;
        use std::sync::mpsc::channel;

        let (_dir, layout, did) = repo();
        let repo = layout.create(&did).unwrap();
        let contender_repo = layout.open(&did).unwrap();

        let order: Mutex<Vec<&str>> = Mutex::new(Vec::new());
        let order_ref = &order;
        let (contending, observed) = channel();

        std::thread::scope(|scope| {
            repo.with_ref_txn(|_txn| {
                order_ref.lock().unwrap().push("txn-enter");
                scope.spawn(move || {
                    contending.send(()).unwrap();
                    contender_repo.with_ref_lock(|| order_ref.lock().unwrap().push("contender"));
                });
                observed.recv().unwrap();
                std::thread::yield_now();
                order_ref.lock().unwrap().push("txn-exit");
            });
        });

        assert_eq!(
            *order.lock().unwrap(),
            ["txn-enter", "txn-exit", "contender"],
            "object migration runs inside the transaction body, so no other ref-lock holder can observe the half-applied push"
        );
    }

    #[test]
    fn create_with_sha256_object_format_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path()).with_object_format(ObjectFormat::SHA256);
        let did = RepoDid::new("did:plc:squid").unwrap();

        let repo = layout.create(&did).unwrap();
        assert_eq!(repo.object_format(), ObjectFormat::SHA256);
        let oid = Oid::from(repo.git().write_blob(b"hello sha256\n").unwrap().detach());
        assert_eq!(
            oid.to_hex().len(),
            64,
            "sha256 repo names objects with 32-byte digests"
        );

        let reopened = layout.open(&did).unwrap();
        assert_eq!(
            reopened.object_format(),
            ObjectFormat::SHA256,
            "object format survives reopen, read from repo config"
        );
    }

    #[test]
    fn compare_and_swap_governs_every_ref_write() {
        let (_dir, layout, did) = repo();
        let repo = layout.create(&did).unwrap();
        let main = head_ref();

        repo.update_ref(&RefUpdate::Create {
            name: main.clone(),
            new: oid(A),
        })
        .unwrap();
        assert!(
            repo.update_ref(&RefUpdate::Create {
                name: main.clone(),
                new: oid(B),
            })
            .is_err()
        );
        assert!(
            repo.update_ref(&RefUpdate::Update {
                name: main.clone(),
                old: oid(B),
                new: oid(A),
            })
            .is_err()
        );
        repo.update_ref(&RefUpdate::Update {
            name: main.clone(),
            old: oid(A),
            new: oid(B),
        })
        .unwrap();
        let refs = repo.references().unwrap();
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, oid(B));

        drop(repo);
        let repo = layout.open(&did).unwrap();
        assert_eq!(
            repo.find_ref(&main).unwrap(),
            Some(oid(B)),
            "update is visible after reopen"
        );

        repo.update_ref(&RefUpdate::Delete {
            name: main.clone(),
            old: oid(B),
        })
        .unwrap();
        assert!(repo.references().unwrap().is_empty());

        let x = RefName::new("refs/heads/x").unwrap();
        let y = RefName::new("refs/heads/y").unwrap();
        repo.update_refs(&[
            RefUpdate::Create {
                name: x.clone(),
                new: oid(A),
            },
            RefUpdate::Create {
                name: y.clone(),
                new: oid(A),
            },
        ])
        .unwrap();
        let result = repo.update_refs(&[
            RefUpdate::Update {
                name: x.clone(),
                old: oid(A),
                new: oid(B),
            },
            RefUpdate::Update {
                name: y.clone(),
                old: oid(B),
                new: oid(A),
            },
        ]);
        assert!(
            result.is_err(),
            "batch with one stale compare-and-swap must fail as a whole"
        );
        assert_eq!(
            repo.find_ref(&x).unwrap(),
            Some(oid(A)),
            "valid edit in failed batch must roll back"
        );
        assert_eq!(repo.find_ref(&y).unwrap(), Some(oid(A)));
    }

    #[test]
    fn ref_writes_leave_a_recoverable_reflog_under_the_knot_identity() {
        let (_dir, layout, did) = repo();
        let repo = layout.create(&did).unwrap();

        let committer = repo
            .git()
            .committer()
            .expect("committer is always pinned so reflog writes never depend on ambient config")
            .expect("pinned committer signature parses");
        assert_eq!(committer.name.to_string(), REFLOG_COMMITTER_NAME);
        assert_eq!(committer.email.to_string(), REFLOG_COMMITTER_EMAIL);

        repo.update_ref(&RefUpdate::Create {
            name: head_ref(),
            new: oid(A),
        })
        .unwrap();
        repo.update_ref(&RefUpdate::Update {
            name: head_ref(),
            old: oid(A),
            new: oid(B),
        })
        .unwrap();
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/tags/v1").unwrap(),
            new: oid(A),
        })
        .unwrap();

        let logs = repo.git().git_dir().join("logs");
        let branch = std::fs::read_to_string(logs.join("refs/heads/main"))
            .expect("branch update must leave reflog so clobbering push is recoverable");
        assert!(
            branch.contains(A) && branch.contains(B) && branch.contains(REFLOG_COMMITTER_NAME),
            "branch reflog records both tips under knot identity:\n{branch}"
        );
        assert!(
            logs.join("refs/tags/v1").exists(),
            "force_create_reflog must log tags too, not just conventional refs/heads set"
        );

        let updates = repo.reflog_updates_since(UnixSeconds::new(0));
        let head_new: Vec<Oid> = updates
            .iter()
            .filter(|update| update.name == head_ref())
            .map(|update| update.new)
            .collect();
        assert!(
            head_new.contains(&oid(A)) && head_new.contains(&oid(B)),
            "both branch tips are recovered from reflog: {head_new:?}"
        );
        let create = updates
            .iter()
            .find(|update| update.name == head_ref() && update.new == oid(A))
            .unwrap();
        assert_eq!(create.old, None, "branch creation has no previous oid");
        let update = updates
            .iter()
            .find(|update| update.name == head_ref() && update.new == oid(B))
            .unwrap();
        assert_eq!(update.old, Some(oid(A)), "branch update records prior oid");
        assert!(
            updates
                .iter()
                .any(|update| update.name.as_str() == "refs/tags/v1" && update.new == oid(A)),
            "tag update is recovered too"
        );
        assert!(
            repo.reflog_updates_since(UnixSeconds::new(i64::MAX))
                .is_empty(),
            "horizon past every entry filters whole reflog out"
        );
    }

    #[test]
    fn advertisement_hides_reserved_refs_and_tracks_each_change() {
        let (_dir, layout, did) = repo();
        let repo = layout.create(&did).unwrap();
        let main = head_ref();
        let feature = RefName::new("refs/heads/feature").unwrap();

        repo.update_ref(&RefUpdate::Create {
            name: main.clone(),
            new: oid(A),
        })
        .unwrap();
        let first = repo.advertised_refs().unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].name.as_str(), "refs/heads/main");
        assert_eq!(
            repo.advertised_refs().unwrap(),
            first,
            "repeated advertisement with no ref change serves same answer"
        );

        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/cobs/sh.tangled.repo.collaborator/limpet").unwrap(),
            new: oid(B),
        })
        .unwrap();
        let advertised = repo.advertised_refs().unwrap();
        assert_eq!(advertised.len(), 1, "cob ref is hidden from advertisement");
        assert_eq!(advertised[0].name.as_str(), "refs/heads/main");
        assert_eq!(repo.references().unwrap().len(), 2);
        assert!(is_reserved(&RefName::new("refs/cobs/x/y").unwrap()));

        repo.update_ref(&RefUpdate::Create {
            name: feature.clone(),
            new: oid(B),
        })
        .unwrap();
        assert_eq!(
            repo.advertised_refs().unwrap().len(),
            2,
            "ref created after advertisement invalidates cached answer"
        );
        repo.update_ref(&RefUpdate::Delete {
            name: feature,
            old: oid(B),
        })
        .unwrap();
        assert_eq!(
            repo.advertised_refs().unwrap().len(),
            1,
            "delete after advertisement invalidates cached answer"
        );

        drop(repo);
        layout.remove(&did).unwrap();
        let repo = layout.create(&did).unwrap();
        repo.update_ref(&RefUpdate::Create {
            name: RefName::new("refs/heads/new").unwrap(),
            new: oid(B),
        })
        .unwrap();
        let advertised = repo.advertised_refs().unwrap();
        assert_eq!(
            advertised.len(),
            1,
            "recreated repo advertises only its own ref"
        );
        assert_eq!(
            advertised[0].name.as_str(),
            "refs/heads/new",
            "deleted repo's cached advertisement mustn't survive recreation at same path"
        );
    }

    #[test]
    fn advertisement_reflects_each_update_under_concurrent_readers() {
        let (_dir, layout, did) = repo();
        let repo = layout.create(&did).unwrap();
        let main = head_ref();
        repo.update_ref(&RefUpdate::Create {
            name: main.clone(),
            new: oid(A),
        })
        .unwrap();
        drop(repo);

        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|scope| {
            (0..4).for_each(|_| {
                scope.spawn(|| {
                    let reader = layout.open(&did).unwrap();
                    while !stop.load(Ordering::Relaxed) {
                        let refs = reader.advertised_refs().unwrap();
                        assert_eq!(refs.len(), 1, "live branch is advertised exactly once");
                        assert!(
                            refs[0].target == oid(A) || refs[0].target == oid(B),
                            "reader must never observe value branch never held"
                        );
                    }
                });
            });

            let writer = layout.open(&did).unwrap();
            (0..64).for_each(|round| {
                let (old, new) = if round % 2 == 0 {
                    (oid(A), oid(B))
                } else {
                    (oid(B), oid(A))
                };
                writer
                    .update_ref(&RefUpdate::Update {
                        name: main.clone(),
                        old,
                        new,
                    })
                    .unwrap();
                assert_eq!(
                    writer.advertised_refs().unwrap()[0].target,
                    new,
                    "advertisement taken after update reflects that update"
                );
            });
            stop.store(true, Ordering::Relaxed);
        });
    }

    #[test]
    fn create_honors_configured_default_branch() {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path()).with_default_branch(BranchName::new("trunk").unwrap());
        let repo = layout
            .create(&RepoDid::new("did:plc:squid").unwrap())
            .unwrap();
        assert_eq!(repo.default_branch().unwrap().as_str(), "refs/heads/trunk");
    }

    #[test]
    fn concurrent_create_has_exactly_one_winner() {
        let (_dir, layout, did) = repo();
        layout.create(&did).unwrap();
        let main = head_ref();

        const C: &str = "3333333333333333333333333333333333333333";
        const D: &str = "4444444444444444444444444444444444444444";
        let winners = std::thread::scope(|scope| {
            let handles = [A, B, C, D].map(|hex| {
                let layout = layout.clone();
                let did = did.clone();
                let main = main.clone();
                scope.spawn(move || {
                    layout
                        .open(&did)
                        .unwrap()
                        .update_ref(&RefUpdate::Create {
                            name: main,
                            new: oid(hex),
                        })
                        .is_ok()
                })
            });
            handles
                .into_iter()
                .map(|handle| handle.join().unwrap())
                .filter(|created| *created)
                .count()
        });

        assert_eq!(winners, 1, "concurrent creates of one ref mustn't both win");
        assert_eq!(layout.open(&did).unwrap().references().unwrap().len(), 1);
    }

    #[test]
    fn symref_cycle_does_not_overflow_references() {
        let (_dir, layout, did) = repo();
        let repo = layout.create(&did).unwrap();
        let symref = |name: &str, target: &str| RefEdit {
            change: Change::Update {
                log: LogChange {
                    mode: RefLog::AndReference,
                    force_create_reflog: false,
                    message: "cycle".into(),
                },
                expected: PreviousValue::Any,
                new: Target::Symbolic(FullName::try_from(target).unwrap()),
            },
            name: FullName::try_from(name).unwrap(),
            deref: false,
        };
        repo.git()
            .edit_reference(symref("refs/cycle/a", "refs/cycle/b"))
            .unwrap();
        repo.git()
            .edit_reference(symref("refs/cycle/b", "refs/cycle/a"))
            .unwrap();

        let refs = repo.references().unwrap();
        assert!(
            refs.iter()
                .all(|record| !record.name.as_str().starts_with("refs/cycle/")),
            "cyclic symref must be skipped, not resolved"
        );
    }
}
