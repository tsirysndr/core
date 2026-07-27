use std::path::Path;

use gix::lock::acquire::Fail;
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::{FullName, Target, file::transaction::PackedRefs};
use knot_types::UnixSeconds;

use crate::error::{GitError, backend};
use crate::repo::{Repo, fsync_if_present};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackRefsReport {
    pub packed: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReflogReport {
    pub files: usize,
    pub dropped: usize,
}

impl Repo {
    pub fn pack_refs(&self) -> Result<PackRefsReport, GitError> {
        self.locked(|| self.pack_refs_locked())
    }

    fn pack_refs_locked(&self) -> Result<PackRefsReport, GitError> {
        clear_stale_ref_locks(self.git().git_dir());
        let references = self.git().references().map_err(backend)?;
        let edits: Vec<RefEdit> = references
            .all()
            .map_err(backend)?
            .filter_map(Result::ok)
            .filter_map(|reference| {
                let oid = reference.target().try_id()?.to_owned();
                let name: FullName = reference.name().to_owned();
                Some(RefEdit {
                    change: Change::Update {
                        log: LogChange {
                            mode: RefLog::AndReference,
                            force_create_reflog: false,
                            message: "knot pack-refs".into(),
                        },
                        expected: PreviousValue::Any,
                        new: Target::Object(oid),
                    },
                    name,
                    deref: false,
                })
            })
            .collect();
        let packed = edits.len();
        if packed == 0 {
            return Ok(PackRefsReport { packed });
        }
        let committer: Option<gix::actor::SignatureRef<'_>> = None;
        self.git()
            .refs
            .transaction()
            .packed_refs(
                PackedRefs::DeletionsAndNonSymbolicUpdatesRemoveLooseSourceReference(Box::new(
                    &self.git().objects,
                )),
            )
            .prepare(edits, Fail::Immediately, Fail::Immediately)
            .map_err(backend)?
            .commit(committer)
            .map_err(backend)?;
        let git_dir = self.git().git_dir();
        fsync_if_present(&git_dir.join("packed-refs"))?;
        fsync_if_present(&git_dir.join("refs"))?;
        fsync_if_present(git_dir)?;
        Ok(PackRefsReport { packed })
    }

    pub fn expire_reflogs(&self, floor_seconds: UnixSeconds) -> Result<ReflogReport, GitError> {
        self.with_ref_lock(|| self.expire_reflogs_locked(floor_seconds))
    }

    fn expire_reflogs_locked(&self, floor_seconds: UnixSeconds) -> Result<ReflogReport, GitError> {
        let logs_dir = self.git().git_dir().join("logs");
        if !logs_dir.exists() {
            return Ok(ReflogReport {
                files: 0,
                dropped: 0,
            });
        }
        let touched_dirs = walkdir::WalkDir::new(&logs_dir)
            .into_iter()
            .filter_map(Result::ok)
            .filter(|entry| entry.file_type().is_file())
            .map(|entry| entry.into_path())
            .filter(|path| {
                if is_maintenance_temp(path) {
                    let _ = std::fs::remove_file(path);
                    false
                } else {
                    true
                }
            })
            .try_fold(
                (0usize, 0usize, std::collections::BTreeSet::new()),
                |(files, dropped, mut dirs), path| {
                    let removed = expire_reflog_file(&path, floor_seconds)?;
                    if let Some(parent) = path.parent() {
                        dirs.insert(parent.to_path_buf());
                    }
                    Ok::<_, GitError>((files + 1, dropped + removed, dirs))
                },
            )?;
        let (files, dropped, dirs) = touched_dirs;
        dirs.iter().try_for_each(|dir| fsync_if_present(dir))?;
        Ok(ReflogReport { files, dropped })
    }
}

fn clear_stale_ref_locks(git_dir: &Path) {
    let _ = std::fs::remove_file(git_dir.join("packed-refs.lock"));
    walkdir::WalkDir::new(git_dir.join("refs"))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "lock"))
        .for_each(|entry| {
            let _ = std::fs::remove_file(entry.path());
        });
}

fn is_maintenance_temp(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.contains(".knot-tmp."))
}

fn expire_reflog_file(path: &Path, floor_seconds: UnixSeconds) -> Result<usize, GitError> {
    let raw = std::fs::read(path).map_err(|error| GitError::Maintenance(error.to_string()))?;
    if raw.is_empty() {
        return Ok(0);
    }
    let lines: Vec<&[u8]> = split_keep_lines(&raw);
    let total = lines.len();
    let last_index = total - 1;
    let kept: Vec<&[u8]> = lines
        .iter()
        .enumerate()
        .filter(|(index, line)| {
            *index == last_index
                || reflog_line_seconds(line).is_none_or(|secs| secs >= floor_seconds)
        })
        .map(|(_, line)| *line)
        .collect();
    let dropped = total - kept.len();
    if dropped == 0 {
        return Ok(0);
    }
    let rewritten: Vec<u8> = kept.concat();
    rewrite_atomic(path, &rewritten)?;
    Ok(dropped)
}

fn split_keep_lines(raw: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = 0usize;
    raw.iter().enumerate().for_each(|(index, byte)| {
        if *byte == b'\n' {
            out.push(&raw[start..=index]);
            start = index + 1;
        }
    });
    if start < raw.len() {
        out.push(&raw[start..]);
    }
    out
}

fn reflog_line_seconds(line: &[u8]) -> Option<UnixSeconds> {
    let tab = line.iter().position(|byte| *byte == b'\t')?;
    let before = &line[..tab];
    let text = std::str::from_utf8(before).ok()?;
    let mut tokens = text.split_whitespace().rev();
    let _tz = tokens.next()?;
    tokens.next()?.parse::<i64>().ok().map(UnixSeconds::new)
}

fn rewrite_atomic(path: &Path, contents: &[u8]) -> Result<(), GitError> {
    knot_resource::atomic_write_bytes(path, contents, knot_resource::FileMode::Inherited)?;
    fsync_if_present(path)
}

#[cfg(test)]
mod tests {
    use knot_types::{AuthorName, BranchName, Email, Oid, RefName, RepoDid, UnixSeconds};

    use crate::{EntryKind, Identity, Layout, NewCommit, RefUpdate, StagedAction, StagedChange};

    // ah yes, of course, little johnny 4b
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    fn identity() -> Identity {
        Identity {
            name: AuthorName::new("nel"),
            email: Email::new("nel@oyster.cafe"),
            time: UnixSeconds::new(1_700_000_000),
            offset_seconds: 0,
        }
    }

    fn commit_on(repo: &crate::Repo, body: u8, parent: Option<Oid>) -> Oid {
        let tree = repo
            .write_staged_tree(
                Oid::from_hex(EMPTY_TREE).unwrap(),
                &[StagedChange {
                    path: knot_types::RepoPath::new(format!("file{body}.txt")).unwrap(),
                    action: StagedAction::Put {
                        content: vec![body],
                        kind: EntryKind::Blob,
                    },
                }],
            )
            .unwrap();
        repo.write_commit(&NewCommit {
            tree,
            parents: parent.into_iter().collect(),
            author: identity(),
            committer: identity(),
            message: format!("commit {body}"),
            extra_headers: Vec::new(),
        })
        .unwrap()
    }

    #[test]
    fn pack_refs_moves_loose_refs_into_packed_refs_and_resolves() {
        let scan = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:squid").unwrap();
        let repo = layout.create(&did).unwrap();
        let main = RefName::new("refs/heads/main").unwrap();
        let side = RefName::new("refs/heads/side").unwrap();

        let base = commit_on(&repo, 0, None);
        let tip = commit_on(&repo, 1, Some(base));
        repo.update_ref(&RefUpdate::Create {
            name: main.clone(),
            new: tip,
        })
        .unwrap();
        repo.update_ref(&RefUpdate::Create {
            name: side.clone(),
            new: base,
        })
        .unwrap();

        let git_dir = repo.git().git_dir().to_path_buf();
        assert!(git_dir.join("refs/heads/main").exists());

        let report = repo.pack_refs().unwrap();
        assert!(report.packed >= 2, "both branches are packed");
        assert!(
            git_dir.join("packed-refs").exists(),
            "packed-refs file is written"
        );
        assert!(
            !git_dir.join("refs/heads/main").exists(),
            "loose ref file is removed once packed"
        );
        assert_eq!(repo.find_ref(&main).unwrap(), Some(tip));
        assert_eq!(repo.find_ref(&side).unwrap(), Some(base));
    }

    #[test]
    fn pack_refs_clears_stale_lock_files_left_by_a_crash() {
        let scan = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:barnacle").unwrap();
        let repo = layout.create(&did).unwrap();
        let main = RefName::new("refs/heads/main").unwrap();

        let tip = commit_on(&repo, 0, None);
        repo.update_ref(&RefUpdate::Create {
            name: main.clone(),
            new: tip,
        })
        .unwrap();

        let git_dir = repo.git().git_dir().to_path_buf();
        std::fs::write(git_dir.join("packed-refs.lock"), b"").unwrap();
        std::fs::write(git_dir.join("refs/heads/main.lock"), b"").unwrap();

        let report = repo
            .pack_refs()
            .expect("crashed prior run's stale locks mustn't wedge next pack-refs");
        assert!(report.packed >= 1);
        assert!(
            !git_dir.join("packed-refs.lock").exists(),
            "stale packed-refs lock is cleared"
        );
        assert!(
            !git_dir.join("refs/heads/main.lock").exists(),
            "stale per-ref lock is cleared"
        );
        assert_eq!(repo.find_ref(&main).unwrap(), Some(tip));
    }

    #[test]
    fn expire_reflogs_drops_old_entries_but_keeps_the_newest() {
        let scan = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:limpet").unwrap();
        let repo = layout.create(&did).unwrap();
        let main = RefName::new("refs/heads/main").unwrap();

        let c0 = commit_on(&repo, 0, None);
        let c1 = commit_on(&repo, 1, Some(c0));
        let c2 = commit_on(&repo, 2, Some(c1));
        repo.update_ref(&RefUpdate::Create {
            name: main.clone(),
            new: c0,
        })
        .unwrap();
        repo.update_ref(&RefUpdate::Update {
            name: main.clone(),
            old: c0,
            new: c1,
        })
        .unwrap();
        repo.update_ref(&RefUpdate::Update {
            name: main.clone(),
            old: c1,
            new: c2,
        })
        .unwrap();

        let log_path = repo.git().git_dir().join("logs/refs/heads/main");
        let before = std::fs::read(&log_path).unwrap();
        let line_count = before.iter().filter(|byte| **byte == b'\n').count();
        assert_eq!(line_count, 3, "three ref updates leave three reflog lines");

        let report = repo.expire_reflogs(UnixSeconds::new(i64::MAX / 2)).unwrap();
        assert!(
            report.files >= 2,
            "main branch reflog and HEAD reflog are both rewritten"
        );
        assert_eq!(
            report.dropped, 4,
            "future floor drops all but newest line of each of two reflogs"
        );
        let after = std::fs::read(&log_path).unwrap();
        assert_eq!(after.iter().filter(|byte| **byte == b'\n').count(), 1);
        assert_eq!(repo.find_ref(&main).unwrap(), Some(c2));

        let untouched = repo.expire_reflogs(UnixSeconds::new(0)).unwrap();
        assert_eq!(untouched.dropped, 0, "zero floor keeps everything");
    }
}
