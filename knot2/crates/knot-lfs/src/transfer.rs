use std::io::{Read, Write};

use gix_packetline::PacketLineRef;
use gix_packetline::blocking_io::{StreamingPeekableIter, encode};
use knot_messages::{
    AlgorithmKey, CommandKey, DeclaredComputedKey, DeclaredLimitKey, DeclaredReceivedKey,
    DetailKey, FreeFloorKey, LfsMessages, OidKey, ValueKey, VersionKey, WhatLimitKey,
};
use knot_types::{HttpStatus, RepoDid};

use crate::store::for_each_chunk;
use crate::{
    BatchObject, ClaimedSize, LfsError, LfsOid, LfsSize, LfsStore, MAX_BATCH_OBJECTS,
    UploadAdmission,
};

pub const CAPABILITY_VERSION: &str = "version=1";
const PKT_DATA_MAX: usize = 65516;
const MAX_MESSAGE_ARGS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransferOp {
    Upload,
    Download,
}

impl TransferOp {
    pub fn parse(token: &str) -> Option<Self> {
        match token {
            "upload" => Some(Self::Upload),
            "download" => Some(Self::Download),
            _ => None,
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn serve_transfer(
    store: &dyn LfsStore,
    admission: &dyn UploadAdmission,
    repo: &RepoDid,
    op: TransferOp,
    messages: &LfsMessages,
    input: impl Read,
    mut output: impl Write,
) -> Result<(), LfsError> {
    write_text(&mut output, CAPABILITY_VERSION)?;
    write_flush(&mut output)?;
    let mut session = Session {
        store,
        admission,
        repo,
        op,
        messages,
        lines: StreamingPeekableIter::new(input, &[], false),
        out: output,
    };
    let mut done = false;
    std::iter::from_fn(|| {
        (!done).then(|| {
            session.step().map(|flow| match flow {
                Flow::Continue => (),
                Flow::Quit => done = true,
            })
        })
    })
    .try_for_each(std::convert::identity)
}

enum Flow {
    Continue,
    Quit,
}

enum Pkt {
    Data(Vec<u8>),
    Flush,
    Delim,
    Eof,
}

enum Ended {
    Flush,
    Delim,
}

fn next_pkt<R: Read>(lines: &mut StreamingPeekableIter<R>) -> Result<Pkt, LfsError> {
    match lines.read_line() {
        None => Ok(Pkt::Eof),
        Some(Err(source)) if source.kind() == std::io::ErrorKind::UnexpectedEof => Ok(Pkt::Eof),
        Some(Err(source)) => Err(LfsError::Channel { source }),
        Some(Ok(Err(fault))) => Err(LfsError::Framing {
            detail: fault.to_string(),
        }),
        Some(Ok(Ok(PacketLineRef::Data(payload)))) => Ok(Pkt::Data(payload.to_vec())),
        Some(Ok(Ok(PacketLineRef::Flush))) => Ok(Pkt::Flush),
        Some(Ok(Ok(PacketLineRef::Delimiter))) => Ok(Pkt::Delim),
        Some(Ok(Ok(PacketLineRef::ResponseEnd))) => Err(LfsError::Framing {
            detail: "unexpected response-end packet".to_string(),
        }),
    }
}

fn text_of(payload: Vec<u8>) -> Result<String, LfsError> {
    String::from_utf8(payload)
        .map(|line| line.trim_end_matches('\n').to_string())
        .map_err(|_| LfsError::Framing {
            detail: "non-utf8 text packet".to_string(),
        })
}

fn fault_text(fault: &LfsError, messages: &LfsMessages) -> String {
    match fault {
        LfsError::InvalidOid { value } => messages
            .invalid_oid
            .line(|ValueKey::Value| format!("{value:?}")),
        LfsError::HashMismatch { declared, computed } => {
            messages.hash_mismatch.line(|key| match key {
                DeclaredComputedKey::Declared => declared.to_string(),
                DeclaredComputedKey::Computed => computed.to_string(),
            })
        }
        LfsError::SizeMismatch { declared, received } => {
            messages.size_mismatch.line(|key| match key {
                DeclaredReceivedKey::Declared => declared.to_string(),
                DeclaredReceivedKey::Received => received.to_string(),
            })
        }
        LfsError::SizeLimitExceeded { declared, limit } => {
            messages.size_limit_exceeded.line(|key| match key {
                DeclaredLimitKey::Declared => declared.to_string(),
                DeclaredLimitKey::Limit => limit.to_string(),
            })
        }
        LfsError::FreeSpaceDenied { free, floor } => {
            messages.free_space_denied.line(|key| match key {
                FreeFloorKey::Free => free.to_string(),
                FreeFloorKey::Floor => floor.to_string(),
            })
        }
        LfsError::NotFound { oid } => messages.not_found.line(|OidKey::Oid| oid.to_string()),
        LfsError::Framing { detail } => messages.framing.line(|DetailKey::Detail| detail.clone()),
        LfsError::TooMany { what, limit } => messages.too_many.line(|key| match key {
            WhatLimitKey::What => what.to_string(),
            WhatLimitKey::Limit => limit.to_string(),
        }),
        other => other.to_string(),
    }
}

fn status_of(fault: &LfsError) -> HttpStatus {
    HttpStatus::new(match fault {
        LfsError::NotFound { .. } => 404,
        LfsError::InvalidOid { .. }
        | LfsError::HashMismatch { .. }
        | LfsError::SizeMismatch { .. }
        | LfsError::Framing { .. } => 400,
        LfsError::SizeLimitExceeded { .. } => 413,
        LfsError::FreeSpaceDenied { .. } => 429,
        _ => 500,
    })
}

fn write_text(out: &mut impl Write, line: &str) -> Result<(), LfsError> {
    encode::data_to_write(format!("{line}\n").as_bytes(), &mut *out)
        .map(|_| ())
        .map_err(|source| LfsError::Channel { source })
}

fn write_flush(out: &mut impl Write) -> Result<(), LfsError> {
    encode::flush_to_write(&mut *out)
        .and_then(|_| out.flush().map(|()| 0))
        .map(|_| ())
        .map_err(|source| LfsError::Channel { source })
}

fn write_delim(out: &mut impl Write) -> Result<(), LfsError> {
    encode::delim_to_write(&mut *out)
        .map(|_| ())
        .map_err(|source| LfsError::Channel { source })
}

fn arg_value<'a>(args: &'a [String], key: &str) -> Option<&'a str> {
    args.iter().find_map(|arg| {
        arg.strip_prefix(key)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

struct Session<'a, R: Read, W: Write> {
    store: &'a dyn LfsStore,
    admission: &'a dyn UploadAdmission,
    repo: &'a RepoDid,
    op: TransferOp,
    messages: &'a LfsMessages,
    lines: StreamingPeekableIter<R>,
    out: W,
}

impl<R: Read, W: Write> Session<'_, R, W> {
    fn step(&mut self) -> Result<Flow, LfsError> {
        match next_pkt(&mut self.lines)? {
            Pkt::Eof => Ok(Flow::Quit),
            Pkt::Flush | Pkt::Delim => Err(LfsError::Framing {
                detail: "expected a command packet".to_string(),
            }),
            Pkt::Data(payload) => self.dispatch(text_of(payload)?),
        }
    }

    fn dispatch(&mut self, command: String) -> Result<Flow, LfsError> {
        let (verb, rest) = command
            .split_once(' ')
            .map_or((command.as_str(), ""), |(verb, rest)| (verb, rest));
        match verb {
            "version" => self.handle_version(rest),
            "batch" => self.handle_batch(),
            "put-object" => self.handle_put(rest),
            "verify-object" => self.handle_verify(rest),
            "get-object" => self.handle_get(rest),
            "quit" => {
                self.drain_message()?;
                self.respond_ok(&[])?;
                Ok(Flow::Quit)
            }
            _ => {
                self.drain_message()?;
                let message = self
                    .messages
                    .unknown_command
                    .line(|CommandKey::Command| format!("{verb:?}"));
                self.respond_error(HttpStatus::new(400), &message)?;
                Ok(Flow::Continue)
            }
        }
    }

    fn read_args(&mut self) -> Result<(Vec<String>, Ended), LfsError> {
        let mut args = Vec::new();
        std::iter::from_fn(|| Some(next_pkt(&mut self.lines)))
            .find_map(|pkt| match pkt {
                Err(fault) => Some(Err(fault)),
                Ok(Pkt::Data(payload)) => match text_of(payload) {
                    Ok(_) if args.len() >= MAX_MESSAGE_ARGS => Some(Err(LfsError::TooMany {
                        what: "arguments",
                        limit: MAX_MESSAGE_ARGS,
                    })),
                    Ok(line) => {
                        args.push(line);
                        None
                    }
                    Err(fault) => Some(Err(fault)),
                },
                Ok(Pkt::Flush) => Some(Ok(Ended::Flush)),
                Ok(Pkt::Delim) => Some(Ok(Ended::Delim)),
                Ok(Pkt::Eof) => Some(Err(LfsError::Framing {
                    detail: "message truncated before flush".to_string(),
                })),
            })
            .expect("an endless packet iterator always yields a terminator")
            .map(|ended| (args, ended))
    }

    fn drain_budget(&self) -> LfsSize {
        self.admission.max_object()
    }

    fn drain_to_flush(&mut self, limit: LfsSize) -> Result<(), LfsError> {
        let mut discarded = LfsSize::new(0);
        std::iter::from_fn(|| Some(next_pkt(&mut self.lines)))
            .find_map(|pkt| match pkt {
                Err(fault) => Some(Err(fault)),
                Ok(Pkt::Flush) => Some(Ok(())),
                Ok(Pkt::Eof) => Some(Err(LfsError::Framing {
                    detail: "message truncated before flush".to_string(),
                })),
                Ok(Pkt::Data(payload)) => {
                    discarded = discarded.saturating_add(LfsSize::new(payload.len() as u64));
                    match discarded > limit {
                        true => Some(Err(LfsError::Framing {
                            detail: "message body exceeds the drain bound".to_string(),
                        })),
                        false => None,
                    }
                }
                Ok(Pkt::Delim) => None,
            })
            .expect("an endless packet iterator always yields a terminator")
    }

    fn drain_message(&mut self) -> Result<(), LfsError> {
        match self.read_args()?.1 {
            Ended::Flush => Ok(()),
            Ended::Delim => self.drain_to_flush(self.drain_budget()),
        }
    }

    fn respond_ok(&mut self, args: &[String]) -> Result<(), LfsError> {
        write_text(&mut self.out, "status 200")?;
        args.iter()
            .try_for_each(|arg| write_text(&mut self.out, arg))?;
        write_flush(&mut self.out)
    }

    fn respond_error(&mut self, code: HttpStatus, message: &str) -> Result<(), LfsError> {
        write_text(&mut self.out, &format!("status {:03}", code.get()))?;
        write_delim(&mut self.out)?;
        write_text(&mut self.out, &format!("error: {message}"))?;
        write_flush(&mut self.out)
    }

    fn respond_fault(&mut self, fault: &LfsError) -> Result<(), LfsError> {
        let message = match fault {
            LfsError::Io { .. } => {
                tracing::warn!(repo = self.repo.as_str(), %fault, "lfs store fault");
                "internal storage fault".to_string()
            }
            other => fault_text(other, self.messages),
        };
        self.respond_error(status_of(fault), &message)
    }

    fn handle_version(&mut self, rest: &str) -> Result<Flow, LfsError> {
        self.drain_message()?;
        match rest.trim() {
            "1" => self.respond_ok(&[])?,
            other => {
                let message = self
                    .messages
                    .unsupported_version
                    .line(|VersionKey::Version| format!("{other:?}"));
                self.respond_error(HttpStatus::new(400), &message)?;
            }
        }
        Ok(Flow::Continue)
    }

    fn handle_batch(&mut self) -> Result<Flow, LfsError> {
        let (args, ended) = self.read_args()?;
        if let Some(algo) = arg_value(&args, "hash-algo")
            && algo != crate::HASH_ALGO
        {
            if matches!(ended, Ended::Delim) {
                self.drain_to_flush(self.drain_budget())?;
            }
            let message = self
                .messages
                .unsupported_hash
                .line(|AlgorithmKey::Algorithm| format!("{algo:?}"));
            self.respond_error(HttpStatus::new(400), &message)?;
            return Ok(Flow::Continue);
        }
        let items = match ended {
            Ended::Flush => Ok(Vec::new()),
            Ended::Delim => self.read_batch_items(),
        };
        let items = match items {
            Ok(items) => items,
            Err(fault @ (LfsError::InvalidOid { .. } | LfsError::Framing { .. })) => {
                self.drain_to_flush(self.drain_budget())?;
                self.respond_fault(&fault)?;
                return Ok(Flow::Continue);
            }
            Err(fault) => return Err(fault),
        };
        let lines: Result<Vec<String>, LfsError> =
            items.iter().map(|item| self.batch_line(item)).collect();
        match lines {
            Ok(lines) => {
                write_text(&mut self.out, "status 200")?;
                write_text(&mut self.out, &format!("hash-algo={}", crate::HASH_ALGO))?;
                write_delim(&mut self.out)?;
                lines
                    .iter()
                    .try_for_each(|line| write_text(&mut self.out, line))?;
                write_flush(&mut self.out)?;
            }
            Err(fault) => self.respond_fault(&fault)?,
        }
        Ok(Flow::Continue)
    }

    fn read_batch_items(&mut self) -> Result<Vec<BatchObject>, LfsError> {
        let mut items = Vec::new();
        std::iter::from_fn(|| Some(next_pkt(&mut self.lines)))
            .find_map(|pkt| match pkt {
                Err(fault) => Some(Err(fault)),
                Ok(Pkt::Data(payload)) => match text_of(payload).and_then(parse_batch_item) {
                    Ok(_) if items.len() >= MAX_BATCH_OBJECTS => Some(Err(LfsError::TooMany {
                        what: "batch items",
                        limit: MAX_BATCH_OBJECTS,
                    })),
                    Ok(item) => {
                        items.push(item);
                        None
                    }
                    Err(fault) => Some(Err(fault)),
                },
                Ok(Pkt::Flush) => Some(Ok(())),
                Ok(Pkt::Delim | Pkt::Eof) => Some(Err(LfsError::Framing {
                    detail: "batch items truncated before flush".to_string(),
                })),
            })
            .expect("an endless packet iterator always yields a terminator")
            .map(|()| items)
    }

    fn batch_line(&self, item: &BatchObject) -> Result<String, LfsError> {
        let stored = match self.op {
            TransferOp::Upload => self.store.touch(self.repo, &item.oid)?,
            TransferOp::Download => self.store.probe(self.repo, &item.oid)?,
        };
        let (size, action) = match (self.op, stored) {
            (TransferOp::Upload, Some(_)) => (item.size.get(), "noop"),
            (TransferOp::Upload, None) => (item.size.get(), "upload"),
            (TransferOp::Download, Some(actual)) => (actual.get(), "download"),
            (TransferOp::Download, None) => (item.size.get(), "download"),
        };
        Ok(format!("{} {} {}", item.oid, size, action))
    }

    fn handle_put(&mut self, rest: &str) -> Result<Flow, LfsError> {
        if self.op != TransferOp::Upload {
            self.drain_message()?;
            self.respond_error(HttpStatus::new(403), &self.messages.put_on_download.text())?;
            return Ok(Flow::Continue);
        }
        let (args, ended) = self.read_args()?;
        let checked = LfsOid::new(rest).and_then(|oid| {
            let declared = arg_value(&args, "size")
                .and_then(|value| value.parse().ok())
                .map(ClaimedSize::new)
                .ok_or_else(|| LfsError::Framing {
                    detail: "put-object requires a size argument".to_string(),
                })?;
            let permit = self.admission.admit(declared)?;
            Ok((oid, declared, permit))
        });
        let (oid, declared, permit) = match checked {
            Ok(admitted) => admitted,
            Err(fault) => {
                if matches!(ended, Ended::Delim) {
                    self.drain_to_flush(self.drain_budget())?;
                }
                self.respond_fault(&fault)?;
                return Ok(Flow::Continue);
            }
        };
        if matches!(ended, Ended::Flush) {
            self.respond_error(HttpStatus::new(400), &self.messages.put_no_body.text())?;
            return Ok(Flow::Continue);
        }
        let mut body = PktBody {
            lines: &mut self.lines,
            buffer: Vec::new(),
            offset: 0,
            done: false,
        };
        let stored = self.store.put(self.repo, &oid, declared, &mut body);
        drop(permit);
        let synced = body.done;
        if !synced {
            self.drain_to_flush(self.drain_budget())?;
        }
        match stored {
            Ok(()) => {
                tracing::info!(
                    repo = self.repo.as_str(),
                    oid = oid.as_str(),
                    size = declared.get(),
                    "lfs object received over ssh"
                );
                self.respond_ok(&[])?
            }
            Err(fault) => self.respond_fault(&fault)?,
        }
        Ok(Flow::Continue)
    }

    fn handle_verify(&mut self, rest: &str) -> Result<Flow, LfsError> {
        if self.op != TransferOp::Upload {
            self.drain_message()?;
            self.respond_error(
                HttpStatus::new(403),
                &self.messages.verify_on_download.text(),
            )?;
            return Ok(Flow::Continue);
        }
        let (args, ended) = self.read_args()?;
        if matches!(ended, Ended::Delim) {
            self.drain_to_flush(self.drain_budget())?;
        }
        let declared = arg_value(&args, "size")
            .and_then(|value| value.parse().ok())
            .map(ClaimedSize::new);
        let verdict = LfsOid::new(rest).and_then(|oid| {
            match (self.store.touch(self.repo, &oid)?, declared) {
                (None, _) => Err(LfsError::NotFound { oid }),
                (Some(actual), Some(declared)) if !declared.matches(actual) => {
                    Err(LfsError::SizeMismatch {
                        declared,
                        received: actual,
                    })
                }
                (Some(_), _) => Ok(()),
            }
        });
        match verdict {
            Ok(()) => self.respond_ok(&[])?,
            Err(fault) => self.respond_fault(&fault)?,
        }
        Ok(Flow::Continue)
    }

    fn handle_get(&mut self, rest: &str) -> Result<Flow, LfsError> {
        if self.op != TransferOp::Download {
            self.drain_message()?;
            self.respond_error(HttpStatus::new(403), &self.messages.get_on_upload.text())?;
            return Ok(Flow::Continue);
        }
        self.drain_message()?;
        let opened = LfsOid::new(rest).and_then(|oid| {
            let size = self
                .store
                .probe(self.repo, &oid)?
                .ok_or(LfsError::NotFound { oid: oid.clone() })?;
            let body = self.store.read(self.repo, &oid)?;
            Ok((size, body))
        });
        let (size, mut body) = match opened {
            Ok(found) => found,
            Err(fault) => {
                self.respond_fault(&fault)?;
                return Ok(Flow::Continue);
            }
        };
        write_text(&mut self.out, "status 200")?;
        write_text(&mut self.out, &format!("size={size}"))?;
        write_delim(&mut self.out)?;
        let out = &mut self.out;
        for_each_chunk(&mut body, |chunk| {
            chunk.chunks(PKT_DATA_MAX).try_for_each(|piece| {
                encode::data_to_write(piece, &mut *out)
                    .map(|_| ())
                    .map_err(|source| LfsError::Channel { source })
            })
        })?;
        write_flush(&mut self.out)?;
        tracing::info!(
            repo = self.repo.as_str(),
            oid = rest,
            size = size.get(),
            "lfs object served over ssh"
        );
        Ok(Flow::Continue)
    }
}

fn parse_batch_item(line: String) -> Result<BatchObject, LfsError> {
    let mut tokens = line.split(' ');
    let oid = LfsOid::new(tokens.next().unwrap_or_default())?;
    let size = tokens
        .next()
        .and_then(|token| token.parse().ok())
        .map(ClaimedSize::new)
        .ok_or_else(|| LfsError::Framing {
            detail: format!("malformed batch item {line:?}"),
        })?;
    Ok(BatchObject { oid, size })
}

struct PktBody<'a, R: Read> {
    lines: &'a mut StreamingPeekableIter<R>,
    buffer: Vec<u8>,
    offset: usize,
    done: bool,
}

impl<R: Read> Read for PktBody<'_, R> {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.done {
            return Ok(0);
        }
        if self.offset >= self.buffer.len() {
            match next_pkt(self.lines).map_err(std::io::Error::other)? {
                Pkt::Data(payload) => {
                    self.buffer = payload;
                    self.offset = 0;
                }
                Pkt::Flush => {
                    self.done = true;
                    return Ok(0);
                }
                Pkt::Delim => {
                    return Err(std::io::Error::other("unexpected delimiter in object body"));
                }
                Pkt::Eof => return Err(std::io::Error::other("object body truncated")),
            }
        }
        let take = out.len().min(self.buffer.len() - self.offset);
        out[..take].copy_from_slice(&self.buffer[self.offset..self.offset + take]);
        self.offset += take;
        Ok(take)
    }
}

#[cfg(test)]
mod tests {
    use sha2::{Digest, Sha256};

    use super::*;
    use crate::admission::Unbounded;
    use crate::{FreeSpaceFloor, MemoryStore};

    fn oid_of(bytes: &[u8]) -> LfsOid {
        LfsOid::from_digest(Sha256::digest(bytes).into())
    }

    fn repo() -> RepoDid {
        RepoDid::new("did:plc:squid").unwrap()
    }

    fn put_text(buf: &mut Vec<u8>, line: &str) {
        encode::data_to_write(format!("{line}\n").as_bytes(), &mut *buf).unwrap();
    }

    fn msg(buf: &mut Vec<u8>, command: &str, args: &[&str]) {
        put_text(buf, command);
        args.iter().for_each(|arg| put_text(buf, arg));
        encode::flush_to_write(&mut *buf).unwrap();
    }

    fn msg_lines(buf: &mut Vec<u8>, command: &str, args: &[&str], lines: &[String]) {
        put_text(buf, command);
        args.iter().for_each(|arg| put_text(buf, arg));
        encode::delim_to_write(&mut *buf).unwrap();
        lines.iter().for_each(|line| put_text(buf, line));
        encode::flush_to_write(&mut *buf).unwrap();
    }

    fn msg_data(buf: &mut Vec<u8>, command: &str, args: &[&str], data: &[u8]) {
        put_text(buf, command);
        args.iter().for_each(|arg| put_text(buf, arg));
        encode::delim_to_write(&mut *buf).unwrap();
        data.chunks(PKT_DATA_MAX).for_each(|chunk| {
            encode::data_to_write(chunk, &mut *buf).unwrap();
        });
        encode::flush_to_write(&mut *buf).unwrap();
    }

    #[derive(Debug, PartialEq, Eq, Clone)]
    enum Out {
        Line(String),
        Bin(Vec<u8>),
        Delim,
        Flush,
    }

    fn parse_out(bytes: &[u8]) -> Vec<Out> {
        let mut lines = StreamingPeekableIter::new(bytes, &[], false);
        std::iter::from_fn(|| {
            lines.read_line().and_then(|pkt| {
                if matches!(&pkt, Err(fault) if fault.kind() == std::io::ErrorKind::UnexpectedEof) {
                    return None;
                }
                Some(
                    match pkt.expect("readable output").expect("well-formed output") {
                        PacketLineRef::Data(payload) => std::str::from_utf8(payload)
                            .ok()
                            .filter(|text| text.ends_with('\n'))
                            .map(|text| Out::Line(text.trim_end_matches('\n').to_string()))
                            .unwrap_or_else(|| Out::Bin(payload.to_vec())),
                        PacketLineRef::Flush => Out::Flush,
                        PacketLineRef::Delimiter => Out::Delim,
                        PacketLineRef::ResponseEnd => panic!("server never writes response-end"),
                    },
                )
            })
        })
        .collect()
    }

    fn line(text: &str) -> Out {
        Out::Line(text.to_string())
    }

    fn run(op: TransferOp, store: &dyn LfsStore, script: &[u8]) -> Vec<Out> {
        let mut output = Vec::new();
        serve_transfer(
            store,
            &Unbounded,
            &repo(),
            op,
            &knot_messages::default_catalog().lfs,
            script,
            &mut output,
        )
        .unwrap();
        parse_out(&output)
    }

    fn responses(out: &[Out]) -> Vec<Vec<Out>> {
        out.split_inclusive(|item| *item == Out::Flush)
            .map(<[Out]>::to_vec)
            .collect()
    }

    #[test]
    fn an_upload_session_lands_and_verifies_an_object() {
        let store = MemoryStore::new();
        let seeded: &[u8] = b"already on the server";
        let seeded_oid = oid_of(seeded);
        store
            .put(
                &repo(),
                &seeded_oid,
                ClaimedSize::new(seeded.len() as u64),
                &mut &seeded[..],
            )
            .unwrap();

        let fresh: &[u8] = b"\xffnew binary media\x00with raw bytes";
        let fresh_oid = oid_of(fresh);
        let fresh_len = fresh.len();

        let mut script = Vec::new();
        msg(&mut script, "version 1", &[]);
        msg_lines(
            &mut script,
            "batch",
            &[
                "transfer=ssh",
                "hash-algo=sha256",
                "refname=refs/heads/main",
            ],
            &[
                format!("{seeded_oid} {} extra=ignored", seeded.len()),
                format!("{fresh_oid} {fresh_len}"),
            ],
        );
        msg_data(
            &mut script,
            &format!("put-object {fresh_oid}"),
            &[&format!("size={fresh_len}")],
            fresh,
        );
        msg(
            &mut script,
            &format!("verify-object {fresh_oid}"),
            &[&format!("size={fresh_len}")],
        );
        msg(&mut script, "quit", &[]);

        let out = run(TransferOp::Upload, &store, &script);
        let turns = responses(&out);
        assert_eq!(turns[0], vec![line(CAPABILITY_VERSION), Out::Flush]);
        assert_eq!(turns[1], vec![line("status 200"), Out::Flush]);
        assert_eq!(
            turns[2],
            vec![
                line("status 200"),
                line("hash-algo=sha256"),
                Out::Delim,
                line(&format!("{seeded_oid} {} noop", seeded.len())),
                line(&format!("{fresh_oid} {fresh_len} upload")),
                Out::Flush,
            ]
        );
        assert_eq!(turns[3], vec![line("status 200"), Out::Flush]);
        assert_eq!(turns[4], vec![line("status 200"), Out::Flush]);
        assert_eq!(turns[5], vec![line("status 200"), Out::Flush]);
        assert_eq!(
            store.probe(&repo(), &fresh_oid).unwrap(),
            Some(LfsSize::new(fresh_len as u64))
        );
    }

    #[test]
    fn a_download_session_streams_bytes_and_404s_the_missing() {
        let store = MemoryStore::new();
        let media: &[u8] = b"\xff\x00streamable media";
        let media_oid = oid_of(media);
        let absent = oid_of(b"never uploaded");
        store
            .put(
                &repo(),
                &media_oid,
                ClaimedSize::new(media.len() as u64),
                &mut &media[..],
            )
            .unwrap();

        let mut script = Vec::new();
        msg_lines(
            &mut script,
            "batch",
            &["transfer=ssh", "hash-algo=sha256"],
            &[format!("{media_oid} 1"), format!("{absent} 9")],
        );
        msg(&mut script, &format!("get-object {media_oid}"), &[]);
        msg(&mut script, &format!("get-object {absent}"), &[]);
        msg(&mut script, "quit", &[]);

        let out = run(TransferOp::Download, &store, &script);
        let turns = responses(&out);
        assert_eq!(
            turns[1],
            vec![
                line("status 200"),
                line("hash-algo=sha256"),
                Out::Delim,
                line(&format!("{media_oid} {} download", media.len())),
                line(&format!("{absent} 9 download")),
                Out::Flush,
            ]
        );
        assert_eq!(
            turns[2],
            vec![
                line("status 200"),
                line(&format!("size={}", media.len())),
                Out::Delim,
                Out::Bin(media.to_vec()),
                Out::Flush,
            ]
        );
        assert_eq!(turns[3][0], line("status 404"));
        assert_eq!(turns[4], vec![line("status 200"), Out::Flush]);
    }

    #[test]
    fn the_channel_mode_gates_every_write_and_read_verb() {
        let store = MemoryStore::new();
        let oid = oid_of(b"whatever");

        let mut download_script = Vec::new();
        msg_data(
            &mut download_script,
            &format!("put-object {oid}"),
            &["size=3"],
            b"abc",
        );
        msg(
            &mut download_script,
            &format!("verify-object {oid}"),
            &["size=3"],
        );
        let out = run(TransferOp::Download, &store, &download_script);
        let turns = responses(&out);
        assert_eq!(turns[1][0], line("status 403"));
        assert_eq!(turns[2][0], line("status 403"));
        assert_eq!(store.probe(&repo(), &oid).unwrap(), None);

        let mut upload_script = Vec::new();
        msg(&mut upload_script, &format!("get-object {oid}"), &[]);
        let out = run(TransferOp::Upload, &store, &upload_script);
        assert_eq!(responses(&out)[1][0], line("status 403"));
    }

    #[test]
    fn admission_rejections_surface_as_typed_statuses() {
        struct Deny(LfsSize);
        impl UploadAdmission for Deny {
            fn admit(&self, declared: ClaimedSize) -> Result<crate::UploadPermit, LfsError> {
                match declared.get() > self.0.get() {
                    true => Err(LfsError::SizeLimitExceeded {
                        declared,
                        limit: self.0,
                    }),
                    false => Err(LfsError::FreeSpaceDenied {
                        free: LfsSize::new(1),
                        floor: FreeSpaceFloor::new(2),
                    }),
                }
            }

            fn max_object(&self) -> LfsSize {
                self.0
            }
        }
        let store = MemoryStore::new();
        let body: &[u8] = b"denied";
        let oid = oid_of(body);
        let mut script = Vec::new();
        msg_data(
            &mut script,
            &format!("put-object {oid}"),
            &["size=999"],
            body,
        );
        msg_data(&mut script, &format!("put-object {oid}"), &["size=6"], body);
        let mut output = Vec::new();
        serve_transfer(
            &store,
            &Deny(LfsSize::new(10)),
            &repo(),
            TransferOp::Upload,
            &knot_messages::default_catalog().lfs,
            &script[..],
            &mut output,
        )
        .unwrap();
        let turns = responses(&parse_out(&output));
        assert_eq!(turns[1][0], line("status 413"));
        assert_eq!(turns[2][0], line("status 429"));
        assert_eq!(store.probe(&repo(), &oid).unwrap(), None);
    }

    #[test]
    fn tampered_and_truncated_uploads_fail_closed_and_the_session_survives() {
        let store = MemoryStore::new();
        let body: &[u8] = b"the true bytes";
        let liar = oid_of(b"some other bytes");
        let valid = oid_of(body);

        let mut script = Vec::new();
        msg_data(
            &mut script,
            &format!("put-object {liar}"),
            &[&format!("size={}", body.len())],
            body,
        );
        msg_data(
            &mut script,
            &format!("put-object {valid}"),
            &[&format!("size={}", body.len() + 5)],
            body,
        );
        msg(
            &mut script,
            &format!("verify-object {valid}"),
            &[&format!("size={}", body.len())],
        );
        msg_data(
            &mut script,
            &format!("put-object {valid}"),
            &[&format!("size={}", body.len())],
            body,
        );
        msg(&mut script, "quit", &[]);

        let out = run(TransferOp::Upload, &store, &script);
        let turns = responses(&out);
        assert_eq!(turns[1][0], line("status 400"), "hash mismatch");
        assert_eq!(turns[2][0], line("status 400"), "size mismatch");
        assert_eq!(turns[3][0], line("status 404"), "nothing landed");
        assert_eq!(
            turns[4][0],
            line("status 200"),
            "retry with matching bytes succeeds"
        );
        assert_eq!(
            store.probe(&repo(), &valid).unwrap(),
            Some(LfsSize::new(body.len() as u64))
        );
        assert_eq!(store.probe(&repo(), &liar).unwrap(), None);
    }

    #[test]
    fn hostile_commands_get_clean_rejections_and_never_a_panic() {
        let store = MemoryStore::new();
        let mut script = Vec::new();
        msg(&mut script, "version 9", &[]);
        msg(&mut script, "steal-the-objects now", &[]);
        msg_lines(
            &mut script,
            "batch",
            &["hash-algo=sha256"],
            &["../../../etc/passwd0000000000000000000000000000000000000000 5".to_string()],
        );
        msg_lines(
            &mut script,
            "batch",
            &["hash-algo=sha1"],
            &[format!("{} 5", oid_of(b"x"))],
        );
        msg(&mut script, "quit", &[]);

        let out = run(TransferOp::Upload, &store, &script);
        let turns = responses(&out);
        assert_eq!(turns[1][0], line("status 400"), "unsupported version");
        assert_eq!(turns[2][0], line("status 400"), "unknown command");
        assert_eq!(turns[3][0], line("status 400"), "traversal oid");
        assert_eq!(turns[4][0], line("status 400"), "foreign hash algo");
        assert_eq!(turns[5][0], line("status 200"), "quit still answers");
    }

    #[test]
    fn floods_kill_the_session_instead_of_accumulating() {
        let store = MemoryStore::new();
        let flooded_batch: Vec<String> = (0..MAX_BATCH_OBJECTS + 1)
            .map(|index| format!("{} 1", oid_of(index.to_string().as_bytes())))
            .collect();
        let mut script = Vec::new();
        msg_lines(&mut script, "batch", &["hash-algo=sha256"], &flooded_batch);
        let mut output = Vec::new();
        let verdict = serve_transfer(
            &store,
            &Unbounded,
            &repo(),
            TransferOp::Upload,
            &knot_messages::default_catalog().lfs,
            &script[..],
            &mut output,
        );
        assert!(matches!(
            verdict,
            Err(LfsError::TooMany {
                what: "batch items",
                ..
            })
        ));

        let flooded_args: Vec<&str> = std::iter::repeat_n("size=1", MAX_MESSAGE_ARGS + 1).collect();
        let mut script = Vec::new();
        msg(
            &mut script,
            &format!("put-object {}", oid_of(b"flooded")),
            &flooded_args,
        );
        let mut output = Vec::new();
        let verdict = serve_transfer(
            &store,
            &Unbounded,
            &repo(),
            TransferOp::Upload,
            &knot_messages::default_catalog().lfs,
            &script[..],
            &mut output,
        );
        assert!(matches!(
            verdict,
            Err(LfsError::TooMany {
                what: "arguments",
                ..
            })
        ));
    }

    #[test]
    fn malformed_and_oversized_put_streams_end_the_session() {
        let store = MemoryStore::new();

        let mut sink = Vec::new();
        let garbage = serve_transfer(
            &store,
            &Unbounded,
            &repo(),
            TransferOp::Upload,
            &knot_messages::default_catalog().lfs,
            &b"zzzz not pkt-line at all"[..],
            &mut sink,
        );
        assert!(
            matches!(garbage, Err(LfsError::Framing { .. })),
            "raw garbage on the wire is a framing fault, not a hang"
        );

        let admission = crate::StoreAdmission::new(
            crate::LfsStorePath::new("/"),
            LfsSize::new(10),
            FreeSpaceFloor::new(0),
        );
        let oid = oid_of(b"whatever");
        let mut script = Vec::new();
        put_text(&mut script, &format!("put-object {oid}"));
        put_text(&mut script, "size=1");
        encode::delim_to_write(&mut script).unwrap();
        (0..3).for_each(|_| {
            encode::data_to_write(&[0u8; 100][..], &mut script).unwrap();
        });
        encode::flush_to_write(&mut script).unwrap();
        let mut output = Vec::new();
        let overshoot = serve_transfer(
            &store,
            &admission,
            &repo(),
            TransferOp::Upload,
            &knot_messages::default_catalog().lfs,
            &script[..],
            &mut output,
        );
        assert!(
            matches!(overshoot, Err(LfsError::Framing { .. })),
            "a body overshooting the object size limit ends the session instead of draining unbounded bytes"
        );
        assert_eq!(store.probe(&repo(), &oid).unwrap(), None);
    }

    #[test]
    fn store_faults_reach_the_client_without_the_path() {
        struct BrokenStore;
        impl LfsStore for BrokenStore {
            fn put(
                &self,
                _repo: &RepoDid,
                _oid: &LfsOid,
                _size: ClaimedSize,
                _body: &mut dyn std::io::Read,
            ) -> Result<(), LfsError> {
                // for now!!!!
                unreachable!("this session never puts")
            }

            fn read(
                &self,
                _repo: &RepoDid,
                _oid: &LfsOid,
            ) -> Result<Box<dyn std::io::Read + Send>, LfsError> {
                unreachable!("this session never reads")
            }

            fn probe(&self, _repo: &RepoDid, _oid: &LfsOid) -> Result<Option<LfsSize>, LfsError> {
                Err(LfsError::Io {
                    op: "stat",
                    path: "/srv/secret-lfs-root/plc/sq/uid".into(),
                    source: std::io::Error::other("disk fell off"),
                })
            }

            fn touch(&self, repo: &RepoDid, oid: &LfsOid) -> Result<Option<LfsSize>, LfsError> {
                self.probe(repo, oid)
            }
        }

        let mut script = Vec::new();
        msg_lines(
            &mut script,
            "batch",
            &["hash-algo=sha256"],
            &[format!("{} 5", oid_of(b"whatever"))],
        );
        let out = run(TransferOp::Upload, &BrokenStore, &script);
        let turns = responses(&out);
        assert_eq!(turns[1][0], line("status 500"));
        assert!(turns[1].contains(&line("error: internal storage fault")));
        turns[1].iter().for_each(|item| {
            if let Out::Line(text) = item {
                assert!(!text.contains("secret-lfs-root"), "leaked path in {text:?}");
            }
        });
    }

    #[test]
    fn a_large_object_round_trips_across_many_packets() {
        let store = MemoryStore::new();
        let media: Vec<u8> = (0..500_000u32).map(|n| (n % 251) as u8).collect();
        let oid = oid_of(&media);

        let mut script = Vec::new();
        msg_data(
            &mut script,
            &format!("put-object {oid}"),
            &[&format!("size={}", media.len())],
            &media,
        );
        let out = run(TransferOp::Upload, &store, &script);
        assert_eq!(responses(&out)[1][0], line("status 200"));

        let mut fetch = Vec::new();
        msg(&mut fetch, &format!("get-object {oid}"), &[]);
        let out = run(TransferOp::Download, &store, &fetch);
        let body: Vec<u8> = out
            .iter()
            .skip_while(|item| **item != Out::Delim)
            .filter_map(|item| match item {
                Out::Bin(chunk) => Some(chunk.clone()),
                Out::Line(text) => Some(format!("{text}\n").into_bytes()),
                _ => None,
            })
            .flatten()
            .collect();
        assert_eq!(body, media);
    }
}
