use knot_git::{
    BinaryDiff, Commit, EntryKind, FilePatch, Hunk, LineCount, LineNumber, LineOp, PatchBody,
    PatchStatus, quote_path,
};

use crate::wire::{entry_mode_octal, fold_subject, message_body, rfc2822};

const GRAPH_WIDTH: usize = 60;

fn entry_mode(kind: Option<EntryKind>) -> String {
    kind.map(entry_mode_octal).unwrap_or_default()
}

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
    let old_side = quote_path(&format!("a/{}", patch.path));
    let new_side = quote_path(&format!("b/{}", patch.path));
    let index = format!(
        "index {}..{}",
        patch.old_oid.to_hex(),
        patch.new_oid.to_hex()
    );
    out.push_str(&format!("diff --git {old_side} {new_side}\n"));
    match patch.status {
        PatchStatus::Added => out.push_str(&format!(
            "new file mode {}\n{index}\n",
            entry_mode(patch.new_kind)
        )),
        PatchStatus::Deleted => out.push_str(&format!(
            "deleted file mode {}\n{index}\n",
            entry_mode(patch.old_kind)
        )),
        PatchStatus::Modified if patch.old_kind == patch.new_kind => {
            out.push_str(&format!("{index} {}\n", entry_mode(patch.old_kind)))
        }
        PatchStatus::Modified => {
            out.push_str(&format!(
                "old mode {}\nnew mode {}\n",
                entry_mode(patch.old_kind),
                entry_mode(patch.new_kind)
            ));
            match patch.old_oid == patch.new_oid {
                true => {}
                false => out.push_str(&format!("{index}\n")),
            }
        }
    }
    let old_label = match patch.status {
        PatchStatus::Added => "/dev/null",
        _ => old_side.as_str(),
    };
    let new_label = match patch.status {
        PatchStatus::Deleted => "/dev/null",
        _ => new_side.as_str(),
    };
    match &patch.body {
        PatchBody::Binary(BinaryDiff::Encoded { text, .. }) => out.push_str(text),
        PatchBody::Binary(BinaryDiff::Omitted(_)) => out.push_str(&format!(
            "Binary files {old_label} and {new_label} differ\n"
        )),
        PatchBody::Binary(BinaryDiff::Unchanged(_)) => {}
        PatchBody::Text(hunks) if hunks.is_empty() => {}
        PatchBody::Text(hunks) => {
            out.push_str(&format!("--- {old_label}\n+++ {new_label}\n"));
            hunks.iter().for_each(|hunk| render_hunk(out, hunk));
        }
    }
}

pub(crate) fn render_patches(patches: &[FilePatch]) -> String {
    patches.iter().fold(String::new(), |mut out, patch| {
        render_file(&mut out, patch);
        out
    })
}

fn stat_counts(patch: &FilePatch) -> (usize, usize) {
    patch.hunks().iter().fold((0, 0), |(added, deleted), hunk| {
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
    let named: Vec<(String, &FilePatch)> = patches
        .iter()
        .map(|patch| (quote_path(patch.path.as_str()), patch))
        .collect();
    let width = named.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
    let rows: String = named
        .iter()
        .map(|(name, patch)| match &patch.body {
            PatchBody::Binary(BinaryDiff::Unchanged(_)) => format!(" {name:<width$} | Bin\n"),
            PatchBody::Binary(binary) => {
                let sizes = binary.sizes();
                format!(
                    " {name:<width$} | Bin {} -> {} bytes\n",
                    sizes.old, sizes.new
                )
            }
            PatchBody::Text(_) => {
                let (added, deleted) = stat_counts(patch);
                let total = added + deleted;
                format!(" {name:<width$} | {total} {}\n", graph(added, deleted))
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
    let modes: String = named
        .iter()
        .filter_map(|(name, patch)| {
            let (old, new) = (entry_mode(patch.old_kind), entry_mode(patch.new_kind));
            match patch.status {
                PatchStatus::Added => Some(format!(" create mode {new} {name}\n")),
                PatchStatus::Deleted => Some(format!(" delete mode {old} {name}\n")),
                PatchStatus::Modified if patch.old_kind != patch.new_kind => {
                    Some(format!(" mode change {old} => {new} {name}\n"))
                }
                PatchStatus::Modified => None,
            }
        })
        .collect();
    format!("{rows}{summary}{modes}")
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
