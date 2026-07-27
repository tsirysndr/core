use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::ser::SerializeSeq;
use serde::{Serialize, Serializer};

use knot_git::{
    BranchInfo, BranchTip, Commit, CommitChangeId, EntryKind, FilePatch, Hunk, Identity, LineOp,
    PatchStatus, TagInfo,
};
use knot_types::{AuthorName, Email, Oid, TagName};

pub(crate) const ZERO_TIME: &str = "0001-01-01T00:00:00Z";

fn display_opt<S: Serializer>(
    value: &Option<CommitChangeId>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    match value {
        Some(id) => serializer.serialize_str(id.as_str()),
        None => serializer.serialize_none(),
    }
}

fn zoned(seconds: i64, offset_seconds: i32) -> chrono::DateTime<chrono::FixedOffset> {
    let offset = chrono::FixedOffset::east_opt(offset_seconds)
        .unwrap_or_else(|| chrono::FixedOffset::east_opt(0).expect("zero offset is valid"));
    chrono::DateTime::from_timestamp(seconds, 0)
        .unwrap_or_default()
        .with_timezone(&offset)
}

pub(crate) fn rfc3339(seconds: i64, offset_seconds: i32) -> String {
    zoned(seconds, offset_seconds).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

pub(crate) fn rfc2822(seconds: i64, offset_seconds: i32) -> String {
    zoned(seconds, offset_seconds).to_rfc2822()
}

pub(crate) struct HashBytes(pub Oid);

impl Serialize for HashBytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let bytes = self.0.object_id();
        let mut seq = serializer.serialize_seq(Some(bytes.as_bytes().len()))?;
        bytes
            .as_bytes()
            .iter()
            .try_for_each(|byte| seq.serialize_element(byte))?;
        seq.end()
    }
}

pub(crate) struct Base64Bytes(Vec<u8>);

impl Serialize for Base64Bytes {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&STANDARD.encode(&self.0))
    }
}

#[derive(Serialize)]
pub(crate) struct SignatureWire {
    #[serde(rename = "Name")]
    pub name: AuthorName,
    #[serde(rename = "Email")]
    pub email: Email,
    #[serde(rename = "When")]
    pub when: String,
}

impl SignatureWire {
    pub fn of(identity: &Identity) -> Self {
        Self {
            name: identity.name.clone(),
            email: identity.email.clone(),
            when: rfc3339(identity.time.get(), identity.offset_seconds),
        }
    }

    pub fn utc(identity: &Identity) -> Self {
        Self {
            name: identity.name.clone(),
            email: identity.email.clone(),
            when: rfc3339(identity.time.get(), 0),
        }
    }

    pub fn zero() -> Self {
        Self {
            name: AuthorName::new(""),
            email: Email::new(""),
            when: ZERO_TIME.to_string(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct CommitWire {
    pub hash: HashBytes,
    pub author: SignatureWire,
    pub committer: SignatureWire,
    pub message: String,
    pub tree: Oid,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub parent_hashes: Vec<HashBytes>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pgp_signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merge_tag: Option<String>,
    #[serde(
        skip_serializing_if = "Option::is_none",
        serialize_with = "display_opt"
    )]
    pub change_id: Option<CommitChangeId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extra_headers: Option<std::collections::BTreeMap<String, Base64Bytes>>,
    pub this: Oid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<Oid>,
}

impl CommitWire {
    pub fn of(commit: &Commit) -> Self {
        let extra_headers: std::collections::BTreeMap<String, Base64Bytes> = commit
            .extra_headers
            .iter()
            .map(|(name, value)| (name.clone(), Base64Bytes(value.clone())))
            .collect();
        Self {
            hash: HashBytes(commit.id),
            author: SignatureWire::of(&commit.author),
            committer: SignatureWire::of(&commit.committer),
            message: commit.message.clone(),
            tree: commit.tree,
            parent_hashes: commit.parents.iter().map(|oid| HashBytes(*oid)).collect(),
            pgp_signature: commit.pgp_signature.clone(),
            merge_tag: commit.merge_tag.clone(),
            change_id: commit.change_id(),
            extra_headers: (!extra_headers.is_empty()).then_some(extra_headers),
            this: commit.id,
            parent: commit.parents.first().copied(),
        }
    }
}

#[derive(Serialize)]
pub(crate) struct BranchCommitWire {
    #[serde(rename = "Hash")]
    pub hash: HashBytes,
    #[serde(rename = "Author")]
    pub author: SignatureWire,
    #[serde(rename = "Committer")]
    pub committer: SignatureWire,
    #[serde(rename = "MergeTag")]
    pub merge_tag: String,
    #[serde(rename = "PGPSignature")]
    pub pgp_signature: String,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "TreeHash")]
    pub tree_hash: HashBytes,
    #[serde(rename = "ParentHashes")]
    pub parent_hashes: Vec<HashBytes>,
    #[serde(rename = "Encoding")]
    pub encoding: String,
    #[serde(rename = "ExtraHeaders")]
    pub extra_headers: Option<()>,
}

#[derive(Serialize)]
pub(crate) struct Reference {
    pub name: String,
    pub hash: Oid,
}

#[derive(Serialize)]
pub(crate) struct BranchWire {
    pub reference: Reference,
    pub commit: BranchCommitWire,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub is_default: bool,
}

impl BranchWire {
    pub fn of(branch: &BranchInfo, is_default: bool, absent: Oid) -> Self {
        Self {
            reference: Reference {
                name: branch.name.to_string(),
                hash: branch.tip.id(),
            },
            commit: match &branch.tip {
                BranchTip::Commit(commit) => BranchCommitWire {
                    hash: HashBytes(commit.id),
                    author: SignatureWire::utc(&commit.author),
                    committer: SignatureWire::utc(&commit.committer),
                    merge_tag: String::new(),
                    pgp_signature: String::new(),
                    message: commit.message.trim_end().to_string(),
                    tree_hash: HashBytes(commit.tree),
                    parent_hashes: commit.parents.iter().map(|oid| HashBytes(*oid)).collect(),
                    encoding: String::new(),
                    extra_headers: None,
                },
                BranchTip::Opaque { id, message, .. } => BranchCommitWire {
                    hash: HashBytes(*id),
                    author: SignatureWire::zero(),
                    committer: SignatureWire::zero(),
                    merge_tag: String::new(),
                    pgp_signature: String::new(),
                    message: message.trim_end().to_string(),
                    tree_hash: HashBytes(absent),
                    parent_hashes: Vec::new(),
                    encoding: String::new(),
                    extra_headers: None,
                },
            },
            is_default,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct TagObjectWire {
    #[serde(rename = "Hash")]
    pub hash: HashBytes,
    #[serde(rename = "Name")]
    pub name: TagName,
    #[serde(rename = "Tagger")]
    pub tagger: SignatureWire,
    #[serde(rename = "Message")]
    pub message: String,
    #[serde(rename = "PGPSignature")]
    pub pgp_signature: String,
    #[serde(rename = "TargetType")]
    pub target_type: i8,
    #[serde(rename = "Target")]
    pub target: HashBytes,
}

#[derive(Serialize)]
pub(crate) struct TagWire {
    pub name: TagName,
    pub hash: Oid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<TagObjectWire>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub message: String,
}

const TARGET_TYPE_TAG: i8 = 4;

impl TagWire {
    pub fn of(info: &TagInfo) -> Self {
        let message = recombine_message(&info.message);
        let tag =
            info.annotated.as_ref().map(|annotated| TagObjectWire {
                hash: HashBytes(info.id),
                name: info.name.clone(),
                tagger: annotated.tagger.as_ref().map(SignatureWire::utc).unwrap_or(
                    SignatureWire {
                        name: AuthorName::new(""),
                        email: Email::new(""),
                        when: rfc3339(0, 0),
                    },
                ),
                message: message.clone(),
                pgp_signature: annotated.pgp_signature.clone().unwrap_or_default(),
                target_type: TARGET_TYPE_TAG,
                target: HashBytes(annotated.target),
            });
        Self {
            name: info.name.clone(),
            hash: info.id,
            tag,
            message,
        }
    }
}

pub(crate) fn fold_subject(message: &str) -> String {
    message
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .lines()
        .map(str::trim_end)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(crate) fn message_body(message: &str) -> String {
    message
        .split_once("\n\n")
        .map(|(_, body)| body.trim_matches('\n').to_string())
        .unwrap_or_default()
}

fn recombine_message(message: &str) -> String {
    let subject = fold_subject(message);
    let body = message_body(message);
    match (subject.is_empty(), body.is_empty()) {
        (_, true) => subject,
        (true, false) => body,
        (false, false) => format!("{subject}\n\n{body}"),
    }
}

#[derive(Serialize)]
pub(crate) struct LineWire {
    #[serde(rename = "Op")]
    pub op: u8,
    #[serde(rename = "Line")]
    pub line: String,
}

#[derive(Serialize)]
pub(crate) struct TextFragmentWire {
    #[serde(rename = "Comment")]
    pub comment: String,
    #[serde(rename = "OldPosition")]
    pub old_position: i64,
    #[serde(rename = "OldLines")]
    pub old_lines: i64,
    #[serde(rename = "NewPosition")]
    pub new_position: i64,
    #[serde(rename = "NewLines")]
    pub new_lines: i64,
    #[serde(rename = "LinesAdded")]
    pub lines_added: i64,
    #[serde(rename = "LinesDeleted")]
    pub lines_deleted: i64,
    #[serde(rename = "LeadingContext")]
    pub leading_context: i64,
    #[serde(rename = "TrailingContext")]
    pub trailing_context: i64,
    #[serde(rename = "Lines")]
    pub lines: Vec<LineWire>,
}

impl TextFragmentWire {
    pub fn of(hunk: &Hunk) -> Self {
        let lines: Vec<LineWire> = hunk
            .lines
            .iter()
            .map(|line| LineWire {
                op: match line.op {
                    LineOp::Context => 0,
                    LineOp::Delete => 1,
                    LineOp::Add => 2,
                },
                line: String::from_utf8_lossy(&line.text).into_owned(),
            })
            .collect();
        let leading = lines.iter().take_while(|line| line.op == 0).count();
        let trailing = if leading == lines.len() {
            0
        } else {
            lines.iter().rev().take_while(|line| line.op == 0).count()
        };
        Self {
            comment: String::new(),
            old_position: hunk.old_start.get() as i64,
            old_lines: hunk.old_lines.get() as i64,
            new_position: hunk.new_start.get() as i64,
            new_lines: hunk.new_lines.get() as i64,
            lines_added: hunk.added().get() as i64,
            lines_deleted: hunk.deleted().get() as i64,
            leading_context: leading as i64,
            trailing_context: trailing as i64,
            lines,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct DiffNameWire {
    pub old: String,
    pub new: String,
}

#[derive(Serialize)]
pub(crate) struct DiffWire {
    pub name: DiffNameWire,
    pub text_fragments: Option<Vec<TextFragmentWire>>,
    pub is_binary: bool,
    pub is_new: bool,
    pub is_delete: bool,
    pub is_copy: bool,
    pub is_rename: bool,
}

impl DiffWire {
    pub fn of(patch: &FilePatch) -> Self {
        let fragments: Vec<TextFragmentWire> =
            patch.hunks.iter().map(TextFragmentWire::of).collect();
        Self {
            name: DiffNameWire {
                old: match patch.status {
                    PatchStatus::Added => String::new(),
                    _ => patch.path.to_string(),
                },
                new: match patch.status {
                    PatchStatus::Deleted => String::new(),
                    _ => patch.path.to_string(),
                },
            },
            text_fragments: (!fragments.is_empty()).then_some(fragments),
            is_binary: patch.is_binary,
            is_new: patch.status == PatchStatus::Added,
            is_delete: patch.status == PatchStatus::Deleted,
            is_copy: false,
            is_rename: false,
        }
    }
}

#[derive(Serialize)]
pub(crate) struct DiffStatWire {
    pub insertions: i64,
    pub deletions: i64,
    pub files_changed: i64,
}

#[derive(Serialize)]
pub(crate) struct NiceDiffWire {
    pub commit: CommitWire,
    pub stat: DiffStatWire,
    pub diff: Option<Vec<DiffWire>>,
}

pub(crate) fn nice_diff(commit: &Commit, patches: &[FilePatch]) -> NiceDiffWire {
    let diffs: Vec<DiffWire> = patches.iter().map(DiffWire::of).collect();
    let stat = DiffStatWire {
        insertions: patches
            .iter()
            .flat_map(|patch| patch.hunks.iter())
            .map(|hunk| hunk.added().get() as i64)
            .sum(),
        deletions: patches
            .iter()
            .flat_map(|patch| patch.hunks.iter())
            .map(|hunk| hunk.deleted().get() as i64)
            .sum(),
        files_changed: patches.len() as i64,
    };
    NiceDiffWire {
        commit: CommitWire::of(commit),
        stat,
        diff: (!diffs.is_empty()).then_some(diffs),
    }
}

#[derive(Serialize)]
pub(crate) struct PatchIdentityWire {
    #[serde(rename = "Name")]
    pub name: AuthorName,
    #[serde(rename = "Email")]
    pub email: Email,
}

#[derive(Serialize)]
pub(crate) struct FormatPatchWire {
    #[serde(rename = "Files")]
    pub files: Option<Vec<FileWire>>,
    #[serde(rename = "SHA")]
    pub sha: Oid,
    #[serde(rename = "Author")]
    pub author: Option<PatchIdentityWire>,
    #[serde(rename = "AuthorDate")]
    pub author_date: String,
    #[serde(rename = "Committer")]
    pub committer: Option<()>,
    #[serde(rename = "CommitterDate")]
    pub committer_date: String,
    #[serde(rename = "Title")]
    pub title: String,
    #[serde(rename = "Body")]
    pub body: String,
    #[serde(rename = "SubjectPrefix")]
    pub subject_prefix: String,
    #[serde(rename = "BodyAppendix")]
    pub body_appendix: String,
    #[serde(rename = "RawHeaders")]
    pub raw_headers: Option<std::collections::BTreeMap<String, Vec<String>>>,
    #[serde(rename = "Raw")]
    pub raw: String,
}

pub(crate) fn normalize_message_section<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    lines
        .map(str::trim_end)
        .fold((String::new(), 0usize), |(mut out, blanks), line| {
            if line.is_empty() {
                return (out, blanks + 1);
            }
            if !out.is_empty() {
                out.push('\n');
                if blanks > 0 {
                    out.push('\n');
                }
            }
            out.push_str(line);
            (out, 0)
        })
        .0
}

fn entry_mode_decimal(kind: EntryKind) -> u32 {
    match kind {
        EntryKind::Tree => 0o040000,
        EntryKind::Blob => 0o100644,
        EntryKind::BlobExecutable => 0o100755,
        EntryKind::Link => 0o120000,
        EntryKind::Commit => 0o160000,
    }
}

pub(crate) fn entry_mode_octal(kind: EntryKind) -> String {
    format!("{:06o}", entry_mode_decimal(kind))
}

#[derive(Serialize)]
pub(crate) struct FileWire {
    #[serde(rename = "OldName")]
    pub old_name: String,
    #[serde(rename = "NewName")]
    pub new_name: String,
    #[serde(rename = "IsNew")]
    pub is_new: bool,
    #[serde(rename = "IsDelete")]
    pub is_delete: bool,
    #[serde(rename = "IsCopy")]
    pub is_copy: bool,
    #[serde(rename = "IsRename")]
    pub is_rename: bool,
    #[serde(rename = "OldMode")]
    pub old_mode: u32,
    #[serde(rename = "NewMode")]
    pub new_mode: u32,
    #[serde(rename = "OldOIDPrefix")]
    pub old_oid_prefix: String,
    #[serde(rename = "NewOIDPrefix")]
    pub new_oid_prefix: String,
    #[serde(rename = "Score")]
    pub score: i64,
    #[serde(rename = "TextFragments")]
    pub text_fragments: Option<Vec<TextFragmentWire>>,
    #[serde(rename = "IsBinary")]
    pub is_binary: bool,
    #[serde(rename = "BinaryFragment")]
    pub binary_fragment: Option<()>,
    #[serde(rename = "ReverseBinaryFragment")]
    pub reverse_binary_fragment: Option<()>,
}

impl FileWire {
    pub fn of(patch: &FilePatch) -> Self {
        let fragments: Vec<TextFragmentWire> =
            patch.hunks.iter().map(TextFragmentWire::of).collect();
        let same_mode = patch.old_kind.is_some() && patch.old_kind == patch.new_kind;
        Self {
            old_name: match patch.status {
                PatchStatus::Added => String::new(),
                _ => patch.path.to_string(),
            },
            new_name: match patch.status {
                PatchStatus::Deleted => String::new(),
                _ => patch.path.to_string(),
            },
            is_new: patch.status == PatchStatus::Added,
            is_delete: patch.status == PatchStatus::Deleted,
            is_copy: false,
            is_rename: false,
            old_mode: match patch.status {
                PatchStatus::Added => 0,
                PatchStatus::Deleted | PatchStatus::Modified => {
                    patch.old_kind.map(entry_mode_decimal).unwrap_or(0)
                }
            },
            new_mode: match patch.status {
                PatchStatus::Added => patch.new_kind.map(entry_mode_decimal).unwrap_or(0),
                PatchStatus::Deleted => 0,
                PatchStatus::Modified if same_mode => 0,
                PatchStatus::Modified => patch.new_kind.map(entry_mode_decimal).unwrap_or(0),
            },
            old_oid_prefix: patch.old_oid.to_hex(),
            new_oid_prefix: patch.new_oid.to_hex(),
            score: 0,
            text_fragments: (!fragments.is_empty()).then_some(fragments),
            is_binary: patch.is_binary,
            binary_fragment: None,
            reverse_binary_fragment: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{rfc2822, rfc3339};

    const A_JUNE_INSTANT: i64 = 1_717_236_600;

    #[test]
    fn rfc3339_renders_the_commits_own_offset_independent_of_the_host_zone() {
        assert!(rfc3339(A_JUNE_INSTANT, 7200).ends_with("+02:00"));
        assert!(rfc3339(A_JUNE_INSTANT, -18000).ends_with("-05:00"));
        assert!(rfc3339(A_JUNE_INSTANT, 0).ends_with('Z'));
    }

    #[test]
    fn rfc2822_keeps_the_signed_offset() {
        assert!(rfc2822(A_JUNE_INSTANT, 7200).ends_with("+0200"));
        assert!(rfc2822(A_JUNE_INSTANT, -18000).ends_with("-0500"));
    }
}
