use knot_git::{Commit, FilePatch, Hunk, LineCount, LineNumber, LineOp, PatchStatus};

use crate::wire::{entry_mode_octal, fold_subject, message_body, rfc2822};

const GRAPH_WIDTH: usize = 60;

fn span(start: LineNumber, lines: LineCount) -> String {
    match lines.get() {
        1 => format!("{}", start.get()),
        _ => format!("{},{}", start.get(), lines.get()),
    }
}

fn render_hunk(out: &mut String, hunk: &Hunk) {
    out.push_str(&format!(
        "@@ -{} +{} @@\n",
        span(hunk.old_start, hunk.old_lines),
        span(hunk.new_start, hunk.new_lines)
    ));
    hunk.lines.iter().for_each(|line| {
        out.push(match line.op {
            LineOp::Context => ' ',
            LineOp::Delete => '-',
            LineOp::Add => '+',
        });
        out.push_str(&String::from_utf8_lossy(&line.text));
        if !line.text.ends_with(b"\n") {
            out.push_str("\n\\ No newline at end of file\n");
        }
    });
}

fn render_file(out: &mut String, patch: &FilePatch) {
    let (a, b) = (&patch.path, &patch.path);
    out.push_str(&format!("diff --git a/{a} b/{b}\n"));
    match patch.status {
        PatchStatus::Added => {
            let mode = patch.new_kind.map(entry_mode_octal).unwrap_or_default();
            out.push_str(&format!("new file mode {mode}\n"));
            out.push_str(&format!(
                "index {}..{}\n",
                patch.old_oid.to_hex(),
                patch.new_oid.to_hex()
            ));
        }
        PatchStatus::Deleted => {
            let mode = patch.old_kind.map(entry_mode_octal).unwrap_or_default();
            out.push_str(&format!("deleted file mode {mode}\n"));
            out.push_str(&format!(
                "index {}..{}\n",
                patch.old_oid.to_hex(),
                patch.new_oid.to_hex()
            ));
        }
        PatchStatus::Modified => {
            if patch.old_kind == patch.new_kind {
                let mode = patch.old_kind.map(entry_mode_octal).unwrap_or_default();
                out.push_str(&format!(
                    "index {}..{} {mode}\n",
                    patch.old_oid.to_hex(),
                    patch.new_oid.to_hex()
                ));
            } else {
                let old = patch.old_kind.map(entry_mode_octal).unwrap_or_default();
                let new = patch.new_kind.map(entry_mode_octal).unwrap_or_default();
                out.push_str(&format!("old mode {old}\nnew mode {new}\n"));
                out.push_str(&format!(
                    "index {}..{}\n",
                    patch.old_oid.to_hex(),
                    patch.new_oid.to_hex()
                ));
            }
        }
    }
    let old_label = match patch.status {
        PatchStatus::Added => "/dev/null".to_string(),
        _ => format!("a/{a}"),
    };
    let new_label = match patch.status {
        PatchStatus::Deleted => "/dev/null".to_string(),
        _ => format!("b/{b}"),
    };
    if patch.is_binary {
        out.push_str(&format!(
            "Binary files {old_label} and {new_label} differ\n"
        ));
        return;
    }
    if patch.hunks.is_empty() {
        return;
    }
    out.push_str(&format!("--- {old_label}\n+++ {new_label}\n"));
    patch.hunks.iter().for_each(|hunk| render_hunk(out, hunk));
}

pub(crate) fn render_patches(patches: &[FilePatch]) -> String {
    patches.iter().fold(String::new(), |mut out, patch| {
        render_file(&mut out, patch);
        out
    })
}

fn stat_counts(patch: &FilePatch) -> (usize, usize) {
    patch.hunks.iter().fold((0, 0), |(added, deleted), hunk| {
        (
            added + hunk.added().get() as usize,
            deleted + hunk.deleted().get() as usize,
        )
    })
}

fn graph(added: usize, deleted: usize) -> String {
    let total = added + deleted;
    let (added, deleted) = if total > GRAPH_WIDTH {
        (added * GRAPH_WIDTH / total, deleted * GRAPH_WIDTH / total)
    } else {
        (added, deleted)
    };
    format!("{}{}", "+".repeat(added), "-".repeat(deleted))
}

fn diffstat(patches: &[FilePatch]) -> String {
    let width = patches
        .iter()
        .map(|patch| patch.path.as_str().len())
        .max()
        .unwrap_or(0);
    let rows: String = patches
        .iter()
        .map(|patch| {
            if patch.is_binary {
                format!(" {:<width$} | Bin\n", patch.path)
            } else {
                let (added, deleted) = stat_counts(patch);
                format!(
                    " {:<width$} | {} {}\n",
                    patch.path,
                    added + deleted,
                    graph(added, deleted)
                )
            }
        })
        .collect();
    let (added, deleted) = patches.iter().fold((0, 0), |(a, d), patch| {
        let (pa, pd) = stat_counts(patch);
        (a + pa, d + pd)
    });
    let files = patches.len();
    let mut summary = format!(" {files} file{} changed", if files == 1 { "" } else { "s" });
    if added > 0 {
        summary.push_str(&format!(
            ", {added} insertion{}(+)",
            if added == 1 { "" } else { "s" }
        ));
    }
    if deleted > 0 {
        summary.push_str(&format!(
            ", {deleted} deletion{}(-)",
            if deleted == 1 { "" } else { "s" }
        ));
    }
    summary.push('\n');
    let created: String = patches
        .iter()
        .filter(|patch| patch.status == PatchStatus::Added)
        .map(|patch| {
            format!(
                " create mode {} {}\n",
                patch.new_kind.map(entry_mode_octal).unwrap_or_default(),
                patch.path
            )
        })
        .collect();
    let deleted_rows: String = patches
        .iter()
        .filter(|patch| patch.status == PatchStatus::Deleted)
        .map(|patch| {
            format!(
                " delete mode {} {}\n",
                patch.old_kind.map(entry_mode_octal).unwrap_or_default(),
                patch.path
            )
        })
        .collect();
    format!("{rows}{summary}{created}{deleted_rows}")
}

pub(crate) fn render_format_patch(commit: &Commit, patches: &[FilePatch]) -> String {
    let subject = fold_subject(&commit.message);
    let body = message_body(&commit.message);
    let mut out = format!("From {} Mon Sep 17 00:00:00 2001\n", commit.id.to_hex());
    out.push_str(&format!(
        "From: {} <{}>\n",
        commit.author.name, commit.author.email
    ));
    out.push_str(&format!(
        "Date: {}\n",
        rfc2822(commit.author.time.get(), commit.author.offset_seconds)
    ));
    out.push_str(&format!("Subject: [PATCH] {subject}\n"));
    if let Some(change_id) = commit.change_id() {
        out.push_str(&format!("Change-Id: {change_id}\n"));
    }
    out.push('\n');
    if !body.is_empty() {
        out.push_str(&body);
        out.push('\n');
    }
    out.push_str("---\n");
    out.push_str(&diffstat(patches));
    out.push('\n');
    out.push_str(&render_patches(patches));
    out.push_str("-- \nknot\n\n");
    out
}
