use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::Write;
use std::ops::ControlFlow;

use flate2::Compression;
use flate2::write::ZlibEncoder;
use gix::diff::blob::unified_diff::{ConsumeHunk, ContextSize, DiffLineKind, HunkHeader};
use gix::diff::blob::{Algorithm, Diff, InternedInput, UnifiedDiff};
use knot_types::{ChangedFiles, ChangedFilesBudget, Listing, Oid, RepoPath};

use crate::base85;
use crate::error::{GitError, backend};
use crate::objects::EntryKind;
use crate::repo::Repo;

const BINARY_SNIFF_BYTES: usize = 8000;
const BLOCK_HEADER_MAX: usize = "literal 18446744073709551615\n".len();
const BINARY_PATCH_HEADER: &str = "GIT binary patch\n";
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BinarySizes {
    pub old: u64,
    pub new: u64,
}

impl BinarySizes {
    fn of(old: &[u8], new: &[u8]) -> Self {
        Self {
            old: old.len() as u64,
            new: new.len() as u64,
        }
    }

    fn wire_bound(self) -> u64 {
        let block = |inflated: u64| {
            let deflated = inflated
                .saturating_add(inflated.div_ceil(8))
                .saturating_add(inflated.div_ceil(64))
                .saturating_add(11);
            base85::encoded_len(deflated).saturating_add(BLOCK_HEADER_MAX as u64 + 1)
        };
        block(self.old)
            .saturating_add(block(self.new))
            .saturating_add(BINARY_PATCH_HEADER.len() as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryBudget {
    Omit,
    Spend { remaining: u64, omitted: bool },
}

impl BinaryBudget {
    pub fn new(bytes: u64) -> Self {
        Self::Spend {
            remaining: bytes,
            omitted: false,
        }
    }

    pub fn omitted(self) -> bool {
        matches!(self, Self::Spend { omitted: true, .. })
    }

    fn admit(&mut self, sizes: BinarySizes) -> bool {
        match self {
            Self::Omit => false,
            Self::Spend { remaining, omitted } => match remaining.checked_sub(sizes.wire_bound()) {
                Some(rest) => {
                    *remaining = rest;
                    true
                }
                None => {
                    *omitted = true;
                    false
                }
            },
        }
    }
}

fn literal_block(content: &[u8], out: &mut String) -> Result<(), GitError> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::fast());
    encoder.write_all(content).map_err(backend)?;
    writeln!(out, "literal {}", content.len()).expect("formatting into a String never fails");
    base85::encode(&encoder.finish().map_err(backend)?, out);
    out.push('\n');
    Ok(())
}

fn encode_binary(old: &[u8], new: &[u8]) -> Result<BinaryDiff, GitError> {
    let mut text = String::from(BINARY_PATCH_HEADER);
    literal_block(new, &mut text)?;
    literal_block(old, &mut text)?;
    Ok(BinaryDiff::Encoded {
        sizes: BinarySizes::of(old, new),
        text,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BinaryDiff {
    Encoded { sizes: BinarySizes, text: String },
    Omitted(BinarySizes),
    Unchanged(u64),
}

impl BinaryDiff {
    pub fn sizes(&self) -> BinarySizes {
        match self {
            Self::Encoded { sizes, .. } | Self::Omitted(sizes) => *sizes,
            Self::Unchanged(bytes) => BinarySizes {
                old: *bytes,
                new: *bytes,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchBody {
    Text(Vec<Hunk>),
    Binary(BinaryDiff),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FilePatch {
    pub status: PatchStatus,
    pub path: RepoPath,
    pub old_oid: Oid,
    pub new_oid: Oid,
    pub old_kind: Option<EntryKind>,
    pub new_kind: Option<EntryKind>,
    pub body: PatchBody,
}

impl FilePatch {
    pub fn is_binary(&self) -> bool {
        matches!(self.body, PatchBody::Binary(_))
    }

    pub fn hunks(&self) -> &[Hunk] {
        match &self.body {
            PatchBody::Text(hunks) => hunks,
            PatchBody::Binary(_) => &[],
        }
    }
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

fn subproject_line(oid: Oid) -> Vec<u8> {
    format!("Subproject commit {}\n", oid.to_hex()).into_bytes()
}

impl Repo {
    fn patch_content(&self, side: &Side) -> Result<Vec<u8>, GitError> {
        match side {
            Side::Absent => Ok(Vec::new()),
            Side::Present { oid, kind } => match kind {
                EntryKind::Commit => Ok(subproject_line(*oid)),
                EntryKind::Tree => Ok(Vec::new()),
                _ => self.read_blob(*oid),
            },
        }
    }

    fn sides_past_diff_budget(
        &self,
        old: &Side,
        new: &Side,
    ) -> Result<Option<BinarySizes>, GitError> {
        let size = |side: &Side| match side {
            Side::Absent
            | Side::Present {
                kind: EntryKind::Tree,
                ..
            } => Ok(None),
            Side::Present {
                oid,
                kind: EntryKind::Commit,
            } => Ok(Some(subproject_line(*oid).len() as u64)),
            Side::Present { oid, .. } => self.blob_size(*oid).map(Some),
        };
        let (old, new) = (size(old)?, size(new)?);
        let past = |bytes: Option<u64>| bytes.is_some_and(|bytes| bytes > MAX_DIFF_BLOB_BYTES);
        Ok((past(old) || past(new)).then(|| BinarySizes {
            old: old.unwrap_or(0),
            new: new.unwrap_or(0),
        }))
    }

    fn file_patch(
        &self,
        status: PatchStatus,
        path: RepoPath,
        old: Side,
        new: Side,
        budget: &mut BinaryBudget,
    ) -> Result<FilePatch, GitError> {
        let same_content = matches!(
            (&old, &new),
            (Side::Present { oid: before, .. }, Side::Present { oid: after, .. })
                if before == after
        );
        let body = match self.sides_past_diff_budget(&old, &new)? {
            Some(sizes) => PatchBody::Binary(BinaryDiff::Omitted(sizes)),
            None => {
                let old_content = self.patch_content(&old)?;
                let new_content = self.patch_content(&new)?;
                match is_binary(&old_content) || is_binary(&new_content) {
                    true => {
                        let sizes = BinarySizes::of(&old_content, &new_content);
                        PatchBody::Binary(match same_content {
                            true => BinaryDiff::Unchanged(sizes.new),
                            false => match budget.admit(sizes) {
                                true => encode_binary(&old_content, &new_content)?,
                                false => BinaryDiff::Omitted(sizes),
                            },
                        })
                    }
                    false => PatchBody::Text(text_hunks(&old_content, &new_content)?),
                }
            }
        };
        Ok(FilePatch {
            status,
            path,
            old_oid: old.oid(self.object_format().null_oid()),
            new_oid: new.oid(self.object_format().null_oid()),
            old_kind: old.kind(),
            new_kind: new.kind(),
            body,
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

    pub fn commit_patches(
        &self,
        range: PatchRange,
        budget: &mut BinaryBudget,
    ) -> Result<Vec<FilePatch>, GitError> {
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
                self.file_patch(status, path, old, new, budget)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_wire_bound_covers_every_byte_the_patch_writes() {
        let payload = |len: usize, fill: fn(usize) -> u8| (0..len).map(fill).collect::<Vec<u8>>();
        [0usize, 1, 3, 4, 51, 52, 53, 1000, 65_536]
            .into_iter()
            .flat_map(|len| {
                let noise = payload(len, |index| (index as u8).wrapping_mul(37) ^ 0x5a);
                [(payload(len, |_| 0), noise.clone()), (noise, Vec::new())]
            })
            .for_each(|(old, new)| {
                let BinaryDiff::Encoded { sizes, text } = encode_binary(&old, &new).unwrap() else {
                    panic!("encode_binary returns Encoded for every payload");
                };
                assert!(
                    text.len() as u64 <= sizes.wire_bound(),
                    "payload of {} bytes: expected at most {}, wrote {}",
                    old.len().max(new.len()),
                    sizes.wire_bound(),
                    text.len()
                );
            });
    }

    #[test]
    fn the_budget_reports_the_first_payload_it_refuses() {
        let sizes = BinarySizes { old: 0, new: 4096 };
        let mut budget = BinaryBudget::new(sizes.wire_bound());
        assert!(budget.admit(sizes));
        assert!(
            !budget.omitted(),
            "omitted is false while admit returns true"
        );
        assert!(!budget.admit(sizes));
        assert!(budget.omitted(), "omitted is true once admit returns false");

        let mut omit = BinaryBudget::Omit;
        assert!(!omit.admit(sizes));
        assert!(
            !omit.omitted(),
            "omitted is false under Omit, where admit always returns false"
        );
    }
}
