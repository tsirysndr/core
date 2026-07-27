use std::sync::Arc;

use axum::Json;
use axum::body::Bytes;
use axum::extract::State;
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, StatusCode};
use serde::{Deserialize, Serialize};

use knot_events::Reservation;
use knot_git::{
    ApplyError, ApplyOutcome, Conflict, Identity, NewCommit, ParsedFile, PatchApplier,
    PatchParseError, RefUpdate, Repo, StagedChange, Staging, is_format_patch,
    parse_mailbox_bounded, parse_patch_bounded,
};
use knot_index::Resolved;
use knot_postreceive::{Actor, Ci};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{
    AuthorName, BranchName, Email, Oid, OwnerDid, RefName, RepoDid, RepoRkey, UnixSeconds,
};

use crate::body::{CommitBody, CommitMessage, Patch, RepoNameArg};
use crate::error::XrpcError;
use crate::reads::{open, repo_not_found, warming};
use crate::{XrpcState, decode, ok_empty, run_blocking};

pub(crate) const MERGE_ROUTE: &str = "/xrpc/sh.tangled.repo.merge";
pub(crate) const MERGE_CHECK_ROUTE: &str = "/xrpc/sh.tangled.repo.mergeCheck";
const MERGE_RETRIES: u32 = 3;
const CONFLICT_MESSAGE: &str = "patch cannot be applied cleanly";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Committer {
    pub name: AuthorName,
    pub email: Email,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MergeInput {
    did: OwnerDid,
    name: RepoNameArg,
    patch: Patch,
    branch: BranchName,
    author_name: Option<AuthorName>,
    author_email: Option<Email>,
    commit_message: Option<CommitMessage>,
    commit_body: Option<CommitBody>,
}

#[derive(Deserialize)]
struct MergeCheckInput {
    did: OwnerDid,
    name: RepoNameArg,
    patch: Patch,
    branch: BranchName,
}

#[derive(Serialize)]
struct ConflictWire {
    filename: String,
    reason: String,
}

#[derive(Serialize)]
struct MergeCheckOutput {
    is_conflicted: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicts: Option<Vec<ConflictWire>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

impl MergeCheckOutput {
    fn clean() -> Self {
        Self {
            is_conflicted: false,
            conflicts: None,
            message: None,
            error: None,
        }
    }

    fn conflicted(conflicts: Vec<Conflict>) -> Self {
        Self {
            is_conflicted: true,
            conflicts: Some(
                conflicts
                    .into_iter()
                    .map(|conflict| ConflictWire {
                        filename: conflict.path,
                        reason: conflict.reason.as_str().to_string(),
                    })
                    .collect(),
            ),
            message: Some(CONFLICT_MESSAGE.to_string()),
            error: None,
        }
    }

    fn broken(error: String) -> Self {
        Self {
            is_conflicted: true,
            conflicts: None,
            message: None,
            error: Some(error),
        }
    }
}

struct CommitSpec {
    files: Vec<ParsedFile>,
    author: Option<MailAuthor>,
    message: String,
    change_id: Option<knot_git::CommitChangeId>,
}

struct MailAuthor {
    name: AuthorName,
    email: Email,
    date: String,
}

fn parse_specs(
    patch: &str,
    message: String,
    author: Option<MailAuthor>,
    max_bytes: u64,
) -> Result<Vec<CommitSpec>, PatchParseError> {
    match is_format_patch(patch) {
        true => Ok(parse_mailbox_bounded(patch, max_bytes)?
            .into_iter()
            .map(|mail| CommitSpec {
                message: mail.commit_message(),
                author: Some(MailAuthor {
                    name: mail.author_name,
                    email: mail.author_email,
                    date: mail.date,
                }),
                change_id: mail.change_id,
                files: mail.files,
            })
            .collect()),
        false => Ok(vec![CommitSpec {
            files: parse_patch_bounded(patch, max_bytes)?,
            author,
            message,
            change_id: None,
        }]),
    }
}

pub(crate) fn resolve_by_name<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    owner: &OwnerDid,
    name: &str,
) -> Result<RepoDid, XrpcError> {
    let rkey = RepoRkey::new(name).map_err(|_| repo_not_found())?;
    match state.index.resolve_repo(owner, &rkey) {
        Resolved::Ready(found) => found.ok_or_else(repo_not_found),
        Resolved::Warming => Err(warming()),
    }
}

fn branch_tip(repo: &Repo, refname: &RefName) -> Result<Oid, XrpcError> {
    repo.find_ref(refname)?
        .ok_or_else(|| XrpcError::invalid_request("no such branch to merge into"))
}

fn mail_time(date: &str, now: UnixSeconds) -> (UnixSeconds, i32) {
    let trimmed = date.trim();
    chrono::DateTime::parse_from_rfc2822(trimmed)
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(trimmed))
        .map(|parsed| {
            (
                UnixSeconds::new(parsed.timestamp()),
                parsed.offset().local_minus_utc(),
            )
        })
        .unwrap_or((now, 0))
}

fn spec_identities(
    spec: &CommitSpec,
    fallback: &Identity,
    now: UnixSeconds,
) -> (Identity, Vec<(String, Vec<u8>)>) {
    let author = match &spec.author {
        Some(mail) => {
            let (time, offset_seconds) = mail_time(&mail.date, now);
            Identity {
                name: mail.name.clone(),
                email: mail.email.clone(),
                time,
                offset_seconds,
            }
        }
        None => fallback.clone(),
    };
    let extra_headers = spec
        .change_id
        .iter()
        .map(|change_id| {
            (
                "change-id".to_string(),
                change_id.as_str().as_bytes().to_vec(),
            )
        })
        .collect();
    (author, extra_headers)
}

enum StageStop {
    Conflict(Vec<Conflict>),
    Apply(ApplyError),
}

fn stage_all(
    repo: &Repo,
    tip: Oid,
    specs: &[CommitSpec],
) -> Result<Result<Vec<Vec<StagedChange>>, Vec<Conflict>>, ApplyError> {
    let mut applier = PatchApplier::new(repo, tip);
    let staged = specs.iter().try_fold(Vec::new(), |mut clean, spec| {
        match applier.step(&spec.files) {
            Ok(ApplyOutcome::Clean(staged)) => {
                clean.push(staged);
                Ok(clean)
            }
            Ok(ApplyOutcome::Conflicted(conflicts)) => Err(StageStop::Conflict(conflicts)),
            Err(error) => Err(StageStop::Apply(error)),
        }
    });
    match staged {
        Ok(clean) => Ok(Ok(clean)),
        Err(StageStop::Conflict(conflicts)) => Ok(Err(conflicts)),
        Err(StageStop::Apply(error)) => Err(error),
    }
}

enum MergeAttempt {
    Done {
        old: Oid,
        new: Oid,
        reservation: Reservation,
    },
    Conflicted(Vec<Conflict>),
    Raced,
}

enum Merged {
    Done {
        old: Oid,
        new: Oid,
        reservation: Reservation,
    },
    Conflicted(Vec<Conflict>),
}

fn attempt_merge(
    repo: &Repo,
    refname: &RefName,
    specs: &[CommitSpec],
    committer: &Committer,
    now: UnixSeconds,
    reserve: &dyn Fn() -> Reservation,
) -> Result<MergeAttempt, XrpcError> {
    if specs.iter().any(|spec| spec.message.trim().is_empty()) {
        return Err(XrpcError::invalid_request("commit message is required"));
    }
    let tip = branch_tip(repo, refname)?;
    let staged = match stage_all(repo, tip, specs).map_err(XrpcError::from)? {
        Ok(staged) => staged,
        Err(conflicts) => return Ok(MergeAttempt::Conflicted(conflicts)),
    };
    let committer_identity = Identity {
        name: committer.name.clone(),
        email: committer.email.clone(),
        time: now,
        offset_seconds: 0,
    };
    let staging = Staging::new(repo).map_err(XrpcError::from)?;
    let work = staging.repo();
    let base_tree = work.find_commit(tip).map_err(XrpcError::from)?.tree;
    let new_tip = specs.iter().zip(staged).try_fold(
        (base_tree, tip),
        |(tree, parent), (spec, staged)| -> Result<(Oid, Oid), XrpcError> {
            let next_tree = work
                .write_staged_tree(tree, &staged)
                .map_err(XrpcError::from)?;
            let (author, extra_headers) = spec_identities(spec, &committer_identity, now);
            let commit = work
                .write_commit(&NewCommit {
                    tree: next_tree,
                    parents: vec![parent],
                    author,
                    committer: committer_identity.clone(),
                    message: spec.message.clone(),
                    extra_headers,
                })
                .map_err(XrpcError::from)?;
            Ok((next_tree, commit))
        },
    )?;
    match repo.find_ref(refname).map_err(XrpcError::from)? {
        Some(current) if current == tip => {}
        _ => return Ok(MergeAttempt::Raced),
    }
    staging.migrate_into(repo).map_err(XrpcError::from)?;
    match repo.update_ref_sealed(
        &RefUpdate::Update {
            name: refname.clone(),
            old: tip,
            new: new_tip.1,
        },
        reserve,
    ) {
        Ok(reservation) => Ok(MergeAttempt::Done {
            old: tip,
            new: new_tip.1,
            reservation,
        }),
        Err(error) => match repo.find_ref(refname) {
            Ok(Some(current)) if current != tip => Ok(MergeAttempt::Raced),
            _ => Err(error.into()),
        },
    }
}

fn merge_with_retry(
    repo: &Repo,
    refname: &RefName,
    specs: &[CommitSpec],
    committer: &Committer,
    now: UnixSeconds,
    attempts: u32,
    reserve: &dyn Fn() -> Reservation,
) -> Result<Merged, XrpcError> {
    match attempt_merge(repo, refname, specs, committer, now, reserve)? {
        MergeAttempt::Done {
            old,
            new,
            reservation,
        } => Ok(Merged::Done {
            old,
            new,
            reservation,
        }),
        MergeAttempt::Conflicted(conflicts) => Ok(Merged::Conflicted(conflicts)),
        MergeAttempt::Raced if attempts > 1 => {
            merge_with_retry(repo, refname, specs, committer, now, attempts - 1, reserve)
        }
        MergeAttempt::Raced => Err(XrpcError::conflict("branch moved during the merge, retry")),
    }
}

fn merge_conflict(conflicts: &[Conflict]) -> XrpcError {
    let detail = conflicts
        .first()
        .map(|conflict| {
            format!(
                "{CONFLICT_MESSAGE}: {} {}",
                conflict.path,
                conflict.reason.as_str()
            )
        })
        .unwrap_or_else(|| CONFLICT_MESSAGE.to_string());
    XrpcError::named(
        StatusCode::CONFLICT,
        "MergeConflict",
        format!("Merge failed due to conflicts: {detail}"),
    )
}

pub(crate) async fn merge<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    headers: HeaderMap,
    method: crate::Method,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let actor = state.authenticate(&headers, &method).await?;
    let input: MergeInput = decode(&body)?;
    let repo_did = resolve_by_name(&state, &input.did, input.name.as_str())?;
    crate::authorize_push(
        &state,
        &actor,
        &repo_did,
        "only repository owner or a collaborator may merge",
    )
    .await?;
    let refname = input.branch.head_ref();
    let committer = state.committer.clone();
    let now = state.now();
    let layout = state.layout.clone();
    let max_patch_bytes = state.byte_limits.patch_decompressed.get();
    let event_repo = repo_did.clone();
    let event_ref = refname.clone();

    let events = Arc::clone(&state.events);
    let outcome = run_blocking(move || {
        let specs = parse_specs(
            input.patch.as_str(),
            unified_message(&input),
            unified_author(&input),
            max_patch_bytes,
        )
        .map_err(|error| XrpcError::invalid_request(error.to_string()))?;
        let repo = open(&layout, &repo_did)?;
        let reserve = || events.reserve();
        merge_with_retry(
            &repo,
            &refname,
            &specs,
            &committer,
            now,
            MERGE_RETRIES,
            &reserve,
        )
    })
    .await?;

    match outcome {
        Merged::Conflicted(conflicts) => Err(merge_conflict(&conflicts)),
        Merged::Done {
            old,
            new,
            reservation,
        } => {
            let owner = crate::current_owner(&state, &event_repo);
            let layout = state.layout.clone();
            let languages_push_budget = state.budgets.languages_push;
            let catalog = Arc::clone(&state.catalog);
            let repo_label = event_repo.as_str().to_string();
            if let Err(error) = run_blocking(move || -> Result<(), XrpcError> {
                let repo = open(&layout, &event_repo)?;
                let update = RefUpdate::Update {
                    name: event_ref,
                    old,
                    new,
                };
                let post_actor = Actor {
                    committer: actor,
                    owner,
                    repo: event_repo,
                };
                knot_postreceive::post_receive(
                    &repo,
                    &post_actor,
                    vec![(update, reservation)],
                    &Ci::Skip,
                    &knot_types::PushOptions::default(),
                    None,
                    languages_push_budget,
                    &catalog.push,
                );
                Ok(())
            })
            .await
            {
                tracing::warn!(repo = %repo_label, %error, "post-receive after merge failed");
            }
            Ok(ok_empty())
        }
    }
}

fn unified_message(input: &MergeInput) -> String {
    let message = input
        .commit_message
        .as_ref()
        .map(|message| message.as_str().to_string())
        .unwrap_or_default();
    match input
        .commit_body
        .as_ref()
        .map(|body| body.as_str())
        .filter(|body| !body.is_empty())
    {
        Some(body) => format!("{message}\n\n{body}"),
        None => message,
    }
}

fn unified_author(input: &MergeInput) -> Option<MailAuthor> {
    match (input.author_name.as_ref(), input.author_email.as_ref()) {
        (Some(name), Some(email)) if !name.as_str().is_empty() && !email.as_str().is_empty() => {
            Some(MailAuthor {
                name: name.clone(),
                email: email.clone(),
                date: String::new(),
            })
        }
        _ => None,
    }
}

pub(crate) async fn merge_check<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    body: Bytes,
) -> Result<Response, XrpcError> {
    let input: MergeCheckInput = decode(&body)?;
    let repo_did = resolve_by_name(&state, &input.did, input.name.as_str())?;
    let refname = input.branch.head_ref();
    let layout = state.layout.clone();
    let max_patch_bytes = state.byte_limits.patch_decompressed.get();

    let output = run_blocking(move || {
        let specs = match parse_specs(input.patch.as_str(), String::new(), None, max_patch_bytes) {
            Ok(specs) => specs,
            Err(error) => return Ok(MergeCheckOutput::broken(error.to_string())),
        };
        let repo = open(&layout, &repo_did)?;
        let tip = branch_tip(&repo, &refname)?;
        match stage_all(&repo, tip, &specs) {
            Ok(Ok(_)) => Ok(MergeCheckOutput::clean()),
            Ok(Err(conflicts)) => Ok(MergeCheckOutput::conflicted(conflicts)),
            Err(ApplyError::TooLarge) => {
                Ok(MergeCheckOutput::broken(ApplyError::TooLarge.to_string()))
            }
            Err(ApplyError::Git(error)) => Err(error.into()),
        }
    })
    .await?;

    Ok(check_response(output))
}

fn check_response(output: MergeCheckOutput) -> Response {
    (StatusCode::OK, Json(output)).into_response()
}
