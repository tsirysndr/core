use std::collections::HashMap;

use flate2::{Decompress, FlushDecompress, Status};
use gix_pack::data::{Entry, entry::Header};

use knot_types::ObjectCount;

use crate::error::{PackError, PackLimit};
use crate::ids::PackOffset;
use crate::meter::{PackLimits, check_depth, malformed, pack_object_count};
use crate::pkt::{self, Frame};

const HEADER_SLACK: usize = 64;

fn pack_start(buf: &[u8]) -> Option<usize> {
    let caps = pkt::first_command(buf)
        .map(pkt::parse_caps)
        .unwrap_or_default();
    let boundary = if caps.push_options { 2 } else { 1 };
    pkt::frames(buf, Some(boundary))
        .filter_map(|item| match item {
            Ok((Frame::Flush, at)) => Some(at),
            _ => None,
        })
        .nth(boundary - 1)
}

fn new_oid_field(line: &[u8]) -> Option<&[u8]> {
    let line = line.split(|byte| *byte == 0).next().unwrap_or(line);
    line.split(|byte| *byte == b' ').nth(1)
}

fn no_pack_needed(buf: &[u8]) -> bool {
    pkt::frames(buf, Some(1))
        .filter_map(|item| match item {
            Ok((Frame::Data(payload), _)) => Some(payload),
            _ => None,
        })
        .all(|line| {
            new_oid_field(line)
                .map(|oid| oid.iter().all(|byte| *byte == b'0'))
                .unwrap_or(false)
        })
}

const PREAMBLE_SCAN_LIMIT: usize = 16 * 1024 * 1024;

trait PackSource {
    fn len(&self) -> u64;
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize>;
}

impl PackSource for [u8] {
    fn len(&self) -> u64 {
        <[u8]>::len(self) as u64
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(<[u8]>::len(self));
        let read = (<[u8]>::len(self) - start).min(buf.len());
        buf[..read].copy_from_slice(&self[start..start + read]);
        Ok(read)
    }
}

struct FileSource<'a> {
    file: &'a std::fs::File,
    len: u64,
}

impl PackSource for FileSource<'_> {
    fn len(&self) -> u64 {
        self.len
    }
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<usize> {
        use std::os::unix::fs::FileExt;
        let read = self.len.saturating_sub(offset).min(buf.len() as u64) as usize;
        self.file.read_exact_at(&mut buf[..read], offset)?;
        Ok(read)
    }
}

fn read_head<S: PackSource + ?Sized>(source: &S, upto: u64) -> Result<Vec<u8>, PackError> {
    let limit = upto.min(PREAMBLE_SCAN_LIMIT as u64) as usize;
    let mut head = vec![0u8; limit];
    let read = source
        .read_at(0, &mut head)
        .map_err(|error| PackError::Pack(format!("pack read: {error}")))?;
    head.truncate(read);
    Ok(head)
}

struct EntryInflate {
    data_offset: PackOffset,
    decompressed_size: u64,
    decompress: Decompress,
    produced: u64,
}

impl EntryInflate {
    fn feed<S: PackSource + ?Sized>(
        &mut self,
        pack: &S,
        scratch: &mut [u8],
        chunk: &mut [u8],
    ) -> Result<Option<PackOffset>, PackError> {
        loop {
            let consumed = self.decompress.total_in();
            let out_before = self.decompress.total_out();
            let read = pack
                .read_at(self.data_offset.get() + consumed, chunk)
                .map_err(|error| PackError::Pack(format!("pack read: {error}")))?;
            let status = self
                .decompress
                .decompress(&chunk[..read], scratch, FlushDecompress::None)
                .map_err(|error| PackError::Pack(format!("inflate: {error}")))?;
            self.produced += self.decompress.total_out() - out_before;
            if self.produced > self.decompressed_size {
                return Err(malformed("object inflates beyond its declared size"));
            }
            match status {
                Status::StreamEnd => {
                    return if self.produced == self.decompressed_size {
                        self.data_offset
                            .get()
                            .checked_add(self.decompress.total_in())
                            .map(|offset| Some(PackOffset::new(offset)))
                            .ok_or_else(|| malformed("pack offset overflow"))
                    } else {
                        Err(malformed("object decompressed size mismatch"))
                    };
                }
                Status::Ok | Status::BufError => {
                    if self.decompress.total_in() == consumed
                        && self.decompress.total_out() == out_before
                    {
                        return Ok(None);
                    }
                }
            }
        }
    }
}

#[derive(Default)]
struct PackProgress {
    num_objects: ObjectCount,
    objects_done: ObjectCount,
    next_offset: PackOffset,
    total_decompressed: u64,
    base_of: HashMap<PackOffset, PackOffset>,
    current: Option<EntryInflate>,
    depth_checked: bool,
}

impl PackProgress {
    fn scan<S: PackSource + ?Sized>(
        &mut self,
        pack: &S,
        limits: &PackLimits,
        kind: gix::hash::Kind,
    ) -> Result<Option<usize>, PackError> {
        let hash_len = kind.len_in_bytes();
        let len = pack.len();
        let mut scratch = [0u8; 8192];
        let mut chunk = [0u8; 8192];
        let mut header = [0u8; HEADER_SLACK];
        loop {
            if self.objects_done == self.num_objects {
                if !self.depth_checked {
                    check_depth(&self.base_of, limits.max_delta_depth)?;
                    self.depth_checked = true;
                }
                let total_len = (self.next_offset.get() as usize)
                    .checked_add(hash_len)
                    .ok_or_else(|| malformed("pack length overflow"))?;
                return Ok((len as usize >= total_len).then_some(total_len));
            }
            match self.current.as_mut() {
                Some(entry) => match entry.feed(pack, &mut scratch, &mut chunk)? {
                    Some(next_offset) => {
                        self.next_offset = next_offset;
                        self.objects_done = self.objects_done.succ();
                        self.current = None;
                    }
                    None => return Ok(None),
                },
                None => {
                    let start = self.next_offset;
                    if start.get() >= len {
                        return Ok(None);
                    }
                    let read = pack
                        .read_at(start.get(), &mut header)
                        .map_err(|error| PackError::Pack(format!("pack read: {error}")))?;
                    let mut reader: &[u8] = &header[..read];
                    let entry = match Entry::from_read(&mut reader, start.get(), hash_len) {
                        Ok(entry) => entry,
                        Err(error) => {
                            return if len.saturating_sub(start.get()) < HEADER_SLACK as u64 {
                                Ok(None)
                            } else {
                                Err(PackError::Pack(error.to_string()))
                            };
                        }
                    };
                    if limits.max_object_bytes.exceeded_by(entry.decompressed_size) {
                        return Err(PackError::LimitExceeded(PackLimit::ObjectBytes));
                    }
                    self.total_decompressed = self
                        .total_decompressed
                        .checked_add(entry.decompressed_size)
                        .ok_or_else(|| malformed("decompressed size overflow"))?;
                    if limits.max_total_bytes.exceeded_by(self.total_decompressed) {
                        return Err(PackError::LimitExceeded(PackLimit::TotalBytes));
                    }
                    if let Header::OfsDelta { base_distance } = entry.header {
                        let base = entry
                            .checked_base_pack_offset(base_distance)
                            .ok_or_else(|| malformed("ofs-delta base out of range"))?;
                        self.base_of.insert(self.next_offset, PackOffset::new(base));
                    }
                    self.current = Some(EntryInflate {
                        data_offset: PackOffset::new(entry.data_offset),
                        decompressed_size: entry.decompressed_size,
                        decompress: Decompress::new(true),
                        produced: 0,
                    });
                }
            }
        }
    }
}

pub struct ReceiveFramer {
    limits: PackLimits,
    kind: gix::hash::Kind,
    pack_start: Option<usize>,
    pack: Option<PackProgress>,
}

impl ReceiveFramer {
    pub fn new(limits: PackLimits, kind: gix::hash::Kind) -> Self {
        Self {
            limits,
            kind,
            pack_start: None,
            pack: None,
        }
    }

    pub fn pack_start(&self) -> Option<usize> {
        self.pack_start
    }

    pub fn advance_bytes(&mut self, buf: &[u8]) -> Result<Option<usize>, PackError> {
        self.advance(buf)
    }

    pub fn advance_file(
        &mut self,
        file: &std::fs::File,
        len: u64,
    ) -> Result<Option<usize>, PackError> {
        self.advance(&FileSource { file, len })
    }

    fn advance<S: PackSource + ?Sized>(&mut self, source: &S) -> Result<Option<usize>, PackError> {
        let len = source.len();
        let pack_start = match self.pack_start {
            Some(start) => start,
            None => {
                let head = read_head(source, len)?;
                match pack_start(&head) {
                    Some(start) => {
                        self.pack_start = Some(start);
                        start
                    }
                    None => return Ok(None),
                }
            }
        };
        if self.pack.is_none() {
            let pack_len = len.saturating_sub(pack_start as u64);
            if pack_len == 0 {
                let head = read_head(source, pack_start as u64)?;
                return Ok(no_pack_needed(&head).then_some(pack_start));
            }
            if pack_len < 12 {
                return Ok(None);
            }
            let mut header = [0u8; 12];
            source
                .read_at(pack_start as u64, &mut header)
                .map_err(|error| PackError::Pack(format!("pack read: {error}")))?;
            if &header[..4] != b"PACK" {
                return Err(malformed("packfile is missing its PACK signature"));
            }
            let num_objects = pack_object_count(&header)?;
            if num_objects > self.limits.max_objects {
                return Err(PackError::LimitExceeded(PackLimit::Objects));
            }
            self.pack = Some(PackProgress {
                num_objects,
                next_offset: PackOffset::new(pack_start as u64 + 12),
                ..PackProgress::default()
            });
        }
        let kind = self.kind;
        self.pack
            .as_mut()
            .expect("pack progress initialized")
            .scan(source, &self.limits, kind)
    }
}

pub fn receive_request_complete(
    buf: &[u8],
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<Option<usize>, PackError> {
    ReceiveFramer::new(*limits, kind).advance_bytes(buf)
}

pub fn archive_request_complete(buf: &[u8]) -> Option<usize> {
    pkt::frames(buf, Some(1)).find_map(|item| match item {
        Ok((Frame::Flush, end)) => Some(end),
        _ => None,
    })
}

#[derive(Default)]
pub struct UploadFramer {
    scanned: usize,
    v2: bool,
    flushes: usize,
    complete: Option<usize>,
}

impl UploadFramer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn advance(&mut self, buf: &[u8]) -> Option<usize> {
        if self.complete.is_some() {
            return self.complete;
        }
        let base = self.scanned;
        for item in pkt::frames(&buf[base..], None) {
            let Ok((frame, at)) = item else { break };
            let boundary = base + at;
            match frame {
                Frame::Data(payload) => {
                    if payload.starts_with(b"command=") {
                        self.v2 = true;
                    }
                    let trimmed = payload
                        .iter()
                        .rposition(|byte| !byte.is_ascii_whitespace())
                        .map(|end| &payload[..=end])
                        .unwrap_or(payload);
                    if !self.v2 && trimmed == b"done" {
                        self.complete = Some(boundary);
                        return self.complete;
                    }
                }
                Frame::Flush => {
                    self.flushes += 1;
                    if self.v2 {
                        self.complete = Some(boundary);
                        return self.complete;
                    }
                }
                _ => {}
            }
            self.scanned = boundary;
        }
        None
    }

    pub fn unanswered_flushes(&self) -> usize {
        self.flushes.saturating_sub(1)
    }
}

pub fn upload_v0_nak() -> Vec<u8> {
    let mut buf = Vec::new();
    pkt::write_data(&mut buf, b"NAK\n").expect("write to in-memory buffer never fails");
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v2_fetch_request() -> Vec<u8> {
        let mut buf = Vec::new();
        pkt::write_data(&mut buf, b"command=fetch\n").unwrap();
        pkt::write_delim(&mut buf).unwrap();
        pkt::write_data(&mut buf, b"want 1111111111111111111111111111111111111111\n").unwrap();
        pkt::write_data(&mut buf, b"want 2222222222222222222222222222222222222222\n").unwrap();
        pkt::write_data(&mut buf, b"done\n").unwrap();
        pkt::write_flush(&mut buf).unwrap();
        buf
    }

    #[test]
    fn upload_framer_completes_a_v2_request_at_the_terminating_flush() {
        let request = v2_fetch_request();
        assert_eq!(UploadFramer::new().advance(&request), Some(request.len()));
    }

    #[test]
    fn upload_framer_fed_one_byte_at_a_time_never_overruns_the_buffer() {
        let request = v2_fetch_request();
        let mut framer = UploadFramer::new();
        let mut buf = Vec::new();
        let mut completed = None;
        for byte in &request {
            buf.push(*byte);
            if let Some(len) = framer.advance(&buf) {
                assert!(
                    len <= buf.len(),
                    "advance returned {len} past buffer of {}",
                    buf.len()
                );
                completed = Some(len);
                break;
            }
        }
        assert_eq!(
            completed,
            Some(request.len()),
            "incrementally fed request completes exactly once the whole buffer has arrived"
        );
    }

    #[test]
    fn upload_framer_counts_v0_have_batch_flushes_without_completing() {
        let mut buf = Vec::new();
        pkt::write_data(&mut buf, b"want 1111111111111111111111111111111111111111\n").unwrap();
        pkt::write_flush(&mut buf).unwrap();
        pkt::write_data(&mut buf, b"have 2222222222222222222222222222222222222222\n").unwrap();
        pkt::write_flush(&mut buf).unwrap();
        let mut framer = UploadFramer::new();
        assert_eq!(framer.advance(&buf), None, "v0 request is open until done");
        assert_eq!(
            framer.unanswered_flushes(),
            1,
            "two flushes seen, one have-batch awaits NAK"
        );
    }
}
