use super::BitPosition;
use crate::error::GitError;

#[derive(Clone)]
pub(crate) struct Bitset {
    words: Vec<u64>,
}

impl Bitset {
    pub(crate) fn zeros(num_bits: usize) -> Self {
        Self {
            words: vec![0u64; num_bits.div_ceil(64)],
        }
    }

    pub(crate) fn from_ewah(
        vector: &gix_bitmap::ewah::Vec,
        num_bits: usize,
    ) -> Result<Self, GitError> {
        if vector.num_bits() > num_bits {
            return Err(GitError::Backend(
                "bitmap entry is wider than the object count".to_string(),
            ));
        }
        let mut bits = Self::zeros(num_bits);
        let complete = vector.for_each_set_bit(|index| {
            (index < num_bits).then(|| bits.set(BitPosition::new(index as u32)))
        });
        match complete {
            Some(()) => Ok(bits),
            None => Err(GitError::Backend("malformed ewah bitmap".to_string())),
        }
    }

    pub(crate) fn set(&mut self, index: BitPosition) {
        let index = index.get() as usize;
        self.words[index / 64] |= 1u64 << (index % 64);
    }

    pub(crate) fn union_with(&mut self, other: &Bitset) {
        self.words
            .iter_mut()
            .zip(&other.words)
            .for_each(|(slot, bits)| *slot |= *bits);
    }

    pub(crate) fn difference_indices<'a>(
        &'a self,
        other: &'a Bitset,
    ) -> impl Iterator<Item = BitPosition> + 'a {
        self.words
            .iter()
            .zip(&other.words)
            .enumerate()
            .flat_map(|(word, (present, absent))| WordBits {
                remaining: present & !absent,
                base: (word as u32) * 64,
            })
    }
}

struct WordBits {
    remaining: u64,
    base: u32,
}

impl Iterator for WordBits {
    type Item = BitPosition;

    fn next(&mut self) -> Option<BitPosition> {
        (self.remaining != 0).then(|| {
            let offset = self.remaining.trailing_zeros();
            self.remaining &= self.remaining - 1;
            BitPosition::new(self.base + offset)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set_bits(bits: &Bitset, other: &Bitset) -> Vec<u32> {
        bits.difference_indices(other)
            .map(BitPosition::get)
            .collect()
    }

    #[test]
    fn difference_yields_ascending_set_minus_set() {
        let mut want = Bitset::zeros(130);
        [1u32, 64, 65, 129]
            .into_iter()
            .for_each(|bit| want.set(BitPosition::new(bit)));
        let mut have = Bitset::zeros(130);
        [64u32, 129]
            .into_iter()
            .for_each(|bit| have.set(BitPosition::new(bit)));
        assert_eq!(set_bits(&want, &have), vec![1, 65]);
    }

    #[test]
    fn union_accumulates_both_operands() {
        let mut acc = Bitset::zeros(70);
        let mut other = Bitset::zeros(70);
        acc.set(BitPosition::new(3));
        other.set(BitPosition::new(3));
        other.set(BitPosition::new(69));
        acc.union_with(&other);
        let empty = Bitset::zeros(70);
        assert_eq!(set_bits(&acc, &empty), vec![3, 69]);
    }
}
