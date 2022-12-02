use num_traits::Zero;
use serde_derive::{Deserialize, Serialize};
use twenty_first::shared_math::b_field_element::BFieldElement;
use twenty_first::util_types::algebraic_hasher::Hashable;

use super::shared::CHUNK_SIZE;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub bits: Vec<u32>,
}

impl Chunk {
    pub fn empty_chunk() -> Self {
        let bits = vec![];
        Chunk { bits }
    }

    pub fn set_bit(&mut self, index: u32) {
        assert!(
            index < CHUNK_SIZE as u32,
            "index cannot exceed chunk size in `set_bit`. CHUNK_SIZE = {}, got index = {}",
            CHUNK_SIZE,
            index
        );
        self.bits.push(index);
        self.bits.sort();
    }

    pub fn unset_bit(&mut self, index: u32) {
        assert!(
            index < CHUNK_SIZE as u32,
            "index cannot exceed chunk size in `unset_bit`. CHUNK_SIZE = {}, got index = {}",
            CHUNK_SIZE,
            index
        );
        let mut drops = vec![];
        for i in 0..self.bits.len() {
            if self.bits[i] == index {
                drops.push(i);
            }
        }

        for d in drops.iter().rev() {
            self.bits.remove(*d);
        }
    }

    pub fn get_bit(&self, index: u32) -> bool {
        assert!(
            index < CHUNK_SIZE as u32,
            "index cannot exceed chunk size in `get_bit`. CHUNK_SIZE = {}, got index = {}",
            CHUNK_SIZE,
            index
        );

        self.bits.contains(&index)
    }

    pub fn or(self, other: Self) -> Self {
        let mut ret = Self::empty_chunk();
        for idx in self.bits {
            ret.bits.push(idx);
        }
        for idx in other.bits {
            ret.bits.push(idx);
        }
        ret.bits.sort();
        ret
    }

    pub fn xor_assign(&mut self, other: Self) {
        let mut drops = vec![];
        for (i, idx) in self.bits.iter().enumerate() {
            if other.bits.contains(idx) {
                drops.push(i);
            }
        }
        for idx in other.bits {
            if !self.bits.contains(&idx) {
                self.bits.push(idx);
            }
        }
        for d in drops.iter().rev() {
            self.bits.remove(*d);
        }
        self.bits.sort();
    }

    pub fn and(self, other: Self) -> Self {
        let mut ret = Self::empty_chunk();
        for idx in self.bits {
            if other.bits.contains(&idx) {
                ret.bits.push(idx);
            }
        }

        ret
    }

    pub fn is_unset(&self) -> bool {
        self.bits.iter().all(|x| x.is_zero())
    }

    pub fn to_indices(&self) -> Vec<u128> {
        self.bits.iter().map(|i| *i as u128).collect()
    }

    pub fn from_indices(indices: &[u128]) -> Self {
        let bits = indices.iter().map(|i| *i as u32).collect();
        Chunk { bits }
    }

    pub fn from_slice(sl: &[u32]) -> Chunk {
        Chunk { bits: sl.to_vec() }
    }
}

impl Hashable for Chunk {
    fn to_sequence(&self) -> Vec<BFieldElement> {
        self.bits
            .iter()
            .flat_map(|&val| val.to_sequence())
            .collect()
    }
}

#[cfg(test)]
mod chunk_tests {
    use num_traits::Zero;
    use rand::{thread_rng, RngCore};
    use std::collections::HashSet;

    use twenty_first::shared_math::b_field_element::BFieldElement;

    use super::*;

    #[test]
    fn constant_sanity_check_test() {
        // This test assumes that the bits in the chunks window are represented as `u32`s. If they are,
        // then the chunk size should be a multiple of 32.
        assert_eq!(0, CHUNK_SIZE % 32);
    }

    #[test]
    fn get_set_unset_bits_pbt() {
        let mut aw = Chunk::empty_chunk();
        for i in 0..CHUNK_SIZE {
            assert!(!aw.get_bit(i as u32));
        }

        let mut prng = thread_rng();
        for _ in 0..CHUNK_SIZE {
            let index = prng.next_u32() % CHUNK_SIZE as u32;
            let set = prng.next_u32() % 2 == 0;
            if set {
                aw.set_bit(index);
            } else {
                aw.unset_bit(index);
            }

            assert_eq!(set, aw.get_bit(index));
        }

        // Set all bits, then check that they are set
        for i in 0..CHUNK_SIZE {
            aw.set_bit(i as u32);
        }

        for i in 0..CHUNK_SIZE {
            assert!(aw.get_bit(i as u32));
        }
    }

    #[test]
    fn chunk_hashpreimage_test() {
        let zero_chunk = Chunk::empty_chunk();
        let zero_chunk_preimage = zero_chunk.to_sequence();
        assert!(zero_chunk_preimage.iter().all(|elem| elem.is_zero()));

        let mut one_chunk = Chunk::empty_chunk();
        one_chunk.set_bit(32);
        let one_chunk_preimage = one_chunk.to_sequence();

        assert_ne!(zero_chunk_preimage, one_chunk_preimage);

        let mut two_ones_chunk = Chunk::empty_chunk();
        two_ones_chunk.set_bit(32);
        two_ones_chunk.set_bit(33);
        let two_ones_preimage = two_ones_chunk.to_sequence();

        assert_ne!(two_ones_preimage, one_chunk_preimage);
        assert_ne!(two_ones_preimage, zero_chunk_preimage);

        // Verify that setting any bit produces a unique hash-preimage value
        let mut previous_values: HashSet<Vec<BFieldElement>> = HashSet::new();
        for i in 0..CHUNK_SIZE {
            let mut chunk = Chunk::empty_chunk();
            chunk.set_bit(i as u32);
            assert!(previous_values.insert(chunk.to_sequence()));
        }
    }

    #[test]
    fn xor_and_and_and_is_unset_test() {
        let mut chunk_a = Chunk::empty_chunk();
        chunk_a.set_bit(12);
        chunk_a.set_bit(13);

        let mut chunk_b = Chunk::empty_chunk();
        chunk_b.set_bit(48);
        chunk_b.set_bit(13);

        let mut expected_xor = Chunk::empty_chunk();
        expected_xor.set_bit(12);
        expected_xor.set_bit(48);

        let mut chunk_c = chunk_a.clone();
        chunk_c.xor_assign(chunk_b.clone());

        assert_eq!(
            expected_xor, chunk_c,
            "XOR on chunks must behave as expected"
        );

        let mut expected_and = Chunk::empty_chunk();
        expected_and.set_bit(13);

        chunk_c = chunk_a.clone().and(chunk_b.clone());
        assert_eq!(
            expected_and, chunk_c,
            "AND on chunks must behave as expected"
        );

        // Verify that `is_unset` behaves as expected
        assert!(!chunk_a.is_unset());
        assert!(!chunk_b.is_unset());
        assert!(!chunk_c.is_unset());
        assert!(Chunk::empty_chunk().is_unset());
    }

    #[test]
    fn serialization_test() {
        // TODO: You could argue that this test doesn't belong here, as it tests the behavior of
        // an imported library. I included it here, though, because the setup seems a bit clumsy
        // to me so far.
        let chunk = Chunk::empty_chunk();
        let json = serde_json::to_string(&chunk).unwrap();
        let s_back = serde_json::from_str::<Chunk>(&json).unwrap();
        assert!(s_back.bits.iter().all(|&x| x == 0u32));
    }

    #[test]
    fn test_indices() {
        let mut chunk = Chunk::empty_chunk();
        let mut rng = thread_rng();
        let num_insertions = 100;
        for _ in 0..num_insertions {
            let index = rng.next_u32() % (CHUNK_SIZE as u32);
            chunk.set_bit(index);
        }

        let indices = chunk.to_indices();

        let reconstructed_chunk = Chunk::from_indices(&indices);

        assert_eq!(chunk, reconstructed_chunk);
    }
}
