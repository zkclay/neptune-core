use std::collections::{HashMap, HashSet};

use itertools::Itertools;

use crate::{
    shared_math::b_field_element::BFieldElement,
    util_types::{
        mmr::{self, membership_proof, mmr_accumulator::MmrAccumulator, mmr_trait::Mmr},
        simple_hasher::{self, ToDigest},
    },
};

pub const WINDOW_SIZE: usize = 30000;
pub const CHUNK_SIZE: usize = 1500;
pub const BATCH_SIZE: usize = 10;
pub const NUM_TRIALS: usize = 160;

pub struct SetCommitment<H: simple_hasher::Hasher> {
    aocl: MmrAccumulator<H>,
    swbf_inactive: MmrAccumulator<H>,
    swbf_active: [bool; WINDOW_SIZE],
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Chunk {
    bits: [bool; CHUNK_SIZE],
}

impl Chunk {
    pub fn hash<H: simple_hasher::Hasher>(&self) -> H::Digest
    where
        Vec<BFieldElement>: ToDigest<<H as simple_hasher::Hasher>::Digest>,
    {
        let num_iterations = CHUNK_SIZE / 63;
        let mut ret: Vec<BFieldElement> = vec![];
        let mut acc: u64;
        for i in 0..num_iterations {
            acc = 0;
            for j in 0..63 {
                acc += if self.bits[i * 63 + j] { 1 << j } else { 0 };
            }
            ret.push(BFieldElement::new(acc));
        }
        if CHUNK_SIZE % 63 != 0 {
            acc = 0;
            for j in 0..CHUNK_SIZE % 63 {
                acc += if self.bits[num_iterations * 63 + j] {
                    1 << j
                } else {
                    0
                };
            }
            ret.push(BFieldElement::new(acc));
        }

        let hasher = H::new();
        hasher.hash(&ret.to_digest())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ChunkDictionary<H: simple_hasher::Hasher> {
    // (*batch* index, membership proof for the whole chunk to which bit belongs, chunk value)
    dictionary: Vec<(u128, mmr::membership_proof::MembershipProof<H>, Chunk)>,
}

impl<H: simple_hasher::Hasher> ChunkDictionary<H> {
    fn default() -> ChunkDictionary<H> {
        Self { dictionary: vec![] }
    }

    pub fn has_duplicates(&self) -> bool {
        let batch_indices: Vec<_> = self.dictionary.iter().map(|(i, _, _)| *i).collect();
        let list_length = batch_indices.len();
        let batch_indices_set: Vec<_> = batch_indices.iter().unique().collect();
        let set_size = batch_indices_set.len();
        set_size != list_length
    }
}

#[derive(Clone, Debug)]
pub struct AdditionRecord<H: simple_hasher::Hasher> {
    commitment: H::Digest,
    aocl_snapshot: MmrAccumulator<H>,
}

impl<H: simple_hasher::Hasher> AdditionRecord<H>
where
    u128: ToDigest<<H as simple_hasher::Hasher>::Digest>,
{
    pub fn has_matching_aocl(&self, aocl_accumulator: &MmrAccumulator<H>) -> bool {
        self.aocl_snapshot.count_leaves() == aocl_accumulator.count_leaves()
            && self.aocl_snapshot.get_peaks() == aocl_accumulator.get_peaks()
    }
}

#[derive(Clone, Debug)]
pub struct RemovalRecord<H: simple_hasher::Hasher> {
    bit_indices: Vec<u128>,
    target_chunks: ChunkDictionary<H>,
}

impl<H: simple_hasher::Hasher> RemovalRecord<H>
where
    u128: ToDigest<<H as simple_hasher::Hasher>::Digest>,
    Vec<BFieldElement>: ToDigest<<H as simple_hasher::Hasher>::Digest>,
{
    pub fn validate(&self, mutator_set: &SetCommitment<H>) -> bool {
        self.target_chunks.dictionary.iter().all(|(i, p, c)| {
            p.verify(
                &mutator_set.swbf_inactive.get_peaks(),
                &c.hash::<H>(),
                mutator_set.swbf_inactive.count_leaves(),
            )
            .0
        })
    }
}

impl<H: simple_hasher::Hasher> SetCommitment<H>
where
    u128: ToDigest<<H as simple_hasher::Hasher>::Digest>,
    Vec<BFieldElement>: ToDigest<<H as simple_hasher::Hasher>::Digest>,
{
    pub fn default() -> Self {
        Self {
            aocl: MmrAccumulator::new(vec![]),
            swbf_inactive: MmrAccumulator::new(vec![]),
            swbf_active: [false; WINDOW_SIZE as usize],
        }
    }

    /**
     * commit
     * Generates an addition record from an item and explicit random-
     * ness. The addition record is itself a commitment to the item,
     * but tailored to adding the item to the mutator set in its
     * current state.
     */
    pub fn commit(&self, item: &H::Digest, randomness: &H::Digest) -> AdditionRecord<H> {
        let hasher = H::new();
        let canonical_commitment = hasher.hash_pair(item, randomness);

        AdditionRecord {
            commitment: canonical_commitment,
            aocl_snapshot: self.aocl.clone(),
        }
    }

    /**
     * get_indices
     * Helper function. Computes the bloom filter bit indices of the
     * item, randomness, index triple.
     */
    pub fn get_indices(item: &H::Digest, randomness: &H::Digest, index: u128) -> Vec<u128> {
        let hasher = H::new();
        let batch_index = index / BATCH_SIZE as u128;
        let timestamp: H::Digest = (index as u128).to_digest();
        let mut rhs = hasher.hash_pair(&timestamp, randomness);
        rhs = hasher.hash_pair(item, &rhs);
        let mut indices: Vec<u128> = vec![];
        for i in 0..NUM_TRIALS {
            let counter: H::Digest = (i as u128).to_digest();
            let pseudorandomness = hasher.hash_pair(&counter, &rhs);
            let bit_index = hasher.sample_index_not_power_of_two(&pseudorandomness, WINDOW_SIZE)
                as u128
                + batch_index * CHUNK_SIZE as u128;
            indices.push(bit_index);
        }

        indices
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
        let bit_indices = Self::get_indices(
            item,
            &membership_proof.randomness,
            membership_proof.auth_path_aocl.data_index,
        );

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
        index % BATCH_SIZE as u128 == 0

        // example cases:
        //  - index == 0 we don't care about
        //  - index == 1 does not generate a slide
        //  - index == n * BATCH_SIZE generates a slide for any n
    }

    ///   add
    ///   Updates the set-commitment with an addition record. The new
    ///   commitment represents the set $S union {c}$ ,
    ///   where S is the set represented by the old
    ///   commitment and c is the commitment to the new item AKA the
    ///   *addition record*.
    pub fn add(&mut self, addition_record: &AdditionRecord<H>) {
        // verify aocl snapshot
        if !addition_record.has_matching_aocl(&self.aocl) {
            panic!("Addition record has aocl snapshot that does not match with the AOCL it is being added to.")
        }

        println!(
            "Adding item. Old AOCL leaf count was {}; new is {}.",
            self.aocl.count_leaves(),
            self.aocl.count_leaves() + 1
        );

        // add to list
        let item_index = self.aocl.count_leaves();
        let batch_index = item_index / BATCH_SIZE as u128;
        self.aocl.append(addition_record.commitment.to_owned()); // ignore auth path

        // if window slides, update filter
        if Self::window_slides(item_index) {
            let chunk: Chunk = Chunk {
                bits: self.swbf_active[..CHUNK_SIZE].try_into().unwrap(),
            };

            // Move window to the right, equivalent to moving values inside window
            // to the left.
            for i in CHUNK_SIZE..WINDOW_SIZE {
                self.swbf_active[i - CHUNK_SIZE] = self.swbf_active[i];
            }

            for i in (WINDOW_SIZE - CHUNK_SIZE)..WINDOW_SIZE {
                self.swbf_active[i] = false;
            }

            // let chunk_digest = chunk.hash::<H>();
            let chunk_digest: H::Digest = chunk.hash::<H>();
            self.swbf_inactive.append(chunk_digest); // ignore auth path

            println!(
                "\n*****====\nJust added new item with window slide; new peaks: {:?}",
                self.swbf_inactive.get_peaks()
            );
        }
    }

    ///
    /// remove
    /// Updates the mutator set so as to remove the item determined by
    /// its removal record, which is updated also
    ///
    pub fn remove(&mut self, removal_record: &RemovalRecord<H>) {
        let batch_index = self.aocl.count_leaves() / BATCH_SIZE as u128;
        let window_start = batch_index * CHUNK_SIZE as u128;

        // set all bits
        let mut new_target_chunks = removal_record.target_chunks.clone();
        for bit_index in removal_record.bit_indices.iter() {
            // if bit is in active part
            if *bit_index >= window_start {
                let relative_index = bit_index - window_start;
                self.swbf_active[relative_index as usize] = true;
                continue;
            }

            // bit is not in active part, so set bits in relevant chunk
            let mut relevant_chunk = new_target_chunks
                .dictionary
                .iter_mut()
                .find(|(i, p, c)| bit_index / CHUNK_SIZE as u128 == *i)
                .unwrap();
            relevant_chunk.2.bits[(bit_index % CHUNK_SIZE as u128) as usize] = true;
        }

        // update mmr
        // to do this, we need to keep track of all membership proofs
        let mut all_membership_proofs: Vec<_> = new_target_chunks
            .dictionary
            .iter()
            .map(|(i, p, c)| p.clone())
            .collect();
        let all_leafs: Vec<_> = new_target_chunks
            .dictionary
            .iter()
            .map(|(i, p, c)| c.hash::<H>())
            .collect();
        let old_leafs: Vec<_> = removal_record
            .target_chunks
            .dictionary
            .iter()
            .map(|(i, p, c)| c.hash::<H>())
            .collect();
        // self.swbf_inactive.update_from_batch_leaf_mutation(all_membership_proofs, all_leafs);
        for i in 0..all_membership_proofs.len() {
            let local_membership_proof = all_membership_proofs[i].clone();
            let local_leaf = all_leafs[i].clone();
            assert!(
                local_membership_proof
                    .verify(
                        &self.swbf_inactive.get_peaks(),
                        &old_leafs[i],
                        self.swbf_inactive.count_leaves(),
                    )
                    .0
            );
            self.swbf_inactive
                .mutate_leaf(&local_membership_proof, &local_leaf);
            mmr::membership_proof::MembershipProof::batch_update_from_leaf_mutation(
                &mut all_membership_proofs,
                &local_membership_proof,
                &local_leaf,
            );
            assert!(
                local_membership_proof
                    .verify(
                        &self.swbf_inactive.get_peaks(),
                        &local_leaf,
                        self.swbf_inactive.count_leaves(),
                    )
                    .0
            );
        }

        // let mut new_membership_proofs: Vec<mmr::membership_proof::MembershipProof<H>> =
        //     removal_record
        //         .target_chunks
        //         .dictionary
        //         .iter()
        //         .map(|x| x.1.clone())
        //         .collect();
        // let chunk_indices: Vec<u128> = removal_record
        //     .target_chunks
        //     .dictionary
        //     .iter()
        //     .map(|x| x.0)
        //     .collect();
        // let mut chunk_2_entry: HashMap<u128, usize> = HashMap::new();
        // println!("Length of chunk_indices = {}", chunk_indices.len());
        // for (entry, chunk) in chunk_indices.into_iter().enumerate() {
        //     chunk_2_entry.insert(chunk, entry);
        //     println!("chunk => entry: {} => {}", chunk, entry);
        // }
        // println!(
        //     "Length of new_membership_proofs = {}",
        //     new_membership_proofs.len()
        // );
        // for bit_index in removal_record.bit_indices.iter() {
        //     // if bit is in active part
        //     if *bit_index >= window_start {
        //         let relative_index = bit_index - window_start;
        //         self.swbf_active[relative_index as usize] = true;
        //         continue;
        //     }

        //     // bit is not in active part, so update mmr
        //     let chunk_index = bit_index / CHUNK_SIZE as u128;
        //     let (_, _, chunk) = removal_record
        //         .target_chunks
        //         .dictionary
        //         .iter_mut()
        //         .find(|(i, _, _)| *i == chunk_index)
        //         .unwrap();
        //     path = new_membership_proofs[i];
        //     chunk.bits[(bit_index % CHUNK_SIZE as u128) as usize] = true;
        //     // let leaf_mutation_membership_proof = new_mps[chunk_2_entry[&chunk_index]].clone();
        //     let new_leaf = chunk.hash::<H>();
        //     self.swbf_inactive.mutate_leaf(&path, &chunk.hash::<H>());
        //     mmr::membership_proof::MembershipProof::batch_update_from_leaf_mutation(
        //         &mut new_membership_proofs,
        //         &path,
        //         &new_leaf,
        //     );

        //     assert!(
        //         path.verify(
        //             &self.swbf_inactive.get_peaks(),
        //             &new_leaf,
        //             self.swbf_inactive.count_leaves(),
        //         )
        //         .0,
        //         "Membership proofs must validate after remove manipulation"
        //     );
        // }
    }

    /**
     * prove
     * Generates a membership proof that will the valid when the item
     * is added to the mutator set.
     */
    pub fn prove(&self, item: &H::Digest, randomness: &H::Digest) -> MembershipProof<H> {
        // compute commitment
        let hasher = H::new();
        let item_commitment = hasher.hash_pair(item, randomness);

        // simulate adding to commitment list
        let new_item_index = self.aocl.count_leaves();
        let aocl_auth_path = self.aocl.clone().append(item_commitment);

        // get indices of bits to be set when item is removed
        let bit_indices = Self::get_indices(item, randomness, new_item_index);

        let mut target_chunks: ChunkDictionary<H> = ChunkDictionary::default();
        // if window slides, filter will be updated
        if Self::window_slides(new_item_index) && new_item_index > 0 {
            let chunk: Chunk = Chunk {
                bits: self.swbf_active[..CHUNK_SIZE].try_into().unwrap(),
            };
            let chunk_digest = chunk.hash::<H>();
            let new_chunk_path = self.swbf_inactive.clone().append(chunk_digest);

            // prepare swbf MMR authentication paths
            for bit_index in bit_indices {
                // compute the index of the boundary between inactive and active parts
                let window_start: u128 = //.
                    (new_item_index / BATCH_SIZE as u128) // which batch
                     * CHUNK_SIZE as u128; // # bits per bach

                // if index lies in inactive part of filter, add an mmr auth path
                if bit_index < window_start {
                    target_chunks.dictionary.push((
                        bit_index / CHUNK_SIZE as u128,
                        new_chunk_path.clone(),
                        chunk,
                    ));
                }
            }
        }

        // return membership proof
        MembershipProof {
            randomness: randomness.to_owned(),
            auth_path_aocl: aocl_auth_path,
            target_chunks,
        }
    }

    pub fn verify(&self, item: &H::Digest, membership_proof: &MembershipProof<H>) -> bool {
        println!("# Verifying item membership proof.");
        println!("aocl leaf count: {}", self.aocl.count_leaves());
        println!(
            "own item index: {}",
            membership_proof.auth_path_aocl.data_index
        );

        // If data index does not exist in AOCL, return false
        if self.aocl.count_leaves() <= membership_proof.auth_path_aocl.data_index {
            return false;
        }

        println!("---");

        // verify that a commitment to the item lives in the aocl mmr
        let hasher = H::new();
        let leaf = hasher.hash_pair(item, &membership_proof.randomness);
        let (is_aocl_member, _) = membership_proof.auth_path_aocl.verify(
            &self.aocl.get_peaks(),
            &leaf,
            self.aocl.count_leaves(),
        );

        // verify that some indicated bits in the swbf are unset
        let mut has_unset_bits = false;
        let mut entries_in_dictionary = true;
        let mut all_auth_paths_are_valid = true;
        let mut no_future_bits = true;
        let item_index = membership_proof.auth_path_aocl.data_index;

        // prepare parameters of inactive part
        let current_batch_index: u128 = (self.aocl.count_leaves() - 1) / BATCH_SIZE as u128;
        println!("current batch index: {}", current_batch_index);
        let window_start = current_batch_index * CHUNK_SIZE as u128;
        let window_stop = window_start + WINDOW_SIZE as u128;
        let bit_indices = Self::get_indices(item, &membership_proof.randomness, item_index);

        // get indices of bits to be set when item is removed
        for bit_index in bit_indices {
            // if bit index is left of the window
            if bit_index < window_start {
                // verify mmr auth path
                let matching_entries: Vec<&(
                    u128,
                    mmr::membership_proof::MembershipProof<H>,
                    Chunk,
                )> = membership_proof
                    .target_chunks
                    .dictionary
                    .iter()
                    .filter(|ch| ch.0 == bit_index / CHUNK_SIZE as u128)
                    .collect();
                assert!(
                    matching_entries.len() < 2,
                    "Found duplicated mathing entries. Got matching_entries.len() = {}",
                    matching_entries.len()
                );

                if matching_entries.len() != 1 {
                    entries_in_dictionary = false;
                    continue;
                }
                let (valid_auth_path, _) = matching_entries[0].1.verify(
                    &self.swbf_inactive.get_peaks(),
                    &matching_entries[0].2.hash::<H>(),
                    self.swbf_inactive.count_leaves(),
                );
                if !valid_auth_path {
                    println!(
                        "In verify, auth path for bit_index {} is not valid. Index: {}; Number of elements: {}; Peaks: {:?}",
                        bit_index,
                        matching_entries[0].1.data_index,
                        self.swbf_inactive.count_leaves(),
                        self.swbf_inactive.get_peaks()
                    );
                }

                all_auth_paths_are_valid = all_auth_paths_are_valid && valid_auth_path;

                // verify that bit is possibly unset
                // let relative_index: u128 = bit_index - window_start;
                let index_within_chunk = bit_index % CHUNK_SIZE as u128;
                if !matching_entries[0].2.bits[index_within_chunk as usize] {
                    has_unset_bits = true;
                }
                continue;
            }

            // Check whether bitindex is a future index, or in the active window
            if bit_index >= window_stop {
                no_future_bits = false;
            } else {
                let relative_index = bit_index - window_start;
                if !self.swbf_active[relative_index as usize] {
                    has_unset_bits = true;
                }
            }
        }

        println!("is_aocl_member: {}", is_aocl_member);
        println!("entries_in_dictionary: {}", entries_in_dictionary);
        println!("all_auth_paths_are_valid: {}", all_auth_paths_are_valid);
        println!("no_future_bits: {}", no_future_bits);
        println!("has_unset_bits: {}", has_unset_bits);

        // return verdict
        is_aocl_member
            && entries_in_dictionary
            && all_auth_paths_are_valid
            && no_future_bits
            && has_unset_bits
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MembershipProof<H: simple_hasher::Hasher> {
    randomness: H::Digest,
    auth_path_aocl: mmr::membership_proof::MembershipProof<H>,
    target_chunks: ChunkDictionary<H>,
}

impl<H: simple_hasher::Hasher> MembershipProof<H>
where
    u128: ToDigest<<H as simple_hasher::Hasher>::Digest>,
    Vec<BFieldElement>: ToDigest<<H as simple_hasher::Hasher>::Digest>,
{
    /**
     * update_from_addition
     * Updates a membership proof in anticipation of an addition to the set.
     */
    pub fn update_from_addition(
        &mut self,
        own_item: &H::Digest,
        mutator_set: &SetCommitment<H>,
        addition_record: &AdditionRecord<H>,
    ) {
        assert!(self.auth_path_aocl.data_index < mutator_set.aocl.count_leaves());
        let new_item_index = mutator_set.aocl.count_leaves();
        let batch_index = new_item_index / BATCH_SIZE as u128;

        // Update AOCL MMR membership proof
        self.auth_path_aocl.update_from_append(
            mutator_set.aocl.count_leaves(),
            &addition_record.commitment,
            &mutator_set.aocl.get_peaks(),
        );

        // if window does not slide, we are done
        if !SetCommitment::<H>::window_slides(new_item_index) {
            println!("Window does not slide; nothing left to to\n");
            let batch_indices_in_dictionary: Vec<_> = self
                .target_chunks
                .dictionary
                .iter()
                .map(|(i, _, _)| *i)
                .collect();
            println!("batch indices in dictionary:");
            for bi in batch_indices_in_dictionary {
                print!("{},", bi);
            }
            println!("");
            println!("has duplicates? {}", self.target_chunks.has_duplicates());
        }
        // window does slide
        else {
            assert!(SetCommitment::<H>::window_slides(new_item_index));
            let old_window_start = (batch_index - 1) * CHUNK_SIZE as u128;
            let new_window_start = batch_index * CHUNK_SIZE as u128;
            let bit_indices = SetCommitment::<H>::get_indices(
                own_item,
                &self.randomness,
                self.auth_path_aocl.data_index,
            );
            let new_chunk = Chunk {
                bits: mutator_set.swbf_active[0..CHUNK_SIZE].try_into().unwrap(),
            };
            let mut mmra_copy = mutator_set.swbf_inactive.clone();
            let new_auth_path: mmr::membership_proof::MembershipProof<H> =
                mmra_copy.append(new_chunk.hash::<H>());
            println!(
                "immediately after appending, new leaf count is {} and new peaks are: {:?}",
                mmra_copy.count_leaves(),
                mmra_copy.get_peaks()
            );
            println!("path: {:?}", new_auth_path.authentication_path);
            println!(
                "path is valid? {}",
                new_auth_path
                    .verify(
                        &mmra_copy.get_peaks(),
                        &new_chunk.hash::<H>(),
                        mmra_copy.count_leaves()
                    )
                    .0
            );
            for bit_index in bit_indices {
                // if bit index is in dictionary, update auth path
                if bit_index < old_window_start {
                    self.target_chunks.dictionary.iter_mut().for_each(
                        |(dict_index, dict_path, _)| {
                            if *dict_index == bit_index / CHUNK_SIZE as u128 {
                                dict_path.update_from_append(
                                    mutator_set.swbf_inactive.count_leaves(),
                                    &new_chunk.hash::<H>(),
                                    &mutator_set.swbf_inactive.get_peaks(),
                                );
                            }
                        },
                    )
                }

                // if bit is in the part that is becoming inactive, add a dictionary entry
                if old_window_start <= bit_index && bit_index < new_window_start {
                    // generate dictionary entry
                    let entry = (
                        bit_index / CHUNK_SIZE as u128,
                        new_auth_path.clone(),
                        new_chunk,
                    );

                    // assert that dictionary entry does not exist already
                    // because if it does then there's nothing left to do
                    if self.target_chunks.dictionary.contains(&entry) {
                        continue;
                    }

                    let batch_indices_in_dictionary: Vec<u128> = self
                        .target_chunks
                        .dictionary
                        .iter()
                        .map(|(i, _, _)| *i)
                        .collect();
                    if batch_indices_in_dictionary.contains(&(bit_index / CHUNK_SIZE as u128)) {
                        println!(
                            "Unexpected duplicate bit index found in target chunks dictionary: {}",
                            bit_index
                        );
                        println!(
                            "old window start = {} <= {} < {} = new_window_start",
                            old_window_start, bit_index, new_window_start
                        );
                        println!(
                            "chunks identical? {}",
                            new_chunk
                                == self
                                    .target_chunks
                                    .dictionary
                                    .iter()
                                    .find(|(i, _, _)| *i == bit_index / CHUNK_SIZE as u128)
                                    .unwrap()
                                    .2
                        );
                        println!(
                            "paths identical? {}",
                            new_auth_path
                                == self
                                    .target_chunks
                                    .dictionary
                                    .iter()
                                    .find(|(i, _, _)| *i == bit_index / CHUNK_SIZE as u128)
                                    .unwrap()
                                    .1
                        );
                    }

                    // add dictionary entry
                    self.target_chunks.dictionary.push(entry);
                    assert!(!self.target_chunks.has_duplicates());
                }

                // if bit is still in active window, do nothing
            }
        }

        println!("Done with Updating from Addition.\n");

        // TODO: Consider if we want a return value indicating if membership proof has changed
    }

    pub fn update_from_remove(
        &mut self,
        mutator_set: &SetCommitment<H>,
        removal_record: &RemovalRecord<H>,
    ) {
        // test
        // self.target_chunks.dictionary.iter().all(|(i, p, c)| {
        //     p.verify(
        //         &mutator_set.swbf_inactive.get_peaks(),
        //         &c.hash::<H>(),
        //         mutator_set.swbf_inactive.count_leaves(),
        //     )
        //     .0
        // })
        assert!(removal_record.validate(&mutator_set));
        for (_, authentication_path, chunk) in self.target_chunks.dictionary.iter() {
            assert!(
                authentication_path
                    .verify(
                        &mutator_set.swbf_inactive.get_peaks(), // old peaks
                        &chunk.hash::<H>(),
                        mutator_set.swbf_inactive.count_leaves(),
                    )
                    .0,
                "Updated membership proofs must be valid"
            );
        }

        // set bits in own chunks
        // for all chunks in the chunk dictionary
        let mut self_updated_chunk_indices: HashSet<u128> = HashSet::new();
        for (own_chunk_index, _, own_chunk) in self.target_chunks.dictionary.iter_mut() {
            // for all bit indices in removal record
            for bit_index in removal_record.bit_indices.iter() {
                // if chunk indices match, set bit
                if bit_index / CHUNK_SIZE as u128 == *own_chunk_index {
                    own_chunk.bits[(bit_index % CHUNK_SIZE as u128) as usize] = true;
                    self_updated_chunk_indices.insert(*own_chunk_index);
                }
            }
        }

        // update membership proofs
        let mut own_membership_proofs_copy: Vec<mmr::membership_proof::MembershipProof<H>> = self
            .target_chunks
            .dictionary
            .iter()
            .map(|(_, p, _)| p.clone())
            .collect();
        let mut modified_leaf_proofs = own_membership_proofs_copy.clone();
        let mut modified_leafs: Vec<_> = self
            .target_chunks
            .dictionary
            .iter()
            .map(|(_, _, c)| c.hash::<H>())
            .collect();

        // Find path update data that is not contained in the membership proof
        // but only in the removal record
        for bit_index in removal_record.bit_indices.iter() {
            if !self_updated_chunk_indices.contains(&(bit_index / CHUNK_SIZE as u128))
                && removal_record
                    .target_chunks
                    .dictionary
                    .iter()
                    .find(|(i, _, _)| *i == bit_index / CHUNK_SIZE as u128)
                    .is_some()
            {
                // Algorithms are like sausages. It is best not to see them being made.
                println!("target found for bit_index = {} !!", bit_index);
                let mut target_leaf = removal_record
                    .target_chunks
                    .dictionary
                    .iter()
                    .find(|(i, _, _)| *i == bit_index / CHUNK_SIZE as u128)
                    .unwrap()
                    .2
                    .clone();
                let target_chunk_index = removal_record
                    .target_chunks
                    .dictionary
                    .iter()
                    .find(|(i, _, _)| *i == bit_index / CHUNK_SIZE as u128)
                    .unwrap()
                    .0;
                for bit_index in removal_record
                    .bit_indices
                    .iter()
                    .filter(|i| target_chunk_index == **i / CHUNK_SIZE as u128)
                {
                    target_leaf.bits[*bit_index as usize % CHUNK_SIZE] = true;
                }
                let target_ap = removal_record
                    .target_chunks
                    .dictionary
                    .iter()
                    .find(|(i, _, _)| *i == bit_index / CHUNK_SIZE as u128)
                    .unwrap()
                    .1
                    .clone();
                modified_leaf_proofs.push(target_ap);
                modified_leafs.push(target_leaf.hash::<H>());
                println!("target_leaf = {:?}", target_leaf);

                self_updated_chunk_indices.insert(bit_index / CHUNK_SIZE as u128);
            }
        }

        println!(
            "own_membership_proofs_copy.len() = {}",
            own_membership_proofs_copy.len()
        );
        mmr::membership_proof::MembershipProof::batch_update_from_batch_leaf_mutation(
            &mut own_membership_proofs_copy,
            &modified_leaf_proofs,
            &modified_leafs,
        );
        for i in 0..own_membership_proofs_copy.len() {
            self.target_chunks.dictionary[i].1 = own_membership_proofs_copy[i].clone();
        }

        // TODO: REMOVE THIS DEBUG STUFF
        let mut swbf_inactive_clone: MmrAccumulator<H> = mutator_set.swbf_inactive.clone();
        let mut modified_leaf_proofs_copy = modified_leaf_proofs.clone();
        for i in 0..modified_leaf_proofs_copy.len() {
            let current_leaf = modified_leafs[i].clone();
            let current_path = modified_leaf_proofs_copy[i].clone();
            swbf_inactive_clone.mutate_leaf(&current_path, &current_leaf);
            mmr::membership_proof::MembershipProof::batch_update_from_leaf_mutation(
                &mut modified_leaf_proofs_copy,
                &current_path,
                &current_leaf,
            );
            assert!(
                current_path
                    .verify(
                        &swbf_inactive_clone.get_peaks(),
                        &current_leaf,
                        swbf_inactive_clone.count_leaves(),
                    )
                    .0
            );
        }

        // test
        for (_, authentication_path, chunk) in self.target_chunks.dictionary.iter_mut() {
            assert!(
                authentication_path
                    .verify(
                        &swbf_inactive_clone.get_peaks(), // old peaks
                        &chunk.hash::<H>(),
                        swbf_inactive_clone.count_leaves(),
                    )
                    .0,
                "Updated membership proofs must be valid"
            );
        }

        // bits in the active part of the filter
        // will be set when change is applied to mutator set
        // no need to anticipate here
    }
}

#[cfg(test)]
mod accumulation_scheme_tests {
    use crate::{
        shared_math::rescue_prime_xlix::{
            neptune_params, RescuePrimeXlix, RP_DEFAULT_OUTPUT_SIZE, RP_DEFAULT_WIDTH,
        },
        util_types::{
            blake3_wrapper,
            simple_hasher::{Hasher, RescuePrimeProduction},
        },
    };
    use rand::prelude::*;
    use rand_chacha::ChaCha20Rng;
    use rand_core::{RngCore, SeedableRng};

    use super::*;

    #[test]
    fn init_test() {
        SetCommitment::<RescuePrimeProduction>::default();
    }

    #[test]
    fn test_membership_proof_update_from_add() {
        let mut mutator_set = SetCommitment::<RescuePrimeXlix<RP_DEFAULT_WIDTH>>::default();
        let hasher: RescuePrimeXlix<RP_DEFAULT_WIDTH> = neptune_params();
        let own_item: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1215)], RP_DEFAULT_OUTPUT_SIZE);
        let randomness: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1776)], RP_DEFAULT_OUTPUT_SIZE);

        let addition_record: AdditionRecord<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            mutator_set.commit(&own_item, &randomness);
        let mut membership_proof: MembershipProof<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            mutator_set.prove(&own_item, &randomness);
        mutator_set.add(&addition_record);

        // Update membership proof with add operation. Verify that it has changed, and that it now fails to verify.
        let new_item: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1648)], RP_DEFAULT_OUTPUT_SIZE);
        let new_randomness: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1807)], RP_DEFAULT_OUTPUT_SIZE);
        let new_addition_record: AdditionRecord<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            mutator_set.commit(&new_item, &new_randomness);
        let original_membership_proof: MembershipProof<RescuePrimeXlix<RP_DEFAULT_WIDTH>> =
            membership_proof.clone();
        membership_proof.update_from_addition(&own_item, &mutator_set, &new_addition_record);
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
        mutator_set.add(&new_addition_record);
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

        let mut mutator_set = SetCommitment::<Hasher>::default();
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
            let membership_proof = mutator_set.prove(&item, &randomness);

            // Update all membership proofs
            for mp in membership_proofs_and_items.iter_mut() {
                mp.0.update_from_addition(&mp.1.into(), &mutator_set, &addition_record);
            }

            // Add the element
            assert!(!mutator_set.verify(&item, &membership_proof));
            mutator_set.add(&addition_record);
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
    fn test_add() {
        let mut mutator_set = SetCommitment::<RescuePrimeXlix<RP_DEFAULT_WIDTH>>::default();
        let hasher: RescuePrimeXlix<RP_DEFAULT_WIDTH> = neptune_params();
        let item: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1215)], RP_DEFAULT_OUTPUT_SIZE);
        let randomness: Vec<BFieldElement> =
            hasher.hash(&vec![BFieldElement::new(1776)], RP_DEFAULT_OUTPUT_SIZE);

        let addition_record = mutator_set.commit(&item, &randomness);
        let membership_proof = mutator_set.prove(&item, &randomness);

        assert!(false == mutator_set.verify(&item, &membership_proof));

        mutator_set.add(&addition_record);

        assert!(true == mutator_set.verify(&item, &membership_proof));
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
        let hasher = Hasher::new();
        let mut mutator_set = SetCommitment::<Hasher>::default();

        // let num_additions = rng.gen_range(0..=100usize);
        let num_additions = 32;
        println!(
            "running multiple additions test for {} additions",
            num_additions
        );

        let mut items_and_membership_proofs: Vec<(Digest, MembershipProof<Hasher>)> = vec![];
        for i in 0..num_additions {
            println!(
                "\n\n\n**********************loop iteration {} / {} **********************",
                i, num_additions
            );
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
            let membership_proof = mutator_set.prove(&new_item, &randomness);
            assert!(!membership_proof.target_chunks.has_duplicates());

            // Update *all* membership proofs with newly added item
            let mut j = 0;
            for (updatee_item, mp) in items_and_membership_proofs.iter_mut() {
                assert!(!mp.target_chunks.has_duplicates());
                assert!(mutator_set.verify(updatee_item, mp));
                mp.update_from_addition(&updatee_item, &mutator_set, &addition_record);
                assert!(!mp.target_chunks.has_duplicates());
                j += 1;
            }

            //assert!(!mutator_set.verify(&item, &membership_proof));
            mutator_set.add(&addition_record);
            assert!(mutator_set.verify(&new_item, &membership_proof));

            for j in 0..items_and_membership_proofs.len() {
                let (old_item, mp) = &items_and_membership_proofs[j];
                // for (item, mp) in items_and_membership_proofs.iter() {
                assert!(!mp.target_chunks.has_duplicates());
                assert!(mutator_set.verify(&old_item, &mp))
            }

            assert!(!membership_proof.target_chunks.has_duplicates());
            items_and_membership_proofs.push((new_item, membership_proof));
        }

        println!("\nDone with 1st loop\n");

        for k in 0..items_and_membership_proofs.len() {
            assert!(mutator_set.verify(
                &items_and_membership_proofs[k].0,
                &items_and_membership_proofs[k].1,
            ));
        }

        for i in 0..num_additions {
            for k in i..items_and_membership_proofs.len() {
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[k].0,
                    &items_and_membership_proofs[k].1,
                ));
            }
            println!("*************************************************** In 2nd loop in test: i = {} ***************************************************", i);
            let (item, mp) = items_and_membership_proofs[i].clone();
            println!(
                "preparing to remove item ... let's see if its membership proof is valid first ..."
            );
            assert!(mutator_set.verify(&item, &mp));
            println!("PASSING 1ST ASSERT!!!1one");

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
                println!(
                    "************************************************ update from remove {} ",
                    j
                );
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[j].0,
                    &items_and_membership_proofs[j].1
                ));
                assert!(removal_record.validate(&mutator_set));
                items_and_membership_proofs[j]
                    .1
                    .update_from_remove(&mutator_set, &removal_record.clone());
                assert!(removal_record.validate(&mutator_set));
            }

            // remove item from set
            println!("\n\n\n Calling remove *************************");
            mutator_set.remove(&mut removal_record);
            println!("Done remove");
            assert!(!mutator_set.verify(&item.into(), &mp));

            for k in (i + 1)..items_and_membership_proofs.len() {
                println!("k = {}", k);
                assert!(mutator_set.verify(
                    &items_and_membership_proofs[k].0,
                    &items_and_membership_proofs[k].1,
                ));
            }

            println!("Done 2nd loop iteration");
        }
    }
}
