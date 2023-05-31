use crate::models::blockchain::block::block_body::BlockBody;
use crate::models::blockchain::block::block_header::BlockHeader;
use crate::models::blockchain::block::block_height::BlockHeight;
use crate::models::blockchain::block::mutator_set_update::*;
use crate::models::blockchain::block::*;
use crate::models::blockchain::shared::*;
use crate::models::blockchain::transaction::amount::Amount;
use crate::models::blockchain::transaction::transaction_kernel::TransactionKernel;
use crate::models::blockchain::transaction::utxo::*;
use crate::models::blockchain::transaction::validity::ValidityLogic;
use crate::models::blockchain::transaction::*;
use crate::models::channel::*;
use crate::models::shared::SIZE_1MB_IN_BYTES;
use crate::models::state::wallet::utxo_notification_pool::{ExpectedUtxo, UtxoNotifier};
use crate::models::state::wallet::WalletSecret;
use crate::models::state::GlobalState;
use crate::util_types::mutator_set::mutator_set_accumulator::MutatorSetAccumulator;
use crate::util_types::mutator_set::mutator_set_trait::{commit, MutatorSet};
use anyhow::{Context, Result};
use futures::channel::oneshot;
use num_traits::identities::Zero;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::select;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::*;
use twenty_first::amount::u32s::U32s;
use twenty_first::shared_math::b_field_element::BFieldElement;
use twenty_first::shared_math::bfield_codec::BFieldCodec;
use twenty_first::shared_math::digest::Digest;
use twenty_first::util_types::algebraic_hasher::AlgebraicHasher;
use twenty_first::util_types::emojihash_trait::Emojihash;

const MOCK_MAX_BLOCK_SIZE: u32 = 1_000_000;

/// Prepare a Block for mining
fn make_block_template(
    previous_block: &Block,
    transaction: Transaction,
) -> (BlockHeader, BlockBody) {
    let additions = transaction.kernel.outputs.clone();
    let removals = transaction.kernel.inputs.clone();
    let mut next_mutator_set_accumulator: MutatorSetAccumulator<Hash> =
        previous_block.body.next_mutator_set_accumulator.clone();

    // Apply the mutator set update to the mutator set accumulator
    // This function mutates the MS accumulator that is given as argument to
    // the function such that the next mutator set accumulator is calculated.
    let mutator_set_update = MutatorSetUpdate::new(removals, additions);
    mutator_set_update
        .apply(&mut next_mutator_set_accumulator)
        .expect("Mutator set mutation must work");

    let block_body: BlockBody = BlockBody {
        transaction,
        next_mutator_set_accumulator: next_mutator_set_accumulator.clone(),
        previous_mutator_set_accumulator: previous_block.body.next_mutator_set_accumulator.clone(),
        stark_proof: vec![],
    };

    let zero = BFieldElement::zero();
    let new_pow_line: U32s<5> =
        previous_block.header.proof_of_work_family + previous_block.header.difficulty;
    let mutator_set_commitment: Digest = next_mutator_set_accumulator.hash();
    let next_block_height = previous_block.header.height.next();
    let mut block_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Got bad time timestamp in mining process")
        .as_millis() as u64;
    if block_timestamp < previous_block.header.timestamp.value() {
        warn!("Received block is timestamped in the future; mining on future-timestamped block.");
        block_timestamp = previous_block.header.timestamp.value() + 1;
    }
    let difficulty: U32s<5> = Block::difficulty_control(previous_block, block_timestamp);

    let block_header = BlockHeader {
        version: zero,
        height: next_block_height,
        mutator_set_hash: mutator_set_commitment,
        prev_block_digest: Hash::hash(&previous_block.header),
        timestamp: BFieldElement::new(block_timestamp),
        nonce: [zero, zero, zero],
        max_block_size: MOCK_MAX_BLOCK_SIZE,
        proof_of_work_line: new_pow_line,
        proof_of_work_family: new_pow_line,
        difficulty,
        block_body_merkle_root: Hash::hash(&block_body),
        uncles: vec![],
    };

    (block_header, block_body)
}

/// Attempt to mine a valid block for the network
async fn mine_block(
    mut block_header: BlockHeader,
    block_body: BlockBody,
    sender: oneshot::Sender<NewBlockFound>,
    state: GlobalState,
    coinbase_utxo_info: ExpectedUtxo,
) {
    info!(
        "Mining on block with {} outputs",
        block_body.transaction.kernel.outputs.len()
    );

    // Mining takes place here
    while Hash::hash(&block_header)
        >= Block::difficulty_to_digest_threshold(block_header.difficulty)
    {
        if state.cli.throttled_mining {
            tokio::time::sleep(Duration::from_micros(100)).await;
        }

        // If the sender is cancelled, the parent to this thread most
        // likely received a new block, and this thread hasn't been stopped
        // yet by the operating system, although the call to abort this
        // thread *has* been made.
        if sender.is_canceled() {
            info!(
                "Abandoning mining of current block with height {}",
                block_header.height
            );
            return;
        }

        // Don't mine if we are syncing
        if block_header.nonce[2].value() % 100 == 0 && state.net.syncing.read().unwrap().to_owned()
        {
            return;
        }

        if block_header.nonce[2].value() == BFieldElement::MAX {
            block_header.nonce[2] = BFieldElement::zero();
            if block_header.nonce[1].value() == BFieldElement::MAX {
                block_header.nonce[1] = BFieldElement::zero();
                block_header.nonce[0].increment();
                continue;
            }
            block_header.nonce[1].increment();
            continue;
        }
        block_header.nonce[2].increment();
    }
    info!(
        "Found valid block with nonce: ({}, {}, {})",
        block_header.nonce[0], block_header.nonce[1], block_header.nonce[2]
    );

    let new_block_info = NewBlockFound {
        block: Box::new(Block::new(block_header, block_body)),
        coinbase_utxo_info: Box::new(coinbase_utxo_info),
    };

    sender
        .send(new_block_info)
        .unwrap_or_else(|_| warn!("Receiver in mining loop closed prematurely"))
}

/// Return the coinbase UTXO for the receiving address and the "sender" randomness
/// used for the canonical AOCL commitment.
fn make_coinbase_transaction(
    coinbase_utxo: &Utxo,
    receiver_digest: &Digest,
    wallet_secret: &WalletSecret,
    block_height: BlockHeight,
    mutator_set_accumulator: MutatorSetAccumulator<Hash>,
) -> (Transaction, Digest) {
    let sender_randomness: Digest =
        wallet_secret.generate_sender_randomness(block_height, *receiver_digest);

    let coinbase_amount = coinbase_utxo
        .coins
        .iter()
        .filter(|coin| coin.type_script_hash == TypeScript::native_coin().hash())
        .map(|coin| {
            *Amount::decode(&coin.state)
                .expect("Make coinbase transaction: failed to parse coin state as amount.")
        })
        .sum();
    let coinbase_addition_record = commit::<Hash>(
        &Hash::hash(coinbase_utxo),
        &sender_randomness,
        receiver_digest,
    );

    let timestamp: BFieldElement = BFieldElement::new(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("Got bad time timestamp in mining process")
            .as_millis()
            .try_into()
            .expect("Must call this function before 584 million years from genesis."),
    );

    let kernel = TransactionKernel {
        inputs: vec![],
        outputs: vec![coinbase_addition_record],
        pubscript_hashes_and_inputs: vec![],
        fee: Amount::zero(),
        timestamp,
        coinbase: Some(coinbase_amount),
        mutator_set_hash: mutator_set_accumulator.hash(),
    };

    let mutator_set_hash = mutator_set_accumulator.hash();

    let primitive_witness = PrimitiveWitness {
        input_utxos: vec![],
        input_lock_scripts: vec![],
        lock_script_witnesses: vec![],
        input_membership_proofs: vec![],
        output_utxos: vec![coinbase_utxo.clone()],
        pubscripts: vec![],
        mutator_set_accumulator,
    };
    let validity_logic =
        ValidityLogic::unproven_from_primitive_witness(&primitive_witness, &kernel);
    (
        Transaction {
            kernel,
            witness: Witness::ValidityLogic((validity_logic, primitive_witness)),
        },
        sender_randomness,
    )
}

/// Create the transaction that goes into the block template. The transaction is
/// built from the mempool and from the coinbase transaction. Also returns the
/// "sender randomness" used in the coinbase transaction.
fn create_block_transaction(
    latest_block: &Block,
    state: &GlobalState,
) -> (Transaction, ExpectedUtxo) {
    let block_capacity_for_transactions = SIZE_1MB_IN_BYTES;

    // Get most valuable transactions from mempool
    let transactions_to_include = state
        .mempool
        .get_transactions_for_block(block_capacity_for_transactions);

    // Build coinbase UTXO
    let transaction_fees = transactions_to_include
        .iter()
        .fold(Amount::zero(), |acc, tx| acc + tx.kernel.fee);

    let coinbase_recipient_spending_key = state
        .wallet_state
        .wallet_secret
        .nth_generation_spending_key(0);
    let receiving_address = coinbase_recipient_spending_key.to_address();
    let next_block_height: BlockHeight = latest_block.header.height.next();

    let lock_script = receiving_address.lock_script();
    let coinbase_amount = Block::get_mining_reward(next_block_height) + transaction_fees;
    let coinbase_utxo = Utxo::new_native_coin(lock_script, coinbase_amount);

    let (coinbase_transaction, coinbase_sender_randomness) = make_coinbase_transaction(
        &coinbase_utxo,
        &receiving_address.privacy_digest,
        &state.wallet_state.wallet_secret,
        next_block_height,
        latest_block.body.next_mutator_set_accumulator.clone(),
    );

    debug!(
        "Creating block transaction with mutator set hash: {}",
        latest_block
            .body
            .next_mutator_set_accumulator
            .hash()
            .emojihash()
    );

    // Merge incoming transactions with the coinbase transaction
    let merged_transaction = transactions_to_include
        .into_iter()
        .fold(coinbase_transaction, |acc, transaction| {
            Transaction::merge_with(acc, transaction)
        });

    let utxo_info_for_coinbase = ExpectedUtxo::new(
        coinbase_utxo,
        coinbase_sender_randomness,
        coinbase_recipient_spending_key.privacy_preimage,
        UtxoNotifier::OwnMiner,
    );

    (merged_transaction, utxo_info_for_coinbase)
}

pub async fn mine(
    mut from_main: watch::Receiver<MainToMiner>,
    to_main: mpsc::Sender<MinerToMain>,
    mut latest_block: Block,
    state: GlobalState,
) -> Result<()> {
    let mut pause_mine = false;
    loop {
        let (sender, receiver) = oneshot::channel::<NewBlockFound>();
        let miner_thread: Option<JoinHandle<()>> = if state.net.syncing.read().unwrap().to_owned() {
            info!("Not mining because we are syncing");
            *state.mining.write().unwrap() = false;
            None
        } else if pause_mine {
            info!("Not mining because mining was paused");
            *state.mining.write().unwrap() = false;
            None
        } else {
            // Build the block template and spawn the worker thread to mine on it
            let (transaction, coinbase_utxo_info) = create_block_transaction(&latest_block, &state);
            let (block_header, block_body) = make_block_template(&latest_block, transaction);
            let miner_task = mine_block(
                block_header,
                block_body,
                sender,
                state.clone(),
                coinbase_utxo_info,
            );
            *state.mining.write().unwrap() = true;
            Some(tokio::spawn(miner_task))
        };

        // Await a message from either the worker thread or from the main loop
        select! {
            changed = from_main.changed() => {
                info!("Mining thread got message from main");
                if let e@Err(_) = changed {
                    return e.context("Miner failed to read from watch channel");
                }

                let main_message: MainToMiner = from_main.borrow_and_update().clone();
                match main_message {
                    MainToMiner::Shutdown => {
                        debug!("Miner shutting down.");

                        if let Some(mt) = miner_thread {
                            mt.abort();
                        }

                        break;
                    }
                    MainToMiner::NewBlock(block) => {
                        if let Some(mt) = miner_thread {
                            mt.abort();
                        }
                        latest_block = *block;
                        info!("Miner thread received regtest block height {}", latest_block.header.height);
                    }
                    MainToMiner::Empty => (),
                    MainToMiner::ReadyToMineNextBlock => {
                        debug!("Got {:?} from `main_loop`", MainToMiner::ReadyToMineNextBlock);
                    }
                    MainToMiner::StopMining => {
                        debug!("Miner shutting down.");

                        pause_mine = true;

                        if let Some(mt) = miner_thread {
                            mt.abort();
                        }
                    }
                    MainToMiner::StartMining => {
                        debug!("Starting miner");

                        pause_mine = false;
                    }
                }
            }
            new_block_res = receiver => {
                let new_block_info = match new_block_res {
                    Ok(res) => res,
                    Err(err) => {
                        warn!("Mining thread was cancelled prematurely. Got: {}", err);
                        continue;
                    }
                };

                // Sanity check, remove for more efficient mining.
                assert!(new_block_info.block.has_proof_of_work(&latest_block), "Own mined block must be valid");
                assert!(new_block_info.block.is_valid(&latest_block), "Own mined block must be valid");

                info!("Found new regtest block with block height {}. Hash: {}", new_block_info.block.header.height, new_block_info.block.hash.emojihash());

                latest_block = *new_block_info.block.to_owned();
                to_main.send(MinerToMain::NewBlockFound(new_block_info)).await?;

                // Wait until `main_loop` has updated `global_state` before proceding. Otherwise, we would use
                // a deprecated version of the mempool to build the next block. We don't mark the from-main loop
                // received value as read yet as this would open up for race conditions if `main_loop` received
                // a block from a peer at the same time as this block was found.
                let _wait = from_main.changed().await;
                let msg = from_main.borrow().clone();
                debug!("Got {:?} msg from main after finding block", msg);
                if !matches!(msg, MainToMiner::ReadyToMineNextBlock) {
                    error!("Got bad message from `main_loop`: {:?}", msg);

                    // TODO: Handle this case
                    // We found a new block but the main thread updated with a block
                    // before our could be registered. We should mine on the one
                    // received from the main loop and not the one we found here.
                }
            }
        }
    }
    debug!("Miner shut down gracefully.");
    Ok(())
}

#[cfg(test)]
mod mine_loop_tests {
    use tracing_test::traced_test;

    use crate::{
        config_models::network::Network, models::state::UtxoReceiverData,
        tests::shared::get_mock_global_state,
    };

    use super::*;

    #[traced_test]
    #[tokio::test]
    async fn block_template_is_valid_test() -> Result<()> {
        // Verify that a block template made with transaction from the mempool is a valid block
        let premine_receiver_global_state = get_mock_global_state(Network::Main, 2, None).await;
        assert!(
            premine_receiver_global_state.mempool.is_empty(),
            "Mempool must be empty at startup"
        );

        // Verify constructed coinbase transaction and block template when mempool is empty
        let genesis_block = Block::genesis_block();
        let (transaction_empty_mempool, _coinbase_sender_randomness) =
            create_block_transaction(&genesis_block, &premine_receiver_global_state);
        assert_eq!(
            1,
            transaction_empty_mempool.kernel.outputs.len(),
            "Coinbase transaction with empty mempool must have exactly one output"
        );
        assert!(
            transaction_empty_mempool.kernel.inputs.is_empty(),
            "Coinbase transaction with empty mempool must have zero inputs"
        );
        let (block_header_template_empty_mempool, block_body_empty_mempool) =
            make_block_template(&genesis_block, transaction_empty_mempool);
        let block_template_empty_mempool = Block::new(
            block_header_template_empty_mempool,
            block_body_empty_mempool,
        );
        assert!(
            block_template_empty_mempool.is_valid(&genesis_block),
            "Block template created by miner with empty mempool must be valid"
        );

        // Add a transaction to the mempool
        let four_neptune_coins = Amount::from(4).to_native_coins();
        let receiver_privacy_digest = Digest::default();
        let sender_randomness = Digest::default();
        let pubscript: PubScript = PubScript::default();
        let pubscript_input: Vec<BFieldElement> = vec![];
        let tx_output = Utxo {
            coins: four_neptune_coins,
            lock_script_hash: LockScript::anyone_can_spend().hash(),
        };
        let tx_by_preminer = premine_receiver_global_state
            .create_transaction(
                vec![
                    (UtxoReceiverData {
                        utxo: tx_output,
                        sender_randomness,
                        receiver_privacy_digest,
                        pubscript,
                        pubscript_input,
                    }),
                ],
                1.into(),
            )
            .await
            .unwrap();
        premine_receiver_global_state
            .mempool
            .insert(&tx_by_preminer);
        assert_eq!(1, premine_receiver_global_state.mempool.len());

        // Build transaction
        let (transaction_non_empty_mempool, _new_coinbase_sender_randomness) =
            create_block_transaction(&genesis_block, &premine_receiver_global_state);
        assert_eq!(
            3,
            transaction_non_empty_mempool.kernel.outputs.len(),
            "Transaction for block with non-empty mempool must contain coinbase output, send output, and change output"
        );
        assert_eq!(1, transaction_non_empty_mempool.kernel.inputs.len(), "Transaction for block with non-empty mempool must contain one input: the genesis UTXO being spent");

        // Build and verify block template
        let (block_header_template, block_body) =
            make_block_template(&genesis_block, transaction_non_empty_mempool);
        let block_template_non_empty_mempool = Block::new(block_header_template, block_body);
        assert!(
            block_template_non_empty_mempool.is_valid(&genesis_block),
            "Block template created by miner with non-empty mempool must be valid"
        );

        Ok(())
    }
}
