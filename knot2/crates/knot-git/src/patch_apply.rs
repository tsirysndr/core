use std::collections::BTreeMap;

use knot_types::{Oid, RepoPath};

use crate::error::{GitError, backend};
use crate::objects::{EntryKind, Identity, signature};
use crate::patch::{Hunk, LineOp, MAX_DIFF_BLOB_BYTES};
use crate::patch_parse::{FileIntent, ParsedFile, PatchPayload};
use crate::repo::Repo;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConflictReason {
    AlreadyExists,
    DoesNotExist,
    DoesNotApply,
}

impl ConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ConflictReason::AlreadyExists => "file already exists",
            ConflictReason::DoesNotExist => "file doesn't exist",
            ConflictReason::DoesNotApply => "patch doesn't apply",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub reason: ConflictReason,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StagedAction {
    Put { content: Vec<u8>, kind: EntryKind },
    PutGitlink { oid: Oid },
    Remove,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedChange {
    pub path: RepoPath,
    pub action: StagedAction,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ApplyOutcome {
    Clean(Vec<StagedChange>),
    Conflicted(Vec<Conflict>),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("file touched by patch exceeds {MAX_DIFF_BLOB_BYTES}-byte limit")]
    TooLarge,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommit {
    pub tree: Oid,
    pub parents: Vec<Oid>,
    pub author: Identity,
    pub committer: Identity,
    pub message: String,
    pub extra_headers: Vec<(String, Vec<u8>)>,
}

fn patch_path(raw: &str) -> Option<RepoPath> {
    RepoPath::new(raw).ok().filter(|path| !path.names_dot_git())
}

fn split_lines(content: &[u8]) -> Vec<&[u8]> {
    content.split_inclusive(|&byte| byte == b'\n').collect()
}

fn images(hunk: &Hunk) -> (Vec<&[u8]>, Vec<&[u8]>) {
    let pick = |keep: fn(LineOp) -> bool| {
        hunk.lines
            .iter()
            .filter(move |line| keep(line.op))
            .map(|line| line.text.as_slice())
            .collect()
    };
    (
        pick(|op| matches!(op, LineOp::Context | LineOp::Delete)),
        pick(|op| matches!(op, LineOp::Context | LineOp::Add)),
    )
}

fn find_match(lines: &[&[u8]], pre: &[&[u8]], cursor: usize, expected: usize) -> Option<usize> {
    let last = lines.len().checked_sub(pre.len())?;
    if last < cursor {
        return None;
    }
    let anchor = expected.clamp(cursor, last);
    let matches_at = |at: usize| {
        lines[at..at + pre.len()]
            .iter()
            .zip(pre)
            .all(|(a, b)| a == b)
    };
    (0..=last - cursor)
        .flat_map(|distance| [anchor.checked_add(distance), anchor.checked_sub(distance)])
        .flatten()
        .filter(|&at| at >= cursor && at <= last)
        .find(|&at| matches_at(at))
}

pub(crate) fn apply_hunks(old: &[u8], hunks: &[Hunk]) -> Option<Vec<u8>> {
    let lines = split_lines(old);
    let (out, cursor) =
        hunks
            .iter()
            .try_fold((Vec::<u8>::new(), 0usize), |(mut out, cursor), hunk| {
                let (pre, post) = images(hunk);
                let has_context = hunk.lines.iter().any(|line| line.op == LineOp::Context);
                let expected = match hunk.old_lines.get() {
                    0 => hunk.old_start.get() as usize,
                    _ => (hunk.old_start.get() as usize).saturating_sub(1),
                };
                let at = match pre.is_empty() {
                    true => expected.clamp(cursor, lines.len()),
                    false => find_match(&lines, &pre, cursor, expected)?,
                };
                if !has_context && !pre.is_empty() && at + pre.len() != lines.len() {
                    return None;
                }
                lines
                    .get(cursor..at)?
                    .iter()
                    .for_each(|line| out.extend_from_slice(line));
                post.iter().for_each(|line| out.extend_from_slice(line));
                Some((out, at + pre.len()))
            })?;
    Some(lines.get(cursor..)?.iter().fold(out, |mut out, line| {
        out.extend_from_slice(line);
        out
    }))
}

pub(crate) fn apply_delta(base: &[u8], delta: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    let declared_base = read_size(delta, &mut pos)?;
    let declared_target = read_size(delta, &mut pos)?;
    if declared_base != base.len() as u64 || declared_target > MAX_DIFF_BLOB_BYTES {
        return None;
    }
    let mut out: Vec<u8> = Vec::with_capacity(declared_target as usize);
    std::iter::from_fn(|| {
        let opcode = *delta.get(pos)?;
        pos += 1;
        Some(match opcode {
            0 => None,
            literal if literal & 0x80 == 0 => {
                let take = literal as usize;
                delta.get(pos..pos + take).map(|bytes| {
                    pos += take;
                    out.extend_from_slice(bytes);
                })
            }
            copy => {
                let mut field = |bit: u8| -> u64 {
                    match copy & bit {
                        0 => 0,
                        _ => {
                            let byte = delta.get(pos).copied().unwrap_or(0);
                            pos += 1;
                            u64::from(byte)
                        }
                    }
                };
                let offset = field(0x01) | field(0x02) << 8 | field(0x04) << 16 | field(0x08) << 24;
                let size = match field(0x10) | field(0x20) << 8 | field(0x40) << 16 {
                    0 => 0x10000,
                    size => size,
                };
                base.get(offset as usize..(offset + size) as usize)
                    .map(|bytes| out.extend_from_slice(bytes))
            }
        })
    })
    .try_for_each(|step| step.map(|_| ()))?;
    (pos == delta.len() && out.len() as u64 == declared_target).then_some(out)
}

fn read_size(delta: &[u8], pos: &mut usize) -> Option<u64> {
    let mut shift = 0u32;
    let mut acc = 0u64;
    std::iter::from_fn(|| {
        let byte = *delta.get(*pos)?;
        *pos += 1;
        acc |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        Some(byte & 0x80 != 0)
    })
    .take(10)
    .find(|more| !more)
    .map(|_| acc)
}

enum OverlayEntry {
    Put { content: Vec<u8>, kind: EntryKind },
    Gitlink { oid: Oid },
    Removed,
}

fn subproject_content(oid: Oid) -> Vec<u8> {
    format!("Subproject commit {}\n", oid.to_hex()).into_bytes()
}

fn parse_subproject(content: &[u8]) -> Option<Oid> {
    let text = std::str::from_utf8(content).ok()?;
    Oid::from_hex(text.strip_prefix("Subproject commit ")?.trim()).ok()
}

struct FoundFile {
    content: Vec<u8>,
    kind: EntryKind,
    oid: Option<Oid>,
}

struct StepView<'r, 'a> {
    repo: &'r Repo,
    base: Oid,
    accumulated: &'a BTreeMap<RepoPath, OverlayEntry>,
    step: BTreeMap<RepoPath, OverlayEntry>,
}

impl StepView<'_, '_> {
    fn overlaid(&self, path: &RepoPath) -> Option<&OverlayEntry> {
        self.step.get(path).or_else(|| self.accumulated.get(path))
    }

    fn current(&self, path: &RepoPath) -> Result<Option<FoundFile>, ApplyError> {
        match self.overlaid(path) {
            Some(OverlayEntry::Removed) => Ok(None),
            Some(OverlayEntry::Put { content, kind }) => Ok(Some(FoundFile {
                content: content.clone(),
                kind: *kind,
                oid: Some(overlay_oid(self.repo, content)?),
            })),
            Some(OverlayEntry::Gitlink { oid }) => Ok(Some(FoundFile {
                content: subproject_content(*oid),
                kind: EntryKind::Commit,
                oid: Some(*oid),
            })),
            None => match self.repo.entry_at(self.base, path)? {
                Some(entry)
                    if matches!(
                        entry.kind,
                        EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
                    ) =>
                {
                    if self.repo.blob_size(entry.oid)? > MAX_DIFF_BLOB_BYTES {
                        return Err(ApplyError::TooLarge);
                    }
                    Ok(Some(FoundFile {
                        content: self.repo.read_blob(entry.oid)?,
                        kind: entry.kind,
                        oid: Some(entry.oid),
                    }))
                }
                Some(entry) if entry.kind == EntryKind::Commit => Ok(Some(FoundFile {
                    content: subproject_content(entry.oid),
                    kind: EntryKind::Commit,
                    oid: Some(entry.oid),
                })),
                _ => Ok(None),
            },
        }
    }

    fn occupied(&self, path: &RepoPath) -> Result<bool, ApplyError> {
        match self.overlaid(path) {
            Some(OverlayEntry::Removed) => Ok(false),
            Some(OverlayEntry::Put { .. } | OverlayEntry::Gitlink { .. }) => Ok(true),
            None => Ok(self.repo.entry_at(self.base, path)?.is_some()),
        }
    }

    fn prefix_is_file(&self, prefix: &RepoPath) -> Result<bool, ApplyError> {
        match self.overlaid(prefix) {
            Some(OverlayEntry::Removed) => Ok(false),
            Some(OverlayEntry::Put { kind, .. }) => Ok(!matches!(kind, EntryKind::Tree)),
            Some(OverlayEntry::Gitlink { .. }) => Ok(true),
            None => Ok(matches!(
                self.repo.entry_at(self.base, prefix)?,
                Some(entry) if !matches!(entry.kind, EntryKind::Tree)
            )),
        }
    }

    fn ancestor_is_file(&self, path: &RepoPath) -> Result<bool, ApplyError> {
        let parts: Vec<&str> = path.as_str().split('/').collect();
        (1..parts.len())
            .map(|end| parts[..end].join("/"))
            .try_fold(false, |blocked, prefix| {
                let prefix =
                    RepoPath::new(prefix).expect("prefix of a valid repo path is well-formed");
                Ok(blocked || self.prefix_is_file(&prefix)?)
            })
    }

    fn put(&mut self, path: &RepoPath, content: Vec<u8>, kind: EntryKind) {
        self.step
            .insert(path.clone(), OverlayEntry::Put { content, kind });
    }

    fn put_gitlink(&mut self, path: &RepoPath, oid: Oid) {
        self.step
            .insert(path.clone(), OverlayEntry::Gitlink { oid });
    }

    fn remove(&mut self, path: &RepoPath) {
        self.step.insert(path.clone(), OverlayEntry::Removed);
    }
}

fn index_matches(actual: Option<Oid>, declared: Option<Oid>) -> bool {
    matches!((actual, declared), (Some(actual), Some(declared)) if actual == declared)
}

fn overlay_oid(repo: &Repo, content: &[u8]) -> Result<Oid, ApplyError> {
    gix::objs::compute_hash(repo.git().object_hash(), gix::objs::Kind::Blob, content)
        .map(Oid::from)
        .map_err(|error| ApplyError::Git(backend(error)))
}

fn transform(
    payload: &PatchPayload,
    old: &[u8],
    old_oid: Option<Oid>,
    declared_old: Option<Oid>,
) -> Option<Vec<u8>> {
    match payload {
        PatchPayload::Text(hunks) => apply_hunks(old, hunks),
        PatchPayload::BinaryLiteral(data) => {
            index_matches(old_oid, declared_old).then(|| data.clone())
        }
        PatchPayload::BinaryDelta(delta) => index_matches(old_oid, declared_old)
            .then(|| apply_delta(old, delta))
            .flatten(),
        PatchPayload::BinaryOpaque => None,
    }
}

fn fresh_content(payload: &PatchPayload) -> Option<Vec<u8>> {
    match payload {
        PatchPayload::Text(hunks) => apply_hunks(&[], hunks),
        PatchPayload::BinaryLiteral(data) => Some(data.clone()),
        PatchPayload::BinaryDelta(_) | PatchPayload::BinaryOpaque => None,
    }
}

fn file_kind(kind: Option<EntryKind>, fallback: EntryKind) -> Option<EntryKind> {
    match kind.unwrap_or(fallback) {
        EntryKind::Tree => None,
        usable => Some(usable),
    }
}

fn stage_content(
    overlay: &mut StepView<'_, '_>,
    path: &RepoPath,
    kind: EntryKind,
    content: Vec<u8>,
) -> Option<Conflict> {
    match kind {
        EntryKind::Commit => match parse_subproject(&content) {
            Some(oid) => {
                overlay.put_gitlink(path, oid);
                None
            }
            None => Some(Conflict {
                path: path.to_string(),
                reason: ConflictReason::DoesNotApply,
            }),
        },
        _ => {
            overlay.put(path, content, kind);
            None
        }
    }
}

fn apply_file(
    overlay: &mut StepView<'_, '_>,
    file: &ParsedFile,
) -> Result<Option<Conflict>, ApplyError> {
    let conflict = |path: &str, reason: ConflictReason| {
        Ok(Some(Conflict {
            path: path.to_string(),
            reason,
        }))
    };
    let Some(path) = patch_path(&file.path) else {
        return conflict(&file.path, ConflictReason::DoesNotApply);
    };
    match &file.intent {
        FileIntent::Create => {
            let Some(kind) = file_kind(file.new_kind, EntryKind::Blob) else {
                return conflict(&file.path, ConflictReason::DoesNotApply);
            };
            if overlay.occupied(&path)? {
                return conflict(&file.path, ConflictReason::AlreadyExists);
            }
            if overlay.ancestor_is_file(&path)? {
                return conflict(&file.path, ConflictReason::DoesNotApply);
            }
            match fresh_content(&file.payload) {
                Some(content) => Ok(stage_content(overlay, &path, kind, content)),
                None => conflict(&file.path, ConflictReason::DoesNotApply),
            }
        }
        FileIntent::Delete => match overlay.current(&path)? {
            None => conflict(&file.path, ConflictReason::DoesNotExist),
            Some(found) => {
                let emptied = match &file.payload {
                    PatchPayload::BinaryOpaque => {
                        index_matches(found.oid, file.old_index).then(Vec::new)
                    }
                    payload => transform(payload, &found.content, found.oid, file.old_index),
                };
                match emptied {
                    Some(rest) if rest.is_empty() => {
                        overlay.remove(&path);
                        Ok(None)
                    }
                    _ => conflict(&file.path, ConflictReason::DoesNotApply),
                }
            }
        },
        FileIntent::Modify => match overlay.current(&path)? {
            None => conflict(&file.path, ConflictReason::DoesNotExist),
            Some(found) => {
                let Some(kind) = file_kind(file.new_kind, found.kind) else {
                    return conflict(&file.path, ConflictReason::DoesNotApply);
                };
                match evolved(&file.payload, &found, file.old_index) {
                    Some(next) => Ok(stage_content(overlay, &path, kind, next)),
                    None => conflict(&file.path, ConflictReason::DoesNotApply),
                }
            }
        },
        FileIntent::Rename { from } | FileIntent::Copy { from } => {
            let Some(source) = patch_path(from) else {
                return conflict(from, ConflictReason::DoesNotApply);
            };
            if overlay.occupied(&path)? {
                return conflict(&file.path, ConflictReason::AlreadyExists);
            }
            if overlay.ancestor_is_file(&path)? {
                return conflict(&file.path, ConflictReason::DoesNotApply);
            }
            match overlay.current(&source)? {
                None => conflict(from, ConflictReason::DoesNotExist),
                Some(found) => {
                    let Some(kind) = file_kind(file.new_kind, found.kind) else {
                        return conflict(&file.path, ConflictReason::DoesNotApply);
                    };
                    match evolved(&file.payload, &found, file.old_index) {
                        Some(next) => {
                            if matches!(&file.intent, FileIntent::Rename { .. }) {
                                overlay.remove(&source);
                            }
                            Ok(stage_content(overlay, &path, kind, next))
                        }
                        None => conflict(&file.path, ConflictReason::DoesNotApply),
                    }
                }
            }
        }
    }
}

fn evolved(
    payload: &PatchPayload,
    found: &FoundFile,
    declared_old: Option<Oid>,
) -> Option<Vec<u8>> {
    match payload {
        PatchPayload::Text(hunks) if hunks.is_empty() => Some(found.content.clone()),
        payload => transform(payload, &found.content, found.oid, declared_old),
    }
}

pub struct PatchApplier<'r> {
    repo: &'r Repo,
    base: Oid,
    accumulated: BTreeMap<RepoPath, OverlayEntry>,
}

impl<'r> PatchApplier<'r> {
    pub fn new(repo: &'r Repo, base_commit: Oid) -> Self {
        Self {
            repo,
            base: base_commit,
            accumulated: BTreeMap::new(),
        }
    }

    pub fn step(&mut self, files: &[ParsedFile]) -> Result<ApplyOutcome, ApplyError> {
        let mut view = StepView {
            repo: self.repo,
            base: self.base,
            accumulated: &self.accumulated,
            step: BTreeMap::new(),
        };
        let conflicts: Vec<Conflict> = files
            .iter()
            .map(|file| apply_file(&mut view, file))
            .collect::<Result<Vec<_>, ApplyError>>()?
            .into_iter()
            .flatten()
            .collect();
        if !conflicts.is_empty() {
            return Ok(ApplyOutcome::Conflicted(conflicts));
        }
        let step = view.step;
        let staged: Vec<StagedChange> = step
            .iter()
            .map(|(path, entry)| StagedChange {
                path: path.clone(),
                action: match entry {
                    OverlayEntry::Put { content, kind } => StagedAction::Put {
                        content: content.clone(),
                        kind: *kind,
                    },
                    OverlayEntry::Gitlink { oid } => StagedAction::PutGitlink { oid: *oid },
                    OverlayEntry::Removed => StagedAction::Remove,
                },
            })
            .collect();
        self.accumulated.extend(step);
        Ok(ApplyOutcome::Clean(staged))
    }
}

impl Repo {
    pub fn write_staged_tree(
        &self,
        base_tree: Oid,
        staged: &[StagedChange],
    ) -> Result<Oid, GitError> {
        let mut editor = self
            .git()
            .edit_tree(base_tree.object_id())
            .map_err(backend)?;
        staged
            .iter()
            .try_for_each(|change| -> Result<(), GitError> {
                match &change.action {
                    StagedAction::Put { content, kind } => {
                        let blob = self.git().write_blob(content).map_err(backend)?.detach();
                        editor
                            .upsert(change.path.as_str(), (*kind).into(), blob)
                            .map_err(backend)?;
                    }
                    StagedAction::PutGitlink { oid } => {
                        editor
                            .upsert(
                                change.path.as_str(),
                                EntryKind::Commit.into(),
                                oid.object_id(),
                            )
                            .map_err(backend)?;
                    }
                    StagedAction::Remove => {
                        editor.remove(change.path.as_str()).map_err(backend)?;
                    }
                }
                Ok(())
            })?;
        Ok(Oid::from(editor.write().map_err(backend)?.detach()))
    }

    pub fn write_commit(&self, new: &NewCommit) -> Result<Oid, GitError> {
        let message = match new.message.ends_with('\n') {
            true => new.message.clone(),
            false => format!("{}\n", new.message),
        };
        let commit = gix::objs::Commit {
            tree: new.tree.object_id(),
            parents: new
                .parents
                .iter()
                .map(|parent| parent.object_id())
                .collect(),
            author: signature(&new.author),
            committer: signature(&new.committer),
            encoding: None,
            message: message.into(),
            extra_headers: new
                .extra_headers
                .iter()
                .map(|(name, value)| {
                    (
                        name.as_str().into(),
                        gix::bstr::BString::from(value.clone()),
                    )
                })
                .collect(),
        };
        Ok(Oid::from(
            self.git().write_object(commit).map_err(backend)?.detach(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::patch::{HunkLine, LineCount, LineNumber};

    type ApplyCase<'a> = (&'a [u8], Vec<Hunk>, Option<&'a [u8]>);

    fn hunk(old_start: u32, old_lines: u32, new_start: u32, new_lines: u32, spec: &str) -> Hunk {
        let lines = spec
            .split('\n')
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (op, text) = match line.as_bytes()[0] {
                    b'-' => (LineOp::Delete, &line[1..]),
                    b'+' => (LineOp::Add, &line[1..]),
                    _ => (LineOp::Context, &line[1..]),
                };
                HunkLine {
                    op,
                    text: format!("{text}\n").into_bytes(),
                }
            })
            .collect();
        Hunk {
            old_start: LineNumber::new(old_start),
            old_lines: LineCount::new(old_lines),
            new_start: LineNumber::new(new_start),
            new_lines: LineCount::new(new_lines),
            lines,
        }
    }

    #[test]
    fn apply_hunks_tracks_position_drift_and_boundaries() {
        let cases: Vec<ApplyCase> = vec![
            (
                b"one\ntwo\nthree\n",
                vec![hunk(1, 3, 1, 3, " one\n-two\n+TWO\n three\n")],
                Some(b"one\nTWO\nthree\n".as_slice()),
            ),
            (
                b"zero\nzero\none\ntwo\nthree\n",
                vec![hunk(1, 3, 1, 3, " one\n-two\n+TWO\n three\n")],
                Some(b"zero\nzero\none\nTWO\nthree\n".as_slice()),
            ),
            (
                b"one\nTWO ALREADY\nthree\n",
                vec![hunk(1, 3, 1, 3, " one\n-two\n+TWO\n three\n")],
                None,
            ),
            (
                b"",
                vec![hunk(0, 0, 1, 2, "+alpha\n+beta\n")],
                Some(b"alpha\nbeta\n".as_slice()),
            ),
            (
                b"only\n",
                vec![hunk(1, 1, 0, 0, "-only\n")],
                Some(b"".as_slice()),
            ),
            (
                b"a\nb\nc\nd\ne\nf\ng\n",
                vec![
                    hunk(1, 2, 1, 3, " a\n+inserted\n b\n"),
                    hunk(6, 2, 7, 2, " f\n-g\n+G\n"),
                ],
                Some(b"a\ninserted\nb\nc\nd\ne\nf\nG\n".as_slice()),
            ),
            (
                b"one\ntwo\nthree\nfour\n",
                vec![hunk(2, 1, 2, 1, "-two\n+TWO\n")],
                None,
            ),
            (
                b"one\ntwo\nthree\nfour\n",
                vec![hunk(2, 1, 1, 0, "-two\n")],
                None,
            ),
            (
                b"one\ntwo\nthree\nfour\n",
                vec![hunk(4, 1, 4, 1, "-four\n+FOUR\n")],
                Some(b"one\ntwo\nthree\nFOUR\n".as_slice()),
            ),
        ];
        cases.iter().for_each(|(old, hunks, expected)| {
            assert_eq!(
                apply_hunks(old, hunks).as_deref(),
                *expected,
                "apply_hunks mismatch for {old:?}"
            );
        });
    }

    #[test]
    fn delta_application_round_trips_copy_and_insert() {
        let base = b"hello world";
        let delta: Vec<u8> = vec![11, 9, 0x90, 5, 4, b'-', b'g', b'i', b'x'];
        assert_eq!(apply_delta(base, &delta), Some(b"hello-gix".to_vec()));
        assert_eq!(apply_delta(b"wrong size base", &delta), None);
        assert_eq!(apply_delta(base, &delta[..5]), None);
    }

    #[test]
    fn unsafe_paths_are_rejected() {
        assert!(patch_path("../escape").is_none());
        assert!(patch_path("/absolute").is_none());
        assert!(patch_path("nested/../escape").is_none());
        assert!(patch_path(".git/hooks/pre-receive").is_none());
        assert!(patch_path("dir/.GIT/config").is_none());
        assert!(patch_path("").is_none());
        assert!(patch_path("src/lib.rs").is_some());
        assert!(patch_path("a b/c.txt").is_some());
    }

    #[test]
    fn subproject_text_round_trips_through_a_commit_oid() {
        let oid = Oid::from_hex("0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(parse_subproject(&subproject_content(oid)), Some(oid));
        assert_eq!(parse_subproject(b"not a subproject line\n"), None);
    }
}
