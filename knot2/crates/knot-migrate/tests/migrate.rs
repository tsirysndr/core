use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use knot_index::{Index, Resolved};
use knot_migrate::adopt;
use knot_migrate::casbin;
use knot_migrate::emit::MasterKeyEnv;
use knot_migrate::emit::{self, ConfigValues};
use knot_migrate::mapping::{self, SkipReason};
use knot_migrate::report;
use knot_migrate::source::{
    SourceDb, SourceDid, SourceError, SourceRepoDid, SourceRkey, SourceSchema,
};
use knot_runtime::{K256Signer, SeededEntropy};
use knot_types::{AccountDid, KnotHostname, KnotId, ObjectFormat, RepoDid};
use url::Url;

const SCHEMA: &str = "
create table repo_keys (
    repo_did text primary key,
    signing_key blob,
    created_at text not null,
    owner_did text,
    repo_name text,
    key_type text not null default 'k256'
);
create table repo_aliases (
    owner_did text not null,
    rkey text not null,
    repo_did text not null,
    rev text not null,
    primary key (owner_did, rkey)
);
create table knot_members (
    id integer primary key autoincrement,
    did text not null,
    rkey text,
    subject text not null,
    created text not null
);
create table collaborators (
    id integer primary key autoincrement,
    repo_did text not null,
    subject_did text not null,
    added_by_did text not null,
    created text not null
);
create table acl (
    p_type varchar(32) default '' not null,
    v0 varchar(255) default '' not null,
    v1 varchar(255) default '' not null,
    v2 varchar(255) default '' not null,
    v3 varchar(255) default '' not null,
    v4 varchar(255) default '' not null,
    v5 varchar(255) default '' not null
);
";

fn fixture_db(path: &Path, with_collaborators_table: bool) {
    let conn = rusqlite::Connection::open(path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute_batch(
        "
        insert into repo_keys (repo_did, signing_key, created_at, owner_did, repo_name) values
          ('did:plc:squid',  x'0101010101010101010101010101010101010101010101010101010101010101', '2026-01-05T10:00:00Z', 'did:plc:nel',  'anemone'),
          ('did:plc:limpet', x'0202020202020202020202020202020202020202020202020202020202020202', '2026-02-01T09:30:00Z', 'did:plc:nel',  'barnacle'),
          ('did:plc:conch',  x'0303030303030303030303030303030303030303030303030303030303030303', '2026-03-10T14:00:00Z', 'did:plc:isabel', 'Test knot'),
          ('did:plc:whelk',  x'0404040404040404040404040404040404040404040404040404040404040404', '2026-04-20T08:15:00Z', 'did:plc:isabel', 'mussel'),
          ('did:plc:nautilus', x'0505050505050505050505050505050505050505050505050505050505050505', '2026-05-01T10:00:00Z', 'did:plc:isabel', 'coralline'),
          ('did:plc:scallop',  x'0606060606060606060606060606060606060606060606060606060606060606', '2026-05-02T10:00:00Z', 'did:plc:isabel', 'seagrass'),
          ('did:plc:clam',     x'0707070707070707070707070707070707070707070707070707070707070707', '2026-05-03T10:00:00Z', 'did:plc:isabel', '&#124;'),
          ('did:web:nel.pet',  x'0808080808080808080808080808080808080808080808080808080808080808', '2026-06-01T10:00:00Z', 'did:plc:nel',    'seashell');
        insert into repo_aliases (owner_did, rkey, repo_did, rev) values
          ('did:plc:nel',  'anemone-old', 'did:plc:squid', '1_2026-01-05T10:00:00Z'),
          ('did:plc:nel',  'anemone',     'did:plc:squid', '3mq2bmuwq7v2t'),
          ('did:plc:isabel', 'Test knot',   'did:plc:conch', '3mniy6vtxn22y'),
          ('did:plc:isabel', 'mussel',      'did:plc:whelk', '3moo4vihsva2t'),
          ('did:plc:isabel', 'seagrass',    'did:plc:nautilus', '3mpwduty3pw2z'),
          ('did:plc:isabel', '&#124;',      'did:plc:clam', '3mq3bmuwq7v2t'),
          ('did:plc:nel',  'vanished',    'did:plc:kelp', '3mq4bmuwq7v2t');
        insert into knot_members (did, subject, created) values
          ('did:plc:bailey', 'did:plc:nel', '2026-01-02T00:00:00Z'),
          ('did:plc:nel',    'did:plc:teq', '2026-01-03T00:00:00Z'),
          ('did:plc:bailey', 'did:plc:teq', '2026-01-04T00:00:00Z'),
          ('did:plc:nel',    'did:plc:olaren', '2026-01-05T00:00:00Z'),
          ('did:plc:teq',    'did:plc:bailey', '2026-01-06T00:00:00Z');
        insert into acl (p_type, v0, v1, v2, v3) values
          ('g', 'did:plc:bailey', 'server:owner',  'thisserver', ''),
          ('g', 'server:owner',   'server:member', 'thisserver', ''),
          ('g', 'did:plc:bailey', 'server:member', 'thisserver', ''),
          ('g', 'did:plc:nel',    'server:member', 'thisserver', ''),
          ('g', 'did:plc:teq',    'server:member', 'thisserver', ''),
          ('g', 'did:plc:uni',    'server:member', 'thisserver', ''),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:squid',  'repo:owner'),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:squid',  'repo:push'),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:squid',  'repo:settings'),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:squid',  'repo:invite'),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:squid',  'repo:delete'),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:limpet', 'repo:owner'),
          ('p', 'did:plc:bailey', 'thisserver', 'did:plc:limpet', 'repo:owner'),
          ('p', 'did:plc:isabel', 'thisserver', 'did:plc:whelk',  'repo:owner'),
          ('p', 'did:plc:isabel', 'thisserver', 'did:plc:nautilus', 'repo:owner'),
          ('p', 'did:plc:isabel', 'thisserver', 'did:plc:scallop',  'repo:owner'),
          ('p', 'did:plc:isabel', 'thisserver', 'did:plc:clam',     'repo:owner'),
          ('p', 'did:plc:nel',  'thisserver', 'did:plc:kelp',   'repo:owner'),
          ('p', 'did:plc:isabel',   'thisserver', 'did:plc:squid',  'repo:collaborator'),
          ('p', 'did:plc:teq',    'thisserver', 'did:plc:limpet', 'repo:collaborator'),
          ('p', 'did:plc:uni',    'thisserver', 'did:plc:whelk',  'repo:collaborator'),
          ('p', 'did:plc:cuttle', 'thisserver', 'did:plc:kelp',   'repo:collaborator'),
          ('p', 'did:plc:periwinkle', 'thisserver', 'did:plc:nel/anemone',  'repo:collaborator'),
          ('p', 'did:plc:teq',        'thisserver', 'did:plc:nel/anemone',  'repo:collaborator'),
          ('p', 'did:plc:nel',        'thisserver', 'did:plc:nel/vanished', 'repo:owner'),
          ('p', 'did:plc:nel',    'thisserver', 'did:web:nel.pet', 'repo:owner'),
          ('p', 'did:plc:nel', 'thisserver', '', 'repo:create'),
          ('p', 'did:plc:nel', 'thisserver', '', 'server:invite');
        ",
    )
    .unwrap();
    if with_collaborators_table {
        conn.execute_batch(
            "
            insert into collaborators (repo_did, subject_did, added_by_did, created) values
              ('did:plc:squid', 'did:plc:isabel',   'did:plc:nel', '2026-01-06T11:00:00Z'),
              ('did:plc:squid', 'did:plc:olaren', 'did:plc:nel', '2026-01-07T12:00:00Z'),
              ('did:plc:kelp',  'did:plc:teq',    'did:plc:nel', '2026-01-08T13:00:00Z'),
              ('did:plc:squid', 'did:plc:isabel',   'did:plc:bailey', '2026-01-09T14:00:00Z'),
              ('did:plc:whelk', 'did:plc:uni',    'did:plc:isabel', '2026-01-10T15:00:00Z'),
              ('did:plc:squid', 'did:plc:teq',    'did:plc:nel', '2026-01-11T16:00:00Z');
            ",
        )
        .unwrap();
    } else {
        conn.execute_batch("drop table collaborators;").unwrap();
    }
}

struct Fixture {
    _dir: tempfile::TempDir,
    db_path: PathBuf,
    source_repos: PathBuf,
    target: PathBuf,
}

fn fixture(with_collaborators_table: bool) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("knotserver.db");
    fixture_db(&db_path, with_collaborators_table);
    let source_repos = dir.path().join("source-repos");
    [
        "did:plc:squid",
        "did:plc:limpet",
        "did:plc:conch",
        "did:plc:nautilus",
        "did:plc:scallop",
        "did:plc:clam",
        "did:web:nel.pet",
    ]
    .iter()
    .for_each(|did| {
        knot_git::Repo::create_with_format(source_repos.join(did), ObjectFormat::SHA1).unwrap();
    });
    std::fs::create_dir_all(source_repos.join("did:plc:whelk")).unwrap();
    Fixture {
        db_path,
        source_repos,
        target: dir.path().join("target"),
        _dir: dir,
    }
}

fn map(fx: &Fixture) -> mapping::Mapping {
    map_probing(fx, |did| adopt::probe_source(&fx.source_repos, did))
}

fn map_probing(
    fx: &Fixture,
    probe: impl Fn(&SourceRepoDid) -> adopt::SourceProbe,
) -> mapping::Mapping {
    let db = SourceDb::open(&fx.db_path).unwrap();
    let repos = db.repos().unwrap();
    let rkeys: BTreeMap<SourceRepoDid, SourceRkey> = repos
        .iter()
        .filter_map(|repo| {
            db.current_rkey(&repo.repo_did)
                .unwrap()
                .map(|rkey| (repo.repo_did.clone(), rkey))
        })
        .collect();
    let resolver = casbin::resolver(repos.iter().map(|repo| {
        (
            repo.owner_did.clone(),
            repo.repo_name.clone(),
            repo.repo_did.clone(),
        )
    }));
    let acl = casbin::decode(&db.acl().unwrap(), &resolver).unwrap();
    match db.schema().unwrap() {
        SourceSchema::Tables => mapping::map_tables(
            &repos,
            &rkeys,
            &db.members().unwrap(),
            &db.collaborators().unwrap(),
            &acl,
            probe,
        )
        .unwrap(),
        SourceSchema::PreFlip => {
            mapping::map_preflip(&repos, &rkeys, &db.members().unwrap(), &acl, probe).unwrap()
        }
    }
}

fn acc(suffix: &str) -> AccountDid {
    AccountDid::new(format!("did:plc:{suffix}")).unwrap()
}

fn srepo(value: &str) -> SourceRepoDid {
    SourceRepoDid::from_column(value)
}

fn sdid(value: &str) -> SourceDid {
    SourceDid::from_column(value)
}

fn subjects(grants: &[mapping::MappedGrant]) -> Vec<&str> {
    grants.iter().map(|grant| grant.subject.as_str()).collect()
}

#[test]
fn table_mapping_reproduces_roster_and_drift() {
    let fx = fixture(true);
    let db = SourceDb::open(&fx.db_path).unwrap();
    assert_eq!(db.schema().unwrap(), SourceSchema::Tables);
    assert_eq!(db.orphan_alias_count().unwrap(), 1);

    let mapping = map(&fx);
    assert_eq!(mapping.knot_owner, acc("bailey"));

    assert_eq!(
        subjects(&mapping.members),
        [
            "did:plc:nel",
            "did:plc:teq",
            "did:plc:olaren",
            "did:plc:uni"
        ]
    );
    let teq = &mapping.members[1];
    assert_eq!(teq.added_by, acc("nel"));
    let uni = &mapping.members[3];
    assert!(uni.unioned);
    assert_eq!(uni.added_by, acc("bailey"));

    let dids: Vec<&str> = mapping.repos.iter().map(|repo| repo.did.as_str()).collect();
    assert_eq!(
        dids,
        [
            "did:plc:squid",
            "did:plc:limpet",
            "did:plc:nautilus",
            "did:web:nel.pet"
        ]
    );
    let web = &mapping.repos[3];
    assert_eq!(web.rkey.as_str(), "seashell");
    assert_eq!(web.name.as_str(), "seashell");
    assert!(web.collaborators.is_empty());
    let nautilus = &mapping.repos[2];
    assert_eq!(nautilus.rkey.as_str(), "seagrass");
    assert_eq!(nautilus.name.as_str(), "coralline");
    let squid = &mapping.repos[0];
    assert_eq!(squid.rkey.as_str(), "anemone");
    assert_eq!(
        subjects(&squid.collaborators),
        ["did:plc:isabel", "did:plc:olaren", "did:plc:teq"]
    );
    assert_eq!(squid.collaborators[0].added_by, acc("nel"));
    let limpet = &mapping.repos[1];
    assert_eq!(limpet.rkey.as_str(), "barnacle");
    assert_eq!(subjects(&limpet.collaborators), ["did:plc:teq"]);
    assert!(limpet.collaborators[0].unioned);
    assert_eq!(limpet.collaborators[0].added_by, acc("nel"));

    let reasons: Vec<(&str, &SkipReason)> = mapping
        .skipped
        .iter()
        .map(|skip| (skip.repo_did.as_str(), &skip.reason))
        .collect();
    assert_eq!(reasons.len(), 4);
    assert!(matches!(
        reasons[0],
        ("did:plc:conch", SkipReason::Name { .. })
    ));
    assert!(
        matches!(reasons[1], ("did:plc:whelk", SkipReason::NoSourceRepo)),
        "a source directory that exists but holds no git repository skips the repo"
    );
    match reasons[2] {
        ("did:plc:clam", SkipReason::Rkey { value }) => {
            assert_eq!(
                value.as_str(),
                "&#124;",
                "an rkey RepoRkey rejects skips the repo even where RepoName accepts the same text"
            );
        }
        other => panic!("unexpected third skip {other:?}"),
    }
    match reasons[3] {
        ("did:plc:scallop", SkipReason::RkeyCollision { rkey, winner }) => {
            assert_eq!(rkey.as_str(), "seagrass");
            assert_eq!(winner.as_str(), "did:plc:nautilus");
        }
        other => panic!("unexpected fourth skip {other:?}"),
    }
    let lost: Vec<Vec<&str>> = mapping
        .skipped
        .iter()
        .map(|skip| {
            skip.lost_collaborators
                .iter()
                .map(AccountDid::as_str)
                .collect()
        })
        .collect();
    assert_eq!(lost, [vec![], vec!["did:plc:uni"], vec![], vec![]]);

    let drift = &mapping.drift;
    assert_eq!(
        drift.acl_only_collaborators,
        [(srepo("did:plc:limpet"), sdid("did:plc:teq"))]
    );
    assert_eq!(
        drift.table_only_collaborators,
        [(srepo("did:plc:squid"), sdid("did:plc:olaren"))]
    );
    assert_eq!(
        drift.slash_resolved_collaborators,
        [(srepo("did:plc:squid"), sdid("did:plc:periwinkle"))]
    );
    assert_eq!(
        drift.orphan_collaborator_pairs,
        [
            (srepo("did:plc:kelp"), sdid("did:plc:cuttle")),
            (srepo("did:plc:kelp"), sdid("did:plc:teq"))
        ]
    );
    assert_eq!(drift.markerless_owner_repos, [srepo("did:plc:conch")]);
    assert_eq!(drift.orphan_owner_markers, [srepo("did:plc:kelp")]);
    assert_eq!(
        drift.extra_owner_markers,
        [(srepo("did:plc:limpet"), sdid("did:plc:bailey"))]
    );
    assert_eq!(drift.acl_only_members, [sdid("did:plc:uni")]);
    assert_eq!(drift.table_only_members, [sdid("did:plc:olaren")]);
    assert_eq!(drift.slash_owner_markers, 1);
    assert_eq!(drift.slash_collab_rows, 2);
    assert_eq!(drift.unresolved_slash_forms, ["did:plc:nel/vanished"]);
}

#[test]
fn collaborators_without_knot_members_is_rejected() {
    let fx = fixture(true);
    rusqlite::Connection::open(&fx.db_path)
        .unwrap()
        .execute_batch("drop table knot_members;")
        .unwrap();
    let db = SourceDb::open(&fx.db_path).unwrap();
    assert!(matches!(
        db.schema(),
        Err(SourceError::CollaboratorsWithoutMembers)
    ));
}

#[test]
fn a_source_table_missing_expected_columns_is_rejected_early() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("knotserver.db");
    rusqlite::Connection::open(&db_path)
        .unwrap()
        .execute_batch(
            "
            create table repo_keys (
                repo_did text primary key,
                signing_key blob,
                created_at text not null
            );
            create table repo_aliases (
                owner_did text not null,
                rkey text not null,
                repo_did text not null,
                rev text not null
            );
            ",
        )
        .unwrap();
    let db = SourceDb::open(&db_path).unwrap();
    match db.schema() {
        Err(SourceError::SchemaMismatch { table, missing }) => {
            assert_eq!(table, "repo_keys");
            assert_eq!(missing, ["owner_did", "repo_name", "key_type"]);
        }
        other => panic!("a repo_keys older than knot's schema must be rejected, got {other:?}"),
    }
}

#[test]
fn preflip_mapping_reads_casbin() {
    let fx = fixture(false);
    let db = SourceDb::open(&fx.db_path).unwrap();
    assert_eq!(db.schema().unwrap(), SourceSchema::PreFlip);

    let mapping = map(&fx);
    assert_eq!(mapping.knot_owner, acc("bailey"));
    assert_eq!(
        subjects(&mapping.members),
        ["did:plc:nel", "did:plc:teq", "did:plc:uni"]
    );
    let nel = &mapping.members[0];
    assert_eq!(nel.added_by, acc("bailey"));
    assert_eq!(nel.created_at, knot_types::UnixSeconds::new(1767312000));
    let uni = &mapping.members[2];
    assert_eq!(uni.added_by, acc("bailey"));
    assert_eq!(uni.created_at, knot_types::UnixSeconds::new(0));
    assert_eq!(mapping.drift.table_only_members, [sdid("did:plc:olaren")]);

    let squid = &mapping.repos[0];
    assert_eq!(
        subjects(&squid.collaborators),
        ["did:plc:isabel", "did:plc:periwinkle", "did:plc:teq"]
    );
    let limpet = &mapping.repos[1];
    assert_eq!(subjects(&limpet.collaborators), ["did:plc:teq"]);
    assert_eq!(
        mapping.drift.orphan_collaborator_pairs,
        [(srepo("did:plc:kelp"), sdid("did:plc:cuttle"))]
    );
    assert_eq!(
        mapping.drift.extra_owner_markers,
        [(srepo("did:plc:limpet"), sdid("did:plc:bailey"))]
    );
    let whelk = mapping
        .skipped
        .iter()
        .find(|skip| skip.repo_did.as_str() == "did:plc:whelk")
        .unwrap();
    assert_eq!(whelk.lost_collaborators, [acc("uni")]);
}

#[test]
fn adoption_and_cobs_boot_a_working_index() {
    let fx = fixture(true);
    let mapping = map(&fx);
    let knot = KnotId::new("did:web:knot.oyster.cafe").unwrap();
    let scan_path = fx.target.join("repos");
    std::fs::create_dir_all(&scan_path).unwrap();
    let layout = knot_git::Layout::new(&scan_path)
        .with_object_format(ObjectFormat::SHA1)
        .reserving_meta(&knot)
        .unwrap();
    let signer = K256Signer::generate(&SeededEntropy::new(7));
    std::os::unix::fs::symlink(
        "config",
        fx.source_repos.join("did:plc:squid").join("config-link"),
    )
    .unwrap();

    let adoption = adopt::adopt_all(
        &layout,
        &fx.source_repos,
        &mapping.repos,
        adopt::SourcePolicy::Preserve,
    )
    .unwrap();
    assert_eq!(adoption.adopted, 4);
    assert_eq!(adoption.transfer, adopt::Transfer::Copy);
    assert_eq!(adoption.already_present, 0);
    assert_eq!(adoption.sha1, 4);
    let adopted_link = layout
        .repo_path(&RepoDid::new("did:plc:squid").unwrap())
        .unwrap()
        .join("config-link");
    assert!(
        std::fs::symlink_metadata(&adopted_link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        std::fs::read_link(&adopted_link).unwrap(),
        Path::new("config")
    );

    let cobs = emit::write_cobs(&layout, &knot, &mapping, &signer).unwrap();
    assert_eq!(cobs.members.appended, 4);
    assert_eq!(cobs.registrations.appended, 4);
    assert_eq!(cobs.collaborators.appended, 4);

    let index = Index::new(layout.meta_path(&knot).unwrap(), layout.clone());
    index.rebuild().unwrap();
    assert_eq!(index.hosted_repos().len(), 4);
    let web = RepoDid::new("did:web:nel.pet").unwrap();
    assert_eq!(
        index.owner_of(&web),
        Resolved::Ready(Some(knot_types::OwnerDid::new("did:plc:nel").unwrap()))
    );
    let squid = RepoDid::new("did:plc:squid").unwrap();
    index.ensure_collaborators(&squid).unwrap();
    assert_eq!(
        index.is_collaborator(&squid, &acc("isabel")),
        Resolved::Ready(true)
    );
    assert_eq!(
        index.is_collaborator(&squid, &acc("olaren")),
        Resolved::Ready(true)
    );
    assert_eq!(
        index.is_collaborator(&squid, &acc("teq")),
        Resolved::Ready(true)
    );
    assert_eq!(
        index.is_collaborator(&squid, &acc("periwinkle")),
        Resolved::Ready(false)
    );
    assert_eq!(
        index.owner_of(&squid),
        Resolved::Ready(Some(knot_types::OwnerDid::new("did:plc:nel").unwrap()))
    );

    let again = adopt::adopt_all(
        &layout,
        &fx.source_repos,
        &mapping.repos,
        adopt::SourcePolicy::Preserve,
    )
    .unwrap();
    assert_eq!(again.adopted, 0);
    assert_eq!(again.already_present, 4);
    let recobs = emit::write_cobs(&layout, &knot, &mapping, &signer).unwrap();
    assert_eq!(recobs.members.appended, 0);
    assert_eq!(recobs.members.already_present, 4);
    assert_eq!(recobs.registrations.appended, 0);
    assert_eq!(recobs.registrations.already_present, 4);
    assert_eq!(recobs.collaborators.appended, 0);
    assert_eq!(recobs.collaborators.already_present, 4);
}

#[test]
fn consuming_the_source_moves_each_adopted_repo_out_of_the_scan_path() {
    let fx = fixture(true);
    let mapping = map(&fx);
    let knot = KnotId::new("did:web:knot.oyster.cafe").unwrap();
    let scan_path = fx.target.join("repos");
    std::fs::create_dir_all(&scan_path).unwrap();
    let layout = knot_git::Layout::new(&scan_path)
        .with_object_format(ObjectFormat::SHA1)
        .reserving_meta(&knot)
        .unwrap();

    let adoption = adopt::adopt_all(
        &layout,
        &fx.source_repos,
        &mapping.repos,
        adopt::SourcePolicy::Consume,
    )
    .unwrap();
    assert_eq!(adoption.transfer, adopt::Transfer::Rename);
    assert_eq!(adoption.adopted, 4);
    assert_eq!(adoption.already_present, 0);
    assert!(
        mapping
            .repos
            .iter()
            .all(|repo| !adopt::source_dir(&fx.source_repos, &repo.source_did).exists()),
        "every adopted repo leaves the source tree"
    );
    assert!(
        fx.source_repos.join("did:plc:conch").is_dir(),
        "a skipped repo stays where it was"
    );

    let again = adopt::adopt_all(
        &layout,
        &fx.source_repos,
        &mapping.repos,
        adopt::SourcePolicy::Consume,
    )
    .unwrap();
    assert_eq!(again.adopted, 0);
    assert_eq!(again.already_present, 4);
}

#[test]
fn adopting_a_repo_that_resolves_to_the_knot_meta_path_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("knotserver.db");
    let conn = rusqlite::Connection::open(&db_path).unwrap();
    conn.execute_batch(SCHEMA).unwrap();
    conn.execute_batch(
        "
        insert into repo_keys (repo_did, signing_key, created_at, owner_did, repo_name) values
          ('did:web:knot.oyster.cafe', x'0909090909090909090909090909090909090909090909090909090909090909', '2026-06-01T10:00:00Z', 'did:plc:nel', 'seashell');
        insert into acl (p_type, v0, v1, v2, v3) values
          ('g', 'did:plc:bailey', 'server:owner', 'thisserver', '');
        ",
    )
    .unwrap();

    let source_repos = dir.path().join("source-repos");
    knot_git::Repo::create_with_format(
        source_repos.join("did:web:knot.oyster.cafe"),
        ObjectFormat::SHA1,
    )
    .unwrap();

    let fx = Fixture {
        db_path,
        source_repos,
        target: dir.path().join("target"),
        _dir: dir,
    };
    let mapping = map(&fx);
    assert_eq!(mapping.repos.len(), 1);

    let knot = KnotId::new("did:web:knot.oyster.cafe").unwrap();
    let scan_path = fx.target.join("repos");
    std::fs::create_dir_all(&scan_path).unwrap();
    let layout = knot_git::Layout::new(&scan_path)
        .with_object_format(ObjectFormat::SHA1)
        .reserving_meta(&knot)
        .unwrap();

    let error = adopt::adopt_all(
        &layout,
        &fx.source_repos,
        &mapping.repos,
        adopt::SourcePolicy::Preserve,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        adopt::AdoptError::ReservesMeta { repo } if repo.as_str() == "did:web:knot.oyster.cafe"
    ));
    assert!(!layout.meta_path(&knot).unwrap().exists());
}

#[test]
fn rendered_config_loads() {
    let dir = tempfile::tempdir().unwrap();
    let scan_path = dir.path().join("re\"pos\u{7f}");
    std::fs::create_dir_all(&scan_path).unwrap();
    let config = emit::render_config(&ConfigValues {
        hostname: KnotHostname::new("knot.oyster.cafe").unwrap(),
        admins: vec![acc("bailey")],
        scan_path: scan_path.clone(),
        ssh_host_key_file: dir.path().join("ssh_host_key"),
        sealed_key_file: dir.path().join("sealed-keys"),
        master_key_env: MasterKeyEnv::new("KNOT_MASTER_KEY").unwrap(),
        object_format: ObjectFormat::SHA1,
        plc_directory: Url::parse("https://plc.directory").unwrap(),
    })
    .unwrap();
    assert!(config.contains("hostname = \"knot.oyster.cafe\""));
    assert!(config.contains("admins = [\"did:plc:bailey\"]"));
    assert!(config.contains("admission = \"closed\""));
    assert!(config.contains("object_format = \"sha1\""));
    let path = dir.path().join("config.toml");
    std::fs::write(&path, &config).unwrap();
    knot_config::load(Some(&path)).unwrap();
}

const HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAMwAAAAtzc2gtZW
QyNTUxOQAAACAuLv0N4MHTuclN/afhoL60chkky1gCLCFCA2T1qOKGhwAAAJgv0qFlL9Kh
ZQAAAAtzc2gtZWQyNTUxOQAAACAuLv0N4MHTuclN/afhoL60chkky1gCLCFCA2T1qOKGhw
AAAEAnnapXprdwlEwD6xIxSqm3szQrvfQdhRp6UfONp85Uky4u/Q3gwdO5yU39p+GgvrRy
GSTLWAIsIUIDZPWo4oaHAAAAEWtub3QtbWlncmF0ZS10ZXN0AQIDBA==
-----END OPENSSH PRIVATE KEY-----
";

const ECDSA_HOST_KEY: &str = "-----BEGIN OPENSSH PRIVATE KEY-----
b3BlbnNzaC1rZXktdjEAAAAABG5vbmUAAAAEbm9uZQAAAAAAAAABAAAAaAAAABNlY2RzYS
1zaGEyLW5pc3RwMjU2AAAACG5pc3RwMjU2AAAAQQS6SLu5jEz+0ScKcByJBs53LlSkz8dT
ELlhV5QrNPQvk+h5UduwxR7ShN3IL9AhjiVugVN3I9vHHB1BwcNXE6exAAAAsD5jy3k+Y8
t5AAAAE2VjZHNhLXNoYTItbmlzdHAyNTYAAAAIbmlzdHAyNTYAAABBBLpIu7mMTP7RJwpw
HIkGzncuVKTPx1MQuWFXlCs09C+T6HlR27DFHtKE3cgv0CGOJW6BU3cj28ccHUHBw1cTp7
EAAAAhAI2ARTG/6mM9qJfmdg8rASQudcrZ5KFLkH6FjB0V6EUgAAAAEWtub3QtbWlncmF0
ZS10ZXN0AQIDBAUG
-----END OPENSSH PRIVATE KEY-----
";

#[test]
fn host_key_import_preserves_every_algorithm() {
    let dir = tempfile::tempdir().unwrap();
    [
        (
            "ssh_host_ed25519_key",
            HOST_KEY,
            ssh_key::Algorithm::Ed25519,
        ),
        (
            "ssh_host_ecdsa_key",
            ECDSA_HOST_KEY,
            ssh_key::Algorithm::Ecdsa {
                curve: ssh_key::EcdsaCurve::NistP256,
            },
        ),
    ]
    .into_iter()
    .for_each(|(name, pem, algorithm)| {
        let source = dir.path().join(name);
        std::fs::write(&source, pem).unwrap();
        let destination = dir.path().join(format!("{name}.imported"));
        let host_key = emit::load_host_key(&source).unwrap();
        assert_eq!(host_key.algorithm, algorithm);
        host_key.write_to(&destination).unwrap();
        assert_eq!(
            std::fs::read(&source).unwrap(),
            std::fs::read(&destination).unwrap(),
            "{name} must round-trip byte-for-byte so the pinned fingerprint survives"
        );
    });
}

fn honors_permission_bits(dir: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let probe = dir.join("permission-probe");
    std::fs::create_dir(&probe).unwrap();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o000)).unwrap();
    let denied = std::fs::read_dir(&probe).is_err();
    std::fs::set_permissions(&probe, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::remove_dir(&probe).unwrap();
    if !denied {
        eprintln!(
            "skipping the permission case: this process reads a 0000 directory, so it's running \
             as root"
        );
    }
    denied
}

#[test]
fn probing_a_source_separates_a_repo_from_an_absence_and_from_an_unreadable_path() {
    let fx = fixture(true);
    std::fs::write(fx.source_repos.join("did:plc:mussel"), "").unwrap();
    let looped = fx.source_repos.join("did:plc:cuttle");
    std::os::unix::fs::symlink(&looped, &looped).unwrap();
    let probe = |did: &str| adopt::probe_source(&fx.source_repos, &srepo(did));
    assert_eq!(probe("did:plc:squid"), adopt::SourceProbe::Repo);
    assert_eq!(
        probe("did:plc:kelp"),
        adopt::SourceProbe::Absent,
        "a repo whose directory an operator deleted mustn't refuse every future migration"
    );
    assert_eq!(
        probe("did:plc:whelk"),
        adopt::SourceProbe::Absent,
        "did:plc:whelk has a readable directory without a HEAD, which the mapping skips on its own"
    );
    assert_eq!(
        probe("did:plc:mussel"),
        adopt::SourceProbe::Absent,
        "a regular file where a repo directory belongs isn't a repository either"
    );
    assert!(
        matches!(probe("did:plc:cuttle"), adopt::SourceProbe::Unreadable(_)),
        "root can't step over a symlink loop, so this case covers the unreadable path under any uid"
    );
}

#[test]
fn a_source_directory_that_the_process_cannot_enter_probes_unreadable() {
    let fx = fixture(true);
    if !honors_permission_bits(&fx.source_repos) {
        return;
    }
    use std::os::unix::fs::PermissionsExt;
    let squid = fx.source_repos.join("did:plc:squid");
    std::fs::set_permissions(&squid, std::fs::Permissions::from_mode(0o000)).unwrap();
    let probed = adopt::probe_source(&fx.source_repos, &srepo("did:plc:squid"));
    std::fs::set_permissions(&squid, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        probed,
        adopt::SourceProbe::Unreadable(std::io::ErrorKind::PermissionDenied),
        "secure mode leaves repo trees at a per-owner uid, and that's how a wrong-user migration \
         sees them"
    );
}

#[test]
fn an_unreadable_source_is_skipped_separately_from_an_absent_source() {
    let fx = fixture(true);
    let mapping = map_probing(&fx, |did| match did.as_str() {
        "did:plc:squid" => adopt::SourceProbe::Unreadable(std::io::ErrorKind::PermissionDenied),
        "did:plc:limpet" => {
            adopt::SourceProbe::Unreadable(std::io::ErrorKind::StaleNetworkFileHandle)
        }
        _ => adopt::SourceProbe::Repo,
    });
    let reason = |wanted: &str| {
        mapping
            .skipped
            .iter()
            .find(|skip| skip.repo_did.as_str() == wanted)
            .map(|skip| skip.reason.clone())
    };
    assert_eq!(
        reason("did:plc:squid"),
        Some(SkipReason::UnreadableSource {
            kind: std::io::ErrorKind::PermissionDenied
        }),
        "the report mustn't call an unreadable repo missing"
    );
    assert_eq!(
        reason("did:plc:limpet"),
        Some(SkipReason::UnreadableSource {
            kind: std::io::ErrorKind::StaleNetworkFileHandle
        }),
        "an error that isn't a permission error mustn't lose the repo either"
    );
    assert_eq!(
        reason("did:plc:kelp"),
        None,
        "did:plc:kelp has an alias and an acl grant without a repo_keys row"
    );
    assert_eq!(
        mapping.unreadable_sources().to_string(),
        "did:plc:squid, did:plc:limpet"
    );
    assert!(!mapping.unreadable_sources().is_empty());
    let readable = map(&fx);
    assert!(readable.unreadable_sources().is_empty());
    assert!(
        readable
            .skipped
            .iter()
            .any(|skip| skip.reason == SkipReason::NoSourceRepo),
        "did:plc:whelk has a directory without a HEAD in it, which stays a plain absence"
    );
}

#[test]
fn naming_unreadable_repos_stops_at_five_and_counts_the_rest() {
    let fx = fixture(true);
    let mapping = map_probing(&fx, |_| {
        adopt::SourceProbe::Unreadable(std::io::ErrorKind::PermissionDenied)
    });
    let listed = mapping.unreadable_sources().to_string();
    assert!(
        listed.starts_with(
            "did:plc:squid, did:plc:limpet, did:plc:whelk, did:plc:nautilus, did:plc:scallop"
        ),
        "{listed}"
    );
    assert!(
        listed.ends_with(", and 1 more"),
        "did:plc:conch and did:plc:clam are skipped before the probe, so 6 of the 8 rows are \
         unreadable: {listed}"
    );
}

fn set_acl_owner(fx: &Fixture, repo: &str, acl_owner: &str) {
    rusqlite::Connection::open(&fx.db_path)
        .unwrap()
        .execute(
            "update acl set v0 = ?1 where v2 = ?2 and v3 = 'repo:owner'",
            rusqlite::params![acl_owner, repo],
        )
        .unwrap();
}

#[test]
fn two_owners_for_one_repo_are_drift_that_the_report_can_render() {
    let fx = fixture(true);
    set_acl_owner(&fx, "did:plc:scallop", "did:plc:teq");
    let mapping = map(&fx);
    assert_eq!(
        mapping.drift.conflicting_owner_markers,
        vec![mapping::OwnerConflict {
            repo: SourceRepoDid::from_column("did:plc:scallop"),
            acl_owner: SourceDid::from_column("did:plc:teq"),
            key_owner: SourceDid::from_column("did:plc:isabel"),
        }]
    );
    assert_eq!(mapping.conflicting_owners().to_string(), "did:plc:scallop");
    assert!(
        !mapping
            .drift
            .extra_owner_markers
            .iter()
            .any(|(repo, _)| repo.as_str() == "did:plc:scallop"),
        "a repo recorded with a different owner mustn't also appear as an extra marker: {:?}",
        mapping.drift.extra_owner_markers
    );
    let rendered = report::Report {
        mapping: &mapping,
        orphan_alias_count: 0,
        adoption: None,
        cobs: None,
    }
    .to_string();
    assert!(
        rendered.contains("repos recorded with different owners in the acl and repo_keys: 1"),
        "{rendered}"
    );
    assert!(
        rendered.contains("did:plc:scallop acl did:plc:teq, repo_keys did:plc:isabel"),
        "{rendered}"
    );
}

#[test]
fn an_owner_marker_beside_the_repo_keys_owner_is_still_an_extra() {
    let mapping = map(&fixture(true));
    assert!(
        mapping.drift.conflicting_owner_markers.is_empty(),
        "did:plc:limpet is recorded with did:plc:bailey beside its repo_keys owner: {:?}",
        mapping.drift.conflicting_owner_markers
    );
    assert!(mapping.conflicting_owners().is_empty());
    assert!(
        mapping.drift.extra_owner_markers.contains(&(
            SourceRepoDid::from_column("did:plc:limpet"),
            SourceDid::from_column("did:plc:bailey")
        )),
        "{:?}",
        mapping.drift.extra_owner_markers
    );
}

fn base64_standard(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn host_key_file(dir: &Path) -> PathBuf {
    let path = dir.join("ssh_host_ed25519_key");
    std::fs::write(&path, HOST_KEY).unwrap();
    path
}

fn run_migrate(fx: &Fixture, host_key: &Path, extra: &[&str]) -> std::process::Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_knot-migrate"))
        .args([
            "--source-db",
            fx.db_path.to_str().unwrap(),
            "--source-repos",
            fx.source_repos.to_str().unwrap(),
            "--host-key",
            host_key.to_str().unwrap(),
            "--hostname",
            "knot.oyster.cafe",
            "--plc-url",
            "https://plc.directory",
            "--target",
            fx.target.to_str().unwrap(),
        ])
        .args(extra)
        .env("KNOT_MASTER_KEY", base64_standard(&[7_u8; 32]))
        .output()
        .unwrap()
}

fn with_unreadable_repo<T>(fx: &Fixture, work: impl FnOnce() -> T) -> Option<T> {
    use std::os::unix::fs::PermissionsExt;
    if !honors_permission_bits(&fx.source_repos) {
        return None;
    }
    let limpet = fx.source_repos.join("did:plc:limpet");
    std::fs::set_permissions(&limpet, std::fs::Permissions::from_mode(0o000)).unwrap();
    let outcome = work();
    std::fs::set_permissions(&limpet, std::fs::Permissions::from_mode(0o755)).unwrap();
    Some(outcome)
}

#[test]
fn a_real_run_refuses_an_unreadable_source_until_it_is_told_to_skip_it() {
    let fx = fixture(true);
    let dir = tempfile::tempdir().unwrap();
    let host_key = host_key_file(dir.path());
    let Some((refused, target_after_refusal, outcome)) = with_unreadable_repo(&fx, || {
        let refused = run_migrate(&fx, &host_key, &[]);
        let target_after_refusal = fx.target.exists();
        (
            refused,
            target_after_refusal,
            run_migrate(&fx, &host_key, &["--skip-unreadable"]),
        )
    }) else {
        return;
    };
    let refusal = String::from_utf8_lossy(&refused.stderr).to_string();
    assert!(!refused.status.success(), "{refusal}");
    assert!(refusal.contains("did:plc:limpet"), "{refusal}");
    assert!(
        !target_after_refusal,
        "a refused run mustn't leave a half-migrated target behind"
    );
    let stdout = String::from_utf8_lossy(&outcome.stdout).to_string();
    let stderr = String::from_utf8_lossy(&outcome.stderr).to_string();
    assert!(outcome.status.success(), "{stdout}{stderr}");
    let adopted = map_probing(&fx, |did| match did.as_str() {
        "did:plc:limpet" => adopt::SourceProbe::Unreadable(std::io::ErrorKind::PermissionDenied),
        _ => adopt::probe_source(&fx.source_repos, did),
    })
    .repos
    .len();
    assert!(
        stdout.contains(&format!("adopted by copy: {adopted} new")),
        "{stdout}"
    );
    assert!(
        stdout.contains("a source path that this process can't read: permission denied"),
        "{stdout}"
    );
    assert!(fx.target.join("config.toml").is_file(), "{stdout}");
}

#[test]
fn a_run_refuses_two_owners_for_one_repo_after_it_reports_them() {
    let fx = fixture(true);
    set_acl_owner(&fx, "did:plc:scallop", "did:plc:teq");
    let dir = tempfile::tempdir().unwrap();
    let host_key = host_key_file(dir.path());
    [vec![], vec!["--dry-run"], vec!["--skip-unreadable"]]
        .into_iter()
        .for_each(|extra| {
            let outcome = run_migrate(&fx, &host_key, &extra);
            let stdout = String::from_utf8_lossy(&outcome.stdout).to_string();
            let stderr = String::from_utf8_lossy(&outcome.stderr).to_string();
            assert!(!outcome.status.success(), "{extra:?}: {stdout}{stderr}");
            assert!(
                stdout.contains("did:plc:scallop acl did:plc:teq, repo_keys did:plc:isabel"),
                "the report has to be on stdout before the refusal, {extra:?}: {stdout}"
            );
            assert!(
                stderr.contains(
                    "the acl and repo_keys are recorded with different owners for did:plc:scallop"
                ),
                "{extra:?}: {stderr}"
            );
            assert!(
                stderr.contains("repo_keys.owner_did"),
                "the refusal has to point at the column to fix, {extra:?}: {stderr}"
            );
            assert!(
                !fx.target.exists(),
                "a refused run mustn't leave a half-migrated target behind, {extra:?}"
            );
        });
}
