use serde::de::{SeqAccess, Visitor};
use serde::ser::SerializeTuple;
use serde::Deserialize;
use serde_derive::Serialize;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::marker::PhantomData;
use std::ops::IndexMut;
use twenty_first::shared_math::b_field_element::BFieldElement;
use twenty_first::shared_math::rescue_prime_digest::Digest;
use twenty_first::util_types::algebraic_hasher::{AlgebraicHasher, Hashable};

use super::chunk_dictionary::ChunkDictionary;
use super::set_commitment::SetCommitment;
use super::shared::{
    bit_indices_to_hash_map, get_batch_mutation_argument_for_removal_record, BATCH_SIZE,
    CHUNK_SIZE, NUM_TRIALS,
};
use twenty_first::util_types::mmr;
use twenty_first::util_types::mmr::mmr_accumulator::MmrAccumulator;
use twenty_first::util_types::mmr::mmr_trait::Mmr;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BitSet([u128; NUM_TRIALS]);

impl BitSet {
    pub fn new(bits: &[u128; NUM_TRIALS]) -> Self {
        Self { 0: *bits }
    }

    pub fn sort_unstable(&mut self) {
        self.0.sort_unstable();
    }

    pub fn to_vec(&self) -> Vec<u128> {
        self.0.to_vec()
    }

    pub fn to_array(&self) -> [u128; NUM_TRIALS] {
        self.0
    }

    pub fn to_array_mut(&mut self) -> &mut [u128; NUM_TRIALS] {
        &mut self.0
    }
}

impl serde::Serialize for BitSet {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut seq = serializer.serialize_tuple(NUM_TRIALS)?;
        for b in self.0 {
            seq.serialize_element(&b)?;
        }
        seq.end()
    }
}

/// ArrayVisitor
/// Used for deserializing large arrays, with size known at compile time.
/// Credit: MikailBag https://github.com/serde-rs/serde/issues/1937
struct ArrayVisitor<T, const N: usize>(PhantomData<T>);

impl<'de, T, const N: usize> Visitor<'de> for ArrayVisitor<T, N>
where
    T: Deserialize<'de>,
{
    type Value = [T; N];

    fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
        formatter.write_str(&format!("an array of length {}", N))
    }

    #[inline]
    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        // can be optimized using MaybeUninit
        let mut data = Vec::with_capacity(N);
        for _ in 0..N {
            match (seq.next_element())? {
                Some(val) => data.push(val),
                None => return Err(serde::de::Error::invalid_length(N, &self)),
            }
        }
        match data.try_into() {
            Ok(arr) => Ok(arr),
            Err(_) => unreachable!(),
        }
    }
}

impl<'de> Deserialize<'de> for BitSet {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(BitSet::new(&deserializer.deserialize_tuple(
            NUM_TRIALS,
            ArrayVisitor::<u128, NUM_TRIALS>(PhantomData),
        )?))
    }
}

impl Error for RemovalRecordError {}

impl fmt::Display for RemovalRecordError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum RemovalRecordError {
    AlreadyExistingChunk(u128),
    MissingChunkOnUpdateFromAdd(u128),
    MissingChunkOnUpdateFromRemove(u128),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
pub struct RemovalRecord<H: AlgebraicHasher> {
    pub bit_indices: BitSet,
    pub target_chunks: ChunkDictionary<H>,
}

impl<H: AlgebraicHasher> RemovalRecord<H> {
    pub fn batch_update_from_addition<MMR: Mmr<H>>(
        removal_records: &mut [&mut Self],
        mutator_set: &mut SetCommitment<H, MMR>,
    ) -> Result<(), Box<dyn Error>> {
        let new_item_index = mutator_set.aocl.count_leaves();

        // if window does not slide, do nothing
        if !SetCommitment::<H, MMR>::window_slides(new_item_index) {
            return Ok(());
        }

        // window does slide
        let new_chunk = mutator_set.swbf_active.slid_chunk();
        let new_chunk_digest: Digest = H::hash(&new_chunk);

        // Insert the new chunk digest into the accumulator-version of the
        // SWBF MMR to get its authentication path. It's important to convert the MMR
        // to an MMR Accumulator here, since we don't want to drag around or clone
        // a whole archival MMR for this operation, as the archival MMR can be in the
        // size of gigabytes, whereas the MMR accumulator should be in the size of
        // kilobytes.
        let mut mmra: MmrAccumulator<H> = mutator_set.swbf_inactive.to_accumulator();
        let new_swbf_auth_path: mmr::mmr_membership_proof::MmrMembershipProof<H> =
            mmra.append(new_chunk_digest);

        // Collect all bit indices for all removal records that are being updated
        let mut chunk_index_to_mp_index: HashMap<u128, Vec<usize>> = HashMap::new();
        removal_records.iter().enumerate().for_each(|(i, rr)| {
            let bits = &rr.bit_indices;
            let chunks_set: HashSet<u128> = bits
                .to_array()
                .iter()
                .map(|x| x / CHUNK_SIZE as u128)
                .collect();
            chunks_set.iter().for_each(|chnkidx| {
                chunk_index_to_mp_index
                    .entry(*chnkidx)
                    .or_insert_with(Vec::new)
                    .push(i)
            });
        });

        // Find the removal records that need a new dictionary entry for the chunk that's being
        // added to the inactive part by this addition.
        let batch_index = new_item_index / BATCH_SIZE as u128;
        let old_window_start_batch_index = batch_index - 1;
        let rrs_for_new_chunk_dictionary_entry: Vec<usize> =
            match chunk_index_to_mp_index.get(&old_window_start_batch_index) {
                Some(vals) => vals.clone(),
                None => vec![],
            };

        // Find the removal records that have dictionary entry MMR membership proofs that need
        // to be updated because of the window sliding.
        let mut rrs_for_batch_append: HashSet<usize> = HashSet::new();
        for (chunk_index, mp_indices) in chunk_index_to_mp_index.into_iter() {
            if chunk_index < old_window_start_batch_index {
                for mp_index in mp_indices {
                    rrs_for_batch_append.insert(mp_index);
                }
            }
        }

        // Perform the updates

        // First insert the new entry into the chunk dictionary for the removal
        // record that need it.
        for i in rrs_for_new_chunk_dictionary_entry.iter() {
            removal_records
                .index_mut(*i)
                .target_chunks
                .dictionary
                .insert(
                    old_window_start_batch_index,
                    (new_swbf_auth_path.clone(), new_chunk.clone()),
                );
        }

        // Collect those MMR membership proofs for chunks whose authentication
        // path might need to be updated due to the insertion of a new leaf in the
        // SWBF MMR.
        // This is a bit ugly and a bit slower than it could be. To prevent this
        // for-loop, you probably could collect the `Vec<&mut mp>` in the code above,
        // instead of just collecting the indices into the removal record vector.
        // It is, however, quite acceptable that many of the MMR membership proofs are
        // repeated since the MMR `batch_update_from_append` handles this optimally.
        // So relegating that bookkeeping to this function instead would not be more
        // efficient.
        let mut mmr_membership_proofs_for_append: Vec<
            &mut mmr::mmr_membership_proof::MmrMembershipProof<H>,
        > = vec![];
        for (i, rr) in removal_records.iter_mut().enumerate() {
            if rrs_for_batch_append.contains(&i) {
                for (_, (mmr_mp, _chnk)) in rr.target_chunks.dictionary.iter_mut() {
                    mmr_membership_proofs_for_append.push(mmr_mp);
                }
            }
        }

        // Perform the update of all the MMR membership proofs contained in the removal records
        mmr::mmr_membership_proof::MmrMembershipProof::<H>::batch_update_from_append(
            &mut mmr_membership_proofs_for_append,
            mutator_set.swbf_inactive.count_leaves(),
            &new_chunk_digest,
            &mutator_set.swbf_inactive.get_peaks(),
        );

        Ok(())
    }

    pub fn batch_update_from_remove(
        removal_records: &mut [&mut Self],
        applied_removal_record: &RemovalRecord<H>,
    ) -> Result<(), Box<dyn Error>> {
        // Set all chunk values to the new values and calculate the mutation argument
        // for the batch updating of the MMR membership proofs.
        let mut chunk_dictionaries: Vec<&mut ChunkDictionary<H>> = removal_records
            .iter_mut()
            .map(|mp| &mut mp.target_chunks)
            .collect();
        let (_mutated_chunks_by_rr_indices, mutation_argument) =
            get_batch_mutation_argument_for_removal_record(
                applied_removal_record,
                &mut chunk_dictionaries,
            );

        // Collect all the MMR membership proofs from the chunk dictionaries.
        let mut own_mmr_mps: Vec<&mut mmr::mmr_membership_proof::MmrMembershipProof<H>> = vec![];
        for chunk_dict in chunk_dictionaries.iter_mut() {
            for (_, (mp, _)) in chunk_dict.dictionary.iter_mut() {
                own_mmr_mps.push(mp);
            }
        }

        // Perform the batch mutation of the MMR membership proofs
        mmr::mmr_membership_proof::MmrMembershipProof::batch_update_from_batch_leaf_mutation(
            &mut own_mmr_mps,
            mutation_argument,
        );

        Ok(())
    }

    /// Validates that a removal record is synchronized against the inactive part of the SWBF
    pub fn validate<M>(&self, mutator_set: &mut SetCommitment<H, M>) -> bool
    where
        M: Mmr<H>,
    {
        let peaks = mutator_set.swbf_inactive.get_peaks();
        self.target_chunks
            .dictionary
            .iter()
            .all(|(_i, (proof, chunk))| {
                let leaf_digest = H::hash(chunk);
                let leaf_count = mutator_set.swbf_inactive.count_leaves();
                let (verified, _final_state) = proof.verify(&peaks, &leaf_digest, leaf_count);

                verified
            })
    }

    /// Returns a hashmap from chunk index to chunk.
    pub fn get_chunk_index_to_bit_indices(&self) -> HashMap<u128, Vec<u128>> {
        bit_indices_to_hash_map(&self.bit_indices.to_array())
    }
}

impl<H: AlgebraicHasher> Hashable for RemovalRecord<H> {
    fn to_sequence(&self) -> Vec<BFieldElement> {
        self.bit_indices
            .to_array()
            .iter()
            .flat_map(|bi| bi.to_sequence())
            .chain(self.target_chunks.to_sequence())
            .collect()
    }
}

#[cfg(test)]
mod removal_record_tests {
    use itertools::Itertools;
    use rand::seq::SliceRandom;
    use rand::{thread_rng, RngCore};

    use crate::test_shared::mutator_set::make_item_and_randomness;
    use crate::util_types::mutator_set::addition_record::AdditionRecord;
    use crate::util_types::mutator_set::ms_membership_proof::MsMembershipProof;
    use crate::util_types::mutator_set::mutator_set_accumulator::MutatorSetAccumulator;
    use crate::util_types::mutator_set::mutator_set_trait::MutatorSet;
    use crate::util_types::mutator_set::shared::{CHUNK_SIZE, NUM_TRIALS};

    use twenty_first::shared_math::rescue_prime_regular::RescuePrimeRegular;
    use twenty_first::utils::{self, has_unique_elements};

    use super::*;

    fn get_mp_and_removal_record() -> (
        MsMembershipProof<RescuePrimeRegular>,
        RemovalRecord<RescuePrimeRegular>,
    ) {
        type H = RescuePrimeRegular;
        let mut accumulator: MutatorSetAccumulator<H> = MutatorSetAccumulator::default();
        let (item, randomness) = make_item_and_randomness();
        let mp: MsMembershipProof<RescuePrimeRegular> = accumulator.prove(&item, &randomness, true);
        let removal_record: RemovalRecord<H> = accumulator.drop(&item, &mp);
        (mp, removal_record)
    }

    #[test]
    fn verify_that_bit_indices_are_sorted_test() {
        let (_mp, removal_record) = get_mp_and_removal_record();

        let bit_indices = removal_record.bit_indices;
        let mut bit_indices_sorted = bit_indices.clone();
        bit_indices_sorted.sort_unstable();
        assert_eq!(
            bit_indices.clone(),
            bit_indices_sorted,
            "bit indices must sorted in the removal record"
        );

        // Alternative way of checking that the indices are sorted (thanks, IRC)
        assert!(
            bit_indices.to_array().windows(2).all(|s| s[0] < s[1]),
            "bit-indices must be sorted"
        );
    }

    #[test]
    fn hash_test() {
        type H = RescuePrimeRegular;

        let (_mp, removal_record) = get_mp_and_removal_record();

        let mut removal_record_alt: RemovalRecord<H> = removal_record.clone();
        assert_eq!(
            H::hash(&removal_record),
            H::hash(&removal_record_alt),
            "Same removal record must hash to same value"
        );
        removal_record_alt.bit_indices.to_array_mut()[NUM_TRIALS / 4] += 1;

        // Sanity check (theoretically, a collision in the bit indices could have happened)
        assert!(
            utils::has_unique_elements(removal_record_alt.bit_indices.to_array()),
            "Sanity check to ensure that bit indices are still all unique"
        );
        assert_ne!(
            H::hash(&removal_record),
            H::hash(&removal_record_alt),
            "Changing a bit index must produce a new hash"
        );
    }

    #[test]
    fn get_chunk_index_to_bit_indices_test() {
        let (mp, removal_record) = get_mp_and_removal_record();

        let chunks2bits = removal_record.get_chunk_index_to_bit_indices();

        // Verify that no indices are repeated in the hash map
        let mut all_bits: Vec<u128> = chunks2bits.clone().into_values().concat();
        all_bits.sort_unstable();
        let mut cached_bits = mp.cached_bits.unwrap();
        cached_bits.sort_unstable();
        assert_eq!(cached_bits.to_vec(), all_bits);
        assert!(has_unique_elements(all_bits.clone()));
        all_bits.dedup();
        assert_eq!(NUM_TRIALS, all_bits.len());

        // Verify that the hash map has put the indices into the correct buckets
        for (key, values) in chunks2bits {
            for value in values {
                assert!((value - key * CHUNK_SIZE as u128) < CHUNK_SIZE as u128);
            }
        }
    }

    #[test]
    fn serialization_test() {
        // TODO: You could argue that this test doesn't belong here, as it tests the behavior of
        // an imported library. I included it here, though, because the setup seems a bit clumsy
        // to me so far.
        type H = RescuePrimeRegular;

        let (_mp, removal_record) = get_mp_and_removal_record();

        let json: String = serde_json::to_string(&removal_record).unwrap();
        let s_back = serde_json::from_str::<RemovalRecord<H>>(&json).unwrap();
        assert_eq!(s_back.bit_indices, removal_record.bit_indices);
        assert_eq!(s_back.target_chunks, removal_record.target_chunks);
    }

    #[test]
    fn simple_remove_test() {
        // Verify that a single element can be added to and removed from the mutator set
        type H = RescuePrimeRegular;
        let mut accumulator: MutatorSetAccumulator<H> = MutatorSetAccumulator::default();
        let (item, randomness) = make_item_and_randomness();
        let mut addition_record: AdditionRecord = accumulator.commit(&item, &randomness);
        let mp = accumulator.prove(&item, &randomness, true);

        assert!(
            !accumulator.verify(&item, &mp),
            "Item must fail to verify before it is added"
        );
        accumulator.add(&mut addition_record);
        let rr = accumulator.drop(&item, &mp);
        assert!(
            accumulator.verify(&item, &mp),
            "Item must succeed in verification after it is added"
        );
        accumulator.remove(&rr);
        assert!(
            !accumulator.verify(&item, &mp),
            "Item must fail to verify after it is removed"
        );
    }

    #[test]
    fn batch_update_from_addition_pbt() {
        // Verify that a single element can be added to and removed from the mutator set
        type H = RescuePrimeRegular;
        let mut accumulator: MutatorSetAccumulator<H> = MutatorSetAccumulator::default();

        let test_iterations = 10;
        for _ in 0..test_iterations {
            let mut removal_records: Vec<(usize, RemovalRecord<H>)> = vec![];
            let mut items = vec![];
            let mut mps = vec![];
            for i in 0..2 * BATCH_SIZE + 4 {
                let (item, randomness) = make_item_and_randomness();

                let mut addition_record: AdditionRecord = accumulator.commit(&item, &randomness);
                let mp = accumulator.prove(&item, &randomness, true);

                // Update all removal records from addition, then add the element
                let update_res_rr = RemovalRecord::batch_update_from_addition(
                    &mut removal_records
                        .iter_mut()
                        .map(|x| &mut x.1)
                        .collect::<Vec<_>>(),
                    &mut accumulator.set_commitment,
                );
                assert!(
                    update_res_rr.is_ok(),
                    "batch update must return OK, i = {}",
                    i
                );
                let update_res_mp = MsMembershipProof::batch_update_from_addition(
                    &mut mps.iter_mut().collect::<Vec<_>>(),
                    &items,
                    &mut accumulator.set_commitment,
                    &addition_record,
                );
                assert!(
                    update_res_mp.is_ok(),
                    "batch update must return OK, i = {}",
                    i
                );
                accumulator.add(&mut addition_record);
                mps.push(mp.clone());
                items.push(item);

                for removal_record in removal_records.iter().map(|x| &x.1) {
                    assert!(
                        removal_record.validate(&mut accumulator.set_commitment),
                        "removal records must validate, i = {}",
                        i
                    );
                }

                let rr = accumulator.drop(&item, &mp);
                removal_records.push((i as usize, rr));
            }

            // pick a random removal record from the list of all removal records and check that it still
            // works.
            //
            // Note that in order to use more than one of the removal records at this points would require
            // updating the remaining removal records from removal, and that's not what we want to test in
            // this function, so we only test one of the removal records here.
            let (chosen_index, random_removal_record) =
                removal_records.choose(&mut rand::thread_rng()).unwrap();
            assert!(accumulator.verify(&items[*chosen_index], &mps[*chosen_index]));
            accumulator.remove(random_removal_record);
            assert!(!accumulator.verify(&items[*chosen_index], &mps[*chosen_index]));
        }
    }

    #[test]
    fn batch_update_from_addition_and_remove_pbt() {
        // Verify that a single element can be added to and removed from the mutator set
        type H = blake3::Hasher;
        let mut accumulator: MutatorSetAccumulator<H> = MutatorSetAccumulator::default();

        let mut removal_records: Vec<(usize, RemovalRecord<H>)> = vec![];
        let mut items = vec![];
        let mut mps = vec![];
        for i in 0..12 * BATCH_SIZE + 4 {
            let (item, randomness) = make_item_and_randomness();

            let mut addition_record: AdditionRecord = accumulator.commit(&item, &randomness);
            let mp = accumulator.prove(&item, &randomness, true);

            // Update all removal records from addition, then add the element
            let update_res_rr = RemovalRecord::batch_update_from_addition(
                &mut removal_records
                    .iter_mut()
                    .map(|x| &mut x.1)
                    .collect::<Vec<_>>(),
                &mut accumulator.set_commitment,
            );
            assert!(
                update_res_rr.is_ok(),
                "batch update must return OK, i = {}",
                i
            );
            let update_res_mp = MsMembershipProof::batch_update_from_addition(
                &mut mps.iter_mut().collect::<Vec<_>>(),
                &items,
                &mut accumulator.set_commitment,
                &addition_record,
            );
            assert!(
                update_res_mp.is_ok(),
                "batch update must return OK, i = {}",
                i
            );
            accumulator.add(&mut addition_record);
            mps.push(mp.clone());
            items.push(item);

            for removal_record in removal_records.iter().map(|x| &x.1) {
                assert!(
                    removal_record.validate(&mut accumulator.set_commitment),
                    "removal records must validate, i = {}",
                    i
                );
            }

            let rr = accumulator.drop(&item, &mp);
            removal_records.push((i as usize, rr));
        }

        // Now apply all removal records one at a time and batch update the remaining removal records
        for i in 0..12 * BATCH_SIZE + 4 {
            let (_chosen_index, random_removal_record) = removal_records
                .choose(&mut rand::thread_rng())
                .unwrap()
                .clone();
            let update_res_rr = RemovalRecord::batch_update_from_remove(
                &mut removal_records
                    .iter_mut()
                    .map(|x| &mut x.1)
                    .collect::<Vec<_>>(),
                &random_removal_record,
            );
            assert!(
                update_res_rr.is_ok(),
                "batch update must return OK, i = {}",
                i
            );

            accumulator.remove(&random_removal_record);

            for removal_record in removal_records.iter().map(|x| &x.1) {
                assert!(
                    removal_record.validate(&mut accumulator.set_commitment),
                    "removal records must validate, i = {}",
                    i
                );
            }
        }
    }

    #[test]
    fn test_bit_set_serialization() {
        let mut rng = thread_rng();
        let original_bitset = BitSet::new(
            &(0..NUM_TRIALS)
                .map(|_| ((rng.next_u64() as u128) << 64) | (rng.next_u64() as u128))
                .collect_vec()
                .try_into()
                .unwrap(),
        );
        let serialized_bitset = serde_json::to_string(&original_bitset).unwrap();
        let reconstructed_bitset: BitSet = serde_json::from_str(&serialized_bitset).unwrap();

        assert_eq!(original_bitset, reconstructed_bitset);
    }
}
