mod archive;
mod bitmap;
mod error;
#[cfg(feature = "instrument")]
pub mod instrument;
mod maintenance;
mod objects;
mod patch;
mod patch_apply;
mod patch_parse;
mod reads;
mod repo;
mod staging;

pub use archive::{ArchiveFormat, ArchiveLimit, ArchivePrefix};
pub use bitmap::{reachable_via_bitmap, verbatim_clone_pack, write_bitmap, write_midx_bitmap};
pub use error::{GitError, SelectionLimit};
pub use maintenance::{PackRefsReport, ReflogReport};
pub use objects::{
    BlobReader, Commit, CommitChangeId, CommitDepth, CommitRange, Comparison, Deepen, EntryKind,
    FileChange, Filter, Haves, Identity, MAX_TREE_DEPTH, PackBudget, PackSelection, ShallowCommits,
    ShallowPlan, Tree, TreeDepth, TreeEntry, Wants,
};
pub use patch::{
    FilePatch, Hunk, HunkLine, LineCount, LineNumber, LineOp, MAX_DIFF_BLOB_BYTES, PatchRange,
    PatchStatus,
};
pub use patch_apply::{
    ApplyError, ApplyOutcome, Conflict, ConflictReason, NewCommit, PatchApplier, StagedAction,
    StagedChange,
};
pub use patch_parse::{
    FileIntent, MailPatch, ParsedFile, PatchParseError, PatchPayload, is_format_patch,
    parse_mailbox, parse_mailbox_bounded, parse_patch, parse_patch_bounded,
};
pub use reads::{
    AnnotatedTag, BranchInfo, BranchTip, LastCommit, LogLimit, LogSkip, PathEntry, SizedEntry,
    Submodule, TagInfo,
};
pub use repo::{
    AdvertScope, HeadRef, Layout, PackHash, PackfileUri, PackfileUrl, RefRecord, RefTxn, RefUpdate,
    ReflogUpdate, Repo, is_branch, is_public_ref, is_reserved, knot_shard, repo_shard,
    screens_reserved,
};
pub use staging::{INCOMING_PREFIX, Staging};

#[doc(hidden)]
pub mod fuzz {
    pub fn patch(data: &[u8]) {
        let text = String::from_utf8_lossy(data);
        let _ = crate::is_format_patch(&text);
        let _ = crate::parse_patch(&text);
        let _ = crate::parse_mailbox(&text);
        let mid = data.len() / 2;
        let _ = crate::patch_apply::apply_delta(&data[..mid], &data[mid..]);
    }
}
