use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::{Json, Router, routing::get};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bytes::Bytes;
use serde_json::json;
use tempfile::TempDir;
use url::Url;

use knot_atproto::{Atproto, knot_did_document};
use knot_cob::{CobHome, CobStore};
use knot_cobs::{Grant, Registration, RegistryChange};
use knot_events::{EventLog, GlobalSubscriberLimit, PerPeerSubscriberLimit, SubscriberGate};
use knot_git::{
    EntryKind, Identity, Layout, NewCommit, RefUpdate, Repo, StagedAction, StagedChange,
};
use knot_index::{Index, Resolved};
use knot_runtime::{
    Clock, Entropy, FakeHttp, HttpRequest, HttpResponse, K256Signer, ManualClock, NetworkError,
    PublicKeyBytes, SeededEntropy, Signer, UnixMicros,
};
use knot_secrets::{MasterKey, SealedStore};
use knot_types::{
    AccountDid, AdmissionPolicy, AuthorName, BranchName, Email, KnotHostname, KnotId,
    KnotServiceUrl, Oid, OwnerDid, RefName, RepoDid, RepoName, RepoRkey, UnixSeconds,
};
use knot_xrpc::{
    BlobReadBudget, BodyLimit, Budgets, ByteLimits, CobLocks, Committer, GlobalInflight,
    GlobalQuota, LanguagesPushBudget, LanguagesReadBudget, LimitConfig, MaxWireBytes,
    PerActorQuota, PerPeerInflight, PreAuthLimiter, ReadBudget, ReservationTtl, Reservations,
    ResponseLimit, TreeReadBudget, XrpcState,
};

use crate::realdata::{
    RealActors, SAMPLE_TAG, SAMPLED_REPOS, STRANGER_POOL, SUBJECT_POOL, admin_signer, owner_signer,
};
use crate::trace::{RepoCollaborators, RoundNumber, Snapshot};
use crate::workload::Rng;

const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
const KNOT_HOST: &str = "knot.nel.pet";
const ADMIN_HOST: &str = "admin.nel.pet";
const STRANGER_SEED_BASE: u64 = 1_000;
const PDS_ENDPOINT: &str = "https://pds.nel.pet";
const START_MICROS: u64 = 1_000_000_000;
const NO_REFLOG_EXPIRY_FLOOR_SECS: i64 = i64::MAX / 4;
const NO_PRUNE_EXPIRY_GRACE_SECS: u64 = (i64::MAX / 4) as u64;
const MAINTAIN_ATTEMPTS: usize = 4;

pub(crate) const SUBJECT_DIDS: [&str; 8] = [
    "did:plc:limpet",
    "did:plc:whelk",
    "did:plc:mussel",
    "did:plc:conch",
    "did:plc:scallop",
    "did:plc:cuttle",
    "did:plc:periwinkle",
    "did:plc:nautilus",
];

pub(crate) type Responder =
    Box<dyn Fn(&HttpRequest) -> Result<HttpResponse, NetworkError> + Send + Sync>;

pub(crate) struct Actor {
    pub host: KnotHostname,
    pub did: AccountDid,
    pub signer: K256Signer,
}

#[derive(Default)]
pub(crate) struct Faults {
    dropped: Mutex<HashSet<KnotHostname>>,
}

impl Faults {
    fn is_dropped(&self, host: &KnotHostname) -> bool {
        self.dropped.lock().expect("faults lock").contains(host)
    }

    pub(crate) fn drop_host(&self, host: &KnotHostname) {
        self.dropped
            .lock()
            .expect("faults lock")
            .insert(host.clone());
    }

    pub(crate) fn clear_host(&self, host: &KnotHostname) {
        self.dropped.lock().expect("faults lock").remove(host);
    }
}

pub(crate) struct Harness {
    _dir: Option<TempDir>,
    pub clock: Arc<ManualClock>,
    pub faults: Arc<Faults>,
    pub layout: Layout,
    pub index: Arc<Index>,
    pub knot_aud: KnotId,
    router: Router,
    pub admin: Actor,
    pub strangers: Vec<Actor>,
    pub subjects: Vec<AccountDid>,
    pub seed_repo: RepoDid,
    options: knot_maintenance::Options,
    maintain_lock: Mutex<()>,
}

impl Harness {
    pub(crate) fn build(seed: u64, stranger_pool: usize) -> Self {
        let dir = tempfile::tempdir().expect("sim tempdir");
        let knot = KnotId::new(format!("did:web:{KNOT_HOST}")).expect("knot did");
        let layout = Layout::new(dir.path().join("repos"))
            .reserving_meta(&knot)
            .expect("reserve meta");
        layout.bootstrap_meta(&knot).expect("bootstrap meta");
        let meta_path = layout.meta_path(&knot).expect("meta path");

        let entropy = Arc::new(SeededEntropy::new(seed ^ 0x5eed_0050));
        let secrets = Arc::new(
            SealedStore::open(
                dir.path().join("keys.sealed"),
                &MasterKey::new([7u8; 32]).unwrap(),
                Box::new(SeededEntropy::new(seed ^ 0x5ec0)),
            )
            .expect("sealed store"),
        );
        let knot_pubkey = secrets.ensure(&knot).expect("seal knot key");
        let knot_service_url =
            KnotServiceUrl::new(format!("https://{KNOT_HOST}")).expect("knot service url");

        let admin = actor(ADMIN_HOST, 1);
        let strangers: Vec<Actor> = (0..stranger_pool)
            .map(|index| {
                let host = format!("stranger{index}.nel.pet");
                actor(&host, STRANGER_SEED_BASE + index as u64)
            })
            .collect();
        let subjects: Vec<AccountDid> = SUBJECT_DIDS
            .iter()
            .map(|did| AccountDid::new(*did).expect("subject did"))
            .collect();

        let seed_repo = RepoDid::new("did:plc:squid").expect("seed repo did");
        seed_on_disk(&layout, &seed_repo);
        register_seed_repo(&meta_path, &knot, &secrets, &admin.did, &seed_repo);

        let index = Arc::new(Index::new(meta_path.clone(), layout.clone()));
        index.rebuild().expect("rebuild index");
        index.warm_collaborators();

        let clock = Arc::new(ManualClock::new(UnixMicros::new(START_MICROS)));
        let pubkeys = pubkey_map(&admin, &strangers);
        let faults = Arc::new(Faults::default());
        let plc = Url::parse("https://plc.directory/").expect("plc url");
        let responder = build_responder(Arc::clone(&faults), pubkeys, HashMap::new(), plc.clone());
        let knot_aud = knot.clone();
        let did_document = knot_did_document(&knot, &knot_pubkey, &knot_service_url);
        let router = assemble_router(StateParts {
            layout: layout.clone(),
            index: Arc::clone(&index),
            responder,
            secrets,
            entropy: entropy as Arc<dyn Entropy>,
            admins: BTreeSet::from([admin.did.clone()]),
            admission: AdmissionPolicy::Closed,
            knot: knot.clone(),
            knot_hostname: KnotHostname::new(KNOT_HOST).unwrap(),
            meta_path,
            knot_service_url,
            service_owner: admin.did.clone(),
            clock: Arc::clone(&clock),
            plc,
            did_document,
        });
        let options = maintenance_options();

        Self {
            _dir: Some(dir),
            clock,
            faults,
            layout,
            index,
            knot_aud,
            router,
            admin,
            strangers,
            subjects,
            seed_repo,
            options,
            maintain_lock: Mutex::new(()),
        }
    }

    pub fn open(target: &Path, seed: u64) -> Result<(Self, RealActors), OpenError> {
        let config =
            knot_config::load(Some(&target.join("config.toml"))).map_err(OpenError::Config)?;
        let hostname =
            KnotHostname::new(config.server.hostname.clone()).map_err(|_| OpenError::Hostname)?;
        let knot = hostname.knot_did();
        let object_format = config.object_format().ok_or(OpenError::ObjectFormat)?;
        let default_branch = BranchName::new(config.repo.default_branch.as_str())
            .map_err(|_| OpenError::DefaultBranch)?;
        let admin_did = config
            .server
            .admins
            .first()
            .cloned()
            .ok_or(OpenError::NoAdmin)?;
        let admission = config.acl.admission;
        let plc = config.atproto.plc_directory.clone();

        let master_key_env = config.secrets.master_key_env.clone();
        let master_key = MasterKey::new(
            STANDARD
                .decode(
                    std::env::var(&master_key_env)
                        .map_err(|_| OpenError::MasterKeyEnv(master_key_env.clone()))?
                        .trim(),
                )
                .map_err(|_| OpenError::MasterKeyDecode)?,
        )?;

        let scratch = materialize_scratch(target, &knot)?;
        let root = scratch.path().to_path_buf();

        let layout = Layout::new(root.join("repos"))
            .with_default_branch(default_branch)
            .with_object_format(object_format)
            .reserving_meta(&knot)?;
        let meta_path = layout.meta_path(&knot)?;
        let secrets = Arc::new(SealedStore::open(
            root.join("sealed-keys"),
            &master_key,
            Box::new(SeededEntropy::new(seed ^ 0x5ec0)),
        )?);
        let knot_pubkey = secrets.ensure(&knot)?;
        let knot_service_url =
            KnotServiceUrl::new(format!("https://{hostname}")).map_err(|_| OpenError::Hostname)?;

        let index = Arc::new(Index::new(meta_path.clone(), layout.clone()));
        index.rebuild()?;
        index.warm_collaborators();

        let mut hosted = index.hosted_repos();
        hosted.sort();
        let rng = Rng::new(seed ^ SAMPLE_TAG);
        let owner_repos: Vec<(RepoDid, OwnerDid)> = SAMPLED_REPOS
            .sample(&hosted, &rng)
            .into_iter()
            .filter_map(|repo| match index.owner_of(&repo) {
                Resolved::Ready(Some(owner)) => Some((repo, owner)),
                _ => None,
            })
            .collect();
        let subjects = SUBJECT_POOL.sample(&distinct_members(&index), &rng);

        let admin = Actor {
            host: hostname.clone(),
            did: admin_did.clone(),
            signer: admin_signer(seed),
        };
        let strangers: Vec<Actor> = (0..STRANGER_POOL.count())
            .map(|index| {
                actor(
                    &format!("stranger{index}.nel.pet"),
                    STRANGER_SEED_BASE + index as u64,
                )
            })
            .collect();

        let mut did_overrides: HashMap<AccountDid, PublicKeyBytes> = HashMap::new();
        did_overrides.insert(admin_did.clone(), admin.signer.public_key());
        owner_repos.iter().for_each(|(_, owner)| {
            did_overrides
                .entry(AccountDid::from(owner.clone()))
                .or_insert_with(|| owner_signer(seed, owner).public_key());
        });

        let seed_repo = owner_repos
            .first()
            .map(|(repo, _)| repo.clone())
            .or_else(|| hosted.first().cloned())
            .ok_or(OpenError::NoRepos)?;

        let clock = Arc::new(ManualClock::new(UnixMicros::new(START_MICROS)));
        let faults = Arc::new(Faults::default());
        let responder = build_responder(
            Arc::clone(&faults),
            pubkey_map(&admin, &strangers),
            did_overrides,
            plc.clone(),
        );
        let entropy = Arc::new(SeededEntropy::new(seed ^ 0x5eed_0050));
        let knot_aud = knot.clone();
        let did_document = knot_did_document(&knot, &knot_pubkey, &knot_service_url);
        let router = assemble_router(StateParts {
            layout: layout.clone(),
            index: Arc::clone(&index),
            responder,
            secrets,
            entropy: entropy as Arc<dyn Entropy>,
            admins: BTreeSet::from([admin_did.clone()]),
            admission,
            knot: knot.clone(),
            knot_hostname: hostname,
            meta_path,
            knot_service_url,
            service_owner: admin_did.clone(),
            clock: Arc::clone(&clock),
            plc,
            did_document,
        });

        let harness = Self {
            _dir: Some(scratch),
            clock,
            faults,
            layout,
            index,
            knot_aud,
            router,
            admin,
            strangers,
            subjects,
            seed_repo,
            options: maintenance_options(),
            maintain_lock: Mutex::new(()),
        };
        Ok((harness, RealActors { owner_repos, seed }))
    }

    pub(crate) fn router(&self) -> Router {
        self.router.clone()
    }

    pub(crate) fn now_seconds(&self) -> UnixSeconds {
        UnixSeconds::new((self.clock.now_unix_micros().get() / 1_000_000) as i64)
    }

    pub(crate) fn advance(&self, delta: std::time::Duration) {
        self.clock.advance(delta);
    }

    pub(crate) fn maintain(&self, repo: &RepoDid) -> Result<(), String> {
        let _serialized = self.maintain_lock.lock().expect("maintenance lock");
        let now = self.now_seconds();
        let attempt = || -> Result<(), knot_maintenance::MaintError> {
            let opened = self
                .layout
                .open(repo)
                .map_err(knot_maintenance::MaintError::from)?;
            knot_maintenance::run_repo(&opened, now, &self.options).map(|_| ())
        };
        (1..MAINTAIN_ATTEMPTS)
            .fold(attempt(), |result, _| result.or_else(|_| attempt()))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn populate(&self, repo: &RepoDid) {
        let opened = self.layout.open(repo).expect("open created repo");
        write_history(&opened, repo.as_str());
    }

    pub(crate) fn snapshot(&self, round: RoundNumber, repos: &[RepoDid]) -> Snapshot {
        let collaborators = repos
            .iter()
            .map(|repo| {
                let _ = self.index.ensure_collaborators(repo);
                RepoCollaborators {
                    repo: repo.clone(),
                    subjects: sorted(grant_subjects(self.index.collaborator_entries(repo))),
                }
            })
            .collect();
        let repo_list = {
            let mut repos: Vec<RepoDid> = self.index.hosted_repos().to_vec();
            repos.sort();
            repos
        };
        Snapshot {
            round,
            clock_micros: self.clock.now_unix_micros(),
            members: sorted(grant_subjects(self.index.member_entries())),
            blocked: sorted(grant_subjects(self.index.blocked_entries())),
            repos: repo_list,
            collaborators,
        }
    }
}

fn actor(host: &str, seed: u64) -> Actor {
    Actor {
        host: KnotHostname::new(host).expect("actor hostname"),
        did: AccountDid::new(format!("did:web:{host}")).expect("actor did"),
        signer: K256Signer::generate(&SeededEntropy::new(seed)),
    }
}

fn pubkey_map(admin: &Actor, strangers: &[Actor]) -> HashMap<KnotHostname, PublicKeyBytes> {
    std::iter::once((admin.host.clone(), admin.signer.public_key()))
        .chain(
            strangers
                .iter()
                .map(|actor| (actor.host.clone(), actor.signer.public_key())),
        )
        .collect()
}

fn build_responder(
    faults: Arc<Faults>,
    pubkeys: HashMap<KnotHostname, PublicKeyBytes>,
    did_overrides: HashMap<AccountDid, PublicKeyBytes>,
    plc: Url,
) -> Responder {
    let plc_signer = K256Signer::generate(&SeededEntropy::new(7));
    Box::new(move |request: &HttpRequest| {
        if request.method == http::Method::POST {
            return Ok(ok_body(Bytes::new()));
        }
        let not_found = || HttpResponse {
            status: http::StatusCode::NOT_FOUND,
            headers: http::HeaderMap::new(),
            body: Bytes::new(),
        };
        let Ok(host) = KnotHostname::new(request.url.host_str().unwrap_or_default()) else {
            return Ok(not_found());
        };
        if faults.is_dropped(&host) {
            return Err(NetworkError::Timeout(
                "identity resolution dropped by simulation".to_string(),
            ));
        }
        let is_plc = request.url.host() == plc.host();
        let requested_did = if is_plc {
            request
                .url
                .path_segments()
                .and_then(|mut segments| segments.rfind(|segment| !segment.is_empty()))
                .and_then(|segment| AccountDid::new(segment).ok())
        } else {
            AccountDid::new(format!("did:web:{host}")).ok()
        };
        let Some(requested_did) = requested_did else {
            return Ok(not_found());
        };
        if let Some(sec1) = did_overrides.get(&requested_did) {
            return Ok(ok_body(did_doc(&requested_did, sec1)));
        }
        if is_plc {
            return Ok(ok_body(did_doc(&requested_did, &plc_signer.public_key())));
        }
        match pubkeys.get(&host) {
            Some(sec1) => Ok(ok_body(did_doc(&requested_did, sec1))),
            None => Ok(not_found()),
        }
    })
}

fn did_doc(did: &AccountDid, sec1: &PublicKeyBytes) -> Bytes {
    let did = did.as_str();
    let multikey = knot_types::crypto::multikey(0xe7, sec1.as_bytes());
    Bytes::from(
        serde_json::to_vec(&json!({
            "id": did,
            "alsoKnownAs": [],
            "verificationMethod": [{
                "id": format!("{did}#atproto"),
                "type": "Multikey",
                "controller": did,
                "publicKeyMultibase": multikey
            }],
            "service": [{
                "id": "#atproto_pds",
                "type": "AtprotoPersonalDataServer",
                "serviceEndpoint": PDS_ENDPOINT
            }]
        }))
        .expect("did doc serializes"),
    )
}

fn ok_body(body: Bytes) -> HttpResponse {
    HttpResponse {
        status: http::StatusCode::OK,
        headers: http::HeaderMap::new(),
        body,
    }
}

fn seed_on_disk(layout: &Layout, did: &RepoDid) {
    let repo = layout.create(did).expect("create seed repo");
    write_history(&repo, "reef");
}

fn write_history(repo: &Repo, marker: &str) {
    let identity = Identity {
        name: AuthorName::new("nel"),
        email: Email::new("nel@oyster.cafe"),
        time: UnixSeconds::new(1_700_000_000),
        offset_seconds: 0,
    };
    let main = RefName::new("refs/heads/main").expect("main ref");
    let first_tree = repo
        .write_staged_tree(
            Oid::from_hex(EMPTY_TREE).expect("empty tree"),
            &[StagedChange {
                path: knot_types::RepoPath::new("README.md").unwrap(),
                action: StagedAction::Put {
                    content: format!("# {marker}\n").into_bytes(),
                    kind: EntryKind::Blob,
                },
            }],
        )
        .expect("first tree");
    let root = repo
        .write_commit(&NewCommit {
            tree: first_tree,
            parents: Vec::new(),
            author: identity.clone(),
            committer: identity.clone(),
            message: "root".to_string(),
            extra_headers: Vec::new(),
        })
        .expect("root commit");
    let second_tree = repo
        .write_staged_tree(
            first_tree,
            &[StagedChange {
                path: knot_types::RepoPath::new("src/main.rs").unwrap(),
                action: StagedAction::Put {
                    content: b"fn main() {}\n".to_vec(),
                    kind: EntryKind::Blob,
                },
            }],
        )
        .expect("second tree");
    let tip = repo
        .write_commit(&NewCommit {
            tree: second_tree,
            parents: vec![root],
            author: identity.clone(),
            committer: identity,
            message: "add main".to_string(),
            extra_headers: Vec::new(),
        })
        .expect("second commit");
    repo.update_ref(&RefUpdate::Create {
        name: main.clone(),
        new: tip,
    })
    .expect("create main");
    repo.set_head(&main).expect("set head");
}

fn register_seed_repo(
    meta_path: &std::path::Path,
    knot: &KnotId,
    secrets: &SealedStore,
    owner: &AccountDid,
    repo: &RepoDid,
) {
    let meta = Repo::open(meta_path).expect("open meta");
    let store = CobStore::new(&meta);
    let signer = secrets.signer(knot).expect("knot signer");
    store
        .create(
            &CobHome::from(knot),
            &RegistryChange::Register(Registration {
                owner: OwnerDid::new(owner.as_str()).expect("owner did"),
                rkey: RepoRkey::new("anemone").expect("rkey"),
                name: RepoName::new("anemone").expect("name"),
                repo: repo.clone(),
                created_at: UnixSeconds::new(1),
            }),
            &signer,
            UnixSeconds::new(1),
        )
        .expect("register seed repo");
}

fn grant_subjects(resolved: Resolved<Vec<Grant>>) -> Vec<AccountDid> {
    match resolved {
        Resolved::Ready(grants) => grants
            .into_iter()
            .map(|grant| grant.subject.clone())
            .collect(),
        Resolved::Warming => Vec::new(),
    }
}

fn sorted<T: Ord>(mut values: Vec<T>) -> Vec<T> {
    values.sort();
    values
}

fn distinct_members(index: &Index) -> Vec<AccountDid> {
    let mut members = grant_subjects(index.member_entries());
    members.sort();
    members.dedup();
    members
}

fn materialize_scratch(target: &Path, knot: &KnotId) -> Result<TempDir, OpenError> {
    let base = target
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let source_repos = target.join("repos");
    let source_meta = Layout::new(source_repos.clone()).meta_path(knot)?;
    let scratch = tempfile::tempdir_in(&base).map_err(OpenError::Scratch)?;
    let root = scratch.path();
    let scratch_repos = root.join("repos");
    let scratch_meta = Layout::new(scratch_repos.clone()).meta_path(knot)?;
    fs::copy(target.join("sealed-keys"), root.join("sealed-keys")).map_err(OpenError::Scratch)?;
    hardlink_tree(&source_repos, &scratch_repos).map_err(OpenError::Scratch)?;
    fs::remove_dir_all(&scratch_meta).map_err(OpenError::Scratch)?;
    copy_tree(&source_meta, &scratch_meta).map_err(OpenError::Scratch)?;
    Ok(scratch)
}

fn hardlink_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    fs::read_dir(src)?.try_for_each(|entry| {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            hardlink_tree(&from, &to)
        } else {
            fs::hard_link(&from, &to).map(|_| ())
        }
    })
}

fn copy_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    fs::read_dir(src)?.try_for_each(|entry| {
        let entry = entry?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_tree(&from, &to)
        } else {
            fs::copy(&from, &to).map(|_| ())
        }
    })
}

fn maintenance_options() -> knot_maintenance::Options {
    knot_maintenance::Options {
        repack_max_objects: knot_maintenance::ObjectCount::new(1_000_000),
        geometric_factor: knot_maintenance::GeometricFactor::full_repack(),
        prune_grace: knot_maintenance::PruneGrace::from_secs(NO_PRUNE_EXPIRY_GRACE_SECS),
        reflog_floor: knot_maintenance::ReflogRetention::from_secs(
            NO_REFLOG_EXPIRY_FLOOR_SECS as u64,
        ),
        commit_graph: true,
        multi_pack_index: true,
        bitmap: true,
    }
}

struct StateParts {
    layout: Layout,
    index: Arc<Index>,
    responder: Responder,
    secrets: Arc<SealedStore>,
    entropy: Arc<dyn Entropy>,
    admins: BTreeSet<AccountDid>,
    admission: AdmissionPolicy,
    knot: KnotId,
    knot_hostname: KnotHostname,
    meta_path: PathBuf,
    knot_service_url: KnotServiceUrl,
    service_owner: AccountDid,
    clock: Arc<ManualClock>,
    plc: Url,
    did_document: serde_json::Value,
}

fn assemble_router(parts: StateParts) -> Router {
    let StateParts {
        layout,
        index,
        responder,
        secrets,
        entropy,
        admins,
        admission,
        knot,
        knot_hostname,
        meta_path,
        knot_service_url,
        service_owner,
        clock,
        plc,
        did_document,
    } = parts;
    let atproto = Arc::new(Atproto::new(
        FakeHttp::new(responder),
        Arc::clone(&clock),
        knot.clone(),
        knot_atproto::PlcDirectory::new(plc).expect("plc directory"),
    ));
    let state = Arc::new(XrpcState {
        layout: layout.clone(),
        index: Arc::clone(&index),
        atproto,
        secrets,
        entropy,
        ci_logs: None,
        admins,
        admission,
        knot_did: knot,
        knot_hostname,
        meta_path,
        knot_service_url,
        limiter: Arc::new(PreAuthLimiter::with_config(LimitConfig {
            rate: None,
            per_peer_inflight: Some(PerPeerInflight::new(4096)),
            global_inflight: Some(GlobalInflight::new(4096)),
        })),
        cob_locks: Arc::new(CobLocks::default()),
        reservations: Arc::new(Reservations::new(
            ReservationTtl::new(1_000_000),
            PerActorQuota::new(256),
            GlobalQuota::new(256),
        )),
        proxy_trust: knot_types::ProxyTrust::default(),
        committer: Committer {
            name: AuthorName::new("knot"),
            email: Email::new("knot@nel.pet"),
        },
        byte_limits: ByteLimits {
            body: BodyLimit::new(256 * 1024),
            response: ResponseLimit::new(8 * 1024 * 1024),
            pack: MaxWireBytes::new(1024 * 1024 * 1024),
            ..ByteLimits::default()
        },
        budgets: Budgets {
            tree_last_commit: TreeReadBudget::new(ReadBudget::Unbounded),
            blob_last_commit: BlobReadBudget::new(ReadBudget::Unbounded),
            languages: LanguagesReadBudget::new(ReadBudget::Unbounded),
            languages_push: LanguagesPushBudget::new(Duration::from_secs(120)),
        },
        git_http: Arc::new(FakeHttp::new(|_request: &HttpRequest| {
            Err(NetworkError::Connect(
                "simulation serves no git upstream".to_string(),
            ))
        })),
        pack_limits: knot_pack::PackLimits::default(),
        service_owner,
        events: Arc::new(EventLog::new(
            Arc::clone(&clock),
            knot_events::ReplayBounds::new(
                knot_events::ReplayEvents::new(4096).expect("replay event maximum is nonzero"),
                knot_events::ReplayBytes::new(64 << 20).expect("replay byte maximum is nonzero"),
            ),
        )),
        subscriber_gate: Arc::new(SubscriberGate::new(
            GlobalSubscriberLimit::new(256),
            PerPeerSubscriberLimit::new(64),
        )),
        maintenance: knot_maintenance::MaintenanceHandle::disabled(),
        appview: knot_types::AppviewEndpoint::new("https://tangled.test").unwrap(),
        slots: knot_resource::Slots::testing(8),
        lfs: None,
        catalog: Arc::new(knot_messages::Catalog::defaults()),
    });
    let resolver: Arc<dyn knot_pack::RepoResolver> = {
        let index = Arc::clone(&index);
        Arc::new(move |target: &knot_pack::RepoTarget| match target {
            knot_pack::RepoTarget::Did(did) => match index.owner_of(did) {
                Resolved::Ready(Some(_)) => knot_pack::RepoLookup::Hosted(did.clone()),
                Resolved::Ready(None) => knot_pack::RepoLookup::Unhosted,
                Resolved::Warming => knot_pack::RepoLookup::Unavailable,
            },
            knot_pack::RepoTarget::OwnerPath(owner, path) => {
                match index.resolve_clone_path(owner, path) {
                    Resolved::Ready(Some(found)) => knot_pack::RepoLookup::Hosted(found),
                    Resolved::Ready(None) => knot_pack::RepoLookup::Unhosted,
                    Resolved::Warming => knot_pack::RepoLookup::Unavailable,
                }
            }
        })
    };
    knot_pack::router(layout, resolver, Arc::clone(&clock) as Arc<dyn Clock>)
        .merge(knot_xrpc::router(Arc::clone(&state)))
        .route(
            "/.well-known/did.json",
            get(move || {
                let document = did_document.clone();
                async move { Json(document) }
            }),
        )
}

#[derive(Debug)]
pub enum OpenError {
    MasterKeyEnv(String),
    MasterKeyDecode,
    NoAdmin,
    NoRepos,
    Scratch(std::io::Error),
    Hostname,
    ObjectFormat,
    DefaultBranch,
    Config(knot_config::LoadError),
    Git(knot_git::GitError),
    Secrets(knot_secrets::SecretsError),
    Index(knot_index::IndexError),
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MasterKeyEnv(name) => write!(f, "{name} is not set"),
            Self::MasterKeyDecode => write!(f, "master key is not valid base64"),
            Self::NoAdmin => write!(f, "config lists no admin"),
            Self::NoRepos => write!(f, "target hosts no repos to sample"),
            Self::Scratch(error) => {
                write!(f, "materialize disposable scratch copy of target: {error}")
            }
            Self::Hostname => write!(f, "config hostname is not a valid knot hostname"),
            Self::ObjectFormat => {
                write!(f, "config git.object_format is not a valid object format")
            }
            Self::DefaultBranch => write!(f, "config default branch is not a valid branch name"),
            Self::Config(error) => write!(f, "read config: {error}"),
            Self::Git(error) => write!(f, "open target repos: {error}"),
            Self::Secrets(error) => write!(f, "open sealed key store: {error}"),
            Self::Index(error) => write!(f, "rebuild index: {error}"),
        }
    }
}

impl std::error::Error for OpenError {}

impl From<knot_git::GitError> for OpenError {
    fn from(error: knot_git::GitError) -> Self {
        Self::Git(error)
    }
}

impl From<knot_secrets::SecretsError> for OpenError {
    fn from(error: knot_secrets::SecretsError) -> Self {
        Self::Secrets(error)
    }
}

impl From<knot_index::IndexError> for OpenError {
    fn from(error: knot_index::IndexError) -> Self {
        Self::Index(error)
    }
}
