use super::{Error, traverse};

#[allow(missing_docs)]
pub struct Item<T> {
    pub offset: crate::data::Offset,
    pub next_offset: crate::data::Offset,
    pub data: T,
}

impl<T> Item<T> {
    pub(crate) fn new(
        offset: crate::data::Offset,
        next_offset: crate::data::Offset,
        data: T,
    ) -> Self {
        Item {
            offset,
            next_offset,
            data,
        }
    }
}

enum Parent {
    Root,
    Base(u32),
    Pending(crate::data::Offset),
}

#[allow(missing_docs)]
pub struct Tree<T> {
    offsets: Vec<crate::data::Offset>,
    data: Vec<T>,
    parent: Vec<Parent>,
}

#[allow(missing_docs)]
impl<T> Tree<T> {
    pub fn with_capacity(num_objects: usize) -> Result<Self, Error> {
        Ok(Tree {
            offsets: Vec::with_capacity(num_objects),
            data: Vec::with_capacity(num_objects),
            parent: Vec::with_capacity(num_objects),
        })
    }

    pub(super) fn num_items(&self) -> usize {
        self.offsets.len()
    }

    fn assert_incrementing(&self, offset: crate::data::Offset) -> Result<(), Error> {
        match self.offsets.last() {
            Some(&last) if offset <= last => Err(Error::InvariantIncreasingPackOffset {
                last_pack_offset: last,
                pack_offset: offset,
            }),
            _ => Ok(()),
        }
    }

    pub fn add_root(&mut self, offset: crate::data::Offset, data: T) -> Result<(), Error> {
        self.assert_incrementing(offset)?;
        self.offsets.push(offset);
        self.data.push(data);
        self.parent.push(Parent::Root);
        Ok(())
    }

    pub fn add_child(
        &mut self,
        base_offset: crate::data::Offset,
        offset: crate::data::Offset,
        data: T,
    ) -> Result<(), Error> {
        self.assert_incrementing(offset)?;
        let parent = match self.offsets.binary_search(&base_offset) {
            Ok(index) => Parent::Base(index as u32),
            Err(_) => Parent::Pending(base_offset),
        };
        self.offsets.push(offset);
        self.data.push(data);
        self.parent.push(parent);
        Ok(())
    }

    pub(super) fn into_forest(
        self,
        pack_entries_end: crate::data::Offset,
    ) -> Result<(Forest<T>, Vec<u32>), traverse::Error> {
        let Tree {
            offsets,
            data,
            mut parent,
        } = self;
        let num_nodes = offsets.len();

        let mut child_start = vec![0u32; num_nodes + 1];
        let mut roots: Vec<u32> = Vec::new();
        for index in 0..num_nodes {
            if let Parent::Pending(base_offset) = parent[index] {
                let base = offsets.binary_search(&base_offset).map_err(|_| {
                    traverse::Error::OutOfPackRefDelta {
                        base_pack_offset: base_offset,
                    }
                })?;
                parent[index] = Parent::Base(base as u32);
            }
            match parent[index] {
                Parent::Root => roots.push(index as u32),
                Parent::Base(base) => child_start[base as usize + 1] += 1,
                Parent::Pending(_) => unreachable!("pending parents were resolved above"),
            }
        }
        for index in 0..num_nodes {
            child_start[index + 1] += child_start[index];
        }
        let mut cursor = child_start.clone();
        let mut child_ids = vec![0u32; child_start[num_nodes] as usize];
        for index in 0..num_nodes {
            if let Parent::Base(base) = parent[index] {
                let slot = cursor[base as usize];
                child_ids[slot as usize] = index as u32;
                cursor[base as usize] = slot + 1;
            }
        }

        Ok((
            Forest {
                offsets,
                data,
                child_start,
                child_ids,
                pack_entries_end,
            },
            roots,
        ))
    }
}

pub(super) struct Forest<T> {
    offsets: Vec<crate::data::Offset>,
    pub(super) data: Vec<T>,
    child_start: Vec<u32>,
    child_ids: Vec<u32>,
    pack_entries_end: crate::data::Offset,
}

impl<T> Forest<T> {
    pub(super) fn offset(&self, id: u32) -> crate::data::Offset {
        self.offsets[id as usize]
    }

    pub(super) fn next_offset(&self, id: u32) -> crate::data::Offset {
        self.offsets
            .get(id as usize + 1)
            .copied()
            .unwrap_or(self.pack_entries_end)
    }

    pub(super) fn entry_slice(&self, id: u32) -> crate::data::EntryRange {
        self.offset(id)..self.next_offset(id)
    }

    pub(super) fn children(&self, id: u32) -> &[u32] {
        let start = self.child_start[id as usize] as usize;
        let end = self.child_start[id as usize + 1] as usize;
        &self.child_ids[start..end]
    }

    pub(super) fn item(&self, id: u32, data: T) -> Item<T> {
        Item::new(self.offset(id), self.next_offset(id), data)
    }
}

#[cfg(test)]
mod tests {
    mod from_offsets_in_pack {
        use std::sync::atomic::AtomicBool;

        use crate as pack;

        const SMALL_PACK_INDEX: &str =
            "objects/pack/pack-a2bf8e71d8c18879e499335762dd95119d93d9f1.idx";
        const SMALL_PACK: &str = "objects/pack/pack-a2bf8e71d8c18879e499335762dd95119d93d9f1.pack";

        const INDEX_V1: &str = "objects/pack/pack-c0438c19fb16422b6bbcce24387b3264416d485b.idx";
        const PACK_FOR_INDEX_V1: &str =
            "objects/pack/pack-c0438c19fb16422b6bbcce24387b3264416d485b.pack";

        use gix_testtools::fixture_path;

        #[test]
        fn v1() -> Result<(), Box<dyn std::error::Error>> {
            tree(INDEX_V1, PACK_FOR_INDEX_V1)
        }

        #[test]
        fn v2() -> Result<(), Box<dyn std::error::Error>> {
            tree(SMALL_PACK_INDEX, SMALL_PACK)
        }

        fn tree(index_path: &str, pack_path: &str) -> Result<(), Box<dyn std::error::Error>> {
            let idx = pack::index::File::at(fixture_path(index_path), gix_hash::Kind::Sha1)?;
            crate::cache::delta::Tree::from_offsets_in_pack(
                &fixture_path(pack_path),
                idx.sorted_offsets().into_iter(),
                &|ofs| *ofs,
                &|id| idx.lookup(id).map(|index| idx.pack_offset_at_index(index)),
                &mut gix_features::progress::Discard,
                &AtomicBool::new(false),
                gix_hash::Kind::Sha1,
            )?;
            Ok(())
        }
    }

    mod size {
        use gix_testtools::size_ok;

        use super::super::Item;

        #[test]
        fn size_of_pack_tree_item() {
            let actual = std::mem::size_of::<[Item<()>; 7_500_000]>();
            let expected = 120_000_000;
            assert!(
                size_ok(actual, expected),
                "we don't want these to grow unnoticed: {actual} <~ {expected}"
            );
        }
    }
}
