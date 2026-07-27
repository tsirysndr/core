use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant, SystemTime};

use knot_cob::{CobHome, CobStore};
use knot_cobs::{Registration, RegistryChange, RepoRef, RepoRegistryCob, deregister_repo};
use knot_git::{Layout, Repo};
use knot_lfs::{ClaimedSize, DiskStore, LfsOid, LfsStore, LfsStorePath};
use knot_runtime::OsEntropy;
use knot_secrets::{MasterKey, SealedStore};
use knot_types::{KnotId, OwnerDid, RepoDid, RepoName, RepoRkey, UnixSeconds};
use sha2::{Digest, Sha256};

const REPO_DID: &str = "did:plc:squid";
const REPO_NAME: &str = "anemone";
const OWNER_DID: &str = "did:plc:nel";
const KNOT_DID: &str = "did:web:nel.pet";
const MEDIA: &[u8] = b"\xff\x00media that mustn't outlive its repo";
const GRACE: Duration = Duration::from_secs(86_400);
const BACKDATE: Duration = Duration::from_secs(60 * 86_400);

fn did() -> RepoDid {
    RepoDid::new(REPO_DID).unwrap()
}

fn knot() -> KnotId {
    KnotId::new(KNOT_DID).unwrap()
}

fn master() -> MasterKey {
    MasterKey::new([7u8; 32]).unwrap()
}

fn media_oid() -> LfsOid {
    LfsOid::from_digest(Sha256::digest(MEDIA).into())
}

struct Paths {
    meta: PathBuf,
    scan: PathBuf,
    store: PathBuf,
    keys: PathBuf,
}

impl Paths {
    fn under(root: &Path) -> Self {
        Self {
            meta: root.join("meta"),
            scan: root.join("repos"),
            store: root.join("lfs"),
            keys: root.join("keys.sealed"),
        }
    }
}

fn build_fixture(root: &Path) -> Paths {
    let paths = Paths::under(root);
    Repo::create(&paths.meta).unwrap();
    Layout::new(&paths.scan).create(&did()).unwrap();

    let secrets = SealedStore::open(&paths.keys, &master(), Box::new(OsEntropy)).unwrap();
    secrets.ensure(&knot()).unwrap();
    let signer = secrets.signer(&knot()).unwrap();
    let meta = Repo::open(&paths.meta).unwrap();
    CobStore::new(&meta)
        .create(
            &CobHome::from(&knot()),
            &RegistryChange::Register(Registration {
                owner: OwnerDid::new(OWNER_DID).unwrap(),
                rkey: RepoRkey::new(REPO_NAME).unwrap(),
                name: RepoName::new(REPO_NAME).unwrap(),
                repo: did(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .unwrap();

    std::fs::create_dir_all(&paths.store).unwrap();
    let store = DiskStore::open(LfsStorePath::new(&paths.store)).unwrap();
    store
        .put(
            &did(),
            &media_oid(),
            ClaimedSize::new(MEDIA.len() as u64),
            &mut &MEDIA[..],
        )
        .unwrap();
    let object_path = store.object_file(&did(), &media_oid()).unwrap().unwrap().1;
    std::fs::OpenOptions::new()
        .write(true)
        .open(object_path)
        .unwrap()
        .set_modified(SystemTime::now() - BACKDATE)
        .unwrap();
    paths
}

fn spawn_worker(paths: &Paths) -> std::process::Child {
    Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "chaos_delete_worker", "--nocapture"])
        .env("KNOT_CHAOS_ROLE", "delete")
        .env("KNOT_CHAOS_META", &paths.meta)
        .env("KNOT_CHAOS_SCAN", &paths.scan)
        .env("KNOT_CHAOS_STORE", &paths.store)
        .env("KNOT_CHAOS_KEYS", &paths.keys)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn chaos worker")
}

#[test]
fn chaos_delete_worker() {
    if std::env::var("KNOT_CHAOS_ROLE").as_deref() != Ok("delete") {
        return;
    }
    let meta = Repo::open(std::env::var("KNOT_CHAOS_META").unwrap()).unwrap();
    let store = CobStore::new(&meta);
    let secrets = SealedStore::open(
        std::env::var("KNOT_CHAOS_KEYS").unwrap(),
        &master(),
        Box::new(OsEntropy),
    )
    .unwrap();
    let signer = secrets.signer(&knot()).unwrap();
    let object = store.list::<RepoRegistryCob>().unwrap()[0];
    deregister_repo(
        &store,
        &CobHome::from(&knot()),
        object,
        RepoRef {
            owner: OwnerDid::new(OWNER_DID).unwrap(),
            rkey: RepoRkey::new(REPO_NAME).unwrap(),
        },
        did(),
        &signer,
        UnixSeconds::new(2),
    )
    .unwrap();
    Layout::new(std::env::var("KNOT_CHAOS_SCAN").unwrap())
        .remove(&did())
        .unwrap();
    DiskStore::open(LfsStorePath::new(
        std::env::var("KNOT_CHAOS_STORE").unwrap(),
    ))
    .unwrap()
    .remove_repo(&did())
    .unwrap();
}

#[test]
fn kill9_between_delete_steps_never_strands_the_store_prefix() {
    let scratch = tempfile::tempdir().unwrap();

    let warm = build_fixture(&scratch.path().join("warm"));
    let started = Instant::now();
    spawn_worker(&warm).wait().unwrap();
    let full = started.elapsed();
    let warm_store = DiskStore::open(LfsStorePath::new(&warm.store)).unwrap();
    assert_eq!(
        warm_store.probe(&did(), &media_oid()).unwrap(),
        None,
        "an uninterrupted delete removes the store prefix itself"
    );

    let fractions = [0.20, 0.35, 0.45, 0.55, 0.65, 0.75, 0.85, 0.95];
    let delays: Vec<Duration> = std::iter::once(Duration::from_millis(1))
        .chain(std::iter::once(Duration::from_millis(3)))
        .chain(fractions.iter().map(|fraction| full.mul_f64(*fraction)))
        .chain(std::iter::once(full.mul_f64(2.0)))
        .collect();

    let outcomes: Vec<bool> = delays
        .iter()
        .enumerate()
        .map(|(trial, delay)| {
            let paths = build_fixture(&scratch.path().join(format!("trial-{trial}")));
            let mut child = spawn_worker(&paths);
            std::thread::sleep(*delay);
            let _ = child.kill();
            child.wait().unwrap();

            let index = knot_index::Index::new(paths.meta.clone(), Layout::new(&paths.scan));
            index.rebuild().unwrap_or_else(|error| {
                panic!("trial {trial}: registry must rebuild after a killed delete: {error}")
            });
            assert_eq!(
                index.coverage().registry,
                knot_index::Coverage::Ready,
                "trial {trial}: a rebuilt projection is ready"
            );
            let hosted: HashSet<RepoDid> = index.hosted_repos().into_iter().collect();
            let registered = hosted.contains(&did());

            let store = DiskStore::open(LfsStorePath::new(&paths.store)).unwrap();
            store
                .sweep_orphans(&hosted, GRACE, SystemTime::now())
                .unwrap_or_else(|error| {
                    panic!("trial {trial}: orphan sweep must run after a killed delete: {error}")
                });
            let present = store.probe(&did(), &media_oid()).unwrap().is_some();
            match registered {
                true => assert!(
                    present,
                    "trial {trial}: a still-registered repo's store prefix is never condemned"
                ),
                false => assert!(
                    !present,
                    "trial {trial}: the orphan sweep reclaims the prefix the killed delete left behind"
                ),
            }
            registered
        })
        .collect();

    assert!(
        outcomes.iter().any(|registered| *registered),
        "some trial must die before the deregister lands, or the chaos window never opened"
    );
    assert!(
        outcomes.iter().any(|registered| !registered),
        "some trial must land the deregister, or the kill delays are all too short"
    );
}
