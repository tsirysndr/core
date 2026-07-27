use std::collections::HashMap;

use knot_types::Oid;

use super::{BitPosition, IndexPosition};

pub(crate) trait Order {
    fn len(&self) -> usize;
    fn index_of(&self, oid: Oid) -> Option<IndexPosition>;
    fn bit_at_index(&self, position: IndexPosition) -> BitPosition;
    fn oid_at_bit(&self, bit: BitPosition) -> Oid;
}

pub(crate) struct OrderTable {
    oids: Vec<Oid>,
    bit_to_index: Vec<IndexPosition>,
    index_to_bit: Vec<BitPosition>,
    by_oid: HashMap<Oid, IndexPosition>,
}

impl OrderTable {
    fn build<K: Ord>(oids: Vec<Oid>, placement: impl Fn(u32) -> K) -> Self {
        let count = oids.len() as u32;
        let mut order: Vec<u32> = (0..count).collect();
        order.sort_by_key(|position| placement(*position));
        let bit_to_index: Vec<IndexPosition> = order
            .iter()
            .map(|position| IndexPosition::new(*position))
            .collect();
        let index_to_bit = order.iter().enumerate().fold(
            vec![BitPosition::new(0); oids.len()],
            |mut table, (bit, position)| {
                table[*position as usize] = BitPosition::new(bit as u32);
                table
            },
        );
        let by_oid: HashMap<Oid, IndexPosition> = oids
            .iter()
            .enumerate()
            .map(|(position, oid)| (*oid, IndexPosition::new(position as u32)))
            .collect();
        Self {
            oids,
            bit_to_index,
            index_to_bit,
            by_oid,
        }
    }

    pub(crate) fn from_index(index: &gix_pack::index::File) -> Self {
        let count = index.num_objects();
        let oids: Vec<Oid> = (0..count)
            .map(|position| Oid::from(index.oid_at_index(position).to_owned()))
            .collect();
        let offsets: Vec<u64> = (0..count)
            .map(|position| index.pack_offset_at_index(position))
            .collect();
        Self::build(oids, |position| offsets[position as usize])
    }

    pub(crate) fn from_file(file: &gix_pack::multi_index::File) -> Self {
        let count = file.num_objects();
        let oids: Vec<Oid> = (0..count)
            .map(|position| Oid::from(file.oid_at_index(position).to_owned()))
            .collect();
        let placement: Vec<(u32, u64)> = (0..count)
            .map(|position| file.pack_id_and_pack_offset_at_index(position))
            .collect();
        Self::build(oids, |position| placement[position as usize])
    }

    pub(crate) fn index_positions_in_bit_order(&self) -> &[IndexPosition] {
        &self.bit_to_index
    }
}

impl Order for OrderTable {
    fn len(&self) -> usize {
        self.oids.len()
    }

    fn index_of(&self, oid: Oid) -> Option<IndexPosition> {
        self.by_oid.get(&oid).copied()
    }

    fn bit_at_index(&self, position: IndexPosition) -> BitPosition {
        self.index_to_bit[position.get() as usize]
    }

    fn oid_at_bit(&self, bit: BitPosition) -> Oid {
        self.oids[self.bit_to_index[bit.get() as usize].get() as usize]
    }
}
