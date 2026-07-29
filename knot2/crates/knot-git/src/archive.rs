use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::AtomicBool;

use gix::bstr::BString;
use knot_types::{Oid, ParseError};

use crate::error::{GitError, backend};
use crate::objects::MAX_TREE_DEPTH;
use crate::repo::Repo;

const TAR_BLOCK: u64 = 512;

knot_types::scalar_newtype! {
    pub struct ArchiveLimit(u64);
}

impl Default for ArchiveLimit {
    fn default() -> Self {
        Self::new(1024 * 1024 * 1024)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveFormat {
    Tar,
    TarGz,
    Zip,
}

impl ArchiveFormat {
    fn gix(self) -> gix_archive::Format {
        match self {
            ArchiveFormat::Tar => gix_archive::Format::Tar,
            ArchiveFormat::TarGz => gix_archive::Format::TarGz {
                compression_level: None,
            },
            ArchiveFormat::Zip => gix_archive::Format::Zip {
                compression_level: None,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchivePrefix(String);

impl ArchivePrefix {
    pub fn new(value: impl Into<String>) -> Result<Self, ParseError> {
        let value = value.into();
        let safe = !value.contains('\0')
            && !value.starts_with(['/', '\\'])
            && value.split(['/', '\\']).all(|component| component != "..");
        match safe {
            true => Ok(Self(value)),
            false => Err(ParseError::Invalid {
                kind: "archive prefix",
                value,
            }),
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Repo {
    pub fn peel_to_tree(&self, oid: Oid) -> Result<Oid, GitError> {
        self.git()
            .find_object(oid.object_id())
            .map_err(backend)?
            .peel_to_tree()
            .map(|tree| Oid::from(tree.id))
            .map_err(backend)
    }

    pub fn write_archive(
        &self,
        tree: Oid,
        format: ArchiveFormat,
        prefix: Option<&ArchivePrefix>,
        limit: ArchiveLimit,
        out: impl std::io::Write + std::io::Seek,
    ) -> Result<(), GitError> {
        self.bound_archive_source(tree.object_id(), limit, MAX_TREE_DEPTH, &mut 0)?;
        let (stream, _index) = self
            .git()
            .worktree_stream(tree.object_id())
            .map_err(backend)?;
        let interrupt = AtomicBool::new(false);
        let mut spool = BoundedSpool {
            inner: out,
            position: 0,
            limit,
            overflowed: false,
        };
        let written = self.git().worktree_archive(
            stream,
            &mut spool,
            gix::progress::Discard,
            &interrupt,
            gix_archive::Options {
                format: format.gix(),
                tree_prefix: prefix.map(|prefix| BString::from(prefix.as_str())),
                modification_time: 0,
            },
        );
        match (written, spool.overflowed) {
            (_, true) => Err(GitError::ArchiveTooLarge { limit }),
            (Ok(()), false) => Ok(()),
            (Err(error), false) => Err(backend(error)),
        }
    }

    fn bound_archive_source(
        &self,
        tree: gix::ObjectId,
        limit: ArchiveLimit,
        nesting: usize,
        spooled: &mut u64,
    ) -> Result<(), GitError> {
        if nesting == 0 {
            return Err(GitError::DepthExceeded("tree nesting"));
        }
        if tree == gix::ObjectId::empty_tree(self.git().object_hash()) {
            return Ok(());
        }
        let object = self.git().find_tree(tree).map_err(backend)?;
        let decoded = object
            .decode()
            .map_err(|error| GitError::Decode(error.to_string()))?;
        decoded.entries.iter().try_for_each(|entry| {
            let oid = entry.oid.to_owned();
            *spooled = spooled.saturating_add(TAR_BLOCK);
            match entry.mode.kind() {
                _ if *spooled > limit.get() => Err(GitError::ArchiveTooLarge { limit }),
                gix::objs::tree::EntryKind::Commit => Ok(()),
                gix::objs::tree::EntryKind::Tree => {
                    self.bound_archive_source(oid, limit, nesting - 1, spooled)
                }
                _ => {
                    let content = self.blob_size(Oid::from(oid))?;
                    *spooled = spooled.saturating_add(content.next_multiple_of(TAR_BLOCK));
                    match *spooled > limit.get() {
                        true => Err(GitError::ArchiveTooLarge { limit }),
                        false => Ok(()),
                    }
                }
            }
        })
    }
}

struct BoundedSpool<W> {
    inner: W,
    position: u64,
    limit: ArchiveLimit,
    overflowed: bool,
}

impl<W: Write> Write for BoundedSpool<W> {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        let remaining = self.limit.get().saturating_sub(self.position);
        if data.len() as u64 > remaining {
            self.overflowed = true;
            return Err(std::io::Error::from(std::io::ErrorKind::WriteZero));
        }
        let written = self.inner.write(data)?;
        self.position = self.position.saturating_add(written as u64);
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl<W: Seek> Seek for BoundedSpool<W> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let position = self.inner.seek(pos)?;
        self.position = position;
        Ok(position)
    }
}

#[cfg(test)]
mod tests {
    use super::{ArchiveLimit, ArchivePrefix, BoundedSpool};
    use std::io::{Seek, SeekFrom, Write};

    fn spool(limit: u64) -> BoundedSpool<std::io::Cursor<Vec<u8>>> {
        BoundedSpool {
            inner: std::io::Cursor::new(Vec::new()),
            position: 0,
            limit: ArchiveLimit::new(limit),
            overflowed: false,
        }
    }

    #[test]
    fn the_spool_refuses_the_write_that_would_pass_the_limit() {
        let mut spool = spool(8);
        assert!(spool.write_all(b"12345678").is_ok());
        assert!(!spool.overflowed);
        assert!(spool.write_all(b"9").is_err());
        assert!(spool.overflowed);
        assert_eq!(
            spool.inner.into_inner(),
            b"12345678",
            "the refused write never reaches the inner writer"
        );
    }

    #[test]
    fn a_seek_backwards_re_credits_the_budget_the_zip_writer_rewinds_over() {
        let mut spool = spool(8);
        spool.write_all(b"12345678").unwrap();
        spool.seek(SeekFrom::Start(4)).unwrap();
        assert_eq!(spool.position, 4);
        spool
            .write_all(b"abcd")
            .expect("rewriting bytes already counted stays within the limit");
        assert!(!spool.overflowed);
    }

    #[test]
    fn a_write_whose_length_would_overflow_the_position_is_refused() {
        let mut spool = spool(u64::MAX);
        spool.position = u64::MAX;
        assert!(
            spool.write_all(b"1").is_err(),
            "the position saturates at u64::MAX, so the spool must refuse the write"
        );
        assert!(spool.overflowed);
    }

    #[test]
    fn a_plain_nested_prefix_is_accepted() {
        assert!(ArchivePrefix::new("squid-main").is_ok());
        assert!(ArchivePrefix::new("nested/path").is_ok());
    }

    #[test]
    fn traversal_is_rejected_across_both_separators() {
        assert!(ArchivePrefix::new("../escape").is_err());
        assert!(ArchivePrefix::new("nested/../escape").is_err());
        assert!(ArchivePrefix::new("..\\escape").is_err());
        assert!(ArchivePrefix::new("nested\\..\\escape").is_err());
        assert!(
            ArchivePrefix::new("dotted-..-name/").is_ok(),
            "a component that merely contains dot-dot is not a traversal"
        );
    }

    #[test]
    fn absolute_and_null_bearing_prefixes_are_rejected() {
        assert!(ArchivePrefix::new("/etc").is_err());
        assert!(ArchivePrefix::new("\\windows").is_err());
        assert!(ArchivePrefix::new("good\0bad").is_err());
    }
}
