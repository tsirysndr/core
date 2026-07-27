use std::sync::atomic::AtomicBool;

use gix::bstr::BString;
use knot_types::{Oid, ParseError};

use crate::error::{GitError, backend};
use crate::repo::Repo;

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
        mut out: impl std::io::Write + std::io::Seek,
    ) -> Result<(), GitError> {
        let (stream, _index) = self
            .git()
            .worktree_stream(tree.object_id())
            .map_err(backend)?;
        let interrupt = AtomicBool::new(false);
        self.git()
            .worktree_archive(
                stream,
                &mut out,
                gix::progress::Discard,
                &interrupt,
                gix_archive::Options {
                    format: format.gix(),
                    tree_prefix: prefix.map(|prefix| BString::from(prefix.as_str())),
                    modification_time: 0,
                },
            )
            .map_err(backend)
    }
}

#[cfg(test)]
mod tests {
    use super::ArchivePrefix;

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
