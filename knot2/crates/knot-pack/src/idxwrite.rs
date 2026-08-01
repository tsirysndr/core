use std::io::{self, BufWriter, Write};
use std::os::unix::fs::FileExt;
use std::sync::Mutex;

use gix::ObjectId;

use crate::error::PackError;
use crate::ids::{Crc32, PackOffset};

const V2_SIGNATURE: &[u8] = &[0xff, 0x74, 0x4f, 0x63];
const V2_VERSION: u32 = 2;
const HIGH_BIT: u32 = 0x8000_0000;
const LARGE_OFFSET_THRESHOLD: u64 = 0x7fff_ffff;
const BUCKETS: usize = 256;
const BUCKET_BUF: usize = 64 * 1024;
const CRC_LEN: usize = 4;
const OFFSET_LEN: usize = 8;

struct Record {
    id: ObjectId,
    crc32: Crc32,
    offset: PackOffset,
}

type Bucket = Mutex<Option<BufWriter<std::fs::File>>>;

pub(crate) struct Spool {
    buckets: Vec<Bucket>,
    record_len: usize,
    hash_len: usize,
}

impl Spool {
    pub(crate) fn new(kind: gix::hash::Kind) -> Self {
        let hash_len = kind.len_in_bytes();
        Self {
            buckets: (0..BUCKETS).map(|_| Mutex::new(None)).collect(),
            record_len: hash_len + CRC_LEN + OFFSET_LEN,
            hash_len,
        }
    }

    pub(crate) fn push(&self, id: ObjectId, crc32: Crc32, offset: PackOffset) -> io::Result<()> {
        let mut guard = self.buckets[id.first_byte() as usize]
            .lock()
            .expect("spool bucket poisoned");
        let writer = match guard.as_mut() {
            Some(writer) => writer,
            None => guard.insert(BufWriter::with_capacity(BUCKET_BUF, tempfile::tempfile()?)),
        };
        writer.write_all(id.as_slice())?;
        writer.write_all(&crc32.get().to_be_bytes())?;
        writer.write_all(&offset.get().to_be_bytes())
    }

    fn cumulative_fanout(&self) -> Result<[u32; 256], PackError> {
        let mut fanout = [0u32; 256];
        self.buckets.iter().enumerate().try_for_each(
            |(bucket, cell)| -> Result<(), PackError> {
                let mut guard = cell.lock().expect("spool bucket poisoned");
                fanout[bucket] = match guard.as_mut() {
                    None => 0,
                    Some(writer) => {
                        writer.flush()?;
                        (writer.get_ref().metadata()?.len() as usize / self.record_len) as u32
                    }
                };
                Ok(())
            },
        )?;
        fanout.iter_mut().fold(0u32, |acc, count| {
            *count += acc;
            *count
        });
        Ok(fanout)
    }

    fn visit_sorted(
        &self,
        mut visit: impl FnMut(&Record) -> Result<(), PackError>,
    ) -> Result<(), PackError> {
        self.buckets.iter().try_for_each(|cell| {
            let mut records = self.read_bucket(cell)?;
            records.sort_unstable_by_key(|record| record.id);
            records.iter().try_for_each(&mut visit)
        })
    }

    fn read_bucket(&self, cell: &Bucket) -> Result<Vec<Record>, PackError> {
        let mut guard = cell.lock().expect("spool bucket poisoned");
        let bytes = match guard.as_mut() {
            None => Vec::new(),
            Some(writer) => {
                writer.flush()?;
                let file = writer.get_ref();
                let len = file.metadata()?.len() as usize;
                let mut bytes = vec![0u8; len];
                file.read_exact_at(&mut bytes, 0)?;
                bytes
            }
        };
        drop(guard);
        bytes
            .chunks_exact(self.record_len)
            .map(|chunk| {
                let (id, rest) = chunk.split_at(self.hash_len);
                Ok(Record {
                    id: ObjectId::try_from(id)
                        .map_err(|error| PackError::Pack(format!("spool record oid: {error}")))?,
                    crc32: Crc32::new(u32::from_be_bytes(
                        rest[..CRC_LEN].try_into().expect("crc slice"),
                    )),
                    offset: PackOffset::new(u64::from_be_bytes(
                        rest[CRC_LEN..].try_into().expect("offset slice"),
                    )),
                })
            })
            .collect()
    }
}

fn feed(out: &mut dyn Write, hasher: &mut gix_hash::Hasher, buf: &[u8]) -> io::Result<()> {
    hasher.update(buf);
    out.write_all(buf)
}

pub(crate) fn write_v2_index(
    out: &mut dyn Write,
    records: &Spool,
    pack_hash: &ObjectId,
    kind: gix::hash::Kind,
) -> Result<ObjectId, PackError> {
    let mut hasher = gix_hash::hasher(kind);
    feed(out, &mut hasher, V2_SIGNATURE)?;
    feed(out, &mut hasher, &V2_VERSION.to_be_bytes())?;

    records
        .cumulative_fanout()?
        .iter()
        .try_for_each(|count| feed(out, &mut hasher, &count.to_be_bytes()))?;
    records.visit_sorted(|record| Ok(feed(out, &mut hasher, record.id.as_slice())?))?;
    records
        .visit_sorted(|record| Ok(feed(out, &mut hasher, &record.crc32.get().to_be_bytes())?))?;

    let mut large_offsets = Vec::<u64>::new();
    records.visit_sorted(|record| {
        let encoded = if record.offset.get() > LARGE_OFFSET_THRESHOLD {
            let position = large_offsets.len() as u32;
            large_offsets.push(record.offset.get());
            position | HIGH_BIT
        } else {
            record.offset.get() as u32
        };
        Ok(feed(out, &mut hasher, &encoded.to_be_bytes())?)
    })?;
    large_offsets
        .iter()
        .try_for_each(|offset| feed(out, &mut hasher, &offset.to_be_bytes()))?;

    feed(out, &mut hasher, pack_hash.as_slice())?;

    let index_hash = hasher
        .try_finalize()
        .map_err(|error| PackError::Pack(format!("finalize index hash: {error}")))?;
    out.write_all(index_hash.as_slice())?;
    out.flush()?;
    Ok(index_hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(seed: u8) -> ObjectId {
        let mut raw = [0u8; 20];
        raw[0] = seed;
        raw[19] = seed;
        ObjectId::try_from(raw.as_slice()).unwrap()
    }

    #[test]
    fn large_offsets_round_trip_through_the_index_reader() {
        let records = [
            (oid(0x02), 0x1111_1111u32, 12u64),
            (oid(0x40), 0x2222_2222, LARGE_OFFSET_THRESHOLD),
            (oid(0x80), 0x3333_3333, 0x1_2345_6789),
            (oid(0xc0), 0x4444_4444, LARGE_OFFSET_THRESHOLD + 1),
        ];
        let spool = Spool::new(gix::hash::Kind::Sha1);
        records.iter().for_each(|(id, crc32, offset)| {
            spool
                .push(*id, Crc32::new(*crc32), PackOffset::new(*offset))
                .unwrap()
        });
        let pack_hash = oid(0xaa);

        let mut buf = Vec::new();
        let index_hash =
            write_v2_index(&mut buf, &spool, &pack_hash, gix::hash::Kind::Sha1).unwrap();

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("pack-under-test.idx");
        std::fs::write(&path, &buf).unwrap();
        let index = gix_pack::index::File::at(&path, gix::hash::Kind::Sha1).unwrap();

        assert_eq!(index.num_objects(), records.len() as u32);
        assert_eq!(index.index_checksum(), index_hash);
        assert_eq!(index.pack_checksum(), pack_hash);
        records.iter().for_each(|(id, crc32, offset)| {
            let at = index
                .lookup(*id)
                .expect("written oid resolves in the index");
            assert_eq!(index.oid_at_index(at), id.as_ref());
            assert_eq!(index.pack_offset_at_index(at), *offset);
            assert_eq!(index.crc32_at_index(at), Some(*crc32));
        });
    }
}
