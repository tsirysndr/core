use std::collections::{HashMap, HashSet};
use std::ops::ControlFlow;
use std::time::Instant;

use gix::bstr::ByteSlice;
use knot_types::{BranchName, Oid, RepoPath, TagName, UnixSeconds};

use crate::error::{GitError, backend};
use crate::objects::{Commit, CommitRange, EntryKind, Identity, identity, map_kind};
use crate::repo::Repo;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizedEntry {
    pub name: String,
    pub oid: Oid,
    pub kind: EntryKind,
    pub size: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathEntry {
    pub oid: Oid,
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LastCommit {
    pub id: Oid,
    pub subject: String,
    pub time: UnixSeconds,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BranchTip {
    Commit(Box<Commit>),
    Opaque {
        id: Oid,
        message: String,
        created_at: UnixSeconds,
    },
}

impl BranchTip {
    pub fn created_at(&self) -> UnixSeconds {
        match self {
            BranchTip::Commit(commit) => commit.committer.time,
            BranchTip::Opaque { created_at, .. } => *created_at,
        }
    }

    pub fn id(&self) -> Oid {
        match self {
            BranchTip::Commit(commit) => commit.id,
            BranchTip::Opaque { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BranchInfo {
    pub name: BranchName,
    pub tip: BranchTip,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnotatedTag {
    pub tagger: Option<Identity>,
    pub pgp_signature: Option<String>,
    pub target: Oid,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagInfo {
    pub name: TagName,
    pub id: Oid,
    pub created_at: UnixSeconds,
    pub message: String,
    pub annotated: Option<AnnotatedTag>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Submodule {
    pub name: String,
    pub path: RepoPath,
    pub url: String,
    pub branch: Option<BranchName>,
}

knot_types::scalar_newtype! {
    pub struct LogSkip(usize);
    pub struct LogLimit(usize);
}

impl Repo {
    pub fn resolve_revision(&self, spec: &str) -> Option<Oid> {
        if spec.is_empty() || spec.contains('\0') {
            return None;
        }
        self.git()
            .rev_parse_single(spec.as_bytes())
            .ok()
            .map(|id| Oid::from(id.detach()))
    }

    pub fn peel_to_commit(&self, oid: Oid) -> Result<Oid, GitError> {
        let peeled = self
            .git()
            .find_object(oid.object_id())
            .map_err(backend)?
            .peel_tags_to_end()
            .map_err(backend)?;
        match peeled.kind {
            gix::object::Kind::Commit => Ok(Oid::from(peeled.id)),
            _ => Err(GitError::ObjectType {
                oid,
                expected: "commit",
            }),
        }
    }

    fn walk_from(
        &self,
        start: Oid,
        hidden: Option<Oid>,
    ) -> Result<impl Iterator<Item = Result<Oid, GitError>> + '_, GitError> {
        let hidden = hidden.filter(|oid| self.contains(*oid)).map(Oid::object_id);
        Ok(self
            .git()
            .rev_walk(Some(start.object_id()))
            .sorting(gix::revision::walk::Sorting::ByCommitTime(
                gix::traverse::commit::simple::CommitTimeOrder::NewestFirst,
            ))
            .with_hidden(hidden)
            .all()
            .map_err(|error| GitError::RevWalk(error.to_string()))?
            .map(|info| {
                info.map(|info| Oid::from(info.id))
                    .map_err(|error| GitError::RevWalk(error.to_string()))
            }))
    }

    pub fn commits_between(
        &self,
        range: CommitRange,
        limit: LogLimit,
    ) -> Result<Vec<Oid>, GitError> {
        self.walk_from(range.head, Some(range.base))?
            .take(limit.get())
            .collect()
    }

    pub fn log_window(
        &self,
        start: Oid,
        skip: LogSkip,
        limit: LogLimit,
    ) -> Result<(Vec<Commit>, usize), GitError> {
        self.walk_from(start, None)?.enumerate().try_fold(
            (Vec::new(), 0usize),
            |(mut window, _), (index, oid)| {
                let oid = oid?;
                if index >= skip.get() && window.len() < limit.get() {
                    window.push(self.find_commit(oid)?);
                }
                Ok((window, index + 1))
            },
        )
    }

    pub fn merge_base(&self, one: Oid, two: Oid) -> Result<Option<Oid>, GitError> {
        use gix::repository::merge_base::Error;
        match self.git().merge_base(one.object_id(), two.object_id()) {
            Ok(id) => Ok(Some(Oid::from(id.detach()))),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(error) => Err(backend(error)),
        }
    }

    // For security purposes.
    // Without this, anyone who knows an oid can read objects from
    // a deleted branch/ unreferenced push.
    //
    // COBs and forky staging refs aren't counted ofc.
    //
    // Oh btw to that end `advertised_refs()` *isn't* what upload-pack "advertises":
    // upload-pack uses
    // `advertised_refs_for(AdvertScope::Upload)` which also omits
    // refs matching `transfer.hideRefs`/`uploadpack.hideRefs`.
    pub fn reachable_from_public(&self, target: Oid) -> Result<bool, GitError> {
        let tips: Vec<Oid> = self
            .advertised_refs()?
            .iter()
            // skip any broken refs
            .filter_map(|record| self.peel_to_commit(record.target).ok())
            .collect();
        if tips.contains(&target) {
            return Ok(true);
        }
        tips.iter().try_fold(false, |found, tip| {
            Ok(found
                || self
                    // Traverse graph, but shouldn't be too hard on CPU
                    // because `commit_graph_if_enabled` isn't directly
                    // on the object db.
                    .merge_base(target, *tip)?
                    .is_some_and(|base| base == target))
        })
    }

    pub fn branch_list(&self) -> Result<Vec<BranchInfo>, GitError> {
        self.branches()?
            .into_iter()
            .filter_map(|record| record.name.branch_name().map(|name| (name, record.target)))
            .map(|(name, target)| self.branch_tip(target).map(|tip| BranchInfo { name, tip }))
            .collect()
    }

    fn branch_tip(&self, target: Oid) -> Result<BranchTip, GitError> {
        let object = self
            .git()
            .find_object(target.object_id())
            .map_err(backend)?;
        match object.kind {
            gix::object::Kind::Commit => self
                .find_commit(target)
                .map(|commit| BranchTip::Commit(Box::new(commit))),
            gix::object::Kind::Tag => {
                let tag = object.try_into_tag().map_err(backend)?;
                let decoded = tag.decode().map_err(backend)?;
                let created_at = decoded
                    .tagger()
                    .map_err(|error| GitError::Decode(error.to_string()))?
                    .map(identity)
                    .transpose()?
                    .map(|tagger| tagger.time)
                    .unwrap_or(UnixSeconds::new(0));
                Ok(BranchTip::Opaque {
                    id: target,
                    message: decoded.message.to_string(),
                    created_at,
                })
            }
            _ => Ok(BranchTip::Opaque {
                id: target,
                message: String::new(),
                created_at: UnixSeconds::new(0),
            }),
        }
    }

    pub fn tag_list(&self) -> Result<Vec<TagInfo>, GitError> {
        self.tags()?
            .into_iter()
            .filter_map(|record| record.name.tag_name().map(|name| (name, record.target)))
            .map(|(name, target)| self.tag_info(name, target))
            .collect()
    }

    fn tag_info(&self, name: TagName, target: Oid) -> Result<TagInfo, GitError> {
        let object = self
            .git()
            .find_object(target.object_id())
            .map_err(backend)?;
        match object.kind {
            gix::object::Kind::Tag => {
                let tag = object.try_into_tag().map_err(backend)?;
                let decoded = tag.decode().map_err(backend)?;
                let tagger = decoded
                    .tagger()
                    .map_err(|error| GitError::Decode(error.to_string()))?
                    .map(identity)
                    .transpose()?;
                let created_at = tagger
                    .as_ref()
                    .map(|tagger| tagger.time)
                    .unwrap_or(UnixSeconds::new(0));
                Ok(TagInfo {
                    name,
                    id: target,
                    created_at,
                    message: decoded.message.to_string(),
                    annotated: Some(AnnotatedTag {
                        tagger,
                        pgp_signature: decoded.pgp_signature.map(|signature| signature.to_string()),
                        target: Oid::from(decoded.target()),
                    }),
                })
            }
            gix::object::Kind::Commit => {
                let commit = self.find_commit(target)?;
                Ok(TagInfo {
                    name,
                    id: target,
                    created_at: commit.committer.time,
                    message: commit.message,
                    annotated: None,
                })
            }
            _ => Ok(TagInfo {
                name,
                id: target,
                created_at: UnixSeconds::new(0),
                message: String::new(),
                annotated: None,
            }),
        }
    }

    fn dir_tree_id(
        &self,
        commit: Oid,
        dir: Option<&RepoPath>,
    ) -> Result<Option<gix::ObjectId>, GitError> {
        let root = self.commit_tree(commit)?;
        let Some(dir) = dir else {
            return Ok(Some(root));
        };
        let tree = self.git().find_tree(root).map_err(backend)?;
        match tree.lookup_entry_by_path(dir.as_str()).map_err(backend)? {
            Some(entry) if entry.mode().is_tree() => Ok(Some(entry.object_id())),
            _ => Ok(None),
        }
    }

    pub(crate) fn root_tree(&self, commit: Oid) -> Result<gix::Tree<'_>, GitError> {
        let root = self.commit_tree(commit)?;
        self.git().find_tree(root).map_err(backend)
    }

    pub fn entry_at(&self, commit: Oid, path: &RepoPath) -> Result<Option<PathEntry>, GitError> {
        let tree = self.root_tree(commit)?;
        Ok(tree
            .lookup_entry_by_path(path.as_str())
            .map_err(backend)?
            .map(|entry| PathEntry {
                oid: Oid::from(entry.object_id()),
                kind: map_kind(entry.mode().kind()),
            }))
    }

    pub fn tree_entries_at(
        &self,
        commit: Oid,
        path: Option<&RepoPath>,
    ) -> Result<Option<Vec<SizedEntry>>, GitError> {
        let Some(path) = path else {
            let tree = self.root_tree(commit)?;
            return self.sized_entries(&tree).map(Some);
        };
        match self.entry_at(commit, path)? {
            None => Ok(None),
            Some(entry) if entry.kind == EntryKind::Tree => self.tree_entries(entry.oid).map(Some),
            Some(entry) if entry.kind == EntryKind::Commit => Ok(None),
            Some(_) => Ok(Some(Vec::new())),
        }
    }

    pub fn tree_entries(&self, tree: Oid) -> Result<Vec<SizedEntry>, GitError> {
        let tree = self.git().find_tree(tree.object_id()).map_err(backend)?;
        self.sized_entries(&tree)
    }

    fn sized_entries(&self, tree: &gix::Tree<'_>) -> Result<Vec<SizedEntry>, GitError> {
        let decoded = tree
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        decoded
            .entries
            .iter()
            .map(|entry| {
                let oid = Oid::from(entry.oid.to_owned());
                let kind = map_kind(entry.mode.kind());
                let size = match kind {
                    EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link => self
                        .git()
                        .try_find_header(oid.object_id())
                        .map_err(|error| GitError::Corrupt {
                            oid,
                            message: error.to_string(),
                        })?
                        .map(|header| header.size())
                        .unwrap_or(0),
                    EntryKind::Tree | EntryKind::Commit => 0,
                };
                Ok(SizedEntry {
                    name: entry.filename.to_string(),
                    oid,
                    kind,
                    size,
                })
            })
            .collect()
    }

    fn entry_oids_of_tree(&self, tree: gix::ObjectId) -> Result<HashMap<String, Oid>, GitError> {
        let tree = self.git().find_tree(tree).map_err(backend)?;
        let decoded = tree
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        Ok(decoded
            .entries
            .iter()
            .map(|entry| (entry.filename.to_string(), Oid::from(entry.oid.to_owned())))
            .collect())
    }

    pub fn last_commits(
        &self,
        start: Oid,
        dir: Option<&RepoPath>,
        names: &[String],
        deadline: Option<Instant>,
    ) -> Result<HashMap<String, LastCommit>, GitError> {
        let mut pending: HashSet<&str> = names.iter().map(String::as_str).collect();
        let mut attributed = HashMap::new();
        let mut dir_trees: HashMap<Oid, Option<gix::ObjectId>> = HashMap::new();
        let mut dir_tree_of = |commit: Oid| -> Result<Option<gix::ObjectId>, GitError> {
            match dir_trees.get(&commit) {
                Some(known) => Ok(*known),
                None => {
                    let id = self.dir_tree_id(commit, dir)?;
                    dir_trees.insert(commit, id);
                    Ok(id)
                }
            }
        };

        let mut step = |oid: Result<Oid, GitError>| -> Result<bool, GitError> {
            if pending.is_empty() || deadline.is_some_and(|deadline| Instant::now() >= deadline) {
                return Ok(false);
            }
            let oid = oid?;
            let commit = self.find_commit(oid)?;
            if commit.parents.len() > 1 {
                return Ok(true);
            }
            let here_tree = dir_tree_of(oid)?;
            let parent_tree = commit
                .parents
                .first()
                .copied()
                .map(&mut dir_tree_of)
                .transpose()?
                .flatten();
            if here_tree == parent_tree || here_tree.is_none() {
                return Ok(true);
            }
            let here = self.entry_oids_of_tree(here_tree.expect("checked above"))?;
            let parent = parent_tree
                .map(|tree| self.entry_oids_of_tree(tree))
                .transpose()?
                .unwrap_or_default();
            let changed: Vec<String> = pending
                .iter()
                .filter(|name| here.contains_key(**name) && here.get(**name) != parent.get(**name))
                .map(|name| name.to_string())
                .collect();
            changed.iter().for_each(|name| {
                pending.remove(name.as_str());
            });
            changed.into_iter().for_each(|name| {
                attributed.insert(
                    name,
                    LastCommit {
                        id: oid,
                        subject: subject_line(&commit.message),
                        time: commit.author.time,
                    },
                );
            });
            Ok(true)
        };

        let flow =
            self.walk_from(start, None)?
                .try_for_each(|oid| -> ControlFlow<Option<GitError>> {
                    match step(oid) {
                        Ok(true) => ControlFlow::Continue(()),
                        Ok(false) => ControlFlow::Break(None),
                        Err(error) => ControlFlow::Break(Some(error)),
                    }
                });
        match flow {
            ControlFlow::Break(Some(error)) => Err(error),
            _ => Ok(attributed),
        }
    }

    pub fn submodules(&self, commit: Oid) -> Result<Vec<Submodule>, GitError> {
        let gitmodules = RepoPath::new(".gitmodules").expect("literal path is well-formed");
        let Some(entry) = self.entry_at(commit, &gitmodules)? else {
            return Ok(Vec::new());
        };
        if !entry.kind.is_file() {
            return Ok(Vec::new());
        }
        let raw = self.read_blob(entry.oid)?;
        Ok(parse_gitmodules(raw.as_bstr().to_str_lossy().as_ref()))
    }
}

fn subject_line(message: &str) -> String {
    message.lines().next().unwrap_or_default().to_string()
}

fn strip_config_comment(line: &str) -> String {
    let flow = line.chars().try_fold(
        (String::new(), false, false),
        |(mut out, quoted, escaped), ch| match (escaped, quoted, ch) {
            (false, false, '#' | ';') => ControlFlow::Break(out),
            (false, _, '"') => {
                out.push(ch);
                ControlFlow::Continue((out, !quoted, false))
            }
            (false, _, '\\') => {
                out.push(ch);
                ControlFlow::Continue((out, quoted, true))
            }
            _ => {
                out.push(ch);
                ControlFlow::Continue((out, quoted, false))
            }
        },
    );
    match flow {
        ControlFlow::Continue((out, _, _)) | ControlFlow::Break(out) => out,
    }
}

fn unquote_config_value(raw: &str) -> String {
    raw.trim()
        .chars()
        .fold((String::new(), false), |(mut out, escaped), ch| {
            match (escaped, ch) {
                (true, 'n') => {
                    out.push('\n');
                    (out, false)
                }
                (true, 't') => {
                    out.push('\t');
                    (out, false)
                }
                (true, 'b') => {
                    out.push('\u{0008}');
                    (out, false)
                }
                (true, other) => {
                    out.push(other);
                    (out, false)
                }
                (false, '\\') => (out, true),
                (false, '"') => (out, false),
                (false, other) => {
                    out.push(other);
                    (out, false)
                }
            }
        })
        .0
}

fn parse_gitmodules(content: &str) -> Vec<Submodule> {
    struct Partial {
        name: String,
        path: Option<String>,
        url: Option<String>,
        branch: Option<String>,
    }
    let finish = |partial: Partial| -> Option<Submodule> {
        Some(Submodule {
            name: partial.name,
            path: RepoPath::new(partial.path?).ok()?,
            url: partial.url?,
            branch: partial
                .branch
                .and_then(|branch| BranchName::new(branch).ok()),
        })
    };
    let (mut sections, last) = content.lines().map(strip_config_comment).fold(
        (Vec::new(), None::<Partial>),
        |(mut done, current), line| {
            let line = line.trim();
            if let Some(rest) = line.strip_prefix("[submodule \"")
                && let Some(name) = rest.strip_suffix("\"]")
            {
                done.extend(current.and_then(&finish));
                return (
                    done,
                    Some(Partial {
                        name: name.to_string(),
                        path: None,
                        url: None,
                        branch: None,
                    }),
                );
            }
            if line.starts_with('[') {
                done.extend(current.and_then(&finish));
                return (done, None);
            }
            let current = current.map(|mut partial| {
                if let Some((key, value)) = line.split_once('=') {
                    let value = unquote_config_value(value);
                    match key.trim() {
                        "path" => partial.path = Some(value),
                        "url" => partial.url = Some(value),
                        "branch" => partial.branch = Some(value),
                        _ => {}
                    }
                }
                partial
            });
            (done, current)
        },
    );
    sections.extend(last.and_then(&finish));
    sections
}
