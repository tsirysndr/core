use axum::body::Bytes;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use futures::future::join_all;
use futures::stream::StreamExt;
use http::Method;
use http::header::AUTHORIZATION;
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tower::ServiceExt;

use knot_runtime::{Entropy, K256Signer, SeededEntropy, Signer};
use knot_types::{AccountDid, HttpStatus, KnotHostname, KnotId, RepoDid, UnixSeconds};

use crate::harness::{Harness, SUBJECT_DIDS};
use crate::trace::{OperationIndex, Outcome, Projection, RoundNumber, Step, Trace, fnv1a};

const SKEW_BACKDATE_SECS: i64 = 600;
const SKEW_LIFETIME_SECS: i64 = 60;

#[derive(Clone, Copy)]
struct RepoIndex(usize);

#[derive(Clone, Copy)]
struct SubjectIndex(usize);

#[derive(Clone, Copy)]
struct StrangerIndex(usize);

#[derive(Clone, Copy)]
enum ReadOp {
    Version,
    Owner,
    ListMembers,
    DidJson,
    InfoRefs(RepoIndex),
    Branches(RepoIndex),
    Log(RepoIndex),
    DescribeRepo(RepoIndex),
    Tree(RepoIndex),
    Blob(RepoIndex),
    Languages(RepoIndex),
}

impl ReadOp {
    fn name(self) -> &'static str {
        match self {
            ReadOp::Version => "version",
            ReadOp::Owner => "owner",
            ReadOp::ListMembers => "listMembers",
            ReadOp::DidJson => "didJson",
            ReadOp::InfoRefs(_) => "infoRefs",
            ReadOp::Branches(_) => "branches",
            ReadOp::Log(_) => "log",
            ReadOp::DescribeRepo(_) => "describeRepo",
            ReadOp::Tree(_) => "tree",
            ReadOp::Blob(_) => "blob",
            ReadOp::Languages(_) => "languages",
        }
    }
}

#[derive(Clone, Copy)]
enum AdminOp {
    AddMember(SubjectIndex),
    RemoveMember(SubjectIndex),
    Ban(SubjectIndex),
    Unban(SubjectIndex),
    CreateRepo(u32),
    AddCollaborator(RepoIndex, SubjectIndex),
}

impl AdminOp {
    fn name(self) -> &'static str {
        match self {
            AdminOp::AddMember(_) => "addMember",
            AdminOp::RemoveMember(_) => "removeMember",
            AdminOp::Ban(_) => "ban",
            AdminOp::Unban(_) => "unban",
            AdminOp::CreateRepo(_) => "createRepo",
            AdminOp::AddCollaborator(_, _) => "addCollaborator",
        }
    }
}

#[derive(Clone, Copy)]
enum Planned {
    Read { op: ReadOp, killed: bool },
    Admin { op: AdminOp, skew: bool },
    Probe { stranger: StrangerIndex, drop: bool },
    Maintain { repo: RepoIndex },
}

pub(crate) struct Round {
    ops: Vec<Planned>,
    advance: std::time::Duration,
}

pub(crate) struct Rng(SeededEntropy);

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(SeededEntropy::new(seed ^ 0x57ee_d000))
    }

    pub(crate) fn below(&self, n: u64) -> u64 {
        self.0.next_u64() % n.max(1)
    }

    pub(crate) fn chance(&self, num: u64, den: u64) -> bool {
        assert!(num <= den, "chance numerator exceeds denominator");
        self.below(den) < num
    }
}

pub(crate) fn plan(seed: u64, rounds: u32, subjects: usize) -> Vec<Round> {
    let rng = Rng::new(seed);
    let mut available: u32 = 1;
    let mut rkey: u32 = 0;
    let mut stranger: usize = 0;
    (0..rounds)
        .map(|round| {
            let (ops, fresh_repos) = if round % 2 == 0 {
                mutate_round(&rng, subjects, available, &mut rkey, &mut stranger)
            } else {
                (read_round(&rng, available), 0)
            };
            let advance = std::time::Duration::from_micros(rng.below(3_000_000));
            available += fresh_repos;
            Round { ops, advance }
        })
        .collect()
}

pub(crate) fn predict(seed: u64, rounds: u32) -> Projection {
    let (members, blocked) = plan(seed, rounds, SUBJECT_DIDS.len())
        .iter()
        .flat_map(|round| round.ops.iter())
        .fold(
            (BTreeSet::<AccountDid>::new(), BTreeSet::<AccountDid>::new()),
            |(mut members, mut blocked), planned| {
                let subject_did = |subject: &SubjectIndex| {
                    AccountDid::new(SUBJECT_DIDS[subject.0]).expect("subject did")
                };
                if let Planned::Admin { op, skew: false } = planned {
                    match op {
                        AdminOp::AddMember(subject) => {
                            members.insert(subject_did(subject));
                        }
                        AdminOp::RemoveMember(subject) => {
                            members.remove(&subject_did(subject));
                        }
                        AdminOp::Ban(subject) => {
                            blocked.insert(subject_did(subject));
                        }
                        AdminOp::Unban(subject) => {
                            blocked.remove(&subject_did(subject));
                        }
                        AdminOp::CreateRepo(_) | AdminOp::AddCollaborator(_, _) => {}
                    }
                }
                (members, blocked)
            },
        );
    Projection {
        members: members.into_iter().collect(),
        blocked: blocked.into_iter().collect(),
    }
}

fn mutate_round(
    rng: &Rng,
    subjects: usize,
    available: u32,
    rkey: &mut u32,
    stranger: &mut usize,
) -> (Vec<Planned>, u32) {
    let mut ops: Vec<Planned> = (0..subjects)
        .filter(|_| rng.chance(2, 3))
        .map(|subject| {
            let subject = SubjectIndex(subject);
            let op = match rng.below(4) {
                0 => AdminOp::AddMember(subject),
                1 => AdminOp::RemoveMember(subject),
                2 => AdminOp::Ban(subject),
                _ => AdminOp::Unban(subject),
            };
            Planned::Admin {
                op,
                skew: rng.chance(1, 5),
            }
        })
        .collect();

    (0..available)
        .filter(|_| rng.chance(1, 2))
        .for_each(|repo| {
            let planned = if rng.chance(1, 2) {
                let subject = SubjectIndex(rng.below(subjects as u64) as usize);
                Planned::Admin {
                    op: AdminOp::AddCollaborator(RepoIndex(repo as usize), subject),
                    skew: rng.chance(1, 6),
                }
            } else {
                Planned::Maintain {
                    repo: RepoIndex(repo as usize),
                }
            };
            ops.push(planned);
        });

    let mut fresh_repos = 0;
    (0..rng.below(3)).for_each(|_| {
        let key = *rkey;
        *rkey += 1;
        let skew = rng.chance(1, 8);
        if !skew {
            fresh_repos += 1;
        }
        ops.push(Planned::Admin {
            op: AdminOp::CreateRepo(key),
            skew,
        });
    });

    (0..rng.below(3)).for_each(|_| {
        let stranger_index = StrangerIndex(*stranger);
        *stranger += 1;
        ops.push(Planned::Probe {
            stranger: stranger_index,
            drop: rng.chance(1, 2),
        });
    });

    (ops, fresh_repos)
}

fn read_round(rng: &Rng, available: u32) -> Vec<Planned> {
    let mut ops: Vec<Planned> = [
        ReadOp::Version,
        ReadOp::Owner,
        ReadOp::ListMembers,
        ReadOp::DidJson,
    ]
    .into_iter()
    .map(|op| Planned::Read {
        op,
        killed: rng.chance(1, 5),
    })
    .collect();
    (0..available)
        .filter(|_| rng.chance(2, 3))
        .for_each(|repo| {
            let repo = RepoIndex(repo as usize);
            let op = match rng.below(7) {
                0 => ReadOp::Branches(repo),
                1 => ReadOp::Log(repo),
                2 => ReadOp::DescribeRepo(repo),
                3 => ReadOp::InfoRefs(repo),
                4 => ReadOp::Tree(repo),
                5 => ReadOp::Blob(repo),
                _ => ReadOp::Languages(repo),
            };
            ops.push(Planned::Read {
                op,
                killed: rng.chance(1, 5),
            });
        });
    ops
}

struct OpResult {
    step: Step,
    created: Option<RepoDid>,
}

pub(crate) async fn execute(harness: Arc<Harness>, seed: u64, plan: Vec<Round>) -> Trace {
    let initial = (
        vec![harness.seed_repo.clone()],
        Vec::<Step>::new(),
        Vec::new(),
    );
    let harness = &harness;
    let (repos, steps, snapshots) = futures::stream::iter(plan.into_iter().enumerate())
        .fold(
            initial,
            |(repos, mut steps, mut snapshots), (round_index, round)| {
                let harness = Arc::clone(harness);
                async move {
                    let round_no = RoundNumber::new(round_index as u32);
                    let drops = arm_drops(&harness, &round.ops);
                    let repos = Arc::new(repos);
                    let tasks = round
                        .ops
                        .iter()
                        .enumerate()
                        .map(|(index, planned)| {
                            let harness = Arc::clone(&harness);
                            let repos = Arc::clone(&repos);
                            let planned = *planned;
                            tokio::spawn(async move {
                                run_op(
                                    &harness,
                                    &repos,
                                    round_no,
                                    OperationIndex::new(index as u32),
                                    planned,
                                )
                                .await
                            })
                        })
                        .collect::<Vec<_>>();
                    let results: Vec<OpResult> = join_all(tasks)
                        .await
                        .into_iter()
                        .map(|joined| joined.expect("sim op task mustn't panic"))
                        .collect();
                    drops
                        .iter()
                        .for_each(|host| harness.faults.clear_host(host));

                    let created: Vec<RepoDid> = results
                        .iter()
                        .filter_map(|result| result.created.clone())
                        .collect();
                    let planned_creates = round
                        .ops
                        .iter()
                        .filter(|planned| {
                            matches!(
                                planned,
                                Planned::Admin {
                                    op: AdminOp::CreateRepo(_),
                                    skew: false,
                                }
                            )
                        })
                        .count();
                    assert_eq!(
                        planned_creates,
                        created.len(),
                        "round {}: {planned_creates} non-skew creates planned but \
                         {} materialized, so plan/execute repo indices have drifted apart",
                        round_no.get(),
                        created.len()
                    );
                    created.iter().for_each(|did| harness.populate(did));
                    steps.extend(results.into_iter().map(|result| result.step));
                    let mut repos = Arc::into_inner(repos)
                        .expect("all op tasks released the round repo snapshot");
                    repos.extend(created);

                    harness.advance(round.advance);
                    snapshots.push(harness.snapshot(round_no, &repos));
                    (repos, steps, snapshots)
                }
            },
        )
        .await;
    let no_fault_creates = steps
        .iter()
        .filter(|step| step.op == "createRepo" && step.fault == "none")
        .count();
    let materialized = repos.len() - 1;
    assert_eq!(
        no_fault_creates, materialized,
        "no-fault createRepo count {no_fault_creates} doesn't match the {materialized} repos \
         materialized: a planned create silently failed and repo_at would have masked the drift"
    );
    Trace {
        seed,
        steps,
        snapshots,
    }
}

fn arm_drops(harness: &Harness, ops: &[Planned]) -> Vec<KnotHostname> {
    let hosts: Vec<KnotHostname> = ops
        .iter()
        .filter_map(|planned| match planned {
            Planned::Probe {
                stranger,
                drop: true,
            } => Some(harness.strangers[stranger.0].host.clone()),
            _ => None,
        })
        .collect();
    hosts.iter().for_each(|host| harness.faults.drop_host(host));
    hosts
}

async fn run_op(
    harness: &Harness,
    repos: &[RepoDid],
    round: RoundNumber,
    index: OperationIndex,
    planned: Planned,
) -> OpResult {
    let make = |op: &'static str, actor: String, fault: &'static str, outcome: Outcome| Step {
        round,
        index,
        op,
        actor,
        fault,
        outcome,
    };

    match planned {
        Planned::Maintain { repo } => {
            let repo = repo_at(repos, repo);
            let outcome = match harness.maintain(repo) {
                Ok(()) => Outcome::Answered {
                    status: HttpStatus::new(200),
                    body: 0,
                },
                Err(message) => Outcome::Answered {
                    status: HttpStatus::new(500),
                    body: fnv1a(message.as_bytes()),
                },
            };
            OpResult {
                step: make("maintain", "knot".to_string(), "none", outcome),
                created: None,
            }
        }
        Planned::Read { op, killed } => {
            let request = read_request(repos, op);
            if killed {
                drive_kill(harness.router(), request.method, &request.uri, request.body).await;
                return OpResult {
                    step: make(op.name(), request.actor, "killed", Outcome::Killed),
                    created: None,
                };
            }
            let (status, body) = http_call(
                harness.router(),
                request.method,
                &request.uri,
                None,
                request.body,
            )
            .await;
            OpResult {
                step: make(
                    op.name(),
                    request.actor,
                    "none",
                    Outcome::Answered {
                        status,
                        body: body_digest(&body),
                    },
                ),
                created: None,
            }
        }
        Planned::Admin { op, skew } => {
            let request = admin_request(harness, repos, op, skew, round, index);
            let (status, body) = http_call(
                harness.router(),
                request.method,
                &request.uri,
                request.token.as_deref(),
                request.body,
            )
            .await;
            let created = match op {
                AdminOp::CreateRepo(_) if status == HttpStatus::new(200) => repo_did_of(&body),
                _ => None,
            };
            OpResult {
                step: make(
                    op.name(),
                    request.actor,
                    if skew { "clock_skew" } else { "none" },
                    Outcome::Answered {
                        status,
                        body: body_digest(&body),
                    },
                ),
                created,
            }
        }
        Planned::Probe { stranger, drop } => {
            let request = probe_request(harness, stranger, round, index);
            let (status, body) = http_call(
                harness.router(),
                request.method,
                &request.uri,
                request.token.as_deref(),
                request.body,
            )
            .await;
            OpResult {
                step: make(
                    "probe",
                    request.actor,
                    if drop { "drop_identity" } else { "none" },
                    Outcome::Answered {
                        status,
                        body: body_digest(&body),
                    },
                ),
                created: None,
            }
        }
    }
}

pub(crate) struct Request {
    pub(crate) method: Method,
    pub(crate) uri: String,
    pub(crate) token: Option<String>,
    pub(crate) body: Bytes,
    pub(crate) actor: String,
}

fn read_request(repos: &[RepoDid], op: ReadOp) -> Request {
    match op {
        ReadOp::Version => get("/xrpc/sh.tangled.knot.version"),
        ReadOp::Owner => get("/xrpc/sh.tangled.owner"),
        ReadOp::ListMembers => {
            get("/xrpc/sh.tangled.knot.listMembers?subject=did:web:knot.nel.pet")
        }
        ReadOp::DidJson => get("/.well-known/did.json"),
        ReadOp::InfoRefs(repo) => Request {
            method: Method::GET,
            uri: format!(
                "/{}/info/refs?service=git-upload-pack",
                repo_at(repos, repo).as_str()
            ),
            token: None,
            body: Bytes::new(),
            actor: "anon".to_string(),
        },
        ReadOp::Branches(repo) => repo_get("branches", "repo", repo_at(repos, repo)),
        ReadOp::Log(repo) => repo_get("log", "repo", repo_at(repos, repo)),
        ReadOp::DescribeRepo(repo) => repo_get("describeRepo", "repoDid", repo_at(repos, repo)),
        ReadOp::Tree(repo) => repo_get("tree", "repo", repo_at(repos, repo)),
        ReadOp::Languages(repo) => repo_get("languages", "repo", repo_at(repos, repo)),
        ReadOp::Blob(repo) => Request {
            method: Method::GET,
            uri: format!(
                "/xrpc/sh.tangled.repo.blob?repo={}&path=README.md",
                enc(repo_at(repos, repo).as_str())
            ),
            token: None,
            body: Bytes::new(),
            actor: "anon".to_string(),
        },
    }
}

fn admin_request(
    harness: &Harness,
    repos: &[RepoDid],
    op: AdminOp,
    skew: bool,
    round: RoundNumber,
    index: OperationIndex,
) -> Request {
    let subjects = &harness.subjects;
    match op {
        AdminOp::AddMember(subject) => admin_post(
            harness,
            "addMember",
            "sh.tangled.knot.addMember",
            json!({ "subject": subjects[subject.0].as_str() }),
            skew,
            round,
            index,
        ),
        AdminOp::RemoveMember(subject) => admin_post(
            harness,
            "removeMember",
            "sh.tangled.knot.removeMember",
            json!({ "subject": subjects[subject.0].as_str() }),
            skew,
            round,
            index,
        ),
        AdminOp::Ban(subject) => admin_post(
            harness,
            "ban",
            "sh.tangled.knot.ban",
            json!({ "subject": subjects[subject.0].as_str() }),
            skew,
            round,
            index,
        ),
        AdminOp::Unban(subject) => admin_post(
            harness,
            "unban",
            "sh.tangled.knot.unban",
            json!({ "subject": subjects[subject.0].as_str() }),
            skew,
            round,
            index,
        ),
        AdminOp::CreateRepo(key) => {
            let name = format!("repo{key}");
            admin_post(
                harness,
                "create",
                "sh.tangled.repo.create",
                json!({ "rkey": name, "name": name }),
                skew,
                round,
                index,
            )
        }
        AdminOp::AddCollaborator(repo, subject) => admin_post(
            harness,
            "addCollaborator",
            "sh.tangled.repo.addCollaborator",
            json!({
                "repo": repo_at(repos, repo).as_str(),
                "subject": subjects[subject.0].as_str(),
            }),
            skew,
            round,
            index,
        ),
    }
}

fn probe_request(
    harness: &Harness,
    stranger: StrangerIndex,
    round: RoundNumber,
    index: OperationIndex,
) -> Request {
    let actor = &harness.strangers[stranger.0];
    let token = mint(
        &actor.signer,
        &actor.did,
        &harness.knot_aud,
        "sh.tangled.knot.addMember",
        jwt_window(harness, false),
        round,
        index,
    );
    Request {
        method: Method::POST,
        uri: "/xrpc/sh.tangled.knot.addMember".to_string(),
        token: Some(token),
        body: encode_body(json!({ "subject": harness.subjects[0].as_str() })),
        actor: actor.host.to_string(),
    }
}

fn get(path: &str) -> Request {
    Request {
        method: Method::GET,
        uri: path.to_string(),
        token: None,
        body: Bytes::new(),
        actor: "anon".to_string(),
    }
}

fn repo_get(method: &str, param: &str, repo: &RepoDid) -> Request {
    Request {
        method: Method::GET,
        uri: format!(
            "/xrpc/sh.tangled.repo.{method}?{param}={}",
            enc(repo.as_str())
        ),
        token: None,
        body: Bytes::new(),
        actor: "anon".to_string(),
    }
}

fn admin_post(
    harness: &Harness,
    method_short: &str,
    nsid: &'static str,
    body: Value,
    skew: bool,
    round: RoundNumber,
    index: OperationIndex,
) -> Request {
    let admin = &harness.admin;
    let token = mint(
        &admin.signer,
        &admin.did,
        &harness.knot_aud,
        nsid,
        jwt_window(harness, skew),
        round,
        index,
    );
    Request {
        method: Method::POST,
        uri: format!("/xrpc/{nsid}"),
        token: Some(token),
        body: encode_body(body),
        actor: format!("admin:{method_short}"),
    }
}

pub(crate) fn jwt_window(harness: &Harness, skew: bool) -> (UnixSeconds, UnixSeconds) {
    let now = harness.now_seconds();
    if skew {
        (
            now.saturating_sub_secs(SKEW_BACKDATE_SECS + SKEW_LIFETIME_SECS),
            now.saturating_sub_secs(SKEW_BACKDATE_SECS),
        )
    } else {
        (now, now.saturating_add_secs(60))
    }
}

pub(crate) fn mint(
    signer: &K256Signer,
    issuer: &AccountDid,
    aud: &KnotId,
    nsid: &str,
    window: (UnixSeconds, UnixSeconds),
    round: RoundNumber,
    index: OperationIndex,
) -> String {
    let header = URL_SAFE_NO_PAD.encode(br#"{"alg":"ES256K","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD.encode(
        serde_json::to_vec(&json!({
            "iss": issuer.as_str(),
            "aud": aud.as_str(),
            "iat": window.0.get(),
            "exp": window.1.get(),
            "jti": format!("sim-{}-{}", round.get(), index.get()),
            "lxm": nsid,
        }))
        .expect("claims serialize"),
    );
    let signing_input = format!("{header}.{payload}");
    let signature = signer.sign(signing_input.as_bytes());
    format!(
        "{signing_input}.{}",
        URL_SAFE_NO_PAD.encode(signature.as_bytes())
    )
}

pub(crate) async fn http_call(
    router: axum::Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Bytes,
) -> (HttpStatus, Bytes) {
    let mut request = http::Request::builder()
        .method(method)
        .uri(uri)
        .body(axum::body::Body::from(body))
        .expect("request builds");
    if let Some(token) = token {
        request.headers_mut().insert(
            AUTHORIZATION,
            http::HeaderValue::from_str(&format!("Bearer {token}")).expect("bearer header"),
        );
    }
    request
        .extensions_mut()
        .insert(axum::extract::ConnectInfo(SocketAddr::from((
            [127, 0, 0, 1],
            4242,
        ))));
    let response = router.oneshot(request).await.expect("router answers");
    let status = HttpStatus::from(response.status());
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    (status, bytes)
}

pub(crate) async fn drive_kill(router: axum::Router, method: Method, uri: &str, body: Bytes) {
    let call = http_call(router, method, uri, None, body);
    futures::pin_mut!(call);
    tokio::select! {
        biased;
        _ = &mut call => {}
        _ = tokio::task::yield_now() => {}
    }
}

pub(crate) fn repo_did_of(body: &Bytes) -> Option<RepoDid> {
    serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("repoDid")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .and_then(|did| RepoDid::new(did).ok())
}

pub(crate) fn body_digest(body: &Bytes) -> u64 {
    match serde_json::from_slice::<Value>(body) {
        Ok(mut value) => {
            canonicalize(&mut value);
            fnv1a(&serde_json::to_vec(&value).expect("canonical body serializes"))
        }
        Err(_) => fnv1a(body),
    }
}

fn canonicalize(value: &mut Value) {
    match value {
        Value::Array(items) => {
            items.iter_mut().for_each(canonicalize);
            items.sort_by_cached_key(|item| serde_json::to_string(item).expect("array item"));
        }
        Value::Object(map) => map.values_mut().for_each(canonicalize),
        _ => {}
    }
}

pub(crate) fn encode_body(value: Value) -> Bytes {
    Bytes::from(serde_json::to_vec(&value).expect("request body serializes"))
}

pub(crate) fn enc(did: &str) -> String {
    did.replace(':', "%3A")
}

fn repo_at(repos: &[RepoDid], index: RepoIndex) -> &RepoDid {
    repos.get(index.0).unwrap_or_else(|| {
        panic!(
            "plan/execute repo drift: index {} exceeds {} repos created so far",
            index.0,
            repos.len()
        )
    })
}
