use std::collections::HashMap;
use std::path::Path;

use gix::ObjectId;
use gix::object::Kind;
use gix::objs::{Find, Write};
use gix_pack::data::{Entry, entry::Header};

use crate::error::{PackError, PackLimit};
use crate::ids::{DeltaDepth, PackOffset, Rounds};
use crate::meter::{PackLimits, inflate_into, pack_object_count};

struct Raw {
    offset: PackOffset,
    header: Header,
    data: Vec<u8>,
}

#[derive(Clone, Copy)]
struct Resolved {
    oid: ObjectId,
    depth: DeltaDepth,
}

fn malformed(message: &str) -> PackError {
    PackError::Pack(message.to_string())
}

pub fn resolve(
    objects_dir: &Path,
    pack: &[u8],
    limits: &PackLimits,
    kind: gix::hash::Kind,
) -> Result<(), PackError> {
    let mut raws = parse_entries(pack, kind)?;
    let odb = gix::odb::at_opts(
        objects_dir,
        std::iter::empty(),
        gix::odb::store::init::Options {
            object_hash: kind,
            ..Default::default()
        },
    )
    .map_err(|error| PackError::Pack(error.to_string()))?;
    let mut done: HashMap<PackOffset, Resolved> = HashMap::new();
    let mut by_oid: HashMap<ObjectId, DeltaDepth> = HashMap::new();
    resolve_rounds(
        &mut raws,
        &odb,
        &mut done,
        &mut by_oid,
        limits.max_delta_depth,
        Rounds::new(limits.max_delta_depth.get() + 2),
    )
}

fn parse_entries(pack: &[u8], kind: gix::hash::Kind) -> Result<Vec<Raw>, PackError> {
    let hash_len = kind.len_in_bytes();
    let trailer = pack
        .len()
        .checked_sub(hash_len)
        .ok_or_else(|| malformed("packfile is truncated"))?;
    let num_objects = pack_object_count(pack)?;

    (0..num_objects.get())
        .try_fold((Vec::new(), PackOffset::new(12)), |(mut acc, offset), _| {
            let mut reader: &[u8] = pack
                .get(offset.get() as usize..trailer)
                .ok_or_else(|| malformed("entry offset past pack end"))?;
            let entry = Entry::from_read(&mut reader, offset.get(), hash_len)
                .map_err(|error| PackError::Pack(error.to_string()))?;
            let data_start = entry.data_offset as usize;
            let mut data = Vec::with_capacity(entry.decompressed_size as usize);
            let consumed = inflate_into(
                pack.get(data_start..)
                    .ok_or_else(|| malformed("entry body past pack end"))?,
                entry.decompressed_size,
                &mut data,
            )?;
            acc.push(Raw {
                offset,
                header: entry.header,
                data,
            });
            Ok::<_, PackError>((acc, PackOffset::new(entry.data_offset + consumed)))
        })
        .map(|(acc, _)| acc)
}

fn resolve_rounds(
    raws: &mut [Raw],
    odb: &gix::odb::Handle,
    done: &mut HashMap<PackOffset, Resolved>,
    by_oid: &mut HashMap<ObjectId, DeltaDepth>,
    max_depth: DeltaDepth,
    rounds_left: Rounds,
) -> Result<(), PackError> {
    let pending: Vec<usize> = (0..raws.len())
        .filter(|index| !done.contains_key(&raws[*index].offset))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let Some(remaining) = rounds_left.next() else {
        return Err(PackError::LimitExceeded(PackLimit::DeltaDepth));
    };
    let progressed = pending.iter().try_fold(false, |progressed, &index| {
        match resolve_one(&raws[index], odb, done, by_oid, max_depth)? {
            Some((kind, data, depth)) => {
                let oid = odb
                    .write_buf(kind, &data)
                    .map_err(|error| PackError::Pack(error.to_string()))?;
                by_oid.insert(oid, depth);
                done.insert(raws[index].offset, Resolved { oid, depth });
                raws[index].data = Vec::new();
                Ok::<_, PackError>(true)
            }
            None => Ok(progressed),
        }
    })?;
    if !progressed {
        return Err(malformed("pack contains unresolvable delta"));
    }
    resolve_rounds(raws, odb, done, by_oid, max_depth, remaining)
}

fn apply_delta_checked(
    base_kind: Kind,
    base_data: &[u8],
    base_depth: DeltaDepth,
    delta: &[u8],
    max_depth: DeltaDepth,
) -> Result<(Kind, Vec<u8>, DeltaDepth), PackError> {
    let depth = base_depth.deeper();
    if depth.exceeds(max_depth) {
        return Err(PackError::LimitExceeded(PackLimit::DeltaDepth));
    }
    Ok((base_kind, apply_delta(base_data, delta)?, depth))
}

fn resolve_one(
    raw: &Raw,
    odb: &gix::odb::Handle,
    done: &HashMap<PackOffset, Resolved>,
    by_oid: &HashMap<ObjectId, DeltaDepth>,
    max_depth: DeltaDepth,
) -> Result<Option<(Kind, Vec<u8>, DeltaDepth)>, PackError> {
    match &raw.header {
        Header::Commit | Header::Tree | Header::Blob | Header::Tag => {
            let kind = raw
                .header
                .as_kind()
                .ok_or_else(|| malformed("base entry has no object kind"))?;
            Ok(Some((kind, raw.data.clone(), DeltaDepth::ZERO)))
        }
        Header::OfsDelta { base_distance } => {
            let base_offset = raw
                .offset
                .checked_sub_distance(*base_distance)
                .filter(|_| *base_distance != 0)
                .ok_or_else(|| malformed("ofs-delta base out of range"))?;
            match done.get(&base_offset) {
                Some(base) => {
                    let mut buf = Vec::new();
                    let found = odb
                        .try_find(&base.oid, &mut buf)
                        .map_err(|error| PackError::Pack(error.to_string()))?
                        .ok_or_else(|| malformed("ofs-delta base missing from odb"))?;
                    apply_delta_checked(found.kind, found.data, base.depth, &raw.data, max_depth)
                        .map(Some)
                }
                None => Ok(None),
            }
        }
        Header::RefDelta { base_id } => {
            let base_depth = by_oid.get(base_id).copied();
            let mut buf = Vec::new();
            match odb
                .try_find(base_id, &mut buf)
                .map_err(|error| PackError::Pack(error.to_string()))?
            {
                Some(base) => apply_delta_checked(
                    base.kind,
                    base.data,
                    base_depth.unwrap_or(DeltaDepth::ZERO),
                    &raw.data,
                    max_depth,
                )
                .map(Some),
                None => Ok(None),
            }
        }
    }
}

enum Op<'a> {
    Copy { start: usize, len: usize },
    Insert(&'a [u8]),
}

fn read_varint(delta: &[u8], pos: &mut usize) -> Result<u64, PackError> {
    let mut shift = 0u32;
    let mut value = 0u64;
    loop {
        let byte = *delta
            .get(*pos)
            .ok_or_else(|| malformed("delta size header truncated"))?;
        *pos += 1;
        value |= u64::from(byte & 0x7f) << shift;
        shift += 7;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
        if shift >= u64::BITS {
            return Err(malformed("delta size header overflows"));
        }
    }
}

fn assemble(
    cmd: u8,
    bit_base: u32,
    count: u32,
    delta: &[u8],
    pos: &mut usize,
) -> Result<u64, PackError> {
    (0..count).try_fold(0u64, |acc, index| {
        if cmd & (1 << (bit_base + index)) == 0 {
            return Ok(acc);
        }
        let byte = *delta
            .get(*pos)
            .ok_or_else(|| malformed("delta copy operand truncated"))?;
        *pos += 1;
        Ok(acc | (u64::from(byte) << (8 * index)))
    })
}

fn ops<'a>(delta: &'a [u8]) -> impl Iterator<Item = Result<Op<'a>, PackError>> {
    let mut pos = 0usize;
    std::iter::from_fn(move || {
        (pos < delta.len()).then(|| {
            let cmd = delta[pos];
            pos += 1;
            if cmd & 0x80 != 0 {
                let offset = assemble(cmd, 0, 4, delta, &mut pos)?;
                let raw_size = assemble(cmd, 4, 3, delta, &mut pos)?;
                let size = if raw_size == 0 { 0x10000 } else { raw_size };
                Ok(Op::Copy {
                    start: offset as usize,
                    len: size as usize,
                })
            } else if cmd != 0 {
                let len = cmd as usize;
                let bytes = delta
                    .get(pos..pos + len)
                    .ok_or_else(|| malformed("delta insert truncated"))?;
                pos += len;
                Ok(Op::Insert(bytes))
            } else {
                Err(malformed("delta uses reserved opcode 0"))
            }
        })
    })
}

fn apply_delta(base: &[u8], delta: &[u8]) -> Result<Vec<u8>, PackError> {
    let mut pos = 0usize;
    let base_size = read_varint(delta, &mut pos)?;
    if base_size as usize != base.len() {
        return Err(malformed("delta base size doesn't match its base object"));
    }
    let target_size = read_varint(delta, &mut pos)?;
    let out =
        ops(&delta[pos..]).try_fold(Vec::with_capacity(target_size as usize), |mut out, op| {
            match op? {
                Op::Copy { start, len } => {
                    let end = start
                        .checked_add(len)
                        .ok_or_else(|| malformed("delta copy range overflows"))?;
                    out.extend_from_slice(
                        base.get(start..end)
                            .ok_or_else(|| malformed("delta copy reads past base"))?,
                    );
                }
                Op::Insert(bytes) => out.extend_from_slice(bytes),
            }
            Ok::<_, PackError>(out)
        })?;
    if out.len() as u64 != target_size {
        return Err(malformed("delta produced wrong target size"));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_delta_reconstructs_copy_and_insert() {
        let base = b"hello world";
        let delta = [0x0b, 0x06, 0x90, 0x05, 0x01, b'!'];
        assert_eq!(apply_delta(base, &delta).unwrap(), b"hello!");
    }

    #[test]
    fn apply_delta_rejects_a_base_size_mismatch() {
        let delta = [0x05, 0x00];
        assert!(apply_delta(b"hi", &delta).is_err());
    }

    #[test]
    fn apply_delta_rejects_a_copy_past_the_base() {
        let delta = [0x02, 0x10, 0x90, 0xff];
        assert!(apply_delta(b"hi", &delta).is_err());
    }

    #[test]
    fn apply_delta_rejects_a_truncated_insert() {
        let delta = [0x02, 0x05, 0x05, b'a', b'b'];
        assert!(apply_delta(b"hi", &delta).is_err());
    }

    #[test]
    fn apply_delta_rejects_the_reserved_opcode() {
        let delta = [0x02, 0x00, 0x00];
        assert!(apply_delta(b"hi", &delta).is_err());
    }

    #[test]
    fn apply_delta_rejects_a_truncated_size_header() {
        let delta = [0x80];
        assert!(apply_delta(b"", &delta).is_err());
    }
}
