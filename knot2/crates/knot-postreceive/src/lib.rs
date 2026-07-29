use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use knot_events::{
    CommitCount, EmailCommitCount, GitRefUpdate, LanguageSize, RefUpdateMeta, Reservation,
};
use knot_git::{EntryKind, Haves, RefUpdate, Repo, Wants};
use knot_messages::{CiLogsKey, PushMessages, UrlKey};
use knot_types::{
    AccountDid, AppviewEndpoint, BranchName, ChangedFiles, CiLogsAddr, Email, Handle, Listing, Oid,
    OriginUrl, OwnerDid, PushOptions, RefName, RefTransition, RepoDid, RepoPath, RepoRkey,
};
use knot_workflow::{Compiled, RawWorkflow, Trigger, WorkflowName};
use url::Url;

const WORKFLOW_DIR: &str = ".tangled/workflows";

pub struct Actor {
    pub committer: AccountDid,
    pub owner: Option<OwnerDid>,
    pub repo: RepoDid,
}

pub enum Ci {
    Skip,
    Compile {
        logs: Option<CiLogsAddr>,
        verbose: bool,
    },
}

pub enum OwnerLabel {
    Handle(Handle),
    Did(OwnerDid),
}

impl OwnerLabel {
    pub fn as_str(&self) -> &str {
        match self {
            OwnerLabel::Handle(handle) => handle.as_str(),
            OwnerLabel::Did(did) => did.as_str(),
        }
    }
}

pub struct PullLink {
    pub appview: AppviewEndpoint,
    pub owner: OwnerLabel,
    pub rkey: RepoRkey,
}

struct SourceBranch(BranchName);
struct TargetBranch(BranchName);

knot_types::scalar_newtype! {
    pub struct LanguagesPushBudget(Duration);
}

struct PushContext<'a> {
    actor: &'a Actor,
    ci: &'a Ci,
    push_options: &'a PushOptions,
    pull: Option<&'a PullLink>,
    languages_budget: LanguagesPushBudget,
    messages: &'a PushMessages,
}

#[allow(clippy::too_many_arguments)]
pub fn post_receive(
    repo: &Repo,
    actor: &Actor,
    applied: Vec<(RefUpdate, Reservation)>,
    ci: &Ci,
    push_options: &PushOptions,
    pull: Option<&PullLink>,
    languages_budget: LanguagesPushBudget,
    messages: &PushMessages,
) -> Vec<String> {
    let context = PushContext {
        actor,
        ci,
        push_options,
        pull,
        languages_budget,
        messages,
    };
    applied
        .into_iter()
        .flat_map(|(update, reservation)| publish_one(repo, update, reservation, &context))
        .collect()
}

fn publish_one(
    repo: &Repo,
    update: RefUpdate,
    reservation: Reservation,
    context: &PushContext,
) -> Vec<String> {
    let name = update.name();
    let transition = update.transition();
    let changed = match transition.new_oid() {
        Some(new) => changed_paths(repo, name, transition.old_oid(), new),
        None => ChangedFiles::none(),
    };
    let pipeline = ci_messages(repo, name, transition, &changed, context);
    let event = GitRefUpdate::new(
        context.actor.repo.clone(),
        context.actor.owner.clone(),
        context.actor.committer.clone(),
    )
    .on_ref(name.clone(), transition, repo.object_format())
    .with_push_options(context.push_options)
    .with_changed_files(changed);
    let event = match transition.new_oid() {
        Some(new) => event.with_meta(ref_update_meta(
            repo,
            name,
            transition.old_oid(),
            new,
            context.languages_budget,
        )),
        None => event,
    };
    reservation.fulfill(&event);

    let pull_link = match (transition, context.pull) {
        (RefTransition::Create { .. }, Some(link)) => {
            pull_request_message(repo, link, name, context.messages, &context.actor.repo)
                .unwrap_or_default()
        }
        _ => Vec::new(),
    };
    pull_link.into_iter().chain(pipeline).collect()
}

fn changed_paths(repo: &Repo, name: &RefName, old: Option<Oid>, new: Oid) -> ChangedFiles {
    let range = knot_git::PatchRange {
        base: old,
        head: new,
    };
    match repo.changed_paths(range) {
        Ok(changed) => {
            if changed.listing() == Listing::Truncated {
                tracing::warn!(
                    ref_name = name.as_str(),
                    path = %repo.path().display(),
                    files = changed.paths().len(),
                    "changed-file listing truncated at the record budget, leaving every paths constraint assumed matched"
                );
            }
            changed
        }
        Err(error) => {
            tracing::warn!(
                ref_name = name.as_str(),
                path = %repo.path().display(),
                %error,
                "changed-file listing failed, leaving every paths constraint assumed matched"
            );
            ChangedFiles::unknown()
        }
    }
}

fn pull_request_message(
    repo: &Repo,
    link: &PullLink,
    name: &RefName,
    messages: &PushMessages,
    repo_did: &RepoDid,
) -> Option<Vec<String>> {
    let branch = branch_short(name)?;
    let default_ref = repo.default_branch()?;
    let default = branch_short(&default_ref)?;
    if branch == default {
        return None;
    }
    repo.find_ref(&default_ref).ok().flatten()?;

    let url = match repo.origin_url() {
        Some(remote) => fork_pull_url(
            &link.appview,
            &SourceBranch(branch),
            &TargetBranch(default),
            remote,
            repo_did,
        )?,
        None => branch_pull_url(
            &link.appview,
            &link.owner,
            &link.rkey,
            &SourceBranch(branch),
            &TargetBranch(default),
        )?,
    };

    Some(messages.pull_request.lines(|UrlKey::Url| url.to_string()))
}

fn branch_pull_url(
    appview: &AppviewEndpoint,
    owner: &OwnerLabel,
    repo_rkey: &RepoRkey,
    source: &SourceBranch,
    target: &TargetBranch,
) -> Option<Url> {
    let mut url = Url::parse(appview.as_str()).ok()?;

    url.path_segments_mut().ok()?.pop_if_empty().extend([
        owner.as_str(),
        repo_rkey.as_str(),
        "pulls",
        "new",
    ]);

    url.query_pairs_mut()
        .append_pair("source", "branch")
        .append_pair("sourceBranch", source.0.as_str())
        .append_pair("targetBranch", target.0.as_str());

    Some(url)
}

fn fork_pull_url(
    appview: &AppviewEndpoint,
    source: &SourceBranch,
    target: &TargetBranch,
    remote: OriginUrl,
    repo_did: &RepoDid,
) -> Option<Url> {
    let remote_url = Url::parse(remote.as_str()).ok()?;

    // TODO: We need to handle file schemes. For now though if the remote is a
    // file scheme a fork PR link won't be created.
    match remote_url.scheme() {
        "http" | "https" => (),
        _ => return None,
    }

    let paths: Vec<&str> = remote_url
        .path_segments()
        .map(|segments| segments.collect())
        .unwrap_or_default();

    let mut url = Url::parse(appview.as_str()).ok()?;

    url.path_segments_mut()
        .ok()?
        .pop_if_empty()
        .extend(paths)
        .extend(["pulls", "new"]);

    url.query_pairs_mut()
        .append_pair("source", "fork")
        .append_pair("sourceBranch", source.0.as_str())
        .append_pair("targetBranch", target.0.as_str())
        .append_pair("fork", repo_did.as_str());

    Some(url)
}

fn ref_update_meta(
    repo: &Repo,
    name: &RefName,
    old: Option<Oid>,
    new: Oid,
    languages_budget: LanguagesPushBudget,
) -> RefUpdateMeta {
    let is_default_ref = is_default_branch(repo, name);
    let by_email = commit_counts(repo, name, old, new);
    let languages = match is_default_ref {
        true => language_sizes(repo, new, languages_budget),
        false => Vec::new(),
    };
    RefUpdateMeta::new(is_default_ref, by_email, languages)
}

fn is_default_branch(repo: &Repo, name: &RefName) -> bool {
    match (branch_short(name), repo.default_branch()) {
        (Some(short), Some(default)) => branch_short(&default) == Some(short),
        _ => false,
    }
}

fn branch_short(name: &RefName) -> Option<BranchName> {
    name.as_str()
        .strip_prefix("refs/heads/")
        .and_then(|short| BranchName::new(short).ok())
}

fn commit_counts(repo: &Repo, name: &RefName, old: Option<Oid>, new: Oid) -> Vec<EmailCommitCount> {
    let tip = match repo.peel_to_commit(new) {
        Ok(tip) => tip,
        Err(error) => {
            tracing::warn!(
                ref_name = name.as_str(),
                path = %repo.path().display(),
                %error,
                "commit tally failed peeling new tip"
            );
            return Vec::new();
        }
    };
    let haves = match old {
        Some(old) => match repo.peel_to_commit(old) {
            Ok(base) => vec![base],
            Err(error) => {
                tracing::warn!(
                    ref_name = name.as_str(),
                    path = %repo.path().display(),
                    %error,
                    "commit tally failed reading prior tip"
                );
                return Vec::new();
            }
        },
        None => sibling_tips(repo, name),
    };
    let walked = match repo.rev_walk(Wants::new(&[tip]), Haves::new(&haves)) {
        Ok(oids) => oids,
        Err(error) => {
            tracing::warn!(
                ref_name = name.as_str(),
                path = %repo.path().display(),
                %error,
                "commit tally walk failed"
            );
            return Vec::new();
        }
    };
    let tallies = walked
        .into_iter()
        .filter_map(|oid| repo.find_commit(oid).ok())
        .fold(
            BTreeMap::<Email, CommitCount>::new(),
            |mut counts, commit| {
                let slot = counts.entry(commit.author.email).or_default();
                *slot = slot.succ();
                counts
            },
        );
    tallies
        .into_iter()
        .map(|(email, count)| EmailCommitCount::new(email, count))
        .collect()
}

fn sibling_tips(repo: &Repo, name: &RefName) -> Vec<Oid> {
    match repo.references() {
        Ok(records) => records
            .into_iter()
            .filter(|record| {
                record.name.as_str() != name.as_str()
                    && record.name.as_str().starts_with("refs/heads/")
            })
            .filter_map(|record| repo.peel_to_commit(record.target).ok())
            .collect(),
        Err(error) => {
            tracing::warn!(
                ref_name = name.as_str(),
                path = %repo.path().display(),
                %error,
                "sibling ref scan failed"
            );
            Vec::new()
        }
    }
}

fn language_sizes(
    repo: &Repo,
    new: Oid,
    languages_budget: LanguagesPushBudget,
) -> Vec<LanguageSize> {
    let deadline = Instant::now() + languages_budget.get();
    match knot_langs::analyze(repo, new, Some(deadline)) {
        Ok(sizes) => sizes
            .into_iter()
            .filter(|(_, size)| size.get() > 0)
            .map(|(name, size)| LanguageSize::new(name, size))
            .collect(),
        Err(error) => {
            tracing::warn!(
                path = %repo.path().display(),
                commit = %new.to_hex(),
                %error,
                "language breakdown failed"
            );
            Vec::new()
        }
    }
}

fn ci_messages(
    repo: &Repo,
    name: &RefName,
    transition: RefTransition,
    changed: &ChangedFiles,
    context: &PushContext,
) -> Vec<String> {
    let Ci::Compile { logs, verbose } = context.ci else {
        return Vec::new();
    };
    let Some(new) = transition.new_oid() else {
        return Vec::new();
    };
    let templates = context.messages;
    let raws = read_workflows(repo, new);
    let compiled = knot_workflow::compile(
        &raws,
        &Trigger::Push {
            ref_name: name.clone(),
        },
        changed,
    );
    let listed = compiled.any_listed_match();
    let Compiled {
        workflows,
        diagnostics,
    } = compiled;
    let mut messages = diagnostics.errors;
    if *verbose {
        let clean = messages.is_empty() && diagnostics.warnings.is_empty();
        messages.extend(diagnostics.warnings);
        match (workflows.is_empty(), clean) {
            (true, _) => messages.extend(templates.pipeline_none.text_lines()),
            (false, true) => messages.extend(templates.pipeline_clean.text_lines()),
            (false, false) => {}
        }
    }
    if let Some(addr) = logs.as_ref().filter(|_| listed) {
        messages.extend(templates.ci_logs.lines(|key| match key {
            CiLogsKey::Host => addr.host().to_string(),
            CiLogsKey::Port => addr.port().to_string(),
            CiLogsKey::Repo => context.actor.repo.to_string(),
            CiLogsKey::Sha => new.to_hex(),
        }));
    }
    messages
}

fn read_workflows(repo: &Repo, new: Oid) -> Vec<RawWorkflow> {
    let commit = match repo.peel_to_commit(new) {
        Ok(commit) => commit,
        Err(error) => {
            tracing::warn!(
                commit = %new.to_hex(),
                path = %repo.path().display(),
                %error,
                "workflow read failed peeling commit"
            );
            return Vec::new();
        }
    };
    let workflow_dir = RepoPath::new(WORKFLOW_DIR).expect("literal workflow dir is well-formed");
    let entries = match repo.tree_entries_at(commit, Some(&workflow_dir)) {
        Ok(entries) => entries.unwrap_or_default(),
        Err(error) => {
            tracing::warn!(
                path = %repo.path().display(),
                commit = %new.to_hex(),
                %error,
                "workflow directory read failed"
            );
            return Vec::new();
        }
    };
    entries
        .into_iter()
        .filter(|entry| matches!(entry.kind, EntryKind::Blob | EntryKind::BlobExecutable))
        .filter_map(|entry| {
            let name = match WorkflowName::new(entry.name.as_str()) {
                Ok(name) => name,
                Err(error) => {
                    tracing::warn!(
                        workflow = %entry.name,
                        path = %repo.path().display(),
                        %error,
                        "workflow name rejected"
                    );
                    return None;
                }
            };
            match repo.read_blob(entry.oid) {
                Ok(contents) => Some(RawWorkflow { name, contents }),
                Err(error) => {
                    tracing::warn!(
                        workflow = %entry.name,
                        path = %repo.path().display(),
                        %error,
                        "workflow unreadable"
                    );
                    None
                }
            }
        })
        .collect()
}
