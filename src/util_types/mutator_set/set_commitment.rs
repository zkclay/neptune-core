use std::{collections::HashMap, error::Error, fmt};

use rayon::iter::{IndexedParallelIterator, IntoParallelIterator, ParallelIterator};

use super::{
    addition_record::AdditionRecord, chunk_dictionary::ChunkDictionary,
    membership_proof::MembershipProof, removal_record::RemovalRecord,
    shared::bit_indices_to_hash_map,
};
use crate::{
    shared_math::b_field_element::BFieldElement,
    util_types::{
        mmr::{self, mmr_trait::Mmr},
        mutator_set::chunk::Chunk,
        simple_hasher::{Hasher, ToDigest},
    },
};

pub const WINDOW_SIZE: usize = 30000;
pub const CHUNK_SIZE: usize = 1500;
pub const BATCH_SIZE: usize = 10;
pub const NUM_TRIALS: usize = 160; // TODO: Change to 160 in production

impl Error for SetCommitmentError {}

impl fmt::Display for SetCommitmentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self)
    }
}

#[derive(PartialEq, Eq, Debug)]
pub enum SetCommitmentError {
    RequestedAoclAuthPathOutOfBounds((u128, u128)),
    RequestedSwbfAuthPathOutOfBounds((u128, u128)),
    MutatorSetIsEmpty,
    RestoreMembershipProofDidNotFindChunkForChunkIndex,
}

#[derive(Clone, Debug)]
pub struct SetCommitment<H: Hasher, MMR: Mmr<H>> {
    pub aocl: MMR,
    pub swbf_inactive: MMR,
    pub swbf_active: [bool; WINDOW_SIZE],
    pub hasher: H,
}

/// Helper function. Computes the bloom filter bit indices of the
/// item, randomness, index triple.
pub fn get_swbf_indices<H: Hasher>(
    hasher: &H,
    item: &H::Digest,
    randomness: &H::Digest,
    aocl_leaf_index: u128,
) -> [u128; NUM_TRIALS]
where
    u128: ToDigest<<H as Hasher>::Digest>,
    Vec<BFieldElement>: ToDigest<<H as Hasher>::Digest>,
{
    let batch_index = aocl_leaf_index / BATCH_SIZE as u128;
    let timestamp: H::Digest = (aocl_leaf_index as u128).to_digest();
    let mut rhs = hasher.hash_pair(&timestamp, randomness);
    rhs = hasher.hash_pair(item, &rhs);
    let mut indices: Vec<u128> = Vec::with_capacity(NUM_TRIALS);

    // Collect all indices in parallel, using counter-mode
    (0..NUM_TRIALS)
        .into_par_iter()
        .map(|i| {
            let counter: H::Digest = (i as u128).to_digest();
            let pseudorandomness = hasher.hash_pair(&counter, &rhs);
            hasher.sample_index_not_power_of_two(&pseudorandomness, WINDOW_SIZE) as u128
                + batch_index * CHUNK_SIZE as u128
        })
        .collect_into_vec(&mut indices);

    // We disallow duplicates, so we have to find N more
    indices.sort_unstable();
    indices.dedup();
    let mut j = NUM_TRIALS;
    while indices.len() < NUM_TRIALS {
        let counter: H::Digest = (j as u128).to_digest();
        let pseudorandomness = hasher.hash_pair(&counter, &rhs);
        let index = hasher.sample_index_not_power_of_two(&pseudorandomness, WINDOW_SIZE) as u128
            + batch_index * CHUNK_SIZE as u128;
        indices.push(index);
        indices.sort_unstable();
        indices.dedup();
        j += 1;
    }

    indices.try_into().unwrap()
}

impl<H, M> SetCommitment<H, M>
where
    u128: ToDigest<<H as Hasher>::Digest>,
    Vec<BFieldElement>: ToDigest<<H as Hasher>::Digest>,
    M: Mmr<H>,
    H: Hasher,
{
    pub fn default() -> Self {
        Self {
            aocl: M::new(vec![]),
            swbf_inactive: M::new(vec![]),
            swbf_active: [false; WINDOW_SIZE as usize],
            hasher: H::new(),
        }
    }

    /// Generates an addition record from an item and explicit random-
    /// ness. The addition record is itself a commitment to the item,
    /// but tailored to adding the item to the mutator set in its
    /// current state.
    pub fn commit(&self, item: &H::Digest, randomness: &H::Digest) -> AdditionRecord<H> {
        let canonical_commitment = self.hasher.hash_pair(item, randomness);

        // It's important to *not* use clone here as that could imply copying a whole
        // archival MMR. Instead we ensure that
        AdditionRecord::new(canonical_commitment, self.aocl.to_accumulator())
    }

    /**
     * drop
     * Generates a removal record with which to update the set commitment.
     */
    pub fn drop(
        &self,
        item: &H::Digest,
        membership_proof: &MembershipProof<H>,
    ) -> RemovalRecord<H> {
        let bit_indices = match membership_proof.cached_bits {
            Some(bits) => bits,
            None => get_swbf_indices(
                &self.hasher,
                item,
                &membership_proof.randomness,
                membership_proof.auth_path_aocl.data_index,
            ),
        };

        RemovalRecord {
            bit_indices,
            target_chunks: membership_proof.target_chunks.clone(),
        }
    }

    /**
     * window_slides
     * Determine if the window slides before absorbing an item,
     * given the index of the to-be-added item.
     */
    pub fn window_slides(index: u128) -> bool {
        index != 0 && index % BATCH_SIZE as u128 == 0

        // example cases:
        //  - index == 0 we don't care about
        //  - index == 1 does not generate a slide
        //  - index == n * BATCH_SIZE generates a slide for any n
    }

    /// Helper function. Like `add` but also returns the chunk that was added to the inactive SWBF
    /// since this is needed by the archival version of the mutator set.
    pub fn add_helper(&mut self, addition_record: &AdditionRecord<H>) -> Option<(u128, Chunk)> {
        // Notice that `add` cannot return a membership proof since `add` cannot know the
        // randomness that was used to create the commitment. This randomness can only be know
        // by the sender and/or receiver of the UTXO. And `add` must be run be all nodes keeping
        // track of the mutator set.
        // verify aocl snapshot
        if !addition_record.has_matching_aocl(&self.aocl.to_accumulator()) {
            panic!("Addition record has aocl snapshot that does not match with the AOCL it is being added to.")
        }

        // add to list
        let item_index = self.aocl.count_leaves();
        self.aocl.append(addition_record.commitment.to_owned()); // ignore auth path

        if !Self::window_slides(item_index) {
            return None;
        }

        // if window slides, update filter
        // First update the inactive part of the SWBF, the SWBF MMR
        let chunk: Chunk = Chunk {
            bits: self.swbf_active[..CHUNK_SIZE].try_into().unwrap(),
        };
        let chunk_digest: H::Digest = chunk.hash::<H>(&self.hasher);
        self.swbf_inactive.append(chunk_digest); // ignore auth path

        // Then move window to the right, equivalent to moving values
        // inside window to the left.
        for i in CHUNK_SIZE..WINDOW_SIZE {
            self.swbf_active[i - CHUNK_SIZE] = self.swbf_active[i];
        }
        for i in (WINDOW_SIZE - CHUNK_SIZE)..WINDOW_SIZE {
            self.swbf_active[i] = false;
        }

        let chunk_index_for_inserted_chunk = self.swbf_inactive.count_leaves() - 1;

        // Return the chunk that was added to the inactive part of the SWBF.
        // This chunk is needed by the Archival mutator set. The Regular
        // mutator set can ignore it.
        Some((chunk_index_for_inserted_chunk, chunk))
    }

    pub fn remove_helper(&mut self, removal_record: &RemovalRecord<H>) -> HashMap<u128, Chunk> {
        let batch_index = (self.aocl.count_leaves() - 1) / BATCH_SIZE as u128;
        let window_start = batch_index * CHUNK_SIZE as u128;

        // set all bits
        let mut new_target_chunks: ChunkDictionary<H> = removal_record.target_chunks.clone();
        let chunk_indices_to_bit_indices: HashMap<u128, Vec<u128>> =
            removal_record.get_chunk_index_to_bit_indices();

        for (chunk_index, bit_indices) in chunk_indices_to_bit_indices {
            if chunk_index >= batch_index {
                for bit_index in bit_indices {
                    let relative_index = bit_index - window_start;
                    self.swbf_active[relative_index as usize] = true;
                }

                continue;
            }

            // If chunk index is not in the active part, set the bits in the relevant chunk
            let mut relevant_chunk = new_target_chunks.dictionary.get_mut(&chunk_index).unwrap();
            for bit_index in bit_indices {
                relevant_chunk.1.bits[(bit_index % CHUNK_SIZE as u128) as usize] = true;
            }
        }

        // update mmr
        // to do this, we need to keep track of all membership proofs
        let mut all_membership_proofs: Vec<_> = new_target_chunks
            .dictionary
            .values()
            .map(|(p, _c)| p.to_owned())
            .collect();
        let all_leafs = new_target_chunks
            .dictionary
            .values()
            .map(|(_p, c)| c.hash::<H>(&self.hasher));
        let mutation_data: Vec<(mmr::membership_proof::MembershipProof<H>, H::Digest)> =
            all_membership_proofs
                .clone()
                .into_iter()
                .zip(all_leafs)
                .collect();

        self.swbf_inactive
            .batch_mutate_leaf_and_update_mps(&mut all_membership_proofs, mutation_data);

        new_target_chunks
            .dictionary
            .into_iter()
            .map(|(chunk_index, (_mp, chunk))| (chunk_index, chunk))
            .collect()
    }

    /**
     * prove
     * Generates a membership proof that will the valid when the item
     * is added to the mutator set.
     */
    pub fn prove(
        &self,
        item: &H::Digest,
        randomness: &H::Digest,
        store_bits: bool,
    ) -> MembershipProof<H> {
        // compute commitment
        let item_commitment = self.hasher.hash_pair(item, randomness);

        // simulate adding to commitment list
        let auth_path_aocl = self.aocl.to_accumulator().append(item_commitment);
        let target_chunks: ChunkDictionary<H> = ChunkDictionary::default();

        // Store the bit indices for later use, as they are expensive to calculate
        let cached_bits: Option<[u128; NUM_TRIALS]> = if store_bits {
            Some(get_swbf_indices(
                &self.hasher,
                item,
                randomness,
                self.aocl.count_leaves(),
            ))
        } else {
            None
        };

        // return membership proof
        MembershipProof {
            randomness: randomness.to_owned(),
            auth_path_aocl,
            target_chunks,
            cached_bits,
        }
    }

    pub fn verify(&self, item: &H::Digest, membership_proof: &MembershipProof<H>) -> bool {
        // If data index does not exist in AOCL, return false
        // This also ensures that no "future" bit indices will be
        // returned from `get_indices`, so we don't have to check for
        // future indices in a separate check.
        if self.aocl.count_leaves() <= membership_proof.auth_path_aocl.data_index {
            return false;
        }

        // verify that a commitment to the item lives in the aocl mmr
        let leaf = self.hasher.hash_pair(item, &membership_proof.randomness);
        let (is_aocl_member, _) = membership_proof.auth_path_aocl.verify(
            &self.aocl.get_peaks(),
            &leaf,
            self.aocl.count_leaves(),
        );
        if !is_aocl_member {
            return false;
        }

        // verify that some indicated bits in the swbf are unset
        let mut has_unset_bits = false;
        let mut entries_in_dictionary = true;
        let mut all_auth_paths_are_valid = true;

        // prepare parameters of inactive part
        let current_batch_index: u128 = (self.aocl.count_leaves() - 1) / BATCH_SIZE as u128;
        let window_start = current_batch_index * CHUNK_SIZE as u128;

        // We use the cached bits if we have them, otherwise they are recalculated
        let all_bit_indices = match membership_proof.cached_bits {
            Some(bits) => bits,
            None => get_swbf_indices(
                &self.hasher,
                item,
                &membership_proof.randomness,
                membership_proof.auth_path_aocl.data_index,
            ),
        };

        let chunk_index_to_bit_indices = bit_indices_to_hash_map(&all_bit_indices);
        'outer: for (chunk_index, bit_indices) in chunk_index_to_bit_indices.into_iter() {
            if chunk_index < current_batch_index {
                // verify mmr auth path
                if !membership_proof
                    .target_chunks
                    .dictionary
                    .contains_key(&chunk_index)
                {
                    entries_in_dictionary = false;
                    break 'outer;
                }

                let mp_and_chunk: &(mmr::membership_proof::MembershipProof<H>, Chunk) =
                    membership_proof
                        .target_chunks
                        .dictionary
                        .get(&chunk_index)
                        .unwrap();
                let (valid_auth_path, _) = mp_and_chunk.0.verify(
                    &self.swbf_inactive.get_peaks(),
                    &mp_and_chunk.1.hash::<H>(&self.hasher),
                    self.swbf_inactive.count_leaves(),
                );

                all_auth_paths_are_valid = all_auth_paths_are_valid && valid_auth_path;

                'inner_inactive: for bit_index in bit_indices {
                    let index_within_chunk = bit_index % CHUNK_SIZE as u128;
                    if !mp_and_chunk.1.bits[index_within_chunk as usize] {
                        has_unset_bits = true;
                        break 'inner_inactive;
                    }
                }
            } else {
                // bits are in active window
                'inner_active: for bit_index in bit_indices {
                    let relative_index = bit_index - window_start;
                    if !self.swbf_active[relative_index as usize] {
                        has_unset_bits = true;
                        break 'inner_active;
                    }
                }
            }
        }

        // return verdict
        is_aocl_member && entries_in_dictionary && all_auth_paths_are_valid && has_unset_bits
    }
}

#[cfg(test)]
mod accumulation_scheme_tests {
    use crate::{
        shared_math::{
            rescue_prime_xlix::{
                neptune_params, RescuePrimeXlix, RP_DEFAULT_OUTPUT_SIZE, RP_DEFAULT_WIDTH,
            },
            traits::GetRandomElements,
        },
        util_types::{
            blake3_wrapper,
            mmr::{archival_mmr::ArchivalMmr, mmr_accumulator::MmrAccumulator},
            simple_hasher::Hasher,
        },
        utils::has_unique_elements,
    };
    use rand::prelude::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::{RngCore, SeedableRng};

    use super::*;

    #[test]
    fn ms_get_indices_test() {
        // Test that `get_indices` behaves as expected. I.e. that it does not return any
        // duplicates, and always returns something of length `NUM_TRIALS`.
        let hasher: RescuePrimeXlix<RP_DEFAULT_WIDTH> = neptune_params();
        let mut prng = thread_rng();
        let item: Vec<BFieldElement> = hasher.hash(
            &BFieldElement::random_elements(3, &mut prng),
            RP_DEFAULT_OUTPUT_SIZE,
        );
        let randomness: Vec<BFieldElement> = hasher.hash(
            &BFieldElement::random_elements(3, &mut prng),
            RP_DEFAULT_OUTPUT_SIZE,
        );
        let ret: [u128; NUM_TRIALS] = get_swbf_indices(&hasher, &item, &randomness, 0);
        assert_eq!(NUM_TRIALS, ret.len());
        assert!(has_unique_elements(ret));
        assert!(ret.iter().all(|&x| x < WINDOW_SIZE as u128));
    }

    #[test]
    fn init_test() {
        SetCommitment::<
            RescuePrimeXlix<RP_DEFAULT_WIDTH>,
            MmrAccumulator<RescuePrimeXlix<RP_DEFAULT_WIDTH>>,
        >::default();
        SetCommitment::<
            RescuePrimeXlix<RP_DEFAULT_WIDTH>,
            ArchivalMmr<RescuePrimeXlix<RP_DEFAULT_WIDTH>>,
        >::default();
    }

    #[test]
    fn verify_future_bits_test() {
        // Ensure that `verify` does not crash when given a membership proof
        // that represents a future addition to the AOCL.
        let mut mutator_set = SetCommitment::<
            RescuePrimeXlix<RP_DEFAULT_WIDTH>,
            MmrAccumulator<RescuePrimeXlix<RP_DEFAULT_WIDTH>>,
        >::default();
        let empty_mutator_set = SetCommitment::<
            RescuePrimeXlix<RP_DEFAULT_WIDTH>,
            MmrAccumulator<RescuePrimeXlix<RP_DEFAULT_WIDTH>>,
        >::default();
        let hasher: RescuePrimeXlix<RP_DEFAULT_WIDTH> = neptune_params();
        let mut prng = thread_rng();
        for _ in 0..2 * BATCH_SIZE + 2 {
            let item: Vec<BFieldElement> = hasher.hash(
                &BFieldElement::random_elements(3, &mut prng),
                RP_DEFAULT_OUTPUT_SIZE,
            );
            let randomness: Vec<BFieldElement> = hasher.hash(
                &BFieldElement::random_elements(3, &mut prng),
                RP_DEFAULT_OUTPUT_SIZE,
            );

            let addition_record: AdditionRecord<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
                mutator_set.commit(&item, &randomness);
            let membership_proof: MembershipProof<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
                mutator_set.prove(&item, &randomness, false);
            mutator_set.add_helper(&addition_record);
            assert!(mutator_set.verify(&item, &membership_proof));

            // Verify that a future membership proof returns false and does not crash
            assert!(!empty_mutator_set.verify(&item, &membership_proof));
        }
    }

    #[test]
    fn test_membership_proof_update_from_add() {
        let mut mutator_set = SetCommitment::<
            RescuePrimeXlix<RP_DEFAULT_WIDTH>,
            MmrAccumulator<RescuePrimeXlix<RP_DEFAULT_WIDTH>>,
        >::default();
        let hasher: RescuePrimeXlix<RP_DEFAULT_WIDTH> = neptune_params();
        let own_item: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1215)], RP_DEFAULT_OUTPUT_SIZE);
        let randomness: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1776)], RP_DEFAULT_OUTPUT_SIZE);

        let addition_record: AdditionRecord<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            mutator_set.commit(&own_item, &randomness);
        let mut membership_proof: MembershipProof<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            mutator_set.prove(&own_item, &randomness, false);
        mutator_set.add_helper(&addition_record);

        // Update membership proof with add operation. Verify that it has changed, and that it now fails to verify.
        let new_item: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1648)], RP_DEFAULT_OUTPUT_SIZE);
        let new_randomness: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1807)], RP_DEFAULT_OUTPUT_SIZE);
        let new_addition_record: AdditionRecord<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            mutator_set.commit(&new_item, &new_randomness);
        let original_membership_proof: MembershipProof<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            membership_proof.clone();
        let changed_mp = match membership_proof.update_from_addition(
            &own_item,
            &mutator_set,
            &new_addition_record,
        ) {
            Ok(changed) => changed,
            Err(err) => panic!("{}", err),
        };
        assert!(
            changed_mp,
            "Update must indicate that membership proof has changed"
        );
        assert_ne!(
            original_membership_proof.auth_path_aocl,
            membership_proof.auth_path_aocl
        );
        assert!(
            mutator_set.verify(&own_item, &original_membership_proof),
            "Original membership proof must verify prior to addition"
        );
        assert!(
            !mutator_set.verify(&own_item, &membership_proof),
            "New membership proof must fail to verify prior to addition"
        );

        // Insert the new element into the mutator set, then verify that the membership proof works and
        // that the original membership proof is invalid.
        mutator_set.add_helper(&new_addition_record);
        assert!(
            !mutator_set.verify(&own_item, &original_membership_proof),
            "Original membership proof must fail to verify after addition"
        );
        assert!(
            mutator_set.verify(&own_item, &membership_proof),
            "New membership proof must verify after addition"
        );
    }

    #[test]
    fn membership_proof_updating_from_add_pbt() {
        type Hasher = blake3::Hasher;
        let mut rng = ChaCha20Rng::from_seed(
            vec![vec![0, 1, 4, 33], vec![0; 28]]
                .concat()
                .try_into()
                .unwrap(),
        );

        let mut mutator_set = SetCommitment::<Hasher, MmrAccumulator<Hasher>>::default();
        let hasher: Hasher = blake3::Hasher::new();

        let num_additions = rng.gen_range(0..=100i32);
        println!(
            "running multiple additions test for {} additions",
            num_additions
        );

        let mut membership_proofs_and_items: Vec<(
            MembershipProof<Hasher>,
            blake3_wrapper::Blake3Hash,
        )> = vec![];
        for i in 0..num_additions {
            println!("loop iteration {}", i);
            let item: blake3_wrapper::Blake3Hash = hasher.hash(
                &(0..3)
                    .map(|_| BFieldElement::new(rng.next_u64()))
                    .collect::<Vec<_>>(),
            );
            let randomness = hasher.hash(
                &(0..3)
                    .map(|_| BFieldElement::new(rng.next_u64()))
                    .collect::<Vec<_>>(),
            );

            let addition_record = mutator_set.commit(&item, &randomness);
            let membership_proof = mutator_set.prove(&item, &randomness, false);

            // Update all membership proofs
            for (mp, item) in membership_proofs_and_items.iter_mut() {
                let original_mp = mp.clone();
                let changed_res = mp.update_from_addition(item, &mutator_set, &addition_record);
                assert!(changed_res.is_ok());

                // verify that the boolean returned value from the updater method is set correctly
                assert_eq!(changed_res.unwrap(), original_mp != *mp);
            }

            // Add the element
            assert!(!mutator_set.verify(&item, &membership_proof));
            mutator_set.add_helper(&addition_record);
            assert!(mutator_set.verify(&item, &membership_proof));
            membership_proofs_and_items.push((membership_proof, item));

            // Verify that all membership proofs work
            assert!(membership_proofs_and_items
                .clone()
                .into_iter()
                .all(|(mp, item)| mutator_set.verify(&item.into(), &mp)));
        }
    }

    #[test]
    fn test_add_and_prove() {
        type Hasher = RescuePrimeXlix<RP_DEFAULT_WIDTH>;
        type Mmr = MmrAccumulator<Hasher>;
        let mut mutator_set = SetCommitment::<Hasher, Mmr>::default();
        let hasher: RescuePrimeXlix<RP_DEFAULT_WIDTH> = neptune_params();
        let item0: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1215)], RP_DEFAULT_OUTPUT_SIZE);
        let randomness0: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1776)], RP_DEFAULT_OUTPUT_SIZE);

        let addition_record = mutator_set.commit(&item0, &randomness0);
        let membership_proof = mutator_set.prove(&item0, &randomness0, false);

        assert!(!mutator_set.verify(&item0, &membership_proof));

        mutator_set.add_helper(&addition_record);

        assert!(mutator_set.verify(&item0, &membership_proof));

        // Insert a new item and verify that this still works
        let item1: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1846)], RP_DEFAULT_OUTPUT_SIZE);
        let randomness1: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(2009)], RP_DEFAULT_OUTPUT_SIZE);
        let addition_record = mutator_set.commit(&item1, &randomness1);
        let membership_proof = mutator_set.prove(&item1, &randomness1, false);
        assert!(!mutator_set.verify(&item1, &membership_proof));
        mutator_set.add_helper(&addition_record);
        assert!(mutator_set.verify(&item1, &membership_proof));

        // Insert ~2*BATCH_SIZE  more elements and
        // verify that it works throughout. The reason we insert this many
        // is that we want to make sure that the window slides into a new
        // position.
        let mut prng = thread_rng();
        for _ in 0..2 * BATCH_SIZE + 4 {
            let item: Vec<BFieldElement> = hasher.hash(
                &BFieldElement::random_elements(2, &mut prng),
                RP_DEFAULT_OUTPUT_SIZE,
            );
            let randomness: Vec<BFieldElement> = hasher.hash(
                &BFieldElement::random_elements(2, &mut prng),
                RP_DEFAULT_OUTPUT_SIZE,
            );
            let addition_record = mutator_set.commit(&item, &randomness);
            let membership_proof = mutator_set.prove(&item, &randomness, false);
            assert!(!mutator_set.verify(&item, &membership_proof));
            mutator_set.add_helper(&addition_record);
            assert!(mutator_set.verify(&item, &membership_proof));
        }
    }

    #[test]
    fn batch_update_from_addition_and_removal_test() {
        // set up rng
        let mut rng = ChaCha20Rng::from_seed(
            vec![vec![0, 1, 4, 33], vec![0; 28]]
                .concat()
                .try_into()
                .unwrap(),
        );

        type Hasher = blake3::Hasher;
        type Digest = blake3_wrapper::Blake3Hash;
        type Mmr = MmrAccumulator<Hasher>;
        let hasher = Hasher::new();
        let mut mutator_set = SetCommitment::<Hasher, Mmr>::default();

        // It's important to test number of additions around the shifting of the window,
        // i.e. around batch size.
        let num_additions_list = vec![
            1,
            2,
            BATCH_SIZE - 1,
            BATCH_SIZE,
            BATCH_SIZE + 1,
            6 * BATCH_SIZE - 1,
            6 * BATCH_SIZE,
            6 * BATCH_SIZE + 1,
        ];

        let mut membership_proofs: Vec<MembershipProof<Hasher>> = vec![];
        let mut items: Vec<Digest> = vec![];

        for num_additions in num_additions_list {
            for _ in 0..num_additions {
                let new_item = hasher.hash(
                    &(0..3)
                        .map(|_| BFieldElement::new(rng.next_u64()))
                        .collect::<Vec<_>>(),
                );
                let randomness = hasher.hash(
                    &(0..3)
                        .map(|_| BFieldElement::new(rng.next_u64()))
                        .collect::<Vec<_>>(),
                );

                let addition_record = mutator_set.commit(&new_item, &randomness);
                let membership_proof = mutator_set.prove(&new_item, &randomness, true);

                // Update *all* membership proofs with newly added item
                let batch_update_res = MembershipProof::<Hasher>::batch_update_from_addition(
                    &mut membership_proofs.iter_mut().collect::<Vec<_>>(),
                    &items,
                    &mutator_set,
                    &addition_record,
                );
                assert!(batch_update_res.is_ok());

                mutator_set.add_helper(&addition_record);
                assert!(mutator_set.verify(&new_item, &membership_proof));

                for (_, (mp, item)) in membership_proofs.iter().zip(items.iter()).enumerate() {
                    assert!(mutator_set.verify(&item, &mp));
                }

                membership_proofs.push(membership_proof);
                items.push(new_item);
            }

            // Remove items from MS, and verify correct updating of membership proofs
            for _ in 0..num_additions {
                let item = items.pop().unwrap();
                let mp = membership_proofs.pop().unwrap();
                assert!(mutator_set.verify(&item, &mp));

                // generate removal record
                let mut removal_record: RemovalRecord<Hasher> = mutator_set.drop(&item.into(), &mp);
                assert!(removal_record.validate(&mutator_set));

                // update membership proofs
                let res = MembershipProof::batch_update_from_remove(
                    &mut membership_proofs.iter_mut().collect::<Vec<_>>(),
                    &removal_record,
                );
                assert!(res.is_ok());

                // remove item from set
                mutator_set.remove_helper(&mut removal_record);
                assert!(!mutator_set.verify(&item.into(), &mp));

                for (item, mp) in items.iter().zip(membership_proofs.iter()) {
                    assert!(mutator_set.verify(item, mp));
                }
            }
        }
    }

    #[test]
    fn test_multiple_adds() {
        // set up rng
        let mut rng = ChaCha20Rng::from_seed(
            vec![vec![0, 1, 4, 33], vec![0; 28]]
                .concat()
                .try_into()
                .unwrap(),
        );

        type Hasher = blake3::Hasher;
        type Digest = blake3_wrapper::Blake3Hash;
        type Mmr = MmrAccumulator<Hasher>;
        let hasher = Hasher::new();
        let mut mutator_set = SetCommitment::<Hasher, Mmr>::default();

        let num_additions = 65;

        let mut items_and_membership_proofs: Vec<(Digest, MembershipProof<Hasher>)> = vec![];

        for _ in 0..num_additions {
            let new_item = hasher.hash(
                &(0..3)
                    .map(|_| BFieldElement::new(rng.next_u64()))
                    .collect::<Vec<_>>(),
            );
            let randomness = hasher.hash(
                &(0..3)
                    .map(|_| BFieldElement::new(rng.next_u64()))
                    .collect::<Vec<_>>(),
            );

            let addition_record = mutator_set.commit(&new_item, &randomness);
            let membership_proof = mutator_set.prove(&new_item, &randomness, false);

            // Update *all* membership proofs with newly added item
            for (updatee_item, mp) in items_and_membership_proofs.iter_mut() {
                let original_mp = mp.clone();
                assert!(mutator_set.verify(updatee_item, mp));
                let changed_res =
                    mp.update_from_addition(&updatee_item, &mutator_set, &addition_record);
                assert!(changed_res.is_ok());

                // verify that the boolean returned value from the updater method is set correctly
                assert_eq!(changed_res.unwrap(), original_mp != *mp);
            }

            mutator_set.add_helper(&addition_record);
            assert!(mutator_set.verify(&new_item, &membership_proof));

            for j in 0..items_and_membership_proofs.len() {
                let (old_item, mp) = &items_and_membership_proofs[j];
                assert!(mutator_set.verify(&old_item, &mp))
            }

            items_and_membership_proofs.push((new_item, membership_proof));
        }

        // Verify all membership proofs
        for k in 0..items_and_membership_proofs.len() {
            assert!(mutator_set.verify(
                &items_and_membership_proofs[k].0,
                &items_and_membership_proofs[k].1,
            ));
        }

        // Remove items from MS, and verify correct updating of membership proof
        for i in 0..num_additions {
            for k in i..items_and_membership_proofs.len() {
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[k].0,
                    &items_and_membership_proofs[k].1,
                ));
            }
            let (item, mp) = items_and_membership_proofs[i].clone();

            assert!(mutator_set.verify(&item, &mp));

            // generate removal record
            let mut removal_record: RemovalRecord<Hasher> = mutator_set.drop(&item.into(), &mp);
            assert!(removal_record.validate(&mutator_set));
            for k in i..items_and_membership_proofs.len() {
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[k].0,
                    &items_and_membership_proofs[k].1,
                ));
            }

            // update membership proofs
            for j in (i + 1)..num_additions {
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[j].0,
                    &items_and_membership_proofs[j].1
                ));
                assert!(removal_record.validate(&mutator_set));
                let update_res = items_and_membership_proofs[j]
                    .1
                    .update_from_remove(&removal_record.clone());
                assert!(update_res.is_ok());
                assert!(removal_record.validate(&mutator_set));
            }

            // remove item from set
            mutator_set.remove_helper(&mut removal_record);
            assert!(!mutator_set.verify(&item.into(), &mp));

            for k in (i + 1)..items_and_membership_proofs.len() {
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[k].0,
                    &items_and_membership_proofs[k].1,
                ));
            }
        }
    }
}
