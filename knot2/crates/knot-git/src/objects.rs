use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, Read};
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use knot_types::{AuthorName, Email, ObjectCount, Oid, ParseError, RepoPath, UnixSeconds};

use crate::error::{GitError, SelectionLimit};
use crate::repo::Repo;

// why? idk. should we let this be deeper
pub const MAX_TREE_DEPTH: usize = 1024;
const MAX_TAG_DEPTH: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct Wants<'a>(&'a [Oid]);

#[derive(Debug, Clone, Copy)]
pub struct Haves<'a>(&'a [Oid]);

#[derive(Debug, Clone, Copy)]
pub struct ShallowCommits<'a>(&'a [Oid]);

impl<'a> Wants<'a> {
    pub fn new(oids: &'a [Oid]) -> Self {
        Self(oids)
    }

    pub fn as_slice(self) -> &'a [Oid] {
        self.0
    }
}

impl<'a> Haves<'a> {
    pub fn new(oids: &'a [Oid]) -> Self {
        Self(oids)
    }

    pub fn as_slice(self) -> &'a [Oid] {
        self.0
    }
}

impl<'a> ShallowCommits<'a> {
    pub fn new(oids: &'a [Oid]) -> Self {
        Self(oids)
    }

    pub fn as_slice(self) -> &'a [Oid] {
        self.0
    }
}

enum Peeled {
    Commit(gix::ObjectId),
    Direct(gix::ObjectId),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    pub name: AuthorName,
    pub email: Email,
    pub time: UnixSeconds,
    pub offset_seconds: i32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub id: Oid,
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Identity,
    pub committer: Identity,
    pub message: String,
    pub pgp_signature: Option<String>,
    pub merge_tag: Option<String>,
    pub extra_headers: Vec<(String, Vec<u8>)>,
}

impl Commit {
    pub fn change_id(&self) -> Option<CommitChangeId> {
        self.extra_headers
            .iter()
            .find(|(name, _)| name == "change-id")
            .and_then(|(_, value)| std::str::from_utf8(value).ok())
            .and_then(|value| CommitChangeId::new(value).ok())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommitChangeId(String);

impl CommitChangeId {
    pub fn new(value: impl Into<String>) -> Result<Self, ParseError> {
        let value = value.into();
        let valid =
            !value.is_empty() && value.len() <= 100 && value.chars().all(|c| c.is_ascii_graphic());
        match valid {
            true => Ok(Self(value)),
            false => Err(ParseError::Invalid {
                kind: "commit change-id",
                value,
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for CommitChangeId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.pad(&self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    Tree,
    Blob,
    BlobExecutable,
    Link,
    Commit,
}

impl EntryKind {
    pub fn mode_octal(self) -> &'static str {
        match self {
            EntryKind::Tree => "0040000",
            EntryKind::Blob => "0100644",
            EntryKind::BlobExecutable => "0100755",
            EntryKind::Link => "0120000",
            EntryKind::Commit => "0160000",
        }
    }

    pub fn is_file(self) -> bool {
        matches!(self, EntryKind::Blob | EntryKind::BlobExecutable)
    }

    pub fn from_git_mode(mode: &str) -> Option<EntryKind> {
        match mode.trim() {
            "100644" | "100664" => Some(EntryKind::Blob),
            "100755" => Some(EntryKind::BlobExecutable),
            "120000" => Some(EntryKind::Link),
            "160000" => Some(EntryKind::Commit),
            "040000" | "40000" => Some(EntryKind::Tree),
            _ => None,
        }
    }
}

impl From<EntryKind> for gix::objs::tree::EntryKind {
    fn from(kind: EntryKind) -> Self {
        match kind {
            EntryKind::Tree => Self::Tree,
            EntryKind::Blob => Self::Blob,
            EntryKind::BlobExecutable => Self::BlobExecutable,
            EntryKind::Link => Self::Link,
            EntryKind::Commit => Self::Commit,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeEntry {
    pub name: String,
    pub oid: Oid,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tree {
    pub entries: Vec<TreeEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileChange {
    Added {
        path: RepoPath,
        oid: Oid,
    },
    Deleted {
        path: RepoPath,
        oid: Oid,
    },
    Modified {
        path: RepoPath,
        old: Oid,
        new: Oid,
    },
    Renamed {
        from: RepoPath,
        to: RepoPath,
        old: Oid,
        new: Oid,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Comparison {
    pub commits: Vec<Oid>,
    pub changes: Vec<FileChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CommitRange {
    pub base: Oid,
    pub head: Oid,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct TreeDepth(u32);

impl TreeDepth {
    pub const fn new(depth: u32) -> Self {
        Self(depth)
    }

    pub const fn deeper(self) -> Self {
        Self(self.0.saturating_add(1))
    }

    const fn is_exhausted(self) -> bool {
        self.0 == 0
    }

    const fn shallower(self) -> Self {
        Self(self.0.saturating_sub(1))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    None,
    BlobNone,
    BlobLimit(u64),
    TreeDepth(TreeDepth),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommitDepth(u32);

impl CommitDepth {
    pub const fn new(depth: u32) -> Self {
        Self(depth)
    }

    const fn deeper(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Deepen {
    pub depth: Option<CommitDepth>,
    pub since: Option<UnixSeconds>,
    pub not: Vec<Oid>,
    pub relative: bool,
}

impl Deepen {
    pub fn is_shallow_request(&self) -> bool {
        self.depth.is_some() || self.since.is_some() || !self.not.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShallowPlan {
    pub commits: Vec<Oid>,
    pub shallow: Vec<Oid>,
    pub unshallow: Vec<Oid>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackSelection {
    pub send: Vec<Oid>,
    pub client_has: HashSet<Oid>,
}

#[derive(Debug, Clone, Copy)]
pub struct PackBudget {
    max_objects: ObjectCount,
    stall: Option<Duration>,
}

impl PackBudget {
    pub fn new(max_objects: ObjectCount, stall: Duration) -> Self {
        Self {
            max_objects,
            stall: Some(stall),
        }
    }

    pub fn unbounded() -> Self {
        Self {
            max_objects: ObjectCount::new(usize::MAX),
            stall: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Walked {
    budget: PackBudget,
    count: usize,
    deadline: Option<Instant>,
}

impl Walked {
    pub(crate) fn new(budget: PackBudget) -> Self {
        Self {
            budget,
            count: 0,
            deadline: budget.stall.map(|stall| Instant::now() + stall),
        }
    }

    pub(crate) fn tick(&mut self) -> Result<(), GitError> {
        self.count += 1;
        if self.count > self.budget.max_objects.get() {
            return Err(GitError::Selection(SelectionLimit::Objects));
        }
        advance_stall(&mut self.deadline, self.budget.stall)
    }
}

fn advance_stall(deadline: &mut Option<Instant>, stall: Option<Duration>) -> Result<(), GitError> {
    if let (Some(deadline), Some(stall)) = (deadline.as_mut(), stall) {
        let now = Instant::now();
        if now >= *deadline {
            return Err(GitError::Selection(SelectionLimit::Time));
        }
        *deadline = now + stall;
    }
    Ok(())
}

const MAX_LOOSE_HEADER: usize = 64;

const PARALLEL_SELECT_MIN: usize = 4096;

struct SharedWalk<'a> {
    counter: &'a AtomicUsize,
    max_objects: usize,
    stall: Option<Duration>,
    deadline: Option<Instant>,
}

impl<'a> SharedWalk<'a> {
    fn new(counter: &'a AtomicUsize, budget: &PackBudget) -> Self {
        Self {
            counter,
            max_objects: budget.max_objects.get(),
            stall: budget.stall,
            deadline: budget.stall.map(|stall| Instant::now() + stall),
        }
    }

    fn tick(&mut self) -> Result<(), GitError> {
        let count = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
        if count > self.max_objects {
            return Err(GitError::Selection(SelectionLimit::Objects));
        }
        advance_stall(&mut self.deadline, self.stall)
    }
}

pub enum BlobReader {
    Loose(std::io::BufReader<flate2::read::ZlibDecoder<std::fs::File>>),
    Packed(std::io::Cursor<Vec<u8>>),
}

impl Read for BlobReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            BlobReader::Loose(reader) => reader.read(buf),
            BlobReader::Packed(reader) => reader.read(buf),
        }
    }
}

fn skip_loose_header(reader: &mut impl std::io::BufRead, oid: Oid) -> Result<(), GitError> {
    let corrupt = |message: String| GitError::Corrupt { oid, message };
    let mut header = Vec::with_capacity(MAX_LOOSE_HEADER);
    reader
        .take(MAX_LOOSE_HEADER as u64)
        .read_until(0, &mut header)
        .map_err(|error| corrupt(error.to_string()))?;
    if header.last() != Some(&0) || !header.starts_with(b"blob ") {
        return Err(corrupt("loose object header isn't blob".to_string()));
    }
    Ok(())
}

pub(crate) fn identity(signature: gix::actor::SignatureRef<'_>) -> Result<Identity, GitError> {
    let time = signature
        .time()
        .map_err(|error| GitError::Decode(error.to_string()))?;
    Ok(Identity {
        name: AuthorName::new(signature.name.to_string()),
        email: Email::new(signature.email.to_string()),
        time: UnixSeconds::new(time.seconds),
        offset_seconds: time.offset,
    })
}

pub(crate) fn signature(identity: &Identity) -> gix::actor::Signature {
    let clean = |raw: &str| raw.replace(['<', '>'], "").trim().to_string();
    gix::actor::Signature {
        name: clean(identity.name.as_str()).into(),
        email: clean(identity.email.as_str()).into(),
        time: gix::date::Time {
            seconds: identity.time.get(),
            offset: identity.offset_seconds,
        },
    }
}

pub(crate) fn map_kind(kind: gix::objs::tree::EntryKind) -> EntryKind {
    use gix::objs::tree::EntryKind as Source;
    match kind {
        Source::Tree => EntryKind::Tree,
        Source::Blob => EntryKind::Blob,
        Source::BlobExecutable => EntryKind::BlobExecutable,
        Source::Link => EntryKind::Link,
        Source::Commit => EntryKind::Commit,
    }
}

impl Repo {
    fn load_object(&self, oid: Oid) -> Result<gix::Object<'_>, GitError> {
        #[cfg(feature = "instrument")]
        crate::instrument::record_read();
        match self.git().try_find_object(oid.object_id()) {
            Ok(Some(object)) => Ok(object),
            Ok(None) => Err(GitError::ObjectNotFound(oid)),
            Err(error) => Err(GitError::Corrupt {
                oid,
                message: error.to_string(),
            }),
        }
    }

    pub fn find_commit(&self, oid: Oid) -> Result<Commit, GitError> {
        let object = self.load_object(oid)?;
        let commit = object.try_into_commit().map_err(|_| GitError::ObjectType {
            oid,
            expected: "commit",
        })?;
        let tree = Oid::from(
            commit
                .tree_id()
                .map_err(|error| GitError::Decode(error.to_string()))?
                .detach(),
        );
        let parents = commit
            .parent_ids()
            .map(|id| Oid::from(id.detach()))
            .collect();
        let author = identity(
            commit
                .author()
                .map_err(|error| GitError::Decode(error.to_string()))?,
        )?;
        let committer = identity(
            commit
                .committer()
                .map_err(|error| GitError::Decode(error.to_string()))?,
        )?;
        let message = commit
            .message_raw()
            .map_err(|error| GitError::Decode(error.to_string()))?
            .to_string();
        let decoded = commit
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        let (mut pgp_signature, mut merge_tag) = (None, None);
        let extra_headers = decoded
            .extra_headers
            .iter()
            .filter_map(|(name, value)| match name.to_string().as_str() {
                "gpgsig" => {
                    pgp_signature = Some(value.to_string());
                    None
                }
                "mergetag" => {
                    merge_tag = Some(value.to_string());
                    None
                }
                other => Some((other.to_string(), value.to_vec())),
            })
            .collect();
        Ok(Commit {
            id: oid,
            tree,
            parents,
            author,
            committer,
            message,
            pgp_signature,
            merge_tag,
            extra_headers,
        })
    }

    pub fn find_tree(&self, oid: Oid) -> Result<Tree, GitError> {
        let object = self.load_object(oid)?;
        let tree = object.try_into_tree().map_err(|_| GitError::ObjectType {
            oid,
            expected: "tree",
        })?;
        let decoded = tree
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        let entries = decoded
            .entries
            .iter()
            .map(|entry| TreeEntry {
                name: entry.filename.to_string(),
                oid: Oid::from(entry.oid.to_owned()),
                kind: map_kind(entry.mode.kind()),
            })
            .collect();
        Ok(Tree { entries })
    }

    pub fn blob_size(&self, oid: Oid) -> Result<u64, GitError> {
        match self.git().try_find_header(oid.object_id()) {
            Ok(Some(header)) if header.kind() == gix::object::Kind::Blob => Ok(header.size()),
            Ok(Some(_)) => Err(GitError::ObjectType {
                oid,
                expected: "blob",
            }),
            Ok(None) => Err(GitError::ObjectNotFound(oid)),
            Err(error) => Err(GitError::Corrupt {
                oid,
                message: error.to_string(),
            }),
        }
    }

    pub fn read_blob(&self, oid: Oid) -> Result<Vec<u8>, GitError> {
        let object = self.load_object(oid)?;
        let mut blob = object.try_into_blob().map_err(|_| GitError::ObjectType {
            oid,
            expected: "blob",
        })?;
        Ok(blob.take_data())
    }

    fn loose_object_path(&self, oid: Oid) -> PathBuf {
        let hex = oid.to_hex();
        let (shard, rest) = hex.split_at(2);
        self.git().git_dir().join("objects").join(shard).join(rest)
    }

    pub fn remove_loose_object(&self, oid: Oid) -> Result<(), GitError> {
        match std::fs::remove_file(self.loose_object_path(oid)) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(GitError::RemoveObject {
                oid,
                message: error.to_string(),
            }),
        }
    }

    pub fn open_blob(&self, oid: Oid) -> Result<(u64, BlobReader), GitError> {
        let header = match self.git().try_find_header(oid.object_id()) {
            Ok(Some(header)) => header,
            Ok(None) => return Err(GitError::ObjectNotFound(oid)),
            Err(error) => {
                return Err(GitError::Corrupt {
                    oid,
                    message: error.to_string(),
                });
            }
        };
        if header.kind() != gix::object::Kind::Blob {
            return Err(GitError::ObjectType {
                oid,
                expected: "blob",
            });
        }
        let size = header.size();
        match std::fs::File::open(self.loose_object_path(oid)) {
            Ok(file) => {
                let mut reader = std::io::BufReader::new(flate2::read::ZlibDecoder::new(file));
                skip_loose_header(&mut reader, oid)?;
                Ok((size, BlobReader::Loose(reader)))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok((
                size,
                BlobReader::Packed(std::io::Cursor::new(self.read_blob(oid)?)),
            )),
            Err(error) => Err(GitError::Corrupt {
                oid,
                message: error.to_string(),
            }),
        }
    }

    fn graph_tree_and_parents(&self, commit: Oid) -> Option<(gix::ObjectId, Vec<gix::ObjectId>)> {
        let graph = self.commit_graph()?;
        let node = graph.commit_by_id(commit.object_id())?;
        let tree = node.root_tree_id().to_owned();
        let parents = node
            .iter_parents()
            .filter_map(Result::ok)
            .map(|position| graph.commit_at(position).id().to_owned())
            .collect();
        Some((tree, parents))
    }

    pub(crate) fn commit_tree(&self, commit: Oid) -> Result<gix::ObjectId, GitError> {
        if let Some(node) = self
            .commit_graph()
            .and_then(|graph| graph.commit_by_id(commit.object_id()))
        {
            return Ok(node.root_tree_id().to_owned());
        }
        let object = self.load_object(commit)?;
        let commit_object = object.try_into_commit().map_err(|_| GitError::ObjectType {
            oid: commit,
            expected: "commit",
        })?;
        Ok(commit_object
            .tree_id()
            .map_err(|error| GitError::Decode(error.to_string()))?
            .detach())
    }

    pub fn diff(&self, range: CommitRange) -> Result<Vec<FileChange>, GitError> {
        let old_tree_oid = self.commit_tree(range.base)?;
        let new_tree_oid = self.commit_tree(range.head)?;
        let old_tree = self
            .load_object(Oid::from(old_tree_oid))?
            .try_into_tree()
            .map_err(|error| GitError::Backend(error.to_string()))?;
        let new_tree = self
            .load_object(Oid::from(new_tree_oid))?
            .try_into_tree()
            .map_err(|error| GitError::Backend(error.to_string()))?;

        let mut changes = Vec::new();
        old_tree
            .changes()
            .map_err(|error| GitError::Backend(error.to_string()))?
            .for_each_to_obtain_tree(&new_tree, |change| {
                use gix::object::tree::diff::Change;
                let tree_path = |location: &gix::bstr::BStr| {
                    RepoPath::new(location.to_string())
                        .map_err(|error| GitError::Decode(error.to_string()))
                };
                let mapped = match change {
                    Change::Addition { location, id, .. } => FileChange::Added {
                        path: tree_path(location)?,
                        oid: Oid::from(id.detach()),
                    },
                    Change::Deletion { location, id, .. } => FileChange::Deleted {
                        path: tree_path(location)?,
                        oid: Oid::from(id.detach()),
                    },
                    Change::Modification {
                        location,
                        previous_id,
                        id,
                        ..
                    } => FileChange::Modified {
                        path: tree_path(location)?,
                        old: Oid::from(previous_id.detach()),
                        new: Oid::from(id.detach()),
                    },
                    Change::Rewrite {
                        source_location,
                        location,
                        source_id,
                        id,
                        ..
                    } => FileChange::Renamed {
                        from: tree_path(source_location)?,
                        to: tree_path(location)?,
                        old: Oid::from(source_id.detach()),
                        new: Oid::from(id.detach()),
                    },
                };
                changes.push(mapped);
                Ok::<_, GitError>(ControlFlow::Continue(()))
            })
            .map_err(|error| GitError::Backend(error.to_string()))?;
        Ok(changes)
    }

    pub fn compare(&self, range: CommitRange) -> Result<Comparison, GitError> {
        let commits = self.rev_walk(Wants::new(&[range.head]), Haves::new(&[range.base]))?;
        let changes = self.diff(range)?;
        Ok(Comparison { commits, changes })
    }

    fn peel(
        &self,
        oid: gix::ObjectId,
        tags: &mut Vec<Oid>,
        depth: usize,
    ) -> Result<Peeled, GitError> {
        if depth == 0 {
            return Err(GitError::DepthExceeded("annotated tag chain"));
        }
        let object = self.load_object(Oid::from(oid))?;
        match object.kind {
            gix::object::Kind::Commit => Ok(Peeled::Commit(oid)),
            gix::object::Kind::Tree | gix::object::Kind::Blob => Ok(Peeled::Direct(oid)),
            gix::object::Kind::Tag => {
                tags.push(Oid::from(oid));
                let target = object
                    .try_into_tag()
                    .map_err(|error| GitError::Decode(error.to_string()))?
                    .target_id()
                    .map_err(|error| GitError::Decode(error.to_string()))?
                    .detach();
                self.peel(target, tags, depth - 1)
            }
        }
    }

    pub fn peeled_target(&self, oid: Oid) -> Result<Option<Oid>, GitError> {
        let object = self.load_object(oid)?;
        match object.kind {
            gix::object::Kind::Tag => {
                let mut tags = Vec::new();
                let peeled = match self.peel(oid.object_id(), &mut tags, MAX_TAG_DEPTH)? {
                    Peeled::Commit(id) | Peeled::Direct(id) => Oid::from(id),
                };
                Ok(Some(peeled))
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn commit_tree_and_parents(
        &self,
        commit: Oid,
    ) -> Result<(gix::ObjectId, Vec<gix::ObjectId>), GitError> {
        if let Some(found) = self.graph_tree_and_parents(commit) {
            return Ok(found);
        }
        let object = self.load_object(commit)?;
        let commit_object = object.try_into_commit().map_err(|_| GitError::ObjectType {
            oid: commit,
            expected: "commit",
        })?;
        let tree = commit_object
            .tree_id()
            .map_err(|error| GitError::Decode(error.to_string()))?
            .detach();
        let parents = commit_object.parent_ids().map(|id| id.detach()).collect();
        Ok((tree, parents))
    }

    fn blob_kept(&self, oid: gix::ObjectId, filter: Filter) -> Result<bool, GitError> {
        match filter {
            Filter::None | Filter::TreeDepth(_) => Ok(true),
            Filter::BlobNone => Ok(false),
            Filter::BlobLimit(limit) => {
                let header = self
                    .git()
                    .try_find_header(oid)
                    .map_err(|error| GitError::Corrupt {
                        oid: Oid::from(oid),
                        message: error.to_string(),
                    })?
                    .ok_or(GitError::ObjectNotFound(Oid::from(oid)))?;
                Ok(header.size() < limit)
            }
        }
    }

    fn walk_tree(
        &self,
        tree: gix::ObjectId,
        seen: &mut HashSet<gix::ObjectId>,
        visit: &mut dyn FnMut(Oid),
        filter: Filter,
        nesting: usize,
        walked: &mut Walked,
    ) -> Result<(), GitError> {
        if nesting == 0 {
            return Err(GitError::DepthExceeded("tree nesting"));
        }
        if tree == gix::ObjectId::empty_tree(self.git().object_hash()) {
            return Ok(());
        }
        if !seen.insert(tree) {
            return Ok(());
        }
        visit(Oid::from(tree));
        walked.tick()?;
        let object = self.load_object(Oid::from(tree))?;
        let decoded = object
            .try_into_tree()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        let decoded = decoded
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        decoded.entries.iter().try_for_each(|entry| {
            let oid = entry.oid.to_owned();
            match entry.mode.kind() {
                gix::objs::tree::EntryKind::Tree => {
                    self.walk_tree(oid, seen, visit, filter, nesting - 1, walked)
                }
                gix::objs::tree::EntryKind::Commit => Ok(()),
                _ => {
                    if !seen.contains(&oid) && self.blob_kept(oid, filter)? {
                        seen.insert(oid);
                        visit(Oid::from(oid));
                        walked.tick()?;
                    }
                    Ok(())
                }
            }
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_tree_depth(
        &self,
        tree: gix::ObjectId,
        remaining: TreeDepth,
        seen: &mut HashSet<gix::ObjectId>,
        expanded: &mut HashMap<gix::ObjectId, TreeDepth>,
        visit: &mut dyn FnMut(Oid),
        nesting: usize,
        walked: &mut Walked,
    ) -> Result<(), GitError> {
        if nesting == 0 {
            return Err(GitError::DepthExceeded("tree nesting"));
        }
        if remaining.is_exhausted() || tree == gix::ObjectId::empty_tree(self.git().object_hash()) {
            return Ok(());
        }
        if expanded
            .get(&tree)
            .is_some_and(|deepest| *deepest >= remaining)
        {
            return Ok(());
        }
        expanded.insert(tree, remaining);
        if seen.insert(tree) {
            visit(Oid::from(tree));
            walked.tick()?;
        }
        let object = self.load_object(Oid::from(tree))?;
        let decoded = object
            .try_into_tree()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        let decoded = decoded
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        let next = remaining.shallower();
        decoded.entries.iter().try_for_each(|entry| {
            let oid = entry.oid.to_owned();
            match entry.mode.kind() {
                gix::objs::tree::EntryKind::Tree => {
                    self.walk_tree_depth(oid, next, seen, expanded, visit, nesting - 1, walked)
                }
                gix::objs::tree::EntryKind::Commit => Ok(()),
                _ => {
                    if !next.is_exhausted() && seen.insert(oid) {
                        visit(Oid::from(oid));
                        walked.tick()?;
                    }
                    Ok(())
                }
            }
        })
    }

    fn walk_root_tree(
        &self,
        tree: gix::ObjectId,
        seen: &mut HashSet<gix::ObjectId>,
        expanded: &mut HashMap<gix::ObjectId, TreeDepth>,
        visit: &mut dyn FnMut(Oid),
        filter: Filter,
        walked: &mut Walked,
    ) -> Result<(), GitError> {
        match filter {
            Filter::TreeDepth(max) => {
                self.walk_tree_depth(tree, max, seen, expanded, visit, MAX_TREE_DEPTH, walked)
            }
            _ => self.walk_tree(tree, seen, visit, filter, MAX_TREE_DEPTH, walked),
        }
    }

    fn walk_tree_shared(
        &self,
        tree: gix::ObjectId,
        seen: &scc::HashSet<gix::ObjectId>,
        out: &mut Vec<Oid>,
        nesting: usize,
        walk: &mut SharedWalk,
    ) -> Result<(), GitError> {
        if nesting == 0 {
            return Err(GitError::DepthExceeded("tree nesting"));
        }
        if tree == gix::ObjectId::empty_tree(self.git().object_hash()) {
            return Ok(());
        }
        if seen.insert_sync(tree).is_err() {
            return Ok(());
        }
        out.push(Oid::from(tree));
        walk.tick()?;
        let object = self.load_object(Oid::from(tree))?;
        let decoded = object
            .try_into_tree()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        let decoded = decoded
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        decoded.entries.iter().try_for_each(|entry| {
            let oid = entry.oid.to_owned();
            match entry.mode.kind() {
                gix::objs::tree::EntryKind::Tree => {
                    self.walk_tree_shared(oid, seen, out, nesting - 1, walk)
                }
                gix::objs::tree::EntryKind::Commit => Ok(()),
                _ => {
                    if seen.insert_sync(oid).is_ok() {
                        out.push(Oid::from(oid));
                        walk.tick()?;
                    }
                    Ok(())
                }
            }
        })
    }

    fn walk_send_trees(
        &self,
        send: &[(Oid, gix::ObjectId, Vec<gix::ObjectId>)],
        seen: HashSet<gix::ObjectId>,
        mut out: Vec<Oid>,
        budget: PackBudget,
    ) -> Result<Vec<Oid>, GitError> {
        let seen: scc::HashSet<gix::ObjectId> = seen.into_iter().collect();
        let counter = AtomicUsize::new(out.len());
        let path = self.path().to_owned();
        let walk =
            |batch: &[(Oid, gix::ObjectId, Vec<gix::ObjectId>)]| -> Result<Vec<Oid>, GitError> {
                let local = Repo::open(&path)?;
                let mut walk = SharedWalk::new(&counter, &budget);
                batch.iter().try_fold(
                    Vec::new(),
                    |mut acc, (commit, tree, _)| -> Result<Vec<Oid>, GitError> {
                        if seen.insert_sync(commit.object_id()).is_ok() {
                            acc.push(*commit);
                        }
                        local.walk_tree_shared(
                            *tree,
                            &seen,
                            &mut acc,
                            MAX_TREE_DEPTH,
                            &mut walk,
                        )?;
                        Ok(acc)
                    },
                )
            };
        out.extend(knot_resource::map_chunks(send, walk)?);
        Ok(out)
    }

    fn collect_direct(
        &self,
        oid: Oid,
        seen: &mut HashSet<gix::ObjectId>,
        expanded: &mut HashMap<gix::ObjectId, TreeDepth>,
        visit: &mut dyn FnMut(Oid),
        filter: Filter,
        walked: &mut Walked,
    ) -> Result<(), GitError> {
        let object = self.load_object(oid)?;
        match (object.kind, filter) {
            (gix::object::Kind::Tree, Filter::TreeDepth(max)) => self.walk_tree_depth(
                oid.object_id(),
                max.deeper(),
                seen,
                expanded,
                visit,
                MAX_TREE_DEPTH,
                walked,
            ),
            (gix::object::Kind::Tree, _) => {
                self.walk_tree(oid.object_id(), seen, visit, filter, MAX_TREE_DEPTH, walked)
            }
            _ => {
                if seen.insert(oid.object_id()) {
                    visit(oid);
                    walked.tick()?;
                }
                Ok(())
            }
        }
    }

    fn commit_time(&self, commit: gix::ObjectId) -> Result<UnixSeconds, GitError> {
        let object = self.load_object(Oid::from(commit))?;
        let commit = object.try_into_commit().map_err(|_| GitError::ObjectType {
            oid: Oid::from(commit),
            expected: "commit",
        })?;
        let time = commit
            .committer()
            .map_err(|error| GitError::Decode(error.to_string()))?
            .time()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        Ok(UnixSeconds::new(time.seconds))
    }

    pub fn shallow_walk(
        &self,
        wants: Wants<'_>,
        deepen: &Deepen,
        client_shallow: ShallowCommits<'_>,
    ) -> Result<ShallowPlan, GitError> {
        let want_commits: Vec<gix::ObjectId> = wants
            .as_slice()
            .iter()
            .map(|want| self.peel(want.object_id(), &mut Vec::new(), MAX_TAG_DEPTH))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|peeled| match peeled {
                Peeled::Commit(commit) => Some(commit),
                Peeled::Direct(_) => None,
            })
            .collect();

        let excluded: HashSet<gix::ObjectId> = if deepen.not.is_empty() {
            HashSet::new()
        } else {
            self.rev_walk(Wants::new(&deepen.not), Haves::new(&[]))?
                .into_iter()
                .map(Oid::object_id)
                .collect()
        };

        let drop = |oid: gix::ObjectId, depth: CommitDepth| -> Result<bool, GitError> {
            if excluded.contains(&oid) {
                return Ok(true);
            }
            if let Some(max) = deepen.depth
                && depth > max
            {
                return Ok(true);
            }
            if let Some(since) = deepen.since
                && self.commit_time(oid)? < since
            {
                return Ok(true);
            }
            Ok(false)
        };

        let grafts = self.shallow_grafts()?;
        let mut min_depth: HashMap<gix::ObjectId, CommitDepth> = HashMap::new();
        let mut parents_of: HashMap<gix::ObjectId, Vec<gix::ObjectId>> = HashMap::new();
        let mut queue: VecDeque<(gix::ObjectId, CommitDepth)> = want_commits
            .iter()
            .map(|commit| (*commit, CommitDepth::new(1)))
            .collect();
        if deepen.relative {
            client_shallow
                .as_slice()
                .iter()
                .for_each(|oid| queue.push_back((oid.object_id(), CommitDepth::new(0))));
        }
        while let Some((commit, depth)) = queue.pop_front() {
            if drop(commit, depth)? {
                continue;
            }
            if min_depth.get(&commit).is_some_and(|seen| *seen <= depth) {
                continue;
            }
            min_depth.insert(commit, depth);
            let (_, parents) = self.commit_tree_and_parents(Oid::from(commit))?;
            if !grafts.contains(&commit) {
                parents
                    .iter()
                    .for_each(|parent| queue.push_back((*parent, depth.deeper())));
            }
            parents_of.insert(commit, parents);
        }

        let included: HashSet<gix::ObjectId> = min_depth.keys().copied().collect();
        let boundary: HashSet<gix::ObjectId> = included
            .iter()
            .filter(|commit| {
                parents_of
                    .get(*commit)
                    .is_some_and(|parents| parents.iter().any(|parent| !included.contains(parent)))
            })
            .copied()
            .collect();

        let commits: Vec<Oid> = min_depth.keys().map(|oid| Oid::from(*oid)).collect();
        let shallow: Vec<Oid> = boundary.iter().map(|oid| Oid::from(*oid)).collect();
        let unshallow: Vec<Oid> = client_shallow
            .as_slice()
            .iter()
            .filter(|oid| {
                min_depth.contains_key(&oid.object_id()) && !boundary.contains(&oid.object_id())
            })
            .copied()
            .collect();
        Ok(ShallowPlan {
            commits,
            shallow,
            unshallow,
        })
    }

    pub fn select_shallow_objects(
        &self,
        wants: Wants,
        commits: ShallowCommits,
        haves: Haves,
        filter: Filter,
        budget: PackBudget,
    ) -> Result<PackSelection, GitError> {
        let wants = wants.as_slice();
        let commits = commits.as_slice();
        let haves = haves.as_slice();
        let mut walked = Walked::new(budget);
        let mut seen: HashSet<gix::ObjectId> = HashSet::new();
        let mut expanded: HashMap<gix::ObjectId, TreeDepth> = HashMap::new();
        let mut have_commits: Vec<gix::ObjectId> = Vec::new();
        haves
            .iter()
            .filter(|have| self.contains(**have))
            .try_for_each(|have| -> Result<(), GitError> {
                match self.peel(have.object_id(), &mut Vec::new(), MAX_TAG_DEPTH)? {
                    Peeled::Commit(commit) => {
                        have_commits.push(commit);
                        let tree = self.commit_tree(Oid::from(commit))?;
                        self.walk_tree(
                            tree,
                            &mut seen,
                            &mut |_| {},
                            Filter::None,
                            MAX_TREE_DEPTH,
                            &mut walked,
                        )
                    }
                    Peeled::Direct(direct) => self.collect_direct(
                        Oid::from(direct),
                        &mut seen,
                        &mut expanded,
                        &mut |_| {},
                        Filter::None,
                        &mut walked,
                    ),
                }
            })?;
        let client_has: HashSet<Oid> = seen
            .iter()
            .copied()
            .chain(have_commits)
            .map(Oid::from)
            .collect();

        let mut want_tags = Vec::new();
        wants.iter().try_for_each(|want| -> Result<(), GitError> {
            self.peel(want.object_id(), &mut want_tags, MAX_TAG_DEPTH)
                .map(|_| ())
        })?;

        let mut out: Vec<Oid> = Vec::new();
        want_tags
            .iter()
            .try_for_each(|tag| -> Result<(), GitError> {
                if seen.insert(tag.object_id()) {
                    out.push(*tag);
                    walked.tick()?;
                }
                Ok(())
            })?;
        commits
            .iter()
            .try_for_each(|commit| -> Result<(), GitError> {
                if seen.insert(commit.object_id()) {
                    out.push(*commit);
                    walked.tick()?;
                }
                let tree = self.commit_tree(*commit)?;
                self.walk_root_tree(
                    tree,
                    &mut seen,
                    &mut expanded,
                    &mut |oid| out.push(oid),
                    filter,
                    &mut walked,
                )
            })?;
        Ok(PackSelection {
            send: out,
            client_has,
        })
    }

    pub fn select_pack_objects(&self, wants: Wants, haves: Haves) -> Result<Vec<Oid>, GitError> {
        self.select_pack_objects_filtered(wants, haves, Filter::None, PackBudget::unbounded())
            .map(|selection| selection.send)
    }

    pub fn clone_roots(&self, wants: &[Oid], budget: PackBudget) -> Result<Vec<Oid>, GitError> {
        let mut walked = Walked::new(budget);
        let mut want_tags = Vec::new();
        let mut want_commits = Vec::new();
        let mut want_direct = Vec::new();
        wants.iter().try_for_each(|want| -> Result<(), GitError> {
            match self.peel(want.object_id(), &mut want_tags, MAX_TAG_DEPTH)? {
                Peeled::Commit(commit) => want_commits.push(Oid::from(commit)),
                Peeled::Direct(direct) => want_direct.push(Oid::from(direct)),
            }
            Ok(())
        })?;
        let commits =
            self.rev_walk_each(Wants::new(&want_commits), Haves::new(&[]), &mut walked)?;
        Ok(want_tags
            .into_iter()
            .chain(commits)
            .chain(want_direct)
            .collect())
    }

    pub fn reachable_commits(
        &self,
        tips: &[Oid],
        budget: PackBudget,
    ) -> Result<HashSet<Oid>, GitError> {
        let mut walked = Walked::new(budget);
        let commits: Vec<Oid> = self
            .peel_to_commits(tips.iter().copied())?
            .into_iter()
            .map(Oid::from)
            .collect();
        Ok(self
            .rev_walk_each(Wants::new(&commits), Haves::new(&[]), &mut walked)?
            .into_iter()
            .collect())
    }

    fn peel_to_commits(
        &self,
        oids: impl Iterator<Item = Oid>,
    ) -> Result<Vec<gix::ObjectId>, GitError> {
        oids.map(|oid| self.peel(oid.object_id(), &mut Vec::new(), MAX_TAG_DEPTH))
            .filter_map(|peeled| match peeled {
                Ok(Peeled::Commit(commit)) => Some(Ok(commit)),
                Ok(Peeled::Direct(_)) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    pub fn wants_satisfied_by(&self, wants: Wants, haves: Haves) -> Result<bool, GitError> {
        let wants = wants.as_slice();
        let haves = haves.as_slice();
        let commons: HashSet<gix::ObjectId> = self
            .peel_to_commits(haves.iter().copied().filter(|have| self.contains(*have)))?
            .into_iter()
            .collect();
        if commons.is_empty() {
            return Ok(false);
        }
        let oldest = commons
            .iter()
            .map(|commit| self.commit_time(*commit))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .min()
            .unwrap_or(UnixSeconds::new(i64::MIN));
        let grafts = self.shallow_grafts()?;

        let reaches_a_common = |want: gix::ObjectId| -> Result<bool, GitError> {
            let mut seen: HashSet<gix::ObjectId> = HashSet::new();
            let mut queue: VecDeque<gix::ObjectId> = VecDeque::from([want]);
            while let Some(commit) = queue.pop_front() {
                if commons.contains(&commit) {
                    return Ok(true);
                }
                if !seen.insert(commit) || grafts.contains(&commit) {
                    continue;
                }
                if self.commit_time(commit)? < oldest {
                    continue;
                }
                let (_, parents) = self.commit_tree_and_parents(Oid::from(commit))?;
                queue.extend(parents);
            }
            Ok(false)
        };

        self.peel_to_commits(wants.iter().copied())?
            .into_iter()
            .try_fold(true, |all, want| Ok(all && reaches_a_common(want)?))
    }

    pub fn select_pack_objects_filtered(
        &self,
        wants: Wants,
        haves: Haves,
        filter: Filter,
        budget: PackBudget,
    ) -> Result<PackSelection, GitError> {
        let wants = wants.as_slice();
        let haves = haves.as_slice();
        let mut walked = Walked::new(budget);
        let mut expanded: HashMap<gix::ObjectId, TreeDepth> = HashMap::new();
        let mut want_tags = Vec::new();
        let mut want_commits = Vec::new();
        let mut want_direct = Vec::new();
        wants.iter().try_for_each(|want| -> Result<(), GitError> {
            match self.peel(want.object_id(), &mut want_tags, MAX_TAG_DEPTH)? {
                Peeled::Commit(commit) => want_commits.push(Oid::from(commit)),
                Peeled::Direct(direct) => want_direct.push(Oid::from(direct)),
            }
            Ok(())
        })?;

        let mut have_tags = Vec::new();
        let mut have_commits = Vec::new();
        let mut have_direct = Vec::new();
        haves
            .iter()
            .filter(|have| self.contains(**have))
            .try_for_each(|have| -> Result<(), GitError> {
                match self.peel(have.object_id(), &mut have_tags, MAX_TAG_DEPTH)? {
                    Peeled::Commit(commit) => have_commits.push(Oid::from(commit)),
                    Peeled::Direct(direct) => have_direct.push(Oid::from(direct)),
                }
                Ok(())
            })?;

        let grafts = self.shallow_grafts()?;
        let send: Vec<(Oid, gix::ObjectId, Vec<gix::ObjectId>)> = self
            .rev_walk_each(
                Wants::new(&want_commits),
                Haves::new(&have_commits),
                &mut walked,
            )?
            .into_iter()
            .map(|commit| {
                let (tree, parents) = self.commit_tree_and_parents(commit)?;
                let parents = match grafts.contains(&commit.object_id()) {
                    true => Vec::new(),
                    false => parents,
                };
                Ok::<_, GitError>((commit, tree, parents))
            })
            .collect::<Result<_, _>>()?;
        let send_set: HashSet<gix::ObjectId> = send
            .iter()
            .map(|(commit, _, _)| commit.object_id())
            .collect();

        let mut uninteresting: HashSet<gix::ObjectId> =
            have_tags.iter().map(|tag| tag.object_id()).collect();
        let boundary_commits: Vec<gix::ObjectId> = have_commits
            .iter()
            .map(|commit| commit.object_id())
            .chain(
                send.iter()
                    .flat_map(|(_, _, parents)| parents.iter().copied())
                    .filter(|parent| !send_set.contains(parent)),
            )
            .collect();
        boundary_commits
            .iter()
            .try_for_each(|commit| -> Result<(), GitError> {
                let tree = self.commit_tree(Oid::from(*commit))?;
                self.walk_tree(
                    tree,
                    &mut uninteresting,
                    &mut |_| {},
                    Filter::None,
                    MAX_TREE_DEPTH,
                    &mut walked,
                )
            })?;
        have_direct.iter().try_for_each(|direct| {
            self.collect_direct(
                *direct,
                &mut uninteresting,
                &mut expanded,
                &mut |_| {},
                Filter::None,
                &mut walked,
            )
        })?;
        let client_has: HashSet<Oid> = uninteresting
            .iter()
            .copied()
            .chain(boundary_commits)
            .map(Oid::from)
            .collect();

        let mut seen = uninteresting;
        let mut out: Vec<Oid> = Vec::new();
        want_tags
            .iter()
            .try_for_each(|tag| -> Result<(), GitError> {
                if seen.insert(tag.object_id()) {
                    out.push(*tag);
                    walked.tick()?;
                }
                Ok(())
            })?;
        if matches!(filter, Filter::None)
            && want_direct.is_empty()
            && send.len() >= PARALLEL_SELECT_MIN
        {
            out = self.walk_send_trees(&send, seen, out, walked.budget)?;
        } else {
            send.iter()
                .try_for_each(|(commit, tree, _)| -> Result<(), GitError> {
                    if seen.insert(commit.object_id()) {
                        out.push(*commit);
                    }
                    self.walk_root_tree(
                        *tree,
                        &mut seen,
                        &mut expanded,
                        &mut |oid| out.push(oid),
                        filter,
                        &mut walked,
                    )
                })?;
            want_direct.iter().try_for_each(|direct| {
                self.collect_direct(
                    *direct,
                    &mut seen,
                    &mut expanded,
                    &mut |oid| out.push(oid),
                    filter,
                    &mut walked,
                )
            })?;
        }
        Ok(PackSelection {
            send: out,
            client_has,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use knot_types::RepoDid;

    use super::*;
    use crate::Layout;

    fn seeded() -> (tempfile::TempDir, Layout, RepoDid) {
        let dir = tempfile::tempdir().unwrap();
        let layout = Layout::new(dir.path());
        let did = RepoDid::new("did:plc:squid").unwrap();
        layout.create(&did).unwrap();
        (dir, layout, did)
    }

    #[test]
    fn open_blob_streams_a_loose_blob() {
        let (_dir, layout, did) = seeded();
        let content: Vec<u8> = (0..8192u32).map(|byte| byte as u8).collect();
        let repo = layout.open(&did).unwrap();
        let oid = Oid::from(repo.git().write_blob(&content).unwrap().detach());

        let reread = layout.open(&did).unwrap();
        let (size, mut reader) = reread.open_blob(oid).unwrap();
        assert_eq!(size, content.len() as u64);
        assert!(matches!(reader, BlobReader::Loose(_)));
        let mut buf = Vec::new();
        reader.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, content);
    }

    #[test]
    fn open_blob_rejects_a_non_blob() {
        let (_dir, layout, did) = seeded();
        let repo = layout.open(&did).unwrap();
        let tree = repo
            .git()
            .write_object(gix::objs::Tree {
                entries: Vec::new(),
            })
            .unwrap()
            .detach();
        assert!(matches!(
            repo.open_blob(Oid::from(tree)),
            Err(GitError::ObjectType {
                expected: "blob",
                ..
            })
        ));
    }

    #[test]
    fn corrupt_loose_object_is_a_typed_error_not_a_panic() {
        let (_dir, layout, did) = seeded();
        let repo = layout.open(&did).unwrap();
        let oid = Oid::from(
            repo.git()
                .write_blob(b"hello streaming world\n")
                .unwrap()
                .detach(),
        );
        let loose = repo.loose_object_path(oid);

        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o644)).unwrap();
        std::fs::write(&loose, b"this isn't a valid zlib object").unwrap();

        let reopened = layout.open(&did).unwrap();
        assert!(matches!(
            reopened.read_blob(oid),
            Err(GitError::Corrupt { .. })
        ));
        assert!(matches!(
            reopened.open_blob(oid),
            Err(GitError::Corrupt { .. })
        ));
    }

    #[test]
    fn missing_object_is_not_found() {
        let (_dir, layout, did) = seeded();
        let repo = layout.open(&did).unwrap();
        let absent = Oid::from_hex("dead00000000000000000000000000000000beef").unwrap();
        assert!(matches!(
            repo.read_blob(absent),
            Err(GitError::ObjectNotFound(_))
        ));
        assert!(matches!(
            repo.open_blob(absent),
            Err(GitError::ObjectNotFound(_))
        ));
    }
}
