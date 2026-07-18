use std::collections::BTreeMap;
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Query, Request, State};
use axum::response::{IntoResponse, Response};
use http::{HeaderMap, HeaderValue, StatusCode, header};
use serde::de::{self, Deserializer};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tower::ServiceExt;
use tower_http::services::ServeFile;

use knot_cobs::RepoRef;
use knot_git::{
    ArchiveFormat, Commit, CommitRange, EntryKind, Layout, LogLimit, LogSkip, Repo, SizedEntry,
    is_public_ref, screens_reserved,
};
use knot_index::{Coverage, Resolved};
use knot_runtime::{Clock, HttpTransport};
use knot_types::{AuthorName, Email, Oid, OwnerDid, RepoDid, RepoPath, RepoRkey};

use crate::error::XrpcError;
use crate::patchtext::{render_format_patch, render_patches};
use crate::query::{
    BranchArg, Limit, Offset, Order, RawFlag, RepoArg, Revspec, TagArg, Total, TreePath,
    ValidatedQuery, next_cursor,
};
use crate::wire::{
    BranchWire, CommitWire, FileWire, FormatPatchWire, PatchIdentityWire, TagWire, ZERO_TIME,
    fold_subject, message_body, nice_diff, normalize_message_section, rfc2822, rfc3339,
};
use crate::{XrpcState, run_blocking, sniff};

pub(crate) const TREE_ROUTE: &str = "/xrpc/sh.tangled.repo.tree";
pub(crate) const LOG_ROUTE: &str = "/xrpc/sh.tangled.repo.log";
pub(crate) const BRANCHES_ROUTE: &str = "/xrpc/sh.tangled.repo.branches";
pub(crate) const BRANCH_ROUTE: &str = "/xrpc/sh.tangled.repo.branch";
pub(crate) const TAGS_ROUTE: &str = "/xrpc/sh.tangled.repo.tags";
pub(crate) const TAG_ROUTE: &str = "/xrpc/sh.tangled.repo.tag";
pub(crate) const BLOB_ROUTE: &str = "/xrpc/sh.tangled.repo.blob";
pub(crate) const DIFF_ROUTE: &str = "/xrpc/sh.tangled.repo.diff";
pub(crate) const COMPARE_ROUTE: &str = "/xrpc/sh.tangled.repo.compare";
pub(crate) const ARCHIVE_ROUTE: &str = "/xrpc/sh.tangled.repo.archive";
pub(crate) const LANGUAGES_ROUTE: &str = "/xrpc/sh.tangled.repo.languages";
pub(crate) const GET_DEFAULT_BRANCH_ROUTE: &str = "/xrpc/sh.tangled.repo.getDefaultBranch";
pub(crate) const DESCRIBE_REPO_ROUTE: &str = "/xrpc/sh.tangled.repo.describeRepo";
pub(crate) const LIST_REFS_ROUTE: &str = "/xrpc/sh.tangled.git.listRefs";
pub(crate) const LIST_REPOS_ROUTE: &str = "/xrpc/sh.tangled.sync.listRepos";

const DEFAULT_PAGE: usize = 50;
const MAX_PAGE: usize = 100;
const LIST_REFS_DEFAULT: usize = 100;
const LIST_REFS_MAX: usize = 1000;
const LIST_REPOS_DEFAULT: usize = 50;
const LIST_REPOS_MAX: usize = 1000;
const MAX_BLOB_BYTES: u64 = 25 * 1024 * 1024;
const MAX_COMPARE_COMMITS: usize = 500;
const RAW_CSP: &str = "default-src 'none'; style-src 'unsafe-inline'; sandbox";

pub(crate) fn repo_not_found() -> XrpcError {
    XrpcError::named(
        StatusCode::NOT_FOUND,
        "RepoNotFound",
        "repository not found on this knot",
    )
}

fn ref_not_found() -> XrpcError {
    XrpcError::named(
        StatusCode::NOT_FOUND,
        "RefNotFound",
        "git reference not found",
    )
}

fn blob_too_large() -> XrpcError {
    XrpcError::named(
        StatusCode::PAYLOAD_TOO_LARGE,
        "BlobTooLarge",
        "file is too large to serve",
    )
}

fn blob_serving_limit(raw: bool, response_limit: usize) -> u64 {
    match raw {
        true => MAX_BLOB_BYTES,
        false => MAX_BLOB_BYTES.min(response_limit as u64 / 4 * 3),
    }
}

fn readme_serving_limit(response_limit: usize) -> u64 {
    response_limit as u64 / 8
}

fn names_reserved(refspec: &str) -> bool {
    screens_reserved(refspec) || screens_reserved(&format!("refs/{refspec}"))
}

pub(crate) fn warming() -> XrpcError {
    XrpcError::warming("registry projection is still warming")
}

#[derive(Clone)]
pub(crate) struct HostedRepo(RepoDid);

impl HostedRepo {
    fn registered(did: RepoDid) -> Self {
        Self(did)
    }

    pub(crate) fn into_did(self) -> RepoDid {
        self.0
    }
}

impl std::ops::Deref for HostedRepo {
    type Target = RepoDid;

    fn deref(&self) -> &RepoDid {
        &self.0
    }
}

pub(crate) fn require_hosted<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: RepoDid,
) -> Result<HostedRepo, XrpcError> {
    match state.index.owner_of(&repo) {
        Resolved::Ready(Some(_)) => Ok(HostedRepo::registered(repo)),
        Resolved::Ready(None) => Err(repo_not_found()),
        Resolved::Warming => Err(warming()),
    }
}

pub(crate) fn resolve_repo<H: HttpTransport, C: Clock>(
    state: &XrpcState<H, C>,
    repo: &RepoArg,
) -> Result<HostedRepo, XrpcError> {
    match repo {
        RepoArg::Did(did) => require_hosted(state, did.clone()),
        RepoArg::OwnerRkey { owner, rkey } => match state.index.resolve_repo(owner, rkey) {
            Resolved::Ready(Some(did)) => Ok(HostedRepo::registered(did)),
            Resolved::Ready(None) => Err(repo_not_found()),
            Resolved::Warming => Err(warming()),
        },
    }
}

pub(crate) fn open(layout: &Layout, did: &RepoDid) -> Result<Repo, XrpcError> {
    layout
        .open(did)
        .map_err(|error| XrpcError::internal(format!("cannot open repository: {error}")))
}

fn commit_for(repo: &Repo, refspec: &Revspec) -> Result<Oid, XrpcError> {
    let refspec = refspec.as_str();
    if names_reserved(refspec) {
        return Err(ref_not_found());
    }
    let oid = match refspec.is_empty() {
        true => repo.head().map(|head| head.target),
        false => repo.resolve_revision(refspec),
    }
    .ok_or_else(ref_not_found)?;
    let commit = repo.peel_to_commit(oid).map_err(|_| ref_not_found())?;
    if hidden_staging_commit(repo, refspec) == Some(commit) {
        return Ok(commit);
    }
    match repo.reachable_from_public(commit) {
        Ok(true) => Ok(commit),
        Ok(false) => Err(ref_not_found()),
        Err(error) => Err(error.into()),
    }
}

fn hidden_staging_commit(repo: &Repo, refspec: &str) -> Option<Oid> {
    repo.hidden_ref_commit(refspec)
        .and_then(|oid| repo.peel_to_commit(oid).ok())
}

struct LimitWriter {
    buf: Vec<u8>,
    limit: usize,
}

impl std::io::Write for LimitWriter {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        if self.buf.len() + data.len() > self.limit {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "response exceeds configured maximum size",
            ));
        }
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn json(value: impl Serialize, limit: usize) -> Result<Response, XrpcError> {
    let mut writer = LimitWriter {
        buf: Vec::new(),
        limit,
    };
    match serde_json::to_writer(&mut writer, &value) {
        Ok(()) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            writer.buf,
        )
            .into_response()),
        Err(error) if error.is_io() => Err(XrpcError::request_too_large(
            "response exceeds configured maximum size",
        )),
        Err(error) => Err(XrpcError::internal(format!(
            "failed to serialize response: {error}"
        ))),
    }
}

#[derive(Deserialize)]
pub(crate) struct TreeParams {
    repo: RepoArg,
    #[serde(rename = "ref", default)]
    refspec: Revspec,
    #[serde(default)]
    path: TreePath,
}

#[derive(Serialize)]
struct SignatureOut {
    name: AuthorName,
    email: Email,
    when: String,
}

#[derive(Serialize)]
struct LastCommitOut {
    hash: Oid,
    message: String,
    when: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    author: Option<SignatureOut>,
}

#[derive(Serialize)]
struct TreeEntryOut {
    name: String,
    mode: String,
    size: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    last_commit: Option<LastCommitOut>,
}

#[derive(Serialize)]
struct ReadmeOut {
    filename: String,
    contents: String,
}

#[derive(Serialize)]
struct TreeOut {
    #[serde(rename = "ref")]
    refspec: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    dotdot: Option<String>,
    files: Vec<TreeEntryOut>,
    #[serde(rename = "lastCommit", skip_serializing_if = "Option::is_none")]
    last_commit: Option<LastCommitOut>,
    readme: ReadmeOut,
}

fn is_readme(entry: &SizedEntry) -> bool {
    let lower = entry.name.to_ascii_lowercase();
    entry.kind.is_file()
        && (lower == "readme"
            || lower
                .strip_prefix("readme.")
                .is_some_and(|extension| !extension.is_empty() && !extension.contains('.')))
}

fn readme_of(
    repo: &Repo,
    commit: Oid,
    dir: Option<&RepoPath>,
    entries: &[SizedEntry],
    response_limit: usize,
) -> ReadmeOut {
    entries
        .iter()
        .filter(|entry| is_readme(entry))
        .find_map(|entry| {
            let path = match dir {
                None => RepoPath::new(entry.name.as_str()).ok()?,
                Some(dir) => RepoPath::new(format!("{dir}/{}", entry.name)).ok()?,
            };
            let target = repo.entry_at(commit, &path).ok().flatten()?;
            if repo.blob_size(target.oid).ok()? > readme_serving_limit(response_limit) {
                return None;
            }
            let contents = repo.read_blob(target.oid).ok()?;
            String::from_utf8(contents).ok().map(|contents| ReadmeOut {
                filename: entry.name.clone(),
                contents,
            })
        })
        .unwrap_or(ReadmeOut {
            filename: String::new(),
            contents: String::new(),
        })
}

pub(crate) async fn repo_tree<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<TreeParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    let tree_deadline = state.budgets.tree_last_commit.get().deadline();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let commit = commit_for(&repo, &params.refspec)?;
        let path_not_found = || {
            XrpcError::named(
                StatusCode::NOT_FOUND,
                "PathNotFound",
                "path not found in repository tree",
            )
        };
        let dir = params.path.dir().ok_or_else(path_not_found)?;
        let path = params.path.as_str();
        let entries = repo
            .tree_entries_at(commit, dir)?
            .ok_or_else(path_not_found)?;
        let names: Vec<String> = entries.iter().map(|entry| entry.name.clone()).collect();
        let attributed = repo
            .last_commits(commit, dir, &names, tree_deadline)
            .unwrap_or_default();
        let files: Vec<TreeEntryOut> = entries
            .iter()
            .map(|entry| TreeEntryOut {
                name: entry.name.clone(),
                mode: entry.kind.mode_octal().to_string(),
                size: entry.size as i64,
                last_commit: attributed.get(&entry.name).map(|last| LastCommitOut {
                    hash: last.id,
                    message: last.subject.clone(),
                    when: rfc3339(last.time.get(), 0),
                    author: None,
                }),
            })
            .collect();
        let newest = attributed.values().max_by_key(|last| (last.time, last.id));
        let last_commit = newest.map(|last| LastCommitOut {
            hash: last.id,
            message: last.subject.clone(),
            when: rfc3339(last.time.get(), 0),
            author: repo.find_commit(last.id).ok().map(|commit| SignatureOut {
                name: commit.author.name,
                email: commit.author.email,
                when: String::new(),
            }),
        });
        let readme = readme_of(&repo, commit, dir, &entries, limit);
        let parent = (!path.is_empty()).then(|| path.to_string());
        let dotdot = (!path.is_empty())
            .then(|| path.rsplit_once('/').map(|(parent, _)| parent.to_string()))
            .flatten();
        json(
            TreeOut {
                refspec: params.refspec.as_str().to_string(),
                parent,
                dotdot,
                files,
                last_commit,
                readme,
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct LogParams {
    repo: RepoArg,
    #[serde(rename = "ref", default)]
    refspec: Revspec,
    #[serde(default)]
    path: TreePath,
    #[serde(default)]
    limit: Limit<DEFAULT_PAGE, MAX_PAGE>,
    #[serde(default)]
    cursor: Offset,
}

#[derive(Serialize)]
struct LogOut {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    commits: Vec<CommitWire>,
    #[serde(rename = "ref", skip_serializing_if = "String::is_empty")]
    refspec: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    description: String,
    log: bool,
    #[serde(skip_serializing_if = "is_zero")]
    total: usize,
    page: usize,
    per_page: usize,
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

pub(crate) async fn repo_log<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<LogParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let offset = params.cursor.get();
    let limit = params.limit.get();
    let layout = state.layout.clone();
    let response_limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let start = commit_for(&repo, &params.refspec)?;
        let (commits, total) =
            repo.log_window(start, LogSkip::new(offset), LogLimit::new(limit))?;
        json(
            LogOut {
                commits: commits.iter().map(CommitWire::of).collect(),
                refspec: params.refspec.as_str().to_string(),
                description: params.path.as_str().to_string(),
                log: true,
                total,
                page: (offset / limit) + 1,
                per_page: limit,
            },
            response_limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct BranchesParams {
    repo: RepoArg,
    #[serde(default)]
    limit: Limit<DEFAULT_PAGE, MAX_PAGE>,
    #[serde(default)]
    cursor: Offset,
}

#[derive(Serialize)]
struct BranchesOut {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    branches: Vec<BranchWire>,
}

pub(crate) async fn repo_branches<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<BranchesParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let offset = params.cursor.get();
    let limit = params.limit.get();
    let layout = state.layout.clone();
    let response_limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let mut branches = repo.branch_list()?;
        branches.sort_by(|a, b| {
            b.tip
                .created_at()
                .cmp(&a.tip.created_at())
                .then_with(|| a.name.cmp(&b.name))
        });
        let default = repo
            .default_branch()
            .map(|name| name.as_str().trim_start_matches("refs/heads/").to_string());
        let absent = repo.object_format().null_oid();
        let window: Vec<BranchWire> = branches
            .iter()
            .skip(offset)
            .take(limit)
            .map(|branch| {
                BranchWire::of(
                    branch,
                    default.as_deref() == Some(branch.name.as_str()),
                    absent,
                )
            })
            .rev()
            .collect();
        json(BranchesOut { branches: window }, response_limit)
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct BranchParams {
    repo: RepoArg,
    #[serde(default)]
    name: BranchArg,
}

#[derive(Serialize)]
struct BranchOut {
    name: String,
    hash: String,
    #[serde(rename = "shortHash")]
    short_hash: String,
    when: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
    author: SignatureOut,
    #[serde(rename = "isDefault")]
    is_default: bool,
}

pub(crate) async fn repo_branch<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<BranchParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let Some(name) = params.name.get().cloned() else {
        return Err(XrpcError::invalid_request("missing name parameter"));
    };
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let branch_not_found =
            || XrpcError::named(StatusCode::NOT_FOUND, "BranchNotFound", "branch not found");
        let target = repo
            .find_ref(&name.head_ref())
            .ok()
            .flatten()
            .ok_or_else(branch_not_found)?;
        let commit = repo.find_commit(target).map_err(|_| branch_not_found())?;
        let default = repo
            .default_branch()
            .map(|name| name.as_str().trim_start_matches("refs/heads/").to_string());
        let hash = target.to_hex();
        json(
            BranchOut {
                name: name.to_string(),
                short_hash: hash[..7].to_string(),
                hash,
                when: rfc3339(commit.author.time.get(), commit.author.offset_seconds),
                message: (!commit.message.is_empty()).then(|| commit.message.clone()),
                author: SignatureOut {
                    name: commit.author.name.clone(),
                    email: commit.author.email.clone(),
                    when: rfc3339(commit.author.time.get(), commit.author.offset_seconds),
                },
                is_default: default.as_deref() == Some(name.as_str()),
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct TagsParams {
    repo: RepoArg,
    #[serde(default)]
    limit: Limit<DEFAULT_PAGE, MAX_PAGE>,
    #[serde(default)]
    cursor: Offset,
}

#[derive(Serialize)]
struct TagsOut {
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<TagWire>,
}

pub(crate) async fn repo_tags<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<TagsParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let offset = params.cursor.get();
    let limit = params.limit.get();
    let layout = state.layout.clone();
    let response_limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let mut tags = repo.tag_list()?;
        tags.sort_by(|a, b| {
            b.created_at
                .cmp(&a.created_at)
                .then_with(|| a.name.cmp(&b.name))
        });
        let window: Vec<TagWire> = tags
            .iter()
            .skip(offset)
            .take(limit)
            .map(TagWire::of)
            .collect();
        json(TagsOut { tags: window }, response_limit)
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct TagParams {
    repo: RepoArg,
    #[serde(default)]
    tag: TagArg,
}

#[derive(Serialize)]
struct TagOut {
    tag: TagWire,
}

pub(crate) async fn repo_tag<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<TagParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let Some(name) = params.tag.get().cloned() else {
        return Err(XrpcError::invalid_request("missing tag parameter"));
    };
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let info = repo
            .tag_list()?
            .into_iter()
            .find(|tag| tag.name == name)
            .ok_or_else(|| {
                XrpcError::named(StatusCode::BAD_REQUEST, "TagNotFound", "tag not found")
            })?;
        json(
            TagOut {
                tag: TagWire::of(&info),
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct BlobParams {
    repo: RepoArg,
    #[serde(rename = "ref", default)]
    refspec: Revspec,
    #[serde(default)]
    path: TreePath,
    #[serde(default)]
    raw: RawFlag,
}

#[derive(Serialize)]
struct SubmoduleOut {
    name: String,
    url: String,
    branch: String,
}

#[derive(Serialize)]
struct BlobOut {
    #[serde(rename = "ref")]
    refspec: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    encoding: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    size: Option<i64>,
    #[serde(rename = "isBinary", skip_serializing_if = "Option::is_none")]
    is_binary: Option<bool>,
    #[serde(rename = "mimeType", skip_serializing_if = "Option::is_none")]
    mime_type: Option<&'static str>,
    #[serde(rename = "lastCommit", skip_serializing_if = "Option::is_none")]
    last_commit: Option<LastCommitOut>,
    #[serde(skip_serializing_if = "Option::is_none")]
    submodule: Option<SubmoduleOut>,
}

fn etag_matches(headers: &HeaderMap, etag: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .any(|candidate| {
            candidate == "*" || candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        })
}

fn quoted_etag(digest: &[u8]) -> String {
    format!("\"{}\"", knot_types::lowercase_hex(digest))
}

fn serve_raw(
    headers: &HeaderMap,
    mime: &'static str,
    contents: Vec<u8>,
) -> Result<Response, XrpcError> {
    if mime.starts_with("image/") || mime.starts_with("video/") {
        let etag = quoted_etag(&Sha256::digest(&contents));
        if etag_matches(headers, &etag) {
            return Ok(StatusCode::NOT_MODIFIED.into_response());
        }
        return Ok((
            StatusCode::OK,
            [
                (header::ETAG, etag),
                (header::CONTENT_TYPE, mime.to_string()),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (header::CONTENT_SECURITY_POLICY, RAW_CSP.to_string()),
            ],
            contents,
        )
            .into_response());
    }
    if sniff::is_textual_mime(mime) {
        return Ok((
            StatusCode::OK,
            [
                (header::CACHE_CONTROL, "public, no-cache".to_string()),
                (
                    header::CONTENT_TYPE,
                    "text/plain; charset=utf-8".to_string(),
                ),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff".to_string()),
                (header::CONTENT_SECURITY_POLICY, RAW_CSP.to_string()),
            ],
            contents,
        )
            .into_response());
    }
    Err(XrpcError::named(
        StatusCode::FORBIDDEN,
        "InvalidRequest",
        "only image, video, and text files can be accessed directly",
    ))
}

pub(crate) async fn repo_blob<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<BlobParams>,
    headers: HeaderMap,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    if params.path.as_str().is_empty() {
        return Err(XrpcError::invalid_request("missing path parameter"));
    }
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    let blob_deadline = state.budgets.blob_last_commit.get().deadline();
    run_blocking(move || {
        let refspec = params.refspec.as_str().to_string();
        let path = params.path.as_str().to_string();
        let raw = params.raw.requested();
        let repo = open(&layout, &did)?;
        let commit = commit_for(&repo, &params.refspec)?;
        let submodule = repo
            .submodules(commit)
            .unwrap_or_default()
            .into_iter()
            .find(|submodule| submodule.path.as_str() == path);
        if let Some(submodule) = submodule {
            return json(
                BlobOut {
                    refspec,
                    path,
                    content: None,
                    encoding: None,
                    size: None,
                    is_binary: None,
                    mime_type: None,
                    last_commit: None,
                    submodule: Some(SubmoduleOut {
                        name: submodule.name,
                        url: submodule.url,
                        branch: submodule
                            .branch
                            .map(|branch| branch.to_string())
                            .unwrap_or_default(),
                    }),
                },
                limit,
            );
        }
        let file_not_found = || {
            XrpcError::named(
                StatusCode::NOT_FOUND,
                "FileNotFound",
                "file not found at specified path",
            )
        };
        let file_path = params.path.file().ok_or_else(file_not_found)?;
        let entry = repo
            .entry_at(commit, file_path)?
            .filter(|entry| {
                matches!(
                    entry.kind,
                    EntryKind::Blob | EntryKind::BlobExecutable | EntryKind::Link
                )
            })
            .ok_or_else(file_not_found)?;
        if repo.blob_size(entry.oid).map_err(|_| file_not_found())? > blob_serving_limit(raw, limit)
        {
            return Err(blob_too_large());
        }
        let contents = repo.read_blob(entry.oid).map_err(|_| file_not_found())?;
        let mime = sniff::override_by_extension(&path, sniff::detect_content_type(&contents));

        if raw {
            return serve_raw(&headers, mime, contents);
        }

        let is_binary = !sniff::is_textual_mime(mime);
        let size = contents.len() as i64;
        let (content, encoding) = match is_binary {
            true => (
                base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &contents),
                "base64",
            ),
            false => (String::from_utf8_lossy(&contents).into_owned(), "utf-8"),
        };
        let dir = file_path.parent();
        let name = file_path.file_name().to_string();
        let last_commit = repo
            .last_commits(
                commit,
                dir.as_ref(),
                std::slice::from_ref(&name),
                blob_deadline,
            )
            .ok()
            .and_then(|attributed| attributed.get(&name).cloned())
            .map(|last| LastCommitOut {
                hash: last.id,
                message: last.subject,
                when: rfc3339(last.time.get(), 0),
                author: repo.find_commit(last.id).ok().map(|commit| SignatureOut {
                    name: commit.author.name,
                    email: commit.author.email,
                    when: String::new(),
                }),
            });
        json(
            BlobOut {
                refspec,
                path,
                content: Some(content),
                encoding: Some(encoding),
                size: Some(size),
                is_binary: Some(is_binary),
                mime_type: Some(mime),
                last_commit,
                submodule: None,
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct DiffParams {
    repo: RepoArg,
    #[serde(rename = "ref", default)]
    refspec: Revspec,
}

#[derive(Serialize)]
struct DiffOut {
    #[serde(rename = "ref", skip_serializing_if = "String::is_empty")]
    refspec: String,
    diff: crate::wire::NiceDiffWire,
}

pub(crate) async fn repo_diff<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<DiffParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let target = commit_for(&repo, &params.refspec)?;
        let commit = repo.find_commit(target)?;
        let patches = repo.commit_patches(knot_git::PatchRange {
            base: commit.parents.first().copied(),
            head: target,
        })?;
        json(
            DiffOut {
                refspec: params.refspec.as_str().to_string(),
                diff: nice_diff(&commit, &patches),
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct CompareParams {
    repo: RepoArg,
    #[serde(default)]
    rev1: Revspec,
    #[serde(default)]
    rev2: Revspec,
}

#[derive(Serialize)]
struct CompareOut {
    rev1: String,
    rev2: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    merge_base: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    format_patch: Vec<FormatPatchWire>,
    #[serde(rename = "patch", skip_serializing_if = "String::is_empty")]
    patch_raw: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    combined_patch: Option<Vec<FileWire>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    combined_patch_raw: Option<String>,
}

fn format_patch_entry(
    commit: &Commit,
    patches: &[knot_git::FilePatch],
    raw: &str,
) -> FormatPatchWire {
    let title = fold_subject(&commit.message);
    let mut raw_headers: BTreeMap<String, Vec<String>> = BTreeMap::from([
        (
            "From".to_string(),
            vec![format!("{} <{}>", commit.author.name, commit.author.email)],
        ),
        (
            "Date".to_string(),
            vec![rfc2822(
                commit.author.time.get(),
                commit.author.offset_seconds,
            )],
        ),
        ("Subject".to_string(), vec![format!("[PATCH] {title}")]),
    ]);
    if let Some(change_id) = commit.change_id() {
        raw_headers.insert("Change-Id".to_string(), vec![change_id.to_string()]);
    }
    let files: Vec<FileWire> = patches.iter().map(FileWire::of).collect();
    FormatPatchWire {
        files: (!files.is_empty()).then_some(files),
        sha: commit.id,
        author: Some(PatchIdentityWire {
            name: commit.author.name.clone(),
            email: commit.author.email.clone(),
        }),
        author_date: rfc3339(commit.author.time.get(), commit.author.offset_seconds),
        committer: None,
        committer_date: ZERO_TIME.to_string(),
        title,
        body: normalize_message_section(message_body(&commit.message).lines()),
        subject_prefix: "[PATCH] ".to_string(),
        body_appendix: normalize_message_section(appendix_lines(raw)),
        raw_headers: Some(raw_headers),
        raw: raw.trim().to_string(),
    }
}

fn appendix_lines(raw: &str) -> impl Iterator<Item = &str> {
    raw.split_once("\n---\n")
        .map(|(_, rest)| rest)
        .unwrap_or_default()
        .split("\ndiff --git ")
        .next()
        .unwrap_or_default()
        .lines()
}

pub(crate) async fn repo_compare<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<CompareParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let rev1 = params.rev1.as_str().to_string();
    if rev1.is_empty() {
        return Err(XrpcError::invalid_request("missing rev1 parameter"));
    }
    let rev2 = params.rev2.as_str().to_string();
    if rev2.is_empty() {
        return Err(XrpcError::invalid_request("missing rev2 parameter"));
    }
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let resolve = |rev: &str| {
            let revision_not_found = || {
                XrpcError::named(
                    StatusCode::BAD_REQUEST,
                    "RevisionNotFound",
                    format!("error resolving revision {rev}"),
                )
            };
            if names_reserved(rev) {
                return Err(revision_not_found());
            }
            let commit = repo
                .resolve_revision(rev)
                .and_then(|oid| repo.peel_to_commit(oid).ok())
                .ok_or_else(revision_not_found)?;
            if hidden_staging_commit(&repo, rev) == Some(commit) {
                return Ok(commit);
            }
            match repo.reachable_from_public(commit) {
                Ok(true) => Ok(commit),
                Ok(false) => Err(revision_not_found()),
                Err(error) => Err(error.into()),
            }
        };
        let base = resolve(&rev1)?;
        let head = resolve(&rev2)?;
        let compare_error = |error: knot_git::GitError| {
            XrpcError::named(
                StatusCode::BAD_REQUEST,
                "CompareError",
                format!("error comparing revisions: {error}"),
            )
        };
        let between = repo
            .commits_between(
                CommitRange { base, head },
                LogLimit::new(MAX_COMPARE_COMMITS + 1),
            )
            .map_err(compare_error)?;
        if between.len() > MAX_COMPARE_COMMITS {
            return Err(XrpcError::named(
                StatusCode::BAD_REQUEST,
                "CompareError",
                format!("comparison spans more than maximum of {MAX_COMPARE_COMMITS} commits"),
            ));
        }
        let commits: Vec<Commit> = between
            .into_iter()
            .map(|oid| repo.find_commit(oid))
            .collect::<Result<Vec<_>, _>>()
            .map_err(compare_error)?
            .into_iter()
            .rev()
            .filter(|commit| commit.parents.len() <= 1)
            .collect();
        let entries: Vec<(FormatPatchWire, String)> = commits
            .iter()
            .map(|commit| {
                repo.commit_patches(knot_git::PatchRange {
                    base: commit.parents.first().copied(),
                    head: commit.id,
                })
                .map(|patches| {
                    let raw = render_format_patch(commit, &patches);
                    (format_patch_entry(commit, &patches, &raw), raw)
                })
            })
            .collect::<Result<Vec<_>, _>>()
            .map_err(compare_error)?;
        let patch_raw: String = entries.iter().map(|(_, raw)| format!("{raw}\n")).collect();
        let merge_base = repo.merge_base(base, head).ok().flatten();
        let (combined_patch, combined_patch_raw) = match (entries.len() >= 2, merge_base) {
            (true, Some(merge_base)) => repo
                .commit_patches(knot_git::PatchRange {
                    base: Some(merge_base),
                    head,
                })
                .ok()
                .map(|patches| {
                    (
                        Some(patches.iter().map(FileWire::of).collect::<Vec<_>>()),
                        Some(render_patches(&patches)),
                    )
                })
                .unwrap_or((None, None)),
            _ => (None, None),
        };
        json(
            CompareOut {
                rev1: base.to_hex(),
                rev2: head.to_hex(),
                merge_base: merge_base.map(|oid| oid.to_hex()),
                format_patch: entries.into_iter().map(|(entry, _)| entry).collect(),
                patch_raw,
                combined_patch,
                combined_patch_raw,
            },
            limit,
        )
    })
    .await
}

#[derive(Clone, Copy)]
struct ArchiveFormatArg(ArchiveFormat);

impl Default for ArchiveFormatArg {
    fn default() -> Self {
        ArchiveFormatArg(ArchiveFormat::TarGz)
    }
}

impl ArchiveFormatArg {
    fn format(self) -> ArchiveFormat {
        self.0
    }

    fn name(self) -> &'static str {
        match self.0 {
            ArchiveFormat::Zip => "zip",
            _ => "tar.gz",
        }
    }

    fn content_type(self) -> &'static str {
        match self.0 {
            ArchiveFormat::Zip => "application/zip",
            _ => "application/gzip",
        }
    }
}

impl<'de> Deserialize<'de> for ArchiveFormatArg {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "" | "tar.gz" => Ok(ArchiveFormatArg(ArchiveFormat::TarGz)),
            "zip" => Ok(ArchiveFormatArg(ArchiveFormat::Zip)),
            _ => Err(de::Error::custom(
                "only tar.gz and zip formats are supported",
            )),
        }
    }
}

#[derive(Default)]
struct ArchivePrefixArg(Option<knot_git::ArchivePrefix>);

impl<'de> Deserialize<'de> for ArchivePrefixArg {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        let cleaned = raw
            .split('/')
            .filter(|component| !component.is_empty() && *component != ".")
            .collect::<Vec<_>>()
            .join("/");
        match cleaned.is_empty() {
            true => Ok(Self(None)),
            false => knot_git::ArchivePrefix::new(cleaned)
                .ok()
                .filter(|prefix| {
                    !prefix.as_str().contains('\\') && !prefix.as_str().contains(char::is_control)
                })
                .map(|prefix| Self(Some(prefix)))
                .ok_or_else(|| {
                    de::Error::custom(format!(
                        "archive prefix must stay inside the archive root within {} bytes, and mustn't contain a backslash or a control character",
                        knot_git::ArchivePrefix::MAX_BYTES
                    ))
                }),
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct ArchiveParams {
    repo: RepoArg,
    #[serde(rename = "ref", default)]
    refspec: Revspec,
    #[serde(default)]
    format: ArchiveFormatArg,
    #[serde(default)]
    prefix: ArchivePrefixArg,
}

fn short_ref(refspec: &str) -> &str {
    ["refs/heads/", "refs/tags/", "refs/remotes/", "refs/"]
        .into_iter()
        .find_map(|prefix| refspec.strip_prefix(prefix))
        .unwrap_or(refspec)
}

fn sanitize_filename(name: &str) -> String {
    name.replace(
        |c: char| c.is_ascii_control() || matches!(c, '"' | '\\' | '/'),
        "-",
    )
}

fn rfc5987_encode(name: &str) -> String {
    name.bytes()
        .map(|byte| match byte {
            b'A'..=b'Z'
            | b'a'..=b'z'
            | b'0'..=b'9'
            | b'!'
            | b'#'
            | b'$'
            | b'&'
            | b'+'
            | b'-'
            | b'.'
            | b'^'
            | b'_'
            | b'`'
            | b'|'
            | b'~' => String::from(byte as char),
            _ => format!("%{byte:02X}"),
        })
        .collect()
}

fn content_disposition(filename: &str) -> String {
    let safe = sanitize_filename(filename);
    let ascii: String = safe
        .chars()
        .map(|c| match c.is_ascii() {
            true => c,
            false => '-',
        })
        .collect();
    match safe == ascii {
        true => format!("attachment; filename=\"{ascii}\""),
        false => format!(
            "attachment; filename=\"{ascii}\"; filename*=UTF-8''{}",
            rfc5987_encode(&safe)
        ),
    }
}

fn archive_etag(did: &RepoDid, commit: Oid, format: ArchiveFormat, prefix: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(did.as_str().as_bytes());
    hasher.update(b"\0");
    hasher.update(commit.to_hex().as_bytes());
    hasher.update(b"\0");
    hasher.update(ArchiveFormatArg(format).name().as_bytes());
    hasher.update(b"\0");
    hasher.update(prefix.as_bytes());
    quoted_etag(&hasher.finalize())
}

fn pinned_modified(modified_secs: i64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + std::time::Duration::from_secs(modified_secs.max(0) as u64)
}

fn reconcile_if_range(request: &mut Request, etag: &str, modified_secs: i64) {
    let Some(value) = request
        .headers()
        .get(header::IF_RANGE)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .map(str::to_string)
    else {
        return;
    };
    let resumes = match value.starts_with('"') || value.starts_with("W/") {
        true => value.starts_with('"') && value == etag,
        false => httpdate::parse_http_date(&value)
            .is_ok_and(|client| client == pinned_modified(modified_secs)),
    };
    let headers = request.headers_mut();
    headers.remove(header::IF_RANGE);
    if !resumes {
        headers.remove(header::RANGE);
    }
}

pub(crate) async fn repo_archive<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    mut request: Request,
) -> Result<Response, XrpcError> {
    let params = Query::<ArchiveParams>::try_from_uri(request.uri())
        .map_err(|rejection| XrpcError::invalid_request(rejection.body_text()))?
        .0;
    let did = resolve_repo(&state, &params.repo)?;
    let format = params.format;
    let format_name = format.name();
    let archive_prefix = match &params.prefix.0 {
        Some(prefix) => prefix.clone(),
        None => {
            let registered = state.index.rkey_of(&did);
            let name = match &registered {
                Resolved::Ready(Some(rkey)) => rkey.as_str(),
                _ => params.repo.basename(),
            };
            knot_git::ArchivePrefix::stem(name, short_ref(params.refspec.as_str()))
        }
    };

    let (resolved, modified_secs) = run_blocking({
        let layout = state.layout.clone();
        let did = did.clone();
        let refspec = params.refspec.clone();
        move || {
            let repo = open(&layout, &did)?;
            let commit = commit_for(&repo, &refspec)?;
            let modified_secs = repo
                .find_commit(commit)
                .map(|commit| commit.committer.time.get())
                .unwrap_or(0);
            Ok((commit, modified_secs))
        }
    })
    .await?;

    let link = {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("format", format_name);
        query.append_pair("prefix", archive_prefix.as_str());
        query.append_pair("ref", &resolved.to_hex());
        query.append_pair("repo", &params.repo.to_param());
        format!(
            "<{}/xrpc/sh.tangled.repo.archive?{}>; rel=\"immutable\"",
            state.knot_service_url.as_str(),
            query.finish()
        )
    };

    let etag = archive_etag(&did, resolved, format.format(), archive_prefix.as_str());
    let disposition = content_disposition(&format!("{}.{format_name}", archive_prefix.as_str()));
    if etag_matches(request.headers(), &etag) {
        return Ok((
            StatusCode::NOT_MODIFIED,
            [
                (header::ETAG, etag),
                (header::LINK, link),
                (header::CACHE_CONTROL, "no-cache".to_string()),
            ],
        )
            .into_response());
    }

    let temp = run_blocking({
        let layout = state.layout.clone();
        let did = did.clone();
        let archive_limit = state.byte_limits.archive;
        let tree_prefix = archive_prefix.into_tree_prefix();
        move || {
            let repo = open(&layout, &did)?;
            let tree = repo.peel_to_tree(resolved)?;
            let temp = tempfile::NamedTempFile::new()
                .map_err(|error| XrpcError::internal(format!("cannot spool archive: {error}")))?;
            let mut file = temp
                .reopen()
                .map_err(|error| XrpcError::internal(format!("cannot spool archive: {error}")))?;
            repo.write_archive(
                tree,
                format.format(),
                Some(&tree_prefix),
                archive_limit,
                &mut file,
            )
            .map_err(|error| {
                match matches!(error, knot_git::GitError::ArchiveTooLarge { .. }) {
                    true => XrpcError::from(error),
                    false => XrpcError::named(
                        StatusCode::BAD_REQUEST,
                        "ArchiveError",
                        format!("failed to create archive: {error}"),
                    ),
                }
            })?;
            temp.as_file()
                .set_modified(pinned_modified(modified_secs))
                .map_err(|error| XrpcError::internal(error.to_string()))?;
            Ok(temp)
        }
    })
    .await?;

    let content_type = format.content_type();

    reconcile_if_range(&mut request, &etag, modified_secs);
    let serve_response = ServeFile::new(temp.path())
        .oneshot(request)
        .await
        .unwrap_or_else(|error| match error {});
    drop(temp);

    let mut response = serve_response.map(Body::new);
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    let header_value = |value: &str| {
        HeaderValue::from_str(value).map_err(|error| XrpcError::internal(error.to_string()))
    };
    headers.insert(header::CONTENT_DISPOSITION, header_value(&disposition)?);
    headers.insert(header::LINK, header_value(&link)?);
    headers.insert(header::ETAG, header_value(&etag)?);
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    Ok(response)
}

#[derive(Deserialize)]
pub(crate) struct LanguagesParams {
    repo: RepoArg,
    #[serde(rename = "ref", default)]
    refspec: Revspec,
}

#[derive(Serialize)]
struct LanguageOut {
    name: knot_types::LanguageName,
    size: knot_types::LanguageBytes,
    percentage: i64,
}

#[derive(Serialize)]
struct LanguagesOut {
    #[serde(rename = "ref")]
    refspec: String,
    languages: Option<Vec<LanguageOut>>,
    #[serde(rename = "totalSize", skip_serializing_if = "Option::is_none")]
    total_size: Option<u64>,
    #[serde(rename = "totalFiles", skip_serializing_if = "Option::is_none")]
    total_files: Option<i64>,
}

pub(crate) async fn repo_languages<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<LanguagesParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    let languages_deadline = state.budgets.languages.get().deadline();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let commit = commit_for(&repo, &params.refspec)?;
        let sizes = knot_langs::analyze(&repo, commit, languages_deadline)?;
        let total: u64 = sizes.values().map(|size| size.get()).sum();
        let mut languages: Vec<LanguageOut> = sizes
            .iter()
            .filter(|(_, size)| size.get() > 0)
            .map(|(name, size)| LanguageOut {
                name: *name,
                size: *size,
                percentage: ((size.get() as f64) / (total as f64) * 100.0).round() as i64,
            })
            .collect();
        languages.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.name.cmp(&b.name)));
        let count = languages.len() as i64;
        json(
            LanguagesOut {
                refspec: params.refspec.as_str().to_string(),
                languages: (!languages.is_empty()).then_some(languages),
                total_size: (total > 0).then_some(total),
                total_files: (total > 0).then_some(count),
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct DefaultBranchParams {
    repo: RepoArg,
}

#[derive(Serialize)]
struct DefaultBranchOut {
    name: String,
    hash: String,
    when: String,
}

pub(crate) async fn repo_get_default_branch<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<DefaultBranchParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let layout = state.layout.clone();
    let limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let name = repo
            .default_branch()
            .map(|name| name.as_str().trim_start_matches("refs/heads/").to_string())
            .ok_or_else(|| {
                XrpcError::named(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "InvalidRequest",
                    "failed to get default branch",
                )
            })?;
        json(
            DefaultBranchOut {
                name,
                hash: String::new(),
                when: rfc3339(0, 0),
            },
            limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct DescribeRepoParams {
    #[serde(rename = "repoDid")]
    repo_did: RepoDid,
}

#[derive(Serialize)]
struct DescribeRepoOut {
    #[serde(rename = "repoDid")]
    repo_did: RepoDid,
    #[serde(rename = "ownerDid")]
    owner_did: OwnerDid,
    rkey: RepoRkey,
}

pub(crate) async fn repo_describe_repo<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<DescribeRepoParams>,
) -> Result<Response, XrpcError> {
    let did = params.repo_did;
    let RepoRef { owner, rkey } = match state.index.ownership_of(&did) {
        Resolved::Ready(Some(found)) => found,
        Resolved::Ready(None) => return Err(repo_not_found()),
        Resolved::Warming => return Err(warming()),
    };
    json(
        DescribeRepoOut {
            repo_did: did,
            owner_did: owner,
            rkey,
        },
        state.byte_limits.response.get(),
    )
}

#[derive(Serialize)]
struct DefaultBranchWire {
    #[serde(rename = "ref")]
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    head: Option<String>,
}

#[derive(Deserialize)]
pub(crate) struct ListRefsParams {
    repo: RepoArg,
    #[serde(default)]
    limit: Limit<LIST_REFS_DEFAULT, LIST_REFS_MAX>,
    #[serde(default)]
    cursor: Offset,
}

#[derive(Serialize)]
struct RefWire {
    #[serde(rename = "ref")]
    name: String,
    sha: Oid,
}

#[derive(Serialize)]
struct ListRefsOut {
    refs: Vec<RefWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
    #[serde(rename = "defaultBranch", skip_serializing_if = "Option::is_none")]
    default_branch: Option<DefaultBranchWire>,
}

pub(crate) async fn git_list_refs<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<ListRefsParams>,
) -> Result<Response, XrpcError> {
    let did = resolve_repo(&state, &params.repo)?;
    let offset = params.cursor;
    let limit = params.limit;
    let layout = state.layout.clone();
    let response_limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repo = open(&layout, &did)?;
        let mut refs: Vec<_> = repo
            .references()?
            .into_iter()
            .filter(|record| is_public_ref(&record.name))
            .collect();
        refs.sort_by(|a, b| a.name.as_str().cmp(b.name.as_str()));
        let total = refs.len();
        let window: Vec<RefWire> = refs
            .iter()
            .skip(offset.get())
            .take(limit.get())
            .map(|record| RefWire {
                name: record.name.as_str().to_string(),
                sha: record.target,
            })
            .collect();
        let cursor = next_cursor(offset, limit, Total::new(total));
        let default_branch = repo.head().map(|head| DefaultBranchWire {
            name: head.name.as_str().to_string(),
            head: Some(head.target.to_hex()),
        });
        json(
            ListRefsOut {
                refs: window,
                cursor,
                default_branch,
            },
            response_limit,
        )
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct ListReposParams {
    #[serde(default)]
    limit: Limit<LIST_REPOS_DEFAULT, LIST_REPOS_MAX>,
    #[serde(default)]
    cursor: Offset,
    #[serde(default)]
    order: Order,
}

#[derive(Serialize)]
struct RepoWire {
    repo: RepoDid,
    status: &'static str,
    #[serde(rename = "defaultBranch", skip_serializing_if = "Option::is_none")]
    default_branch: Option<DefaultBranchWire>,
}

#[derive(Serialize)]
struct ListReposOut {
    repos: Vec<RepoWire>,
    #[serde(skip_serializing_if = "Option::is_none")]
    cursor: Option<String>,
}

pub(crate) async fn sync_list_repos<H: HttpTransport, C: Clock>(
    State(state): State<Arc<XrpcState<H, C>>>,
    ValidatedQuery(params): ValidatedQuery<ListReposParams>,
) -> Result<Response, XrpcError> {
    if matches!(state.index.coverage().registry, Coverage::Warming) {
        return Err(warming());
    }
    let offset = params.cursor;
    let limit = params.limit;
    let mut repos = state.index.hosted_repos();
    if params.order.descending() {
        repos.reverse();
    }
    let total = repos.len();
    let page: Vec<RepoDid> = repos
        .into_iter()
        .skip(offset.get())
        .take(limit.get())
        .collect();
    let cursor = next_cursor(offset, limit, Total::new(total));
    let layout = state.layout.clone();
    let response_limit = state.byte_limits.response.get();
    run_blocking(move || {
        let repos: Vec<RepoWire> =
            page.iter()
                .map(|did| RepoWire {
                    repo: did.clone(),
                    status: "active",
                    default_branch: open(&layout, did).ok().and_then(|repo| repo.head()).map(
                        |head| DefaultBranchWire {
                            name: head.name.as_str().to_string(),
                            head: Some(head.target.to_hex()),
                        },
                    ),
                })
                .collect();
        json(ListReposOut { repos, cursor }, response_limit)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::{content_disposition, rfc5987_encode};

    #[test]
    fn content_disposition_quotes_dashes_quotes_and_adds_an_encoded_form_for_non_ascii() {
        let cases: &[(&str, &str)] = &[
            (
                "squid-main.tar.gz",
                "attachment; filename=\"squid-main.tar.gz\"",
            ),
            (
                "squid-a\"b.tar.gz",
                "attachment; filename=\"squid-a-b.tar.gz\"",
            ),
            (
                "squid-café.zip",
                "attachment; filename=\"squid-caf-.zip\"; filename*=UTF-8''squid-caf%C3%A9.zip",
            ),
        ];
        cases.iter().for_each(|(name, expected)| {
            assert_eq!(content_disposition(name), *expected);
        });
    }

    #[test]
    fn rfc5987_percent_encodes_outside_the_attr_char_set() {
        assert_eq!(rfc5987_encode("a b:c"), "a%20b%3Ac");
        assert_eq!(rfc5987_encode("plain-._~"), "plain-._~");
    }
}
