use std::path::Path;
use std::sync::Arc;

use axum::body::Bytes;
use futures::future::join_all;
use futures::stream::StreamExt;
use http::Method;
use serde_json::json;

use knot_runtime::{K256Signer, SeededEntropy};
use knot_types::{AccountDid, HttpStatus, KnotHostname, OwnerDid, RepoDid};

use crate::harness::Harness;
use crate::trace::{OperationIndex, Outcome, RoundNumber, Snapshot, Step, Trace, fnv1a};
use crate::workload::{
    Request, Rng, body_digest, drive_kill, enc, encode_body, http_call, jwt_window, mint,
    repo_did_of,
};

pub(crate) const SAMPLE_TAG: u64 = 0x5a3d_e005;
pub(crate) const SAMPLED_REPOS: SampledRepos = SampledRepos(24);
pub(crate) const SUBJECT_POOL: SubjectPool = SubjectPool(16);
pub(crate) const STRANGER_POOL: StrangerPool = StrangerPool(32);
const ADMIN_SIGNER_TAG: u64 = 0x0ad0_0001;
const DRIVE_TAG: u64 = 0x11c7_0005;

#[derive(Clone, Copy)]
pub(crate) struct SampledRepos(usize);

#[derive(Clone, Copy)]
pub(crate) struct SubjectPool(usize);

#[derive(Clone, Copy)]
pub(crate) struct StrangerPool(usize);

impl SampledRepos {
    pub(crate) fn sample(self, source: &[RepoDid], rng: &Rng) -> Vec<RepoDid> {
        pick(source, self.0, rng)
    }
}

impl SubjectPool {
    pub(crate) fn sample(self, source: &[AccountDid], rng: &Rng) -> Vec<AccountDid> {
        pick(source, self.0, rng)
    }
}

impl StrangerPool {
    pub(crate) fn count(self) -> usize {
        self.0
    }
}

pub struct RealActors {
    pub(crate) owner_repos: Vec<(RepoDid, OwnerDid)>,
    pub(crate) seed: u64,
}

pub(crate) fn owner_signer(seed: u64, owner: &OwnerDid) -> K256Signer {
    K256Signer::generate(&SeededEntropy::new(seed ^ fnv1a(owner.as_str().as_bytes())))
}

pub(crate) fn admin_signer(seed: u64) -> K256Signer {
    K256Signer::generate(&SeededEntropy::new(seed ^ ADMIN_SIGNER_TAG))
}

fn pick<T: Clone>(source: &[T], count: usize, rng: &Rng) -> Vec<T> {
    if source.is_empty() {
        return Vec::new();
    }
    (0..count.min(source.len()))
        .scan(Vec::<usize>::new(), |taken, _| {
            let start = rng.below(source.len() as u64) as usize;
            let index = (start..source.len())
                .chain(0..start)
                .find(|candidate| !taken.contains(candidate))
                .expect("distinct index exists while count <= len");
            taken.push(index);
            Some(source[index].clone())
        })
        .collect()
}

pub async fn run(
    target: &Path,
    seed: u64,
    rounds: u32,
) -> Result<Trace, crate::harness::OpenError> {
    let (harness, actors) = Harness::open(target, seed)?;
    Ok(drive(Arc::new(harness), actors, rounds).await)
}

struct OpResult {
    step: Step,
    created: Option<RepoDid>,
}

enum Exec {
    Http {
        op: &'static str,
        actor: String,
        fault: &'static str,
        request: Request,
        killed: bool,
        capture_create: bool,
    },
    Maintain {
        repo: RepoDid,
    },
}

async fn drive(harness: Arc<Harness>, actors: RealActors, rounds: u32) -> Trace {
    let seed = actors.seed;
    let rng = Rng::new(seed ^ DRIVE_TAG);
    let harness_ref = &harness;
    let actors_ref = &actors;
    let rng_ref = &rng;
    let base: Vec<RepoDid> = actors
        .owner_repos
        .iter()
        .map(|(repo, _)| repo.clone())
        .collect();
    let (_created, steps, snapshots) = futures::stream::iter(0..rounds)
        .fold(
            (
                Vec::<RepoDid>::new(),
                Vec::<Step>::new(),
                Vec::<Snapshot>::new(),
            ),
            |(created, mut steps, mut snapshots), round_index| {
                let harness = Arc::clone(harness_ref);
                let base = &base;
                async move {
                    let round = RoundNumber::new(round_index);
                    let working: Vec<RepoDid> =
                        base.iter().chain(created.iter()).cloned().collect();
                    let execs = if round_index % 2 == 0 {
                        mutate(&harness, actors_ref, &working, rng_ref, round)
                    } else {
                        reads(&working, rng_ref)
                    };
                    let dropped = arm_drops(&harness, &execs);

                    let tasks = execs
                        .into_iter()
                        .enumerate()
                        .map(|(position, exec)| {
                            let harness = Arc::clone(&harness);
                            let index = OperationIndex::new(position as u32);
                            tokio::spawn(async move { run_op(&harness, round, index, exec).await })
                        })
                        .collect::<Vec<_>>();
                    let results: Vec<OpResult> = join_all(tasks)
                        .await
                        .into_iter()
                        .map(|joined| joined.expect("real-data op task mustn't panic"))
                        .collect();
                    dropped
                        .iter()
                        .for_each(|host| harness.faults.clear_host(host));

                    let fresh: Vec<RepoDid> = results
                        .iter()
                        .filter_map(|result| result.created.clone())
                        .collect();
                    fresh.iter().for_each(|did| harness.populate(did));
                    steps.extend(results.into_iter().map(|result| result.step));

                    let mut created = created;
                    created.extend(fresh);
                    harness.advance(round_advance(rng_ref));
                    let snapshot_repos: Vec<RepoDid> =
                        base.iter().chain(created.iter()).cloned().collect();
                    snapshots.push(harness.snapshot(round, &snapshot_repos));
                    (created, steps, snapshots)
                }
            },
        )
        .await;
    Trace {
        seed,
        steps,
        snapshots,
    }
}

fn round_advance(rng: &Rng) -> std::time::Duration {
    std::time::Duration::from_micros(rng.below(3_000_000))
}

fn arm_drops(harness: &Harness, execs: &[Exec]) -> Vec<KnotHostname> {
    let hosts: Vec<KnotHostname> = execs
        .iter()
        .filter_map(|exec| match exec {
            Exec::Http {
                op: "probe",
                fault: "drop_identity",
                actor,
                ..
            } => KnotHostname::new(actor).ok(),
            _ => None,
        })
        .collect();
    hosts.iter().for_each(|host| harness.faults.drop_host(host));
    hosts
}

fn member_verb(draw: u64) -> (&'static str, &'static str) {
    match draw {
        0 => ("sh.tangled.knot.addMember", "addMember"),
        1 => ("sh.tangled.knot.removeMember", "removeMember"),
        2 => ("sh.tangled.knot.ban", "ban"),
        _ => ("sh.tangled.knot.unban", "unban"),
    }
}

struct Caller<'a> {
    signer: &'a K256Signer,
    did: &'a AccountDid,
}

fn signed_post(
    harness: &Harness,
    caller: Caller<'_>,
    nsid: &'static str,
    body: serde_json::Value,
    skew: bool,
    round: RoundNumber,
    slot: usize,
) -> Request {
    let token = mint(
        caller.signer,
        caller.did,
        &harness.knot_aud,
        nsid,
        jwt_window(harness, skew),
        round,
        OperationIndex::new(slot as u32),
    );
    Request {
        method: Method::POST,
        uri: format!("/xrpc/{nsid}"),
        token: Some(token),
        body: encode_body(body),
        actor: String::new(),
    }
}

fn mutate(
    harness: &Harness,
    actors: &RealActors,
    working: &[RepoDid],
    rng: &Rng,
    round: RoundNumber,
) -> Vec<Exec> {
    let subjects = &harness.subjects;
    let mut execs: Vec<Exec> = Vec::new();

    subjects
        .iter()
        .filter(|_| rng.chance(2, 3))
        .for_each(|subject| {
            let (nsid, short) = member_verb(rng.below(4));
            let skew = rng.chance(1, 5);
            let request = signed_post(
                harness,
                Caller {
                    signer: &harness.admin.signer,
                    did: &harness.admin.did,
                },
                nsid,
                json!({ "subject": subject.as_str() }),
                skew,
                round,
                execs.len(),
            );
            execs.push(Exec::Http {
                op: short,
                actor: format!("admin:{short}"),
                fault: skew_fault(skew),
                request,
                killed: false,
                capture_create: false,
            });
        });

    let collaborated: Vec<RepoDid> = if subjects.is_empty() {
        Vec::new()
    } else {
        actors
            .owner_repos
            .iter()
            .filter(|_| rng.chance(1, 2))
            .map(|(repo, owner)| {
                let subject = &subjects[rng.below(subjects.len() as u64) as usize];
                let skew = rng.chance(1, 6);
                let request = signed_post(
                    harness,
                    Caller {
                        signer: &owner_signer(actors.seed, owner),
                        did: &AccountDid::from(owner.clone()),
                    },
                    "sh.tangled.repo.addCollaborator",
                    json!({ "repo": repo.as_str(), "subject": subject.as_str() }),
                    skew,
                    round,
                    execs.len(),
                );
                execs.push(Exec::Http {
                    op: "addCollaborator",
                    actor: format!("owner:{}", owner.as_str()),
                    fault: skew_fault(skew),
                    request,
                    killed: false,
                    capture_create: false,
                });
                repo.clone()
            })
            .collect()
    };

    working
        .iter()
        .filter(|repo| !collaborated.contains(*repo))
        .filter(|_| rng.chance(1, 3))
        .for_each(|repo| execs.push(Exec::Maintain { repo: repo.clone() }));

    (0..rng.below(3)).for_each(|key| {
        let name = format!("sim-repo-{}-{}", round.get(), key);
        let skew = rng.chance(1, 8);
        let request = signed_post(
            harness,
            Caller {
                signer: &harness.admin.signer,
                did: &harness.admin.did,
            },
            "sh.tangled.repo.create",
            json!({ "rkey": name, "name": name }),
            skew,
            round,
            execs.len(),
        );
        execs.push(Exec::Http {
            op: "createRepo",
            actor: "admin:create".to_string(),
            fault: skew_fault(skew),
            request,
            killed: false,
            capture_create: !skew,
        });
    });

    if !subjects.is_empty() {
        let probes = rng.below(3) as usize;
        let slots: Vec<usize> = (0..harness.strangers.len()).collect();
        pick(&slots, probes, rng).into_iter().for_each(|slot| {
            let stranger = &harness.strangers[slot];
            let drop = rng.chance(1, 2);
            let request = signed_post(
                harness,
                Caller {
                    signer: &stranger.signer,
                    did: &stranger.did,
                },
                "sh.tangled.knot.addMember",
                json!({ "subject": subjects[0].as_str() }),
                false,
                round,
                execs.len(),
            );
            execs.push(Exec::Http {
                op: "probe",
                actor: stranger.host.to_string(),
                fault: if drop { "drop_identity" } else { "none" },
                request,
                killed: false,
                capture_create: false,
            });
        });
    }

    execs
}

fn skew_fault(skew: bool) -> &'static str {
    if skew { "clock_skew" } else { "none" }
}

fn reads(working: &[RepoDid], rng: &Rng) -> Vec<Exec> {
    let mut execs: Vec<Exec> = [
        ("version", "/xrpc/sh.tangled.knot.version"),
        ("owner", "/xrpc/sh.tangled.owner"),
        ("didJson", "/.well-known/did.json"),
    ]
    .into_iter()
    .map(|(op, uri)| anon_read(op, uri, rng.chance(1, 5)))
    .collect();

    working
        .iter()
        .filter(|_| rng.chance(2, 3))
        .for_each(|repo| {
            let killed = rng.chance(1, 5);
            let exec = match rng.below(7) {
                0 => repo_read("branches", "repo", repo, killed),
                1 => repo_read("log", "repo", repo, killed),
                2 => repo_read("describeRepo", "repoDid", repo, killed),
                3 => anon_read(
                    "infoRefs",
                    &format!("/{}/info/refs?service=git-upload-pack", repo.as_str()),
                    killed,
                ),
                4 => repo_read("tree", "repo", repo, killed),
                5 => blob_read(repo, killed),
                _ => repo_read("languages", "repo", repo, killed),
            };
            execs.push(exec);
        });
    execs
}

fn anon_read(op: &'static str, uri: &str, killed: bool) -> Exec {
    Exec::Http {
        op,
        actor: "anon".to_string(),
        fault: if killed { "killed" } else { "none" },
        request: Request {
            method: Method::GET,
            uri: uri.to_string(),
            token: None,
            body: Bytes::new(),
            actor: "anon".to_string(),
        },
        killed,
        capture_create: false,
    }
}

fn repo_read(op: &'static str, param: &str, repo: &RepoDid, killed: bool) -> Exec {
    anon_read(
        op,
        &format!("/xrpc/sh.tangled.repo.{op}?{param}={}", enc(repo.as_str())),
        killed,
    )
}

fn blob_read(repo: &RepoDid, killed: bool) -> Exec {
    anon_read(
        "blob",
        &format!(
            "/xrpc/sh.tangled.repo.blob?repo={}&path=README.md",
            enc(repo.as_str())
        ),
        killed,
    )
}

async fn run_op(
    harness: &Harness,
    round: RoundNumber,
    index: OperationIndex,
    exec: Exec,
) -> OpResult {
    match exec {
        Exec::Maintain { repo } => {
            let outcome = match harness.maintain(&repo) {
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
                step: Step {
                    round,
                    index,
                    op: "maintain",
                    actor: "knot".to_string(),
                    fault: "none",
                    outcome,
                },
                created: None,
            }
        }
        Exec::Http {
            op,
            actor,
            fault,
            request,
            killed,
            capture_create,
        } => {
            if killed {
                drive_kill(harness.router(), request.method, &request.uri, request.body).await;
                return OpResult {
                    step: Step {
                        round,
                        index,
                        op,
                        actor,
                        fault: "killed",
                        outcome: Outcome::Killed,
                    },
                    created: None,
                };
            }
            let (status, body) = http_call(
                harness.router(),
                request.method,
                &request.uri,
                request.token.as_deref(),
                request.body,
            )
            .await;
            let created = (capture_create && status == HttpStatus::new(200))
                .then(|| repo_did_of(&body).expect("200 createRepo response carries a repoDid"));
            OpResult {
                step: Step {
                    round,
                    index,
                    op,
                    actor,
                    fault,
                    outcome: Outcome::Answered {
                        status,
                        body: body_digest(&body),
                    },
                },
                created,
            }
        }
    }
}
