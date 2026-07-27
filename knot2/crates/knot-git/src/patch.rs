use std::convert::Infallible;
use std::ops::ControlFlow;

use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{Algorithm, Diff, InternedInput, UnifiedDiff};
use knot_types::{ChangedFiles, ChangedFilesBudget, Listing, Oid, RepoPath};

use crate::error::{GitError, backend};
use crate::objects::EntryKind;
use crate::repo::Repo;

const BINARY_SNIFF_BYTES: usize = 8000;
pub const MAX_DIFF_BLOB_BYTES: u64 = 25 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineOp {
    Context,
    Delete,
    Add,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HunkLine {
    pub op: LineOp,
    pub text: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PatchRange {
    pub base: Option<Oid>,
    pub head: Oid,
}

// Just making sure a count in a start slot doesn't even compile.
knot_types::scalar_newtype! {
    pub struct LineNumber(u32);
    pub struct LineCount(u32);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    pub old_start: LineNumber,
    pub old_lines: LineCount,
    pub new_start: LineNumber,
    pub new_lines: LineCount,
    pub lines: Vec<HunkLine>,
}

impl Hunk {
    pub fn added(&self) -> LineCount {
        self.count(LineOp::Add)
    }

    pub fn deleted(&self) -> LineCount {
        self.count(LineOp::Delete)
    }

    fn count(&self, op: LineOp) -> LineCount {
        LineCount::new(
            self.lines
                .iter()
                .filter(|line| line.op == op)
                .count()
                .try_into()
                .unwrap_or(u32::MAX),
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PatchStatus {
    Added,
    Deleted,
    Modified,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub status: PatchStatus,
    pub path: RepoPath,
    pub old_oid: Oid,
    pub new_oid: Oid,
    pub old_kind: Option<EntryKind>,
    pub new_kind: Option<EntryKind>,
    pub is_binary: bool,
    pub hunks: Vec<Hunk>,
}

fn is_binary(content: &[u8]) -> bool {
    content[..content.len().min(BINARY_SNIFF_BYTES)].contains(&0)
}

struct CollectHunks {
    hunks: Vec<Hunk>,
}

impl ConsumeHunk for CollectHunks {
    type Out = Vec<Hunk>;

    fn consume_hunk(
        &mut self,
        header: HunkHeader,
        lines: &[(DiffLineKind, &[u8])],
    ) -> std::io::Result<()> {
        let map_op = |kind: DiffLineKind| match kind {
            DiffLineKind::Context => LineOp::Context,
            DiffLineKind::Remove => LineOp::Delete,
            DiffLineKind::Add => LineOp::Add,
        };
        let adjust = |start: u32, len: u32| {
            if len == 0 {
                start.saturating_sub(1)
            } else {
                start
            }
        };
        self.hunks.push(Hunk {
            old_start: LineNumber::new(adjust(header.before_hunk_start, header.before_hunk_len)),
            old_lines: LineCount::new(header.before_hunk_len),
            new_start: LineNumber::new(adjust(header.after_hunk_start, header.after_hunk_len)),
            new_lines: LineCount::new(header.after_hunk_len),
            lines: lines
                .iter()
                .map(|(kind, text)| HunkLine {
                    op: map_op(*kind),
                    text: text.to_vec(),
                })
                .collect(),
        });
        Ok(())
    }

    fn finish(self) -> Self::Out {
        self.hunks
    }
}

fn text_hunks(old: &[u8], new: &[u8]) -> Result<Vec<Hunk>, GitError> {
    let input = InternedInput::new(old, new);
    let diff = Diff::compute(Algorithm::Histogram, &input);
    UnifiedDiff::new(
        &diff,
        &input,
        CollectHunks { hunks: Vec::new() },
        ContextSize::symmetrical(3),
    )
    .consume()
    .map_err(backend)
}

enum Side {
    Absent,
    Present { oid: Oid, kind: EntryKind },
}

impl Side {
    fn oid(&self, absent: Oid) -> Oid {
        match self {
            Side::Absent => absent,
            Side::Present { oid, .. } => *oid,
        }
    }

    fn kind(&self) -> Option<EntryKind> {
        match self {
            Side::Absent => None,
            Side::Present { kind, .. } => Some(*kind),
        }
    }
}

impl Repo {
    fn patch_content(&self, side: &Side) -> Result<Vec<u8>, GitError> {
        match side {
            Side::Absent => Ok(Vec::new()),
            Side::Present { oid, kind } => match kind {
                EntryKind::Commit => {
                    Ok(format!("Subproject commit {}\n", oid.to_hex()).into_bytes())
                }
                EntryKind::Tree => Ok(Vec::new()),
                _ => self.read_blob(*oid),
            },
        }
    }

    fn side_within_diff_budget(&self, side: &Side) -> Result<bool, GitError> {
        match side {
            Side::Present {
                oid,
                kind: EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link,
            } => Ok(self.blob_size(*oid)? <= MAX_DIFF_BLOB_BYTES),
            _ => Ok(true),
        }
    }

    fn file_patch(
        &self,
        status: PatchStatus,
        path: RepoPath,
        old: Side,
        new: Side,
    ) -> Result<FilePatch, GitError> {
        let within_budget =
            self.side_within_diff_budget(&old)? && self.side_within_diff_budget(&new)?;
        let (binary, hunks) = match within_budget {
            false => (true, Vec::new()),
            true => {
                let old_content = self.patch_content(&old)?;
                let new_content = self.patch_content(&new)?;
                let binary = is_binary(&old_content) || is_binary(&new_content);
                let hunks = match binary {
                    true => Vec::new(),
                    false => text_hunks(&old_content, &new_content)?,
                };
                (binary, hunks)
            }
        };
        Ok(FilePatch {
            status,
            path,
            old_oid: old.oid(self.object_format().null_oid()),
            new_oid: new.oid(self.object_format().null_oid()),
            old_kind: old.kind(),
            new_kind: new.kind(),
            is_binary: binary,
            hunks,
        })
    }

    fn diff_trees(&self, range: PatchRange) -> Result<(gix::Tree<'_>, gix::Tree<'_>), GitError> {
        let PatchRange {
            base: old_commit,
            head: new_commit,
        } = range;
        let new_tree = self.root_tree(self.peel_to_commit(new_commit)?)?;
        let old_tree = match old_commit {
            Some(commit) => self.root_tree(self.peel_to_commit(commit)?)?,
            None => self.git().empty_tree(),
        };
        Ok((old_tree, new_tree))
    }

    pub fn changed_paths(&self, range: PatchRange) -> Result<ChangedFiles, GitError> {
        let (old_tree, new_tree) = self.diff_trees(range)?;
        let mut budget = ChangedFilesBudget::new();
        let walked = old_tree
            .changes()
            .map_err(backend)?
            .options(|options| {
                options.track_rewrites(None);
            })
            .for_each_to_obtain_tree(&new_tree, |change| -> Result<ControlFlow<()>, Infallible> {
                use gix::object::tree::diff::Change;
                let (location, is_tree) = match change {
                    Change::Addition {
                        location,
                        entry_mode,
                        ..
                    }
                    | Change::Deletion {
                        location,
                        entry_mode,
                        ..
                    } => (location, entry_mode.is_tree()),
                    Change::Modification {
                        location,
                        previous_entry_mode,
                        entry_mode,
                        ..
                    } => (
                        location,
                        previous_entry_mode.is_tree() || entry_mode.is_tree(),
                    ),
                    Change::Rewrite { .. } => return Ok(ControlFlow::Continue(())),
                };
                match (is_tree, RepoPath::new(location.to_string())) {
                    (true, _) => Ok(ControlFlow::Continue(())),
                    (false, Ok(path)) => Ok(budget.admit(path)),
                    (false, Err(_)) => Ok(budget.truncate()),
                }
            });
        let changed = budget.finish();
        // When the above gives us `Break`,
        // gix doesn't return partial-success
        // but instead `Error::Cancelled`.
        // So if the listing comes out truncated,
        // the "error" in `walked` is our own stop-sign given
        // back at us and we ignore it on purpose.
        // If the listing is complete,
        // nothing ever asked to stop
        // and when `walked` errors out it's actually
        // from the diff itself that we should believe.
        match changed.listing() {
            Listing::Truncated => Ok(changed),
            Listing::Complete => walked.map(|_| changed).map_err(backend),
        }
    }

    pub fn commit_patches(&self, range: PatchRange) -> Result<Vec<FilePatch>, GitError> {
        let (old_tree, new_tree) = self.diff_trees(range)?;
        let mut sides: Vec<(PatchStatus, String, Side, Side)> = Vec::new();
        old_tree
            .changes()
            .map_err(backend)?
            .options(|options| {
                options.track_rewrites(None);
            })
            .for_each_to_obtain_tree(&new_tree, |change| {
                use gix::object::tree::diff::Change;
                match change {
                    Change::Addition {
                        location,
                        id,
                        entry_mode,
                        ..
                    } => sides.push((
                        PatchStatus::Added,
                        location.to_string(),
                        Side::Absent,
                        Side::Present {
                            oid: Oid::from(id.detach()),
                            kind: crate::objects::map_kind(entry_mode.kind()),
                        },
                    )),
                    Change::Deletion {
                        location,
                        id,
                        entry_mode,
                        ..
                    } => sides.push((
                        PatchStatus::Deleted,
                        location.to_string(),
                        Side::Present {
                            oid: Oid::from(id.detach()),
                            kind: crate::objects::map_kind(entry_mode.kind()),
                        },
                        Side::Absent,
                    )),
                    Change::Modification {
                        location,
                        previous_id,
                        id,
                        previous_entry_mode,
                        entry_mode,
                    } => sides.push((
                        PatchStatus::Modified,
                        location.to_string(),
                        Side::Present {
                            oid: Oid::from(previous_id.detach()),
                            kind: crate::objects::map_kind(previous_entry_mode.kind()),
                        },
                        Side::Present {
                            oid: Oid::from(id.detach()),
                            kind: crate::objects::map_kind(entry_mode.kind()),
                        },
                    )),
                    Change::Rewrite { .. } => {}
                }
                Ok::<_, std::convert::Infallible>(std::ops::ControlFlow::Continue(()))
            })
            .map_err(backend)?;

        sides
            .into_iter()
            .filter(|(_, _, old, new)| {
                !matches!(
                    (old, new),
                    (
                        Side::Present {
                            kind: EntryKind::Tree,
                            ..
                        },
                        _
                    ) | (
                        _,
                        Side::Present {
                            kind: EntryKind::Tree,
                            ..
                        }
                    )
                )
            })
            .map(|(status, path, old, new)| {
                let path =
                    RepoPath::new(path).map_err(|error| GitError::Decode(error.to_string()))?;
                self.file_patch(status, path, old, new)
            })
            .collect()
    }
}
