use divan::Bencher;
use leveldb::options::Options;
use leveldb_sys::Compression;

use neptune_core::database::storage::storage_vec::traits::StorageVec;
use neptune_core::util_types::mutator_set::addition_record::AdditionRecord;
use neptune_core::util_types::mutator_set::archival_mutator_set::ArchivalMutatorSet;
use neptune_core::util_types::mutator_set::chunk::Chunk;

use rand::Rng;

use neptune_core::database::NeptuneLevelDb;
use neptune_core::models::blockchain::shared::Hash;
use neptune_core::util_types::mutator_set::commit;
use neptune_core::util_types::mutator_set::ms_membership_proof::MsMembershipProof;
use neptune_core::util_types::mutator_set::rusty_archival_mutator_set::RustyArchivalMutatorSet;
use tasm_lib::twenty_first::math::tip5::Digest;

use leveldb::options::ReadOptions;
use leveldb::options::WriteOptions;

use neptune_core::util_types::mutator_set::shared::BATCH_SIZE;

fn main() {
    divan::main();
}

/// These settings affect DB performance and correctness.
///
/// Adjust and re-run the benchmarks to see effects.
///
/// Rust docs:  (basic)
///   https://docs.rs/rs-leveldb/0.1.5/leveldb/database/options/struct.Options.html
///
/// C++ docs:  (complete)
///   https://github.com/google/leveldb/blob/068d5ee1a3ac40dabd00d211d5013af44be55bea/include/leveldb/options.h
fn db_options() -> Option<Options> {
    Some(Options {
        // default: false
        create_if_missing: true,

        // default: false
        error_if_exists: true,

        // default: false
        paranoid_checks: false,

        // default: None  --> (4 * 1024 * 1024)
        write_buffer_size: None,

        // default: None   --> 1000
        max_open_files: None,

        // default: None   -->  4 * 1024
        block_size: None,

        // default: None   -->  16
        block_restart_interval: None,

        // default: Compression::No
        //      or: Compression::Snappy
        compression: Compression::No,

        // default: None   --> 8MB
        cache: None,
        // cache: Some(Cache::new(1024)),
        // note: tests put 128 bytes in each entry.
        // 100 entries = 12,800 bytes.
        // So Cache of 1024 bytes is 8% of total data set.
        // that seems reasonably realistic to get some
        // hits/misses.
    })
}

mod archival_mutator_set {
    use super::*;

    fn make_item_and_randomnesses() -> (Digest, Digest, Digest) {
        let mut rng = rand::thread_rng();
        let item: Digest = rng.gen();
        let sender_randomness: Digest = rng.gen();
        let receiver_preimage: Digest = rng.gen();
        (item, sender_randomness, receiver_preimage)
    }

    async fn prepare_random_addition<
        MmrStorage: StorageVec<Digest> + Send + Sync,
        ChunkStorage: StorageVec<Chunk> + Send + Sync,
    >(
        archival_mutator_set: &mut ArchivalMutatorSet<MmrStorage, ChunkStorage>,
    ) -> (Digest, AdditionRecord, MsMembershipProof) {
        let (item, sender_randomness, receiver_preimage) = make_item_and_randomnesses();
        let addition_record = commit(item, sender_randomness, receiver_preimage.hash::<Hash>());
        let membership_proof = archival_mutator_set
            .prove(item, sender_randomness, receiver_preimage)
            .await;

        (item, addition_record, membership_proof)
    }

    async fn append_impl() {
        // Set up the rusty mutator set with the DB, break out into a function later
        let db = NeptuneLevelDb::open_new_test_database(
            false,
            db_options(),
            Some(ReadOptions {
                verify_checksums: false,
                fill_cache: false,
            }),
            Some(WriteOptions { sync: true }),
        )
        .await
        .unwrap();

        let mut rms: RustyArchivalMutatorSet = RustyArchivalMutatorSet::connect(db).await;
        let archival_mutator_set = rms.ams_mut();

        // Repeatedly insert `AdditionRecord` into empty MutatorSet and revert it
        //
        // This does not reach the sliding window, and the MutatorSet reverts back
        // to being empty on every iteration.
        for _ in 0..2 * BATCH_SIZE {
            let (item, addition_record, membership_proof) =
                prepare_random_addition(archival_mutator_set).await;

            archival_mutator_set.add(&addition_record).await;
            archival_mutator_set.verify(item, &membership_proof).await;

            archival_mutator_set.revert_add(&addition_record).await;
        }

        let n_iterations = 10 * BATCH_SIZE as usize;
        let mut records = Vec::with_capacity(n_iterations);
        let mut commitments_before = Vec::with_capacity(n_iterations);

        // Insert a number of `AdditionRecord`s into MutatorSet and assert their membership.
        for _ in 0..n_iterations {
            let record = prepare_random_addition(archival_mutator_set).await;
            let (item, addition_record, membership_proof) = record.clone();
            records.push(record);
            commitments_before.push(archival_mutator_set.hash().await);
            archival_mutator_set.add(&addition_record).await;
            archival_mutator_set.verify(item, &membership_proof).await;
        }

        // Revert these `AdditionRecord`s in reverse order and assert they're no longer members.
        //
        // This reaches the sliding window every `BATCH_SIZE` iteration.
        for (item, addition_record, membership_proof) in records.into_iter().rev() {
            archival_mutator_set.revert_add(&addition_record).await;
            archival_mutator_set.verify(item, &membership_proof).await;
        }
    }

    fn run_append(bencher: Bencher) {
        let rt = tokio::runtime::Runtime::new().unwrap();

        bencher.bench_local(|| {
            rt.block_on(async {
                append_impl().await;
            });
        });
    }

    #[divan::bench]
    fn append(bencher: Bencher) {
        run_append(bencher);
    }
}
