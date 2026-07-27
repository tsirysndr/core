use std::collections::HashMap;
use std::io::{self, Write};

use flate2::{Decompress, FlushDecompress, Status};
use gix_pack::data::input;
use gix_pack::data::{entry::Header, header};

use knot_types::ObjectCount;

use crate::error::{PackError, PackLimit};
use crate::ids::{DeltaDepth, MaxObjectBytes, MaxTotalBytes, PackOffset};

const MAX_OBJECTS: ObjectCount = ObjectCount::new(16_000_000);
const MAX_OBJECT_BYTES: MaxObjectBytes = MaxObjectBytes::new(512 * 1024 * 1024);
const MAX_TOTAL_BYTES: MaxTotalBytes = MaxTotalBytes::new(16 * 1024 * 1024 * 1024);
const MAX_DELTA_DEPTH: DeltaDepth = DeltaDepth::new(50);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackLimits {
    pub max_objects: ObjectCount,
    pub max_object_bytes: MaxObjectBytes,
    pub max_total_bytes: MaxTotalBytes,
    pub max_delta_depth: DeltaDepth,
}

impl Default for PackLimits {
    fn default() -> Self {
        Self {
            max_objects: MAX_OBJECTS,
            max_object_bytes: MAX_OBJECT_BYTES,
            max_total_bytes: MAX_TOTAL_BYTES,
            max_delta_depth: MAX_DELTA_DEPTH,
        }
    }
}

pub(crate) fn malformed(message: &str) -> PackError {
    PackError::Pack(message.to_string())
}

pub(crate) fn pack_object_count(pack: &[u8]) -> Result<ObjectCount, PackError> {
    let head: [u8; 12] = pack
        .get(..12)
        .and_then(|slice| slice.try_into().ok())
        .ok_or_else(|| malformed("packfile header is truncated"))?;
    header::decode(&head)
        .map(|(_version, num_objects)| ObjectCount::from(num_objects))
        .map_err(|error| PackError::Pack(error.to_string()))
}

pub fn meter(pack: &[u8], limits: &PackLimits, kind: gix::hash::Kind) -> Result<(), PackError> {
    if pack.len() < 12 + kind.len_in_bytes() {
        return Err(malformed("packfile is truncated"));
    }
    meter_entries(
        io::Cursor::new(pack),
        pack_object_count(pack)?,
        limits,
        kind,
    )
    .map(|_| ())
}

pub(crate) fn meter_file(
    pack: &gix_pack::data::File,
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<bool, PackError> {
    let reader = io::BufReader::new(std::fs::File::open(pack.path())?);
    meter_entries(reader, ObjectCount::from(pack.num_objects()), limits, kind)
}

fn meter_entries<R: io::BufRead>(
    reader: R,
    num_objects: ObjectCount,
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<bool, PackError> {
    if num_objects > limits.max_objects {
        return Err(PackError::LimitExceeded(PackLimit::Objects));
    }
    let mut entries = input::BytesToEntriesIter::new_from_header(
        reader,
        input::Mode::Verify,
        input::EntryDataMode::Keep,
        kind,
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;

    let mut total = 0u64;
    let mut thin = false;
    let mut base_of: HashMap<PackOffset, PackOffset> = HashMap::new();
    entries.try_for_each(|entry| -> Result<(), PackError> {
        let entry = entry.map_err(|error| PackError::Pack(error.to_string()))?;
        if limits.max_object_bytes.exceeded_by(entry.decompressed_size) {
            return Err(PackError::LimitExceeded(PackLimit::ObjectBytes));
        }
        total = total
            .checked_add(entry.decompressed_size)
            .ok_or_else(|| malformed("decompressed size overflow"))?;
        if limits.max_total_bytes.exceeded_by(total) {
            return Err(PackError::LimitExceeded(PackLimit::TotalBytes));
        }
        match entry.header {
            Header::OfsDelta { base_distance } => {
                let pack_offset = PackOffset::new(entry.pack_offset);
                let base = pack_offset
                    .checked_sub_distance(base_distance)
                    .ok_or_else(|| malformed("ofs-delta base out of range"))?;
                base_of.insert(pack_offset, base);
                check_delta_result(&entry, limits.max_object_bytes)?;
            }
            Header::RefDelta { .. } => {
                thin = true;
                check_delta_result(&entry, limits.max_object_bytes)?;
            }
            _ => {}
        }
        Ok(())
    })?;

    check_depth(&base_of, limits.max_delta_depth)?;
    Ok(thin)
}

fn check_delta_result(
    entry: &input::Entry,
    max_object_bytes: MaxObjectBytes,
) -> Result<(), PackError> {
    let compressed = entry
        .compressed
        .as_deref()
        .ok_or_else(|| malformed("delta entry missing compressed data"))?;
    let mut peek = HeaderPeek::new();
    inflate_into(compressed, entry.decompressed_size, &mut peek)?;
    if max_object_bytes.exceeded_by(delta_result_size(peek.filled())?) {
        return Err(PackError::LimitExceeded(PackLimit::ObjectBytes));
    }
    Ok(())
}

pub(crate) fn inflate_into(
    input: &[u8],
    expected: u64,
    out: &mut dyn Write,
) -> Result<u64, PackError> {
    let mut decompress = Decompress::new(true);
    let mut scratch = [0u8; 8192];
    let mut produced = 0u64;
    loop {
        let consumed = decompress.total_in() as usize;
        let out_before = decompress.total_out();
        let status = decompress
            .decompress(
                input.get(consumed..).unwrap_or_default(),
                &mut scratch,
                FlushDecompress::None,
            )
            .map_err(|error| PackError::Pack(format!("inflate: {error}")))?;
        let written = (decompress.total_out() - out_before) as usize;
        produced += written as u64;
        if produced > expected {
            return Err(malformed("object inflates beyond its declared size"));
        }
        out.write_all(&scratch[..written])
            .map_err(|error| PackError::Pack(format!("inflate sink: {error}")))?;
        match status {
            Status::StreamEnd => break,
            Status::Ok | Status::BufError => {
                if decompress.total_in() as usize == consumed
                    && decompress.total_out() == out_before
                {
                    return Err(malformed("inflate stalled or pack truncated"));
                }
            }
        }
    }
    if produced != expected {
        return Err(malformed("object decompressed size mismatch"));
    }
    Ok(decompress.total_in())
}

struct HeaderPeek {
    bytes: [u8; 32],
    len: usize,
}

impl HeaderPeek {
    fn new() -> Self {
        Self {
            bytes: [0u8; 32],
            len: 0,
        }
    }

    fn filled(&self) -> &[u8] {
        &self.bytes[..self.len]
    }
}

impl Write for HeaderPeek {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let take = (self.bytes.len() - self.len).min(data.len());
        self.bytes[self.len..self.len + take].copy_from_slice(&data[..take]);
        self.len += take;
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn read_delta_varint(
    data: &[u8],
    pos: usize,
    shift: u32,
    acc: u64,
) -> Result<(u64, usize), PackError> {
    if shift >= u64::BITS {
        return Err(malformed("delta size header overflows"));
    }
    let byte = *data
        .get(pos)
        .ok_or_else(|| malformed("delta size header truncated"))?;
    let acc = acc | (u64::from(byte & 0x7f) << shift);
    if byte & 0x80 == 0 {
        Ok((acc, pos + 1))
    } else {
        read_delta_varint(data, pos + 1, shift + 7, acc)
    }
}

fn delta_result_size(header: &[u8]) -> Result<u64, PackError> {
    let (_base_size, after_base) = read_delta_varint(header, 0, 0, 0)?;
    let (result_size, _) = read_delta_varint(header, after_base, 0, 0)?;
    Ok(result_size)
}

pub(crate) fn check_depth(
    base_of: &HashMap<PackOffset, PackOffset>,
    max: DeltaDepth,
) -> Result<(), PackError> {
    base_of.keys().try_for_each(|start| {
        let mut depth = DeltaDepth::ZERO;
        let mut cursor = *start;
        while let Some(&base) = base_of.get(&cursor) {
            depth = depth.deeper();
            if depth.exceeds(max) {
                return Err(PackError::LimitExceeded(PackLimit::DeltaDepth));
            }
            cursor = base;
        }
        Ok(())
    })
}
