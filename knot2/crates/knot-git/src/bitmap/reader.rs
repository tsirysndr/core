use super::bitset::Bitset;
use super::{BitmapEntryOffset, IndexPosition};
use crate::error::GitError;

const OPT_LOOKUP_TABLE: u16 = 0x10;
const TRIPLET_LEN: usize = 16;

pub(crate) struct Bitmaps<'a> {
    body: &'a [u8],
    table: Vec<(IndexPosition, BitmapEntryOffset)>,
    num_objects: usize,
}

impl Bitmaps<'_> {
    pub(crate) fn bitmap(&self, position: IndexPosition) -> Result<Option<Bitset>, GitError> {
        match self.table.binary_search_by_key(&position, |(pos, _)| *pos) {
            Ok(index) => self.decode_at(position, self.table[index].1).map(Some),
            Err(_) => Ok(None),
        }
    }

    fn decode_at(
        &self,
        commit_pos: IndexPosition,
        offset: BitmapEntryOffset,
    ) -> Result<Bitset, GitError> {
        let start = usize::try_from(offset.get())
            .ok()
            .filter(|start| *start <= self.body.len())
            .ok_or_else(|| GitError::Backend("bitmap entry offset out of range".to_string()))?;
        let entry = &self.body[start..];
        if entry.len() < 6 {
            return Err(GitError::Backend(
                "truncated bitmap entry header".to_string(),
            ));
        }
        if u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]) != commit_pos.get() {
            return Err(GitError::Backend(
                "bitmap lookup points at the wrong commit".to_string(),
            ));
        }
        if entry[4] != 0 {
            return Err(GitError::Backend(
                "xor-compressed bitmap entries are unsupported".to_string(),
            ));
        }
        let (vector, _) = decode(&entry[6..])?;
        Bitset::from_ewah(&vector, self.num_objects)
    }
}

pub(crate) fn parse(
    bytes: &[u8],
    kind: gix::hash::Kind,
    num_objects: usize,
) -> Result<Bitmaps<'_>, GitError> {
    let raw = kind.len_in_bytes();
    let header_len = 12 + raw;
    if bytes.len() < header_len + raw || &bytes[..4] != b"BITM" {
        return Err(GitError::Backend("bitmap header isn't BITM".to_string()));
    }
    let version = u16::from_be_bytes([bytes[4], bytes[5]]);
    if version != 1 {
        return Err(GitError::Backend(format!(
            "unsupported bitmap version {version}"
        )));
    }
    let flags = u16::from_be_bytes([bytes[6], bytes[7]]);
    if flags & OPT_LOOKUP_TABLE == 0 {
        return Err(GitError::Backend(
            "bitmap lacks the lookup table extension".to_string(),
        ));
    }
    let entry_count = u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]) as usize;

    let (body, trailer) = bytes.split_at(bytes.len() - raw);
    let mut hasher = gix_hash::hasher(kind);
    hasher.update(body);
    let digest = hasher
        .try_finalize()
        .map_err(|error| GitError::Backend(format!("bitmap checksum: {error}")))?;
    if digest.as_slice() != trailer {
        return Err(GitError::Backend("bitmap checksum mismatch".to_string()));
    }

    let table_len = entry_count
        .checked_mul(TRIPLET_LEN)
        .filter(|len| header_len + len <= body.len())
        .ok_or_else(|| GitError::Backend("bitmap lookup table overflows the file".to_string()))?;
    let region = &body[body.len() - table_len..];
    let mut table: Vec<(IndexPosition, BitmapEntryOffset)> = (0..entry_count)
        .map(|index| {
            let base = index * TRIPLET_LEN;
            let commit_pos = u32::from_be_bytes(region[base..base + 4].try_into().unwrap());
            let offset = u64::from_be_bytes(region[base + 4..base + 12].try_into().unwrap());
            (
                IndexPosition::new(commit_pos),
                BitmapEntryOffset::new(offset),
            )
        })
        .collect();
    table.sort_unstable_by_key(|(commit_pos, _)| *commit_pos);

    Ok(Bitmaps {
        body,
        table,
        num_objects,
    })
}

fn decode(data: &[u8]) -> Result<(gix_bitmap::ewah::Vec, &[u8]), GitError> {
    gix_bitmap::ewah::decode(data)
        .map_err(|error| GitError::Backend(format!("ewah decode: {error:?}")))
}
