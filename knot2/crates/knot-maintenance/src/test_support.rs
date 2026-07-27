use knot_git::{EntryKind, Identity, NewCommit, Repo, StagedAction, StagedChange};
use knot_types::{AuthorName, Email, ObjectFormat, Oid, UnixSeconds};

use crate::{GeometricFactor, ObjectCount, Options, PruneGrace, ReflogRetention};

pub fn identity() -> Identity {
    Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    }
}

pub fn empty_tree(format: ObjectFormat) -> Oid {
    Oid::from(gix::ObjectId::empty_tree(format.kind()))
}

pub fn commit_on(repo: &Repo, empty_tree: Oid, parents: Vec<Oid>, marker: &str) -> Oid {
    let tree = repo
        .write_staged_tree(
            empty_tree,
            &[StagedChange {
                path: knot_types::RepoPath::new(format!("{marker}.txt")).unwrap(),
                action: StagedAction::Put {
                    content: marker.as_bytes().to_vec(),
                    kind: EntryKind::Blob,
                },
            }],
        )
        .unwrap();
    repo.write_commit(&NewCommit {
        tree,
        parents,
        author: identity(),
        committer: identity(),
        message: marker.to_string(),
        extra_headers: Vec::new(),
    })
    .unwrap()
}

pub fn options() -> Options {
    Options {
        repack_max_objects: ObjectCount::new(1_000_000),
        geometric_factor: GeometricFactor::full_repack(),
        prune_grace: PruneGrace::from_secs(0),
        reflog_floor: ReflogRetention::from_secs(i64::MAX as u64 / 4),
        commit_graph: true,
        multi_pack_index: true,
        bitmap: true,
    }
}
