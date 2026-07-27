use std::collections::HashSet;
use std::time::{Duration, SystemTime};

use knot_git::{GitError, Haves, Repo, Wants};
use knot_types::{Oid, RepoDid, UnixSeconds};

use crate::store::{DiskStore, Reclaimed, expired};
use crate::{LfsError, LfsOid, LfsSize, scan_pointers};

#[derive(Debug, thiserror::Error)]
pub enum GcError {
    #[error("git: {0}")]
    Git(#[from] GitError),
    #[error("store: {0}")]
    Store(#[from] LfsError),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GcReport {
    pub scanned: usize,
    pub marked: usize,
    pub swept: usize,
    pub bytes: LfsSize,
}

fn unix_seconds(now: SystemTime) -> UnixSeconds {
    let secs = now
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|delta| delta.as_secs() as i64)
        .unwrap_or(0);
    UnixSeconds::new(secs)
}

fn reachable_roots(repo: &Repo, floor: UnixSeconds) -> Result<Vec<Oid>, GitError> {
    let mut roots: HashSet<Oid> = repo
        .references()?
        .into_iter()
        .filter(|record| !knot_git::is_reserved(&record.name))
        .map(|record| record.target)
        .collect();
    repo.reflog_updates_since(floor)
        .into_iter()
        .for_each(|update| {
            roots.insert(update.new);
            if let Some(old) = update.old {
                roots.insert(old);
            }
        });
    Ok(roots
        .into_iter()
        .filter(|oid| repo.contains(*oid))
        .collect())
}

fn reachable_pointers(repo: &Repo, floor: UnixSeconds) -> Result<HashSet<LfsOid>, GitError> {
    let roots = reachable_roots(repo, floor)?;
    if roots.is_empty() {
        return Ok(HashSet::new());
    }
    Ok(scan_pointers(repo, Wants::new(&roots), Haves::new(&[]))?
        .into_keys()
        .collect())
}

pub fn collect_repo(
    store: &DiskStore,
    repo: &Repo,
    did: &RepoDid,
    grace: Duration,
    now: SystemTime,
) -> Result<GcReport, GcError> {
    let stored = store.enumerate(did)?;
    if stored.is_empty() {
        return Ok(GcReport::default());
    }
    let floor = unix_seconds(now).saturating_sub_secs(grace.as_secs().min(i64::MAX as u64) as i64);
    let reachable = reachable_pointers(repo, floor)?;
    stored
        .iter()
        .filter(|object| !reachable.contains(&object.oid))
        .filter(|object| expired(now, object.mtime, grace))
        .map(|object| store.collect_expired(did, &object.oid, grace, now))
        .try_fold(
            GcReport {
                scanned: stored.len(),
                ..GcReport::default()
            },
            |mut report, outcome| {
                report.marked += 1;
                if let Reclaimed::Swept(size) = outcome? {
                    report.swept += 1;
                    report.bytes = report.bytes.saturating_add(size);
                }
                Ok::<_, GcError>(report)
            },
        )
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use knot_git::{
        EntryKind, Identity, Layout, NewCommit, RefUpdate, Repo, StagedAction, StagedChange,
    };
    use knot_types::{AuthorName, BranchName, Email, Oid, RefName, RepoDid};
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::store::DiskStore;
    use crate::{ClaimedSize, LfsOid, LfsSize, LfsStore, LfsStorePath};

    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
    const DAY: Duration = Duration::from_secs(86_400);
    const MONTHS: Duration = Duration::from_secs(60 * 86_400);

    struct Fixture {
        _scan: tempfile::TempDir,
        _lfs: tempfile::TempDir,
        store: DiskStore,
        did: RepoDid,
        repo: Repo,
    }

    fn fixture() -> Fixture {
        let scan = tempfile::tempdir().unwrap();
        let lfs = tempfile::tempdir().unwrap();
        let layout = Layout::new(scan.path()).with_default_branch(BranchName::new("main").unwrap());
        let did = RepoDid::new("did:plc:squid").unwrap();
        let repo = layout.create(&did).unwrap();
        let store = DiskStore::open(LfsStorePath::new(lfs.path())).unwrap();
        Fixture {
            _scan: scan,
            _lfs: lfs,
            store,
            did,
            repo,
        }
    }

    fn who(secs: i64) -> Identity {
        Identity {
            name: AuthorName::new("nel"),
            email: Email::new("nel@oyster.cafe"),
            time: knot_types::UnixSeconds::new(secs),
            offset_seconds: 0,
        }
    }

    fn put_media(f: &Fixture, bytes: &[u8]) -> (LfsOid, LfsSize) {
        let oid = LfsOid::from_digest(Sha256::digest(bytes).into());
        let bytes_len = bytes.len() as u64;
        f.store
            .put(&f.did, &oid, ClaimedSize::new(bytes_len), &mut &bytes[..])
            .unwrap();
        (oid, LfsSize::new(bytes_len))
    }

    fn empty_commit(f: &Fixture, message: &str, secs: i64) -> Oid {
        f.repo
            .write_commit(&NewCommit {
                tree: Oid::from_hex(EMPTY_TREE).unwrap(),
                parents: Vec::new(),
                author: who(secs),
                committer: who(secs),
                message: message.to_string(),
                extra_headers: Vec::new(),
            })
            .unwrap()
    }

    fn commit_pointer(f: &Fixture, name: &str, oid: &LfsOid, size: LfsSize) -> Oid {
        let pointer =
            format!("version https://git-lfs.github.com/spec/v1\noid sha256:{oid}\nsize {size}\n")
                .into_bytes();
        let tree = f
            .repo
            .write_staged_tree(
                Oid::from_hex(EMPTY_TREE).unwrap(),
                &[StagedChange {
                    path: knot_types::RepoPath::new("clip.bin").unwrap(),
                    action: StagedAction::Put {
                        content: pointer,
                        kind: EntryKind::Blob,
                    },
                }],
            )
            .unwrap();
        let tip = f
            .repo
            .write_commit(&NewCommit {
                tree,
                parents: Vec::new(),
                author: who(1_700_000_000),
                committer: who(1_700_000_000),
                message: "add media".to_string(),
                extra_headers: Vec::new(),
            })
            .unwrap();
        f.repo
            .update_ref(&RefUpdate::Create {
                name: RefName::new(name).unwrap(),
                new: tip,
            })
            .unwrap();
        tip
    }

    fn age(f: &Fixture, oid: &LfsOid, past: Duration) {
        let path = f.store.object_file(&f.did, oid).unwrap().unwrap().1;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(SystemTime::now() - past)
            .unwrap();
    }

    fn collect(f: &Fixture, grace: Duration) -> GcReport {
        collect_repo(&f.store, &f.repo, &f.did, grace, SystemTime::now()).unwrap()
    }

    #[test]
    fn a_reachable_object_is_never_swept_regardless_of_retention_source() {
        let f = fixture();

        let (live, live_size) = put_media(&f, b"referenced media");
        commit_pointer(&f, "refs/heads/keep", &live, live_size);
        age(&f, &live, MONTHS);

        let (fresh, _) = put_media(&f, b"just uploaded, pointer still in flight");

        let (forced, forced_size) = put_media(&f, b"orphaned by a force push");
        let old = commit_pointer(&f, "refs/heads/main", &forced, forced_size);
        let replacement = empty_commit(&f, "drop media", 1_700_000_100);
        f.repo
            .update_ref(&RefUpdate::Update {
                name: RefName::new("refs/heads/main").unwrap(),
                old,
                new: replacement,
            })
            .unwrap();
        age(&f, &forced, MONTHS);

        let report = collect(&f, Duration::from_secs(14 * 86_400));
        assert_eq!(report.marked, 0, "no reachable object is ever marked");
        assert_eq!(report.swept, 0);
        [&live, &fresh, &forced].iter().for_each(|oid| {
            assert!(
                f.store.probe(&f.did, oid).unwrap().is_some(),
                "a live ref, the grace window, and the reflog old tip each keep their object"
            );
        });
    }

    #[test]
    fn an_unreferenced_expired_object_is_swept_even_after_its_branch_is_gone() {
        let f = fixture();

        let (orphan, orphan_size) = put_media(&f, b"orphaned media");
        age(&f, &orphan, MONTHS);

        let (dropped, dropped_size) = put_media(&f, b"lived on a branch that was deleted");
        let tip = commit_pointer(&f, "refs/heads/topic", &dropped, dropped_size);
        f.repo
            .update_ref(&RefUpdate::Delete {
                name: RefName::new("refs/heads/topic").unwrap(),
                old: tip,
            })
            .unwrap();
        age(&f, &dropped, MONTHS);

        let report = collect(&f, DAY);
        assert_eq!(report.marked, 2);
        assert_eq!(
            report.swept, 2,
            "a plain orphan and a deleted-branch orphan are both swept once the mtime grace expires"
        );
        assert_eq!(report.bytes, orphan_size.saturating_add(dropped_size));
        assert_eq!(f.store.probe(&f.did, &orphan).unwrap(), None);
        assert_eq!(f.store.probe(&f.did, &dropped).unwrap(), None);
    }

    #[test]
    fn a_repo_with_no_stored_objects_never_walks_git() {
        let f = fixture();
        let orphan_pointer = LfsOid::from_digest(Sha256::digest(b"pointer without bytes").into());
        commit_pointer(&f, "refs/heads/main", &orphan_pointer, LfsSize::new(21));
        assert_eq!(collect(&f, DAY), GcReport::default());
    }
}
