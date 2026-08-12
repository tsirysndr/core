use std::io::Read;

use base64::Engine;
use knot_types::{AuthorName, Email, Oid};

use crate::base85;
use crate::objects::{CommitChangeId, EntryKind};
use crate::patch::{Hunk, HunkLine, LineCount, LineNumber, LineOp, MAX_DIFF_BLOB_BYTES};

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum PatchParseError {
    #[error("patch is empty")]
    Empty,
    #[error("patch contains no file changes")]
    NoFiles,
    #[error("malformed patch: {0}")]
    Malformed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileIntent {
    Create,
    Delete,
    Modify,
    Rename { from: String },
    Copy { from: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchPayload {
    Text(Vec<Hunk>),
    BinaryLiteral(Vec<u8>),
    BinaryDelta(Vec<u8>),
    BinaryOpaque,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedFile {
    pub path: String,
    pub intent: FileIntent,
    pub old_kind: Option<EntryKind>,
    pub new_kind: Option<EntryKind>,
    pub old_index: Option<Oid>,
    pub payload: PatchPayload,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailPatch {
    pub author_name: AuthorName,
    pub author_email: Email,
    pub date: String,
    pub subject: String,
    pub body: String,
    pub change_id: Option<CommitChangeId>,
    pub files: Vec<ParsedFile>,
}

impl MailPatch {
    pub fn commit_message(&self) -> String {
        match self.body.is_empty() {
            true => self.subject.clone(),
            false => format!("{}\n\n{}", self.subject, self.body),
        }
    }
}

pub fn is_format_patch(patch: &str) -> bool {
    let lines: Vec<&str> = patch.split('\n').collect();
    if lines.len() < 2 {
        return false;
    }
    let first = lines[0].trim();
    if first.starts_with("From ") && first.contains(" Mon Sep 17 00:00:00 2001") {
        return true;
    }
    lines
        .iter()
        .take(10)
        .map(|line| line.trim())
        .filter(|line| {
            line.starts_with("From: ")
                || line.starts_with("Date: ")
                || line.starts_with("Subject: ")
                || line.starts_with("commit ")
        })
        .count()
        >= 2
}

struct Cursor<'a> {
    lines: &'a [&'a str],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(lines: &'a [&'a str]) -> Self {
        Self { lines, pos: 0 }
    }

    fn peek(&self) -> Option<&'a str> {
        self.lines.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<&'a str> {
        let line = self.peek()?;
        self.pos += 1;
        Some(line)
    }

    fn take_prefix(&mut self, prefix: &str) -> Option<&'a str> {
        let rest = self.peek()?.strip_prefix(prefix)?;
        self.pos += 1;
        Some(rest)
    }
}

fn malformed(message: impl Into<String>) -> PatchParseError {
    PatchParseError::Malformed(message.into())
}

const MAX_TOTAL_PATCH_BYTES: u64 = 128 * 1024 * 1024;

struct Budget {
    remaining: u64,
}

impl Budget {
    fn new(limit: u64) -> Self {
        Self { remaining: limit }
    }

    fn charge(&mut self, bytes: u64) -> Result<(), PatchParseError> {
        self.remaining = self
            .remaining
            .checked_sub(bytes)
            .ok_or_else(|| malformed("patch exceeds total decompressed size budget"))?;
        Ok(())
    }
}

fn unescape_c(bytes: &[u8]) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || match bytes.get(pos..) {
        None | Some([]) => None,
        Some(slice) => {
            let decoded: Option<u8> = match slice {
                [b'\\', b'n', ..] => apply(&mut pos, 2, b'\n'),
                [b'\\', b't', ..] => apply(&mut pos, 2, b'\t'),
                [b'\\', b'"', ..] => apply(&mut pos, 2, b'"'),
                [b'\\', b'\\', ..] => apply(&mut pos, 2, b'\\'),
                [b'\\', a @ b'0'..=b'3', b @ b'0'..=b'7', c @ b'0'..=b'7', ..] => {
                    apply(&mut pos, 4, (a - b'0') * 64 + (b - b'0') * 8 + (c - b'0'))
                }
                [b'\\', ..] => {
                    pos += 1;
                    None
                }
                [byte, ..] => apply(&mut pos, 1, *byte),
                [] => None,
            };
            Some(decoded)
        }
    })
    .collect()
}

fn apply(pos: &mut usize, width: usize, byte: u8) -> Option<u8> {
    *pos += width;
    Some(byte)
}

fn quoted_end(bytes: &[u8]) -> Option<usize> {
    let mut idx = 0usize;
    std::iter::from_fn(move || match bytes.get(idx) {
        Some(b'"') => Some(Some(idx)),
        Some(b'\\') => {
            idx += 2;
            Some(None)
        }
        Some(_) => {
            idx += 1;
            Some(None)
        }
        None => None,
    })
    .flatten()
    .next()
}

fn unquote(raw: &str) -> Result<String, PatchParseError> {
    match raw.strip_prefix('"') {
        None => Ok(raw.to_string()),
        Some(inner) => {
            let end =
                quoted_end(inner.as_bytes()).ok_or_else(|| malformed("unclosed quoted path"))?;
            let unescaped = unescape_c(&inner.as_bytes()[..end])
                .ok_or_else(|| malformed("bad escape in quoted path"))?;
            String::from_utf8(unescaped).map_err(|_| malformed("quoted path isn't utf-8"))
        }
    }
}

const PRINTABLE_ASCII: std::ops::Range<u8> = 0x20..0x7f;

fn needs_quoting(byte: u8) -> bool {
    !PRINTABLE_ASCII.contains(&byte) || matches!(byte, b'"' | b'\\')
}

pub fn quote_path(path: &str) -> String {
    match path.bytes().any(needs_quoting) {
        false => path.to_string(),
        true => {
            let mut quoted = path.bytes().fold(String::from("\""), |mut out, byte| {
                match byte {
                    b'\n' => out.push_str("\\n"),
                    b'\t' => out.push_str("\\t"),
                    b'"' => out.push_str("\\\""),
                    b'\\' => out.push_str("\\\\"),
                    other if !PRINTABLE_ASCII.contains(&other) => {
                        out.push_str(&format!("\\{other:03o}"))
                    }
                    other => out.push(other as char),
                }
                out
            });
            quoted.push('"');
            quoted
        }
    }
}

fn strip_level(path: &str) -> String {
    path.split_once('/')
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_else(|| path.to_string())
}

fn parse_label(raw: &str) -> Result<Option<String>, PatchParseError> {
    let bare = match raw.starts_with('"') {
        true => unquote(raw)?,
        false => raw.split('\t').next().unwrap_or(raw).trim_end().to_string(),
    };
    Ok(match bare.as_str() {
        "/dev/null" => None,
        _ => Some(strip_level(&bare)),
    })
}

fn diff_paths(rest: &str) -> Option<(String, String)> {
    match rest.contains('"') {
        true => {
            let (old, after) = take_path_token(rest)?;
            let (new, _) = take_path_token(after.strip_prefix(' ')?)?;
            Some((strip_level(&old), strip_level(&new)))
        }
        false => unquoted_diff_paths(rest),
    }
}

fn unquoted_diff_paths(rest: &str) -> Option<(String, String)> {
    let split_at = |at: usize| {
        let old = rest.get(..at)?.strip_prefix("a/")?;
        let new = rest.get(at + " b/".len()..)?;
        Some((old, new))
    };
    rest.match_indices(" b/")
        .filter_map(|(at, _)| split_at(at))
        .find(|(old, new)| old == new)
        .or_else(|| split_at(rest.rfind(" b/")?))
        .map(|(old, new)| (old.to_string(), new.to_string()))
}

fn take_path_token(rest: &str) -> Option<(String, &str)> {
    match rest.strip_prefix('"') {
        Some(inner) => {
            let end = quoted_end(inner.as_bytes())?;
            let token = String::from_utf8(unescape_c(&inner.as_bytes()[..end])?).ok()?;
            Some((token, inner.get(end + 1..)?))
        }
        None => {
            let end = rest.find(' ').unwrap_or(rest.len());
            Some((rest[..end].to_string(), &rest[end..]))
        }
    }
}

fn full_oid(hex: &str) -> Option<Oid> {
    Oid::from_hex(hex).ok()
}

fn parse_hunk_header(line: &str) -> Option<(LineNumber, LineCount, LineNumber, LineCount)> {
    let rest = line.strip_prefix("@@ -")?;
    let (old, rest) = rest.split_once(" +")?;
    let (new, _) = rest.split_once(" @@")?;
    let span = |raw: &str| -> Option<(LineNumber, LineCount)> {
        match raw.split_once(',') {
            Some((start, lines)) => Some((
                LineNumber::new(start.parse().ok()?),
                LineCount::new(lines.parse().ok()?),
            )),
            None => Some((LineNumber::new(raw.parse().ok()?), LineCount::new(1))),
        }
    };
    let (old_start, old_lines) = span(old)?;
    let (new_start, new_lines) = span(new)?;
    Some((old_start, old_lines, new_start, new_lines))
}

fn parse_hunk(cursor: &mut Cursor<'_>, budget: &mut Budget) -> Result<Hunk, PatchParseError> {
    let header = cursor.next().ok_or_else(|| malformed("truncated hunk"))?;
    let (old_start, old_lines, new_start, new_lines) =
        parse_hunk_header(header).ok_or_else(|| malformed(format!("bad hunk header {header}")))?;
    let mut lines: Vec<HunkLine> = Vec::new();
    let mut old_left = old_lines.get() as i64;
    let mut new_left = new_lines.get() as i64;
    std::iter::from_fn(|| {
        (old_left > 0 || new_left > 0).then(|| -> Result<(), PatchParseError> {
            let line = cursor
                .next()
                .ok_or_else(|| malformed("hunk ends before its declared length"))?;
            budget.charge(line.len() as u64 + 1)?;
            let push = |lines: &mut Vec<HunkLine>, op: LineOp| {
                let mut text = line.get(1..).unwrap_or("").as_bytes().to_vec();
                text.push(b'\n');
                lines.push(HunkLine { op, text });
            };
            match line.as_bytes().first() {
                Some(b' ') | None => {
                    old_left -= 1;
                    new_left -= 1;
                    push(&mut lines, LineOp::Context);
                    Ok(())
                }
                Some(b'-') => {
                    old_left -= 1;
                    push(&mut lines, LineOp::Delete);
                    Ok(())
                }
                Some(b'+') => {
                    new_left -= 1;
                    push(&mut lines, LineOp::Add);
                    Ok(())
                }
                Some(b'\\') => {
                    strip_last_newline(&mut lines);
                    Ok(())
                }
                _ => Err(malformed(format!("unexpected hunk line {line}"))),
            }
        })
    })
    .try_for_each(|outcome| outcome)?;
    if cursor.peek().is_some_and(|line| line.starts_with('\\')) {
        cursor.next();
        strip_last_newline(&mut lines);
    }
    Ok(Hunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        lines,
    })
}

fn strip_last_newline(lines: &mut [HunkLine]) {
    if let Some(last) = lines.last_mut()
        && last.text.last() == Some(&b'\n')
    {
        last.text.pop();
    }
}

fn parse_hunks(cursor: &mut Cursor<'_>, budget: &mut Budget) -> Result<Vec<Hunk>, PatchParseError> {
    std::iter::from_fn(|| {
        cursor
            .peek()
            .is_some_and(|line| line.starts_with("@@ -"))
            .then(|| parse_hunk(cursor, budget))
    })
    .collect()
}

fn parse_binary_block(
    cursor: &mut Cursor<'_>,
    budget: &mut Budget,
) -> Result<(bool, Vec<u8>), PatchParseError> {
    let header = cursor
        .next()
        .ok_or_else(|| malformed("truncated binary patch"))?;
    let (kind, size) = header
        .split_once(' ')
        .ok_or_else(|| malformed(format!("bad binary patch header {header}")))?;
    let is_delta = match kind {
        "literal" => false,
        "delta" => true,
        _ => return Err(malformed(format!("unknown binary patch kind {kind}"))),
    };
    let size: u64 = size
        .trim()
        .parse()
        .map_err(|_| malformed("bad binary patch size"))?;
    if size > MAX_DIFF_BLOB_BYTES {
        return Err(malformed("binary patch exceeds size limit"));
    }
    budget.charge(size)?;
    let mut packed: Vec<u8> = Vec::new();
    std::iter::from_fn(|| {
        cursor
            .peek()
            .is_some_and(|line| !line.is_empty())
            .then(|| cursor.next().expect("peeked line is present"))
    })
    .try_for_each(|line| {
        base85::decode_line(line, &mut packed)
            .map_err(|_| malformed("invalid base85 line in binary patch"))
    })?;
    cursor.next();
    let mut inflated: Vec<u8> = Vec::new();
    flate2::read::ZlibDecoder::new(packed.as_slice())
        .take(size + 1)
        .read_to_end(&mut inflated)
        .map_err(|error| malformed(format!("bad zlib stream in binary patch: {error}")))?;
    if inflated.len() as u64 != size {
        return Err(malformed("binary patch size doesn't match its header"));
    }
    Ok((is_delta, inflated))
}

fn parse_binary_payload(
    cursor: &mut Cursor<'_>,
    budget: &mut Budget,
) -> Result<PatchPayload, PatchParseError> {
    cursor.next();
    let (is_delta, data) = parse_binary_block(cursor, budget)?;
    if cursor
        .peek()
        .is_some_and(|line| line.starts_with("literal ") || line.starts_with("delta "))
    {
        parse_binary_block(cursor, budget)?;
    }
    Ok(match is_delta {
        true => PatchPayload::BinaryDelta(data),
        false => PatchPayload::BinaryLiteral(data),
    })
}

#[derive(Default)]
struct FileHeaders {
    diff_old: Option<String>,
    diff_new: Option<String>,
    old_mode: Option<EntryKind>,
    new_mode: Option<EntryKind>,
    created: bool,
    deleted: bool,
    rename_from: Option<String>,
    rename_to: Option<String>,
    copy_from: Option<String>,
    copy_to: Option<String>,
    old_index: Option<Oid>,
    label_old: Option<Option<String>>,
    label_new: Option<Option<String>>,
}

fn parse_extended_headers(
    cursor: &mut Cursor<'_>,
    headers: &mut FileHeaders,
) -> Result<(), PatchParseError> {
    std::iter::from_fn(|| {
        let line = cursor.peek()?;
        let step: Option<Result<(), PatchParseError>> = if let Some(rest) =
            line.strip_prefix("old mode ")
        {
            headers.old_mode = EntryKind::from_git_mode(rest);
            Some(Ok(()))
        } else if let Some(rest) = line.strip_prefix("new mode ") {
            headers.new_mode = EntryKind::from_git_mode(rest);
            Some(Ok(()))
        } else if let Some(rest) = line.strip_prefix("new file mode ") {
            headers.created = true;
            headers.new_mode = EntryKind::from_git_mode(rest);
            Some(Ok(()))
        } else if let Some(rest) = line.strip_prefix("deleted file mode ") {
            headers.deleted = true;
            headers.old_mode = EntryKind::from_git_mode(rest);
            Some(Ok(()))
        } else if let Some(rest) = line.strip_prefix("rename from ") {
            Some(unquote(rest).map(|path| {
                headers.rename_from = Some(path);
            }))
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            Some(unquote(rest).map(|path| {
                headers.rename_to = Some(path);
            }))
        } else if let Some(rest) = line.strip_prefix("copy from ") {
            Some(unquote(rest).map(|path| {
                headers.copy_from = Some(path);
            }))
        } else if let Some(rest) = line.strip_prefix("copy to ") {
            Some(unquote(rest).map(|path| {
                headers.copy_to = Some(path);
            }))
        } else if line.starts_with("similarity index ") || line.starts_with("dissimilarity index ")
        {
            Some(Ok(()))
        } else if let Some(rest) = line.strip_prefix("index ") {
            let (oids, mode) = rest
                .split_once(' ')
                .map(|(oids, mode)| (oids, Some(mode)))
                .unwrap_or((rest, None));
            if let Some((old, _)) = oids.split_once("..") {
                headers.old_index = full_oid(old);
            }
            if let Some(kind) = mode.and_then(EntryKind::from_git_mode) {
                headers.old_mode = headers.old_mode.or(Some(kind));
                headers.new_mode = headers.new_mode.or(Some(kind));
            }
            Some(Ok(()))
        } else {
            None
        };
        step.inspect(|_| {
            cursor.next();
        })
    })
    .try_for_each(|outcome| outcome)
}

fn parse_labels_and_hunks(
    cursor: &mut Cursor<'_>,
    headers: &mut FileHeaders,
    budget: &mut Budget,
) -> Result<PatchPayload, PatchParseError> {
    let old_raw = cursor
        .take_prefix("--- ")
        .ok_or_else(|| malformed("expected --- label"))?;
    headers.label_old = Some(parse_label(old_raw)?);
    let new_raw = cursor
        .take_prefix("+++ ")
        .ok_or_else(|| malformed("expected +++ label"))?;
    headers.label_new = Some(parse_label(new_raw)?);
    Ok(PatchPayload::Text(parse_hunks(cursor, budget)?))
}

fn assemble(headers: FileHeaders, payload: PatchPayload) -> Result<ParsedFile, PatchParseError> {
    let FileHeaders {
        diff_old,
        diff_new,
        old_mode,
        new_mode,
        created,
        deleted,
        rename_from,
        rename_to,
        copy_from,
        copy_to,
        old_index,
        label_old,
        label_new,
    } = headers;
    let created = created || matches!(label_old, Some(None));
    let deleted = deleted || matches!(label_new, Some(None));
    let need = |path: Option<String>, what: &str| {
        path.ok_or_else(|| malformed(format!("file section is missing its {what} path")))
    };
    let (intent, path) = match (rename_from, rename_to, copy_from, copy_to) {
        (Some(from), to, _, _) => (FileIntent::Rename { from }, need(to, "rename target")?),
        (_, _, Some(from), to) => (FileIntent::Copy { from }, need(to, "copy target")?),
        _ if created => (
            FileIntent::Create,
            need(label_new.flatten().or(diff_new), "new")?,
        ),
        _ if deleted => (
            FileIntent::Delete,
            need(label_old.flatten().or(diff_old), "old")?,
        ),
        _ => (
            FileIntent::Modify,
            need(label_new.flatten().or(diff_new), "target")?,
        ),
    };
    Ok(ParsedFile {
        path,
        intent,
        old_kind: old_mode,
        new_kind: new_mode,
        old_index,
        payload,
    })
}

fn parse_git_file(
    cursor: &mut Cursor<'_>,
    budget: &mut Budget,
) -> Result<ParsedFile, PatchParseError> {
    let rest = cursor
        .take_prefix("diff --git ")
        .ok_or_else(|| malformed("expected diff --git header"))?;
    let mut headers = FileHeaders::default();
    if let Some((old, new)) = diff_paths(rest) {
        headers.diff_old = Some(old);
        headers.diff_new = Some(new);
    }
    parse_extended_headers(cursor, &mut headers)?;
    let payload = match cursor.peek() {
        Some(line) if line.starts_with("--- ") => {
            parse_labels_and_hunks(cursor, &mut headers, budget)?
        }
        Some("GIT binary patch") => parse_binary_payload(cursor, budget)?,
        Some(line) if line.starts_with("Binary files ") => {
            cursor.next();
            PatchPayload::BinaryOpaque
        }
        _ => PatchPayload::Text(Vec::new()),
    };
    assemble(headers, payload)
}

fn parse_traditional_file(
    cursor: &mut Cursor<'_>,
    budget: &mut Budget,
) -> Result<ParsedFile, PatchParseError> {
    let mut headers = FileHeaders::default();
    let payload = parse_labels_and_hunks(cursor, &mut headers, budget)?;
    assemble(headers, payload)
}

fn at_file_start(cursor: &Cursor<'_>) -> bool {
    match cursor.peek() {
        Some(line) if line.starts_with("diff --git ") => true,
        Some(line) if line.starts_with("--- ") => cursor
            .lines
            .get(cursor.pos + 1)
            .is_some_and(|next| next.starts_with("+++ ")),
        _ => false,
    }
}

fn skip_to_file_start(cursor: &mut Cursor<'_>) -> bool {
    std::iter::from_fn(|| {
        (!at_file_start(cursor) && cursor.peek().is_some()).then(|| cursor.next())
    })
    .for_each(|_| ());
    cursor.peek().is_some()
}

pub fn parse_patch(text: &str) -> Result<Vec<ParsedFile>, PatchParseError> {
    parse_patch_bounded(text, MAX_TOTAL_PATCH_BYTES)
}

pub fn parse_patch_bounded(text: &str, max_bytes: u64) -> Result<Vec<ParsedFile>, PatchParseError> {
    parse_patch_budgeted(text, &mut Budget::new(max_bytes))
}

fn parse_patch_budgeted(
    text: &str,
    budget: &mut Budget,
) -> Result<Vec<ParsedFile>, PatchParseError> {
    if text.trim().is_empty() {
        return Err(PatchParseError::Empty);
    }
    let lines: Vec<&str> = text.split('\n').collect();
    let mut cursor = Cursor::new(&lines);
    let files: Vec<ParsedFile> = std::iter::from_fn(|| {
        skip_to_file_start(&mut cursor).then(|| match cursor.peek() {
            Some(line) if line.starts_with("diff --git ") => parse_git_file(&mut cursor, budget),
            _ => parse_traditional_file(&mut cursor, budget),
        })
    })
    .collect::<Result<_, _>>()?;
    match files.is_empty() {
        true => Err(PatchParseError::NoFiles),
        false => Ok(files),
    }
}

fn is_mail_divider(line: &str) -> bool {
    line.strip_prefix("From ").is_some_and(|rest| {
        rest.len() > 40
            && rest.as_bytes()[..40]
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            && rest.as_bytes()[40] == b' '
    })
}

fn split_mail(text: &str) -> Vec<&str> {
    let starts: Vec<usize> = text
        .split_inclusive('\n')
        .scan(0usize, |offset, line| {
            let start = *offset;
            *offset += line.len();
            Some((start, line))
        })
        .filter(|(_, line)| is_mail_divider(line.trim_end_matches('\n')))
        .map(|(start, _)| start)
        .collect();
    match starts.is_empty() {
        true => vec![text],
        false => {
            let ends = starts
                .iter()
                .skip(1)
                .copied()
                .chain(std::iter::once(text.len()));
            starts
                .iter()
                .copied()
                .zip(ends)
                .map(|(start, end)| &text[start..end])
                .collect()
        }
    }
}

fn decode_q(bytes: &[u8]) -> Option<Vec<u8>> {
    let hex = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    let mut pos = 0usize;
    std::iter::from_fn(move || match bytes.get(pos..) {
        None | Some([]) => None,
        Some(slice) => {
            let decoded: Option<u8> = match slice {
                [b'_', ..] => apply(&mut pos, 1, b' '),
                [b'=', high, low, ..] => {
                    let byte = hex(*high).zip(hex(*low)).map(|(high, low)| high * 16 + low);
                    pos += 3;
                    byte
                }
                [b'=', ..] => {
                    pos += 1;
                    None
                }
                [byte, ..] => apply(&mut pos, 1, *byte),
                [] => None,
            };
            Some(decoded)
        }
    })
    .collect()
}

fn decode_rfc2047_word(word: &str) -> Option<String> {
    let inner = word.strip_prefix("=?")?.strip_suffix("?=")?;
    let (charset, rest) = inner.split_once('?')?;
    let (encoding, payload) = rest.split_once('?')?;
    if !charset.eq_ignore_ascii_case("utf-8") {
        return None;
    }
    let bytes = match encoding {
        "Q" | "q" => decode_q(payload.as_bytes())?,
        "B" | "b" => base64::engine::general_purpose::STANDARD
            .decode(payload)
            .ok()?,
        _ => return None,
    };
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

fn decode_rfc2047(value: &str) -> String {
    value
        .split(' ')
        .filter(|token| !token.is_empty())
        .map(|token| match decode_rfc2047_word(token) {
            Some(decoded) => (true, decoded),
            None => (false, token.to_string()),
        })
        .fold(
            (String::new(), false),
            |(mut acc, prev_encoded), (encoded, text)| {
                if !(acc.is_empty() || prev_encoded && encoded) {
                    acc.push(' ');
                }
                acc.push_str(&text);
                (acc, encoded)
            },
        )
        .0
}

fn strip_subject_prefix(subject: &str) -> String {
    let stripped = std::iter::successors(Some(subject.trim_start()), |current| {
        current
            .strip_prefix('[')
            .and_then(|rest| rest.split_once(']'))
            .map(|(_, tail)| tail.trim_start())
    })
    .last()
    .unwrap_or("");
    match stripped.starts_with('[') {
        true => stripped.to_string(),
        false => stripped.trim_end().to_string(),
    }
}

fn parse_address(raw: &str) -> (AuthorName, Email) {
    let decoded = decode_rfc2047(raw.trim());
    match decoded.rsplit_once('<') {
        Some((name, rest)) => {
            let email = rest.split('>').next().unwrap_or(rest).trim();
            let name = name.trim().trim_matches('"').trim();
            (AuthorName::new(name), Email::new(email))
        }
        None => {
            let bare = decoded.trim();
            (AuthorName::new(bare), Email::new(bare))
        }
    }
}

fn fold_headers(lines: &[&str]) -> Vec<(String, String)> {
    lines.iter().fold(Vec::new(), |mut acc, line| {
        match line.strip_prefix(' ').or_else(|| line.strip_prefix('\t')) {
            Some(continuation) => {
                if let Some(last) = acc.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(continuation.trim());
                }
            }
            None => {
                if let Some((name, value)) = line.split_once(':') {
                    acc.push((name.trim().to_string(), value.trim().to_string()));
                }
            }
        }
        acc
    })
}

fn parse_mail(chunk: &str, budget: &mut Budget) -> Result<MailPatch, PatchParseError> {
    let lines: Vec<&str> = chunk.split('\n').collect();
    let after_divider: &[&str] = match lines.split_first() {
        Some((first, rest)) if is_mail_divider(first) => rest,
        _ => &lines,
    };
    let header_end = after_divider
        .iter()
        .position(|line| line.trim().is_empty())
        .ok_or_else(|| malformed("mail patch has no header separator"))?;
    let headers = fold_headers(&after_divider[..header_end]);
    let header = |name: &str| {
        headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    let (author_name, author_email) = header("From")
        .map(|raw| parse_address(&raw))
        .ok_or_else(|| malformed("mail patch has no From header"))?;
    let rest = &after_divider[header_end + 1..];
    let body_end = rest
        .iter()
        .position(|line| line.trim_end() == "---" || line.starts_with("diff --git "))
        .unwrap_or(rest.len());
    let body = rest[..body_end].join("\n").trim().to_string();
    let files = parse_patch_budgeted(&rest[body_end..].join("\n"), budget)?;
    Ok(MailPatch {
        author_name,
        author_email,
        date: header("Date").unwrap_or_default(),
        subject: strip_subject_prefix(&decode_rfc2047(&header("Subject").unwrap_or_default())),
        body,
        change_id: header("Change-Id").and_then(|raw| CommitChangeId::new(raw).ok()),
        files,
    })
}

pub fn parse_mailbox(text: &str) -> Result<Vec<MailPatch>, PatchParseError> {
    parse_mailbox_bounded(text, MAX_TOTAL_PATCH_BYTES)
}

pub fn parse_mailbox_bounded(
    text: &str,
    max_bytes: u64,
) -> Result<Vec<MailPatch>, PatchParseError> {
    if text.trim().is_empty() {
        return Err(PatchParseError::Empty);
    }
    let mut budget = Budget::new(max_bytes);
    split_mail(text)
        .into_iter()
        .map(|chunk| parse_mail(chunk, &mut budget))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_patch_detection_matches_the_mailbox_heuristic() {
        assert!(is_format_patch(
            "From 0123456789012345678901234567890123456789 Mon Sep 17 00:00:00 2001\nFrom: nel <nel@oyster.cafe>\n"
        ));
        assert!(is_format_patch(
            "From: nel <nel@oyster.cafe>\nSubject: [PATCH] tide pool\n\n"
        ));
        assert!(!is_format_patch(
            "diff --git a/reef.txt b/reef.txt\n--- a/reef.txt\n+++ b/reef.txt\n"
        ));
        assert!(!is_format_patch(""));
    }

    #[test]
    fn a_simple_modification_parses() {
        let patch = "diff --git a/reef.txt b/reef.txt\nindex 1111111..2222222 100644\n--- a/reef.txt\n+++ b/reef.txt\n@@ -1,2 +1,2 @@\n-old line\n+new line\n context\n";
        let files = parse_patch(patch).unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].path, "reef.txt");
        assert_eq!(files[0].intent, FileIntent::Modify);
        let PatchPayload::Text(hunks) = &files[0].payload else {
            panic!("expected text payload");
        };
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].lines.len(), 3);
    }

    #[test]
    fn creations_deletions_and_renames_parse() {
        let patch = concat!(
            "diff --git a/new.txt b/new.txt\n",
            "new file mode 100644\n",
            "index 0000000..2222222\n",
            "--- /dev/null\n",
            "+++ b/new.txt\n",
            "@@ -0,0 +1 @@\n",
            "+hello\n",
            "diff --git a/gone.txt b/gone.txt\n",
            "deleted file mode 100755\n",
            "index 2222222..0000000\n",
            "--- a/gone.txt\n",
            "+++ /dev/null\n",
            "@@ -1 +0,0 @@\n",
            "-bye\n",
            "diff --git a/old.txt b/moved.txt\n",
            "similarity index 100%\n",
            "rename from old.txt\n",
            "rename to moved.txt\n",
        );
        let files = parse_patch(patch).unwrap();
        assert_eq!(files.len(), 3);
        assert_eq!(files[0].intent, FileIntent::Create);
        assert_eq!(files[0].new_kind, Some(EntryKind::Blob));
        assert_eq!(files[1].intent, FileIntent::Delete);
        assert_eq!(files[1].old_kind, Some(EntryKind::BlobExecutable));
        assert_eq!(
            files[2].intent,
            FileIntent::Rename {
                from: "old.txt".to_string()
            }
        );
        assert_eq!(files[2].path, "moved.txt");
    }

    #[test]
    fn the_no_newline_marker_strips_the_trailing_newline() {
        let patch = "diff --git a/reef.txt b/reef.txt\nindex 1111111..2222222 100644\n--- a/reef.txt\n+++ b/reef.txt\n@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n";
        let files = parse_patch(patch).unwrap();
        let PatchPayload::Text(hunks) = &files[0].payload else {
            panic!("expected text payload");
        };
        assert_eq!(hunks[0].lines[0].text, b"old\n".to_vec());
        assert_eq!(hunks[0].lines[1].text, b"new".to_vec());
    }

    #[test]
    fn quoted_paths_unescape() {
        assert_eq!(unquote("\"a/sp ace.txt\"").unwrap(), "a/sp ace.txt");
        assert_eq!(unquote("\"a/tab\\there\"").unwrap(), "a/tab\there");
        assert_eq!(unquote("\"a/\\303\\251\"").unwrap(), "a/é");
        assert!(unquote("\"a/broken").is_err());
    }

    #[test]
    fn a_plain_path_stays_bare_and_a_quoted_one_unquotes_back() {
        [
            "a/reef.txt",
            "a/~tilde",
            "a/{brace}",
            "a/sp ace.txt",
            "a/b/ b/c",
        ]
        .into_iter()
        .for_each(|plain| {
            assert_eq!(quote_path(plain), plain, "git leaves {plain} bare too");
        });
        [
            "a/quote\".txt",
            "a/back\\slash",
            "a/tab\there",
            "a/new\nline",
            "a/é",
            "a/\u{7f}del",
        ]
        .into_iter()
        .for_each(|awkward| {
            let quoted = quote_path(awkward);
            assert!(quoted.starts_with('"'), "{awkward} must come out quoted");
            assert_eq!(
                unquote(&quoted).unwrap(),
                awkward,
                "{awkward} must unquote back to itself"
            );
        });
    }

    #[test]
    fn diff_paths_splits_a_header_whose_path_holds_the_separator() {
        [
            ("a/b/ b/c.bin b/b/ b/c.bin", "b/ b/c.bin", "b/ b/c.bin"),
            (
                "\"a/quote\\\".bin\" \"b/quote\\\".bin\"",
                "quote\".bin",
                "quote\".bin",
            ),
            (
                "a/old name.txt b/new name.txt",
                "old name.txt",
                "new name.txt",
            ),
        ]
        .into_iter()
        .for_each(|(header, old, new)| {
            assert_eq!(
                diff_paths(header),
                Some((old.to_string(), new.to_string())),
                "the a-side and b-side must agree on the split: {header}"
            );
        });
    }

    #[test]
    fn a_binary_file_named_around_the_separator_keeps_its_path() {
        let patch = concat!(
            "diff --git a/b/ b/c.bin b/b/ b/c.bin\n",
            "index 1111111111111111111111111111111111111111..2222222222222222222222222222222222222222 100644\n",
            "GIT binary patch\n",
            "literal 4\n",
            "LcmZRms;UA20^k8}\n",
            "\n",
            "literal 5\n",
            "Mcmb<mNK8rw00gH2p8x;=\n",
            "\n",
        );
        assert_eq!(
            parse_patch(patch).unwrap()[0].path.as_str(),
            "b/ b/c.bin",
            "a binary file has no --- or +++ label, so the header is all the parser gets"
        );
    }

    #[test]
    fn an_index_line_yields_an_oid_at_either_hash_width() {
        [40usize, 64].into_iter().for_each(|width| {
            assert!(
                full_oid(&"a".repeat(width)).is_some(),
                "{width} hex digits is a full oid"
            );
        });
        assert_eq!(
            full_oid("1111111"),
            None,
            "7 hex digits is short of a full oid"
        );
    }

    #[test]
    fn a_mailbox_splits_into_individual_patches() {
        let mbox = concat!(
            "From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001\n",
            "From: nel <nel@oyster.cafe>\n",
            "Date: Tue, 5 Sep 2023 12:00:00 +0530\n",
            "Subject: [PATCH 1/2] first\n",
            "\n",
            "body text\n",
            "---\n",
            " reef.txt | 1 +\n",
            " 1 file changed, 1 insertion(+)\n",
            "\n",
            "diff --git a/reef.txt b/reef.txt\n",
            "new file mode 100644\n",
            "index 0000000..2222222\n",
            "--- /dev/null\n",
            "+++ b/reef.txt\n",
            "@@ -0,0 +1 @@\n",
            "+one\n",
            "-- \n2.43.0\n\n",
            "From 2222222222222222222222222222222222222222 Mon Sep 17 00:00:00 2001\n",
            "From: =?UTF-8?q?t=C3=A9q?= <teq@nel.pet>\n",
            "Date: Tue, 5 Sep 2023 13:00:00 +0530\n",
            "Subject: [PATCH 2/2] second\n",
            "Change-Id: I0123456789abcdef\n",
            "\n",
            "---\n",
            "diff --git a/reef.txt b/reef.txt\n",
            "index 2222222..3333333 100644\n",
            "--- a/reef.txt\n",
            "+++ b/reef.txt\n",
            "@@ -1 +1 @@\n",
            "-one\n",
            "+two\n",
        );
        let mails = parse_mailbox(mbox).unwrap();
        assert_eq!(mails.len(), 2);
        assert_eq!(mails[0].author_name.as_str(), "nel");
        assert_eq!(mails[0].author_email.as_str(), "nel@oyster.cafe");
        assert_eq!(mails[0].subject, "first");
        assert_eq!(mails[0].body, "body text");
        assert_eq!(mails[0].commit_message(), "first\n\nbody text");
        assert_eq!(mails[0].files.len(), 1);
        assert_eq!(mails[1].author_name.as_str(), "téq");
        assert_eq!(
            mails[1].change_id,
            Some(CommitChangeId::new("I0123456789abcdef").unwrap())
        );
        assert_eq!(mails[1].files[0].intent, FileIntent::Modify);
    }

    #[test]
    fn malformed_input_yields_typed_errors() {
        assert_eq!(parse_patch("  \n "), Err(PatchParseError::Empty));
        assert_eq!(parse_patch("hello world\n"), Err(PatchParseError::NoFiles));

        let patch = "diff --git a/r.txt b/r.txt\n--- a/r.txt\n+++ b/r.txt\n@@ -0,0 +1 @@\n+a line that is wider than four bytes\n";
        let mut tight = Budget { remaining: 4 };
        assert_eq!(
            parse_patch_budgeted(patch, &mut tight),
            Err(malformed("patch exceeds total decompressed size budget")),
        );
        let mut roomy = Budget { remaining: 1_000 };
        assert!(parse_patch_budgeted(patch, &mut roomy).is_ok());
    }

    #[test]
    fn pathological_depth_inputs_do_not_overflow_the_stack() {
        let huge = "a".repeat(1_000_000);
        assert_eq!(unquote(&format!("\"{huge}\"")).unwrap(), huge);
        let brackets = "[x]".repeat(500_000);
        assert_eq!(strip_subject_prefix(&brackets), "");
        let encoded = format!("=?utf-8?q?{}?=", "=41".repeat(400_000));
        assert_eq!(decode_rfc2047(&encoded), "A".repeat(400_000));
    }

    #[test]
    fn a_bare_email_from_header_becomes_both_name_and_email() {
        let mbox = concat!(
            "From 1111111111111111111111111111111111111111 Mon Sep 17 00:00:00 2001\n",
            "From: nel@oyster.cafe\n",
            "Date: Tue, 5 Sep 2023 12:00:00 +0000\n",
            "Subject: [PATCH] bare\n",
            "\n",
            "diff --git a/reef.txt b/reef.txt\n",
            "new file mode 100644\n",
            "--- /dev/null\n",
            "+++ b/reef.txt\n",
            "@@ -0,0 +1 @@\n",
            "+hi\n",
        );
        let mails = parse_mailbox(mbox).unwrap();
        assert_eq!(mails[0].author_name.as_str(), "nel@oyster.cafe");
        assert_eq!(mails[0].author_email.as_str(), "nel@oyster.cafe");
    }
}
