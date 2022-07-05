use super::{
    blockchain::{
        block::{
            block_header::PROOF_OF_WORK_COUNT_U32_SIZE, block_height::BlockHeight,
            transfer_block::TransferBlock, Block,
        },
        digest::Digest,
    },
    shared::LatestBlockInfo,
};
use crate::config_models::network::Network;
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, time::SystemTime};
use twenty_first::amount::u32s::U32s;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Peer {
    pub address: SocketAddr,
    pub banscore: u8,
    pub instance_id: u128,
    pub inbound: bool,
    pub last_seen: SystemTime,
    pub version: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct HandshakeData {
    pub latest_block_info: LatestBlockInfo,
    pub listen_address: Option<SocketAddr>,
    pub network: Network,
    pub instance_id: u128,
    pub version: String,
}

/// Used to tell peers that a new block has been found without having toPeerMessage
/// send the entire block
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PeerBlockNotification {
    pub hash: Digest,
    pub height: BlockHeight,
    pub proof_of_work_family: U32s<PROOF_OF_WORK_COUNT_U32_SIZE>,
}

impl From<Block> for PeerBlockNotification {
    fn from(block: Block) -> Self {
        PeerBlockNotification {
            hash: block.hash,
            height: block.header.height,
            proof_of_work_family: block.header.proof_of_work_family,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum ConnectionRefusedReason {
    AlreadyConnected,
    MaxPeerNumberExceeded,
    SelfConnect,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq)]
pub enum ConnectionStatus {
    Refused(ConnectionRefusedReason),
    Accepted,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum PeerMessage {
    Handshake((Vec<u8>, HandshakeData)),
    Block(Box<TransferBlock>),
    BlockNotification(PeerBlockNotification),
    BlockRequestByHeight(BlockHeight),
    BlockRequestByHash(Digest),
    NewTransaction(i32),
    PeerListRequest,
    PeerListResponse(Vec<SocketAddr>),
    Bye,
    ConnectionStatus(ConnectionStatus),
}

#[derive(Clone, Debug)]
pub struct PeerState {
    pub highest_shared_block_height: BlockHeight,
    pub fork_reconciliation_blocks: Vec<Block>,
}

impl PeerState {
    pub fn default() -> Self {
        Self {
            highest_shared_block_height: 0.into(),
            fork_reconciliation_blocks: vec![],
        }
    }
}
